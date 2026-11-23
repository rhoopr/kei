//! One exclusive `{album}` directory, moved as one entry with its user contents.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::download::file::primary_layout;
use crate::fs_util::{ConfinedParents, ConfinedPath, file_identity};
use crate::state::db::primary_layout::directory::{
    AlbumDirectoryBinding, AlbumDirectoryFallback, AlbumDirectoryFile, AlbumDirectoryOperation,
    AlbumDirectoryRoute, AlbumDirectorySource, relocated_path, same_container,
};
use crate::state::db::provider_selection::SelectionPath;

use super::config::DownloadConfig;

fn is_directory_collision(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
}

fn sidecar(path: &Path) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(".xmp");
    path.into()
}

fn claim_for(op: &AlbumDirectoryOperation, path: &Path) -> Option<crate::fs_util::FileIdentity> {
    op.claims
        .iter()
        .find(|claim| claim.path.to_path() == path)
        .map(|claim| claim.identity)
}

/// A destination collision can only be recovered by replanning from the
/// journal's recorded source. Cancel only while that source still holds its
/// journaled identity. The occupied destination is never removed, so the next
/// census may leave at most an orphaned empty name behind.
async fn cancel_unmoved_collision(
    capture: &crate::icloud::photos::inbox::ShadowCapture,
    op: &AlbumDirectoryOperation,
    root: &Path,
) -> Result<()> {
    primary_layout::verify_album_directory(root, &op.old.path.to_path(), op.old.identity).await?;
    capture
        .db
        .cancel_unmoved_album_directory(
            capture.owner.clone(),
            op.operation.clone(),
            op.old.identity,
        )
        .await?;
    Ok(())
}

/// True when every user entry still sits at its journaled source identity, so
/// the independent-file fallback has not moved any of them into the installed
/// directory yet.
async fn user_entries_unmoved(root: &Path, op: &AlbumDirectoryOperation) -> Result<bool> {
    for entry in &op.user_entries {
        let (root, path) = (root.to_owned(), entry.path.to_path());
        let identity = tokio::task::spawn_blocking(move || {
            ConfinedPath::open(&root, &path, ConfinedParents::Existing)
                .and_then(|confined| confined.entry_identity())
        })
        .await?;
        if identity.as_ref().ok() != Some(&entry.identity) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Move one journaled entry. A directory claim is recorded in the journal
/// before the plain rename consumes it, so replay recognises its own claim.
async fn move_entry(
    capture: &crate::icloud::photos::inbox::ShadowCapture,
    operation: &str,
    root: &Path,
    source: &Path,
    destination: &Path,
    identity: crate::fs_util::FileIdentity,
    claim: Option<crate::fs_util::FileIdentity>,
) -> Result<()> {
    let mut claim = claim;
    loop {
        match primary_layout::move_album_entry(root, source, destination, identity, claim).await? {
            primary_layout::AlbumMove::Moved => return Ok(()),
            primary_layout::AlbumMove::Claimed(installed) => {
                capture
                    .db
                    .record_album_directory_claim(
                        capture.owner.clone(),
                        operation.to_owned(),
                        SelectionPath::from_path(destination),
                        installed,
                    )
                    .await?;
                #[cfg(all(test, target_os = "linux"))]
                crate::test_helpers::process_death_point("album-directory-claim-recorded");
                claim = Some(installed);
            }
        }
    }
}

async fn create_directory(root: PathBuf, path: PathBuf) -> Result<crate::fs_util::FileIdentity> {
    tokio::task::spawn_blocking(move || {
        let entry = ConfinedPath::open(&root, &path, ConfinedParents::Create)?;
        let directory = entry.create_new_directory()?;
        let identity = file_identity(&directory)?;
        anyhow::ensure!(
            file_identity(&entry.open_directory()?)? == identity,
            "New album directory changed identity"
        );
        entry.sync_parent()?;
        Ok(identity)
    })
    .await?
}

fn candidate(root: &Path, name: &str, ordinal: u32) -> PathBuf {
    // Album hierarchies use '/' on every provider. Match the native
    // component spelling used by the download planner for durable receipts.
    let path: PathBuf = root.join(name).components().collect();
    if ordinal == 0 {
        path
    } else {
        let leaf = path.file_name().unwrap_or_default().to_string_lossy();
        path.with_file_name(format!("{leaf} ({ordinal})"))
    }
}

pub(super) fn template_parent(config: &DownloadConfig) -> Result<Option<PathBuf>> {
    let Some((prefix, suffix)) = config.folder_structure_albums.split_once("{album}") else {
        return Ok(None);
    };
    if (!prefix.is_empty() && !prefix.ends_with('/'))
        || (!suffix.is_empty() && !suffix.starts_with('/'))
        || prefix.contains('%')
    {
        return Ok(None);
    }
    let prefix = crate::download::paths::expand_named_token(
        prefix,
        "{library}",
        Some(crate::download::paths::truncate_library_zone(
            &config.library,
        )),
    );
    anyhow::ensure!(
        !prefix.contains(['{', '}']),
        "Unsupported album ancestor template"
    );
    let root = crate::fs_util::absolute_lexical(&config.directory)?;
    let parent = crate::fs_util::absolute_lexical(&root.join(prefix.replace("%%", "%")))?;
    anyhow::ensure!(
        parent.starts_with(&root),
        "Album ancestor escapes download root"
    );
    Ok(Some(parent))
}

pub(super) async fn prepare(
    passes: &[AlbumPass],
    config: &DownloadConfig,
    cancel: &CancellationToken,
) -> Result<()> {
    for pass in passes.iter().filter(|pass| pass.kind == PassKind::Album) {
        if cancel.is_cancelled() {
            anyhow::bail!("Album directory preparation interrupted");
        }
        let Some(container) = pass.album.container_id() else {
            continue;
        };
        let (capture, _, _) = pass
            .album
            .current_selection_scope()?
            .context("Album directory requires authenticated provider identity")?;
        let binding = capture
            .db
            .album_directory_binding(
                capture.owner.clone(),
                config.library.to_string(),
                container.to_owned(),
            )
            .await?;
        if binding
            .as_ref()
            .is_some_and(|binding| binding.projection.is_some())
            || (binding.is_none()
                && template_parent(config)?.is_none()
                && config.folder_structure_albums.contains("{album}"))
        {
            prepare_projection(pass, config, binding, cancel).await?;
            continue;
        }
        let Some(root) = binding
            .as_ref()
            .map(|binding| binding.root.to_path())
            .or(template_parent(config)?)
        else {
            anyhow::ensure!(
                binding
                    .as_ref()
                    .is_none_or(|binding| binding.applied_name == pass.album.name.as_ref()),
                "Album rename under a wider template remains unresolved"
            );
            continue;
        };
        // Exclusivity cannot be inferred from a rendered name. Mixed-template
        // selections need a complete overlap proof before they can own a folder.
        let mixed = passes.iter().any(|pass| pass.kind != PassKind::Album);
        tokio::fs::create_dir_all(&root).await?;
        let Some(binding) = binding else {
            if capture
                .db
                .album_directory_has_legacy_paths(
                    capture.owner.clone(),
                    config.library.to_string(),
                    container.to_owned(),
                    SelectionPath::from_path(&candidate(&root, &pass.album.name, 0)),
                )
                .await?
            {
                let path = candidate(&root, &pass.album.name, 0);
                let (open_root, open_path) = (root.clone(), path.clone());
                let identity = tokio::task::spawn_blocking(move || {
                    let entry =
                        ConfinedPath::open(&open_root, &open_path, ConfinedParents::Create)?;
                    let directory = match entry.open_directory() {
                        Ok(directory) => directory,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            entry.create_new_directory()?
                        }
                        Err(error) => return Err(error),
                    };
                    entry.sync_parent()?;
                    file_identity(&directory)
                })
                .await??;
                capture
                    .db
                    .bind_created_album_directory(
                        capture.owner.clone(),
                        AlbumDirectoryBinding {
                            format: 1,
                            library: config.library.to_string(),
                            container: container.to_owned(),
                            scope: pass.album.selection_scope(),
                            pass: super::generation::pass_key(pass),
                            root: SelectionPath::from_path(&root),
                            path: SelectionPath::from_path(&path),
                            identity,
                            generation: 1,
                            applied_name: pass.album.name.to_string(),
                            unclaimed: true,
                            projection: None,
                            additional_projection: None,
                        },
                    )
                    .await?;
                continue;
            }
            let mut ordinal = 0_u32;
            loop {
                anyhow::ensure!(
                    !cancel.is_cancelled(),
                    "Album directory preparation interrupted"
                );
                let path = candidate(&root, &pass.album.name, ordinal);
                if !capture
                    .db
                    .album_directory_path_available(
                        capture.owner.clone(),
                        SelectionPath::from_path(&path),
                    )
                    .await?
                {
                    ordinal = ordinal
                        .checked_add(1)
                        .context("Album directory collision budget exhausted")?;
                    continue;
                }
                match create_directory(root.clone(), path.clone()).await {
                    Ok(identity) => {
                        capture
                            .db
                            .bind_created_album_directory(
                                capture.owner.clone(),
                                AlbumDirectoryBinding {
                                    format: 1,
                                    library: config.library.to_string(),
                                    container: container.to_owned(),
                                    scope: pass.album.selection_scope(),
                                    pass: super::generation::pass_key(pass),
                                    root: SelectionPath::from_path(&root),
                                    path: SelectionPath::from_path(&path),
                                    identity,
                                    generation: 1,
                                    applied_name: pass.album.name.to_string(),
                                    unclaimed: mixed,
                                    projection: None,
                                    additional_projection: None,
                                },
                            )
                            .await?;
                        break;
                    }
                    Err(error) if is_directory_collision(&error) => {
                        ordinal = ordinal
                            .checked_add(1)
                            .context("Album directory collision budget exhausted")?;
                    }
                    Err(error) => return Err(error),
                }
            }
            continue;
        };
        anyhow::ensure!(
            binding.root.to_path() == root
                && same_container(&binding.scope, &pass.album.selection_scope()),
            "Album directory provider or root binding changed"
        );
        let pending = capture
            .db
            .album_directory_operation(capture.owner.clone(), binding.clone())
            .await?;
        let resumed_pending = pending.is_some();
        let binding = if pending.is_none()
            && config.folder_structure_albums.contains("{album}")
            && (binding.additional_projection.is_some()
                || template_parent(config)?.as_ref() != Some(&binding.root.to_path()))
        {
            capture
                .db
                .remember_album_additional_projection(
                    capture.owner.clone(),
                    binding,
                    SelectionPath::from_path(&crate::fs_util::absolute_lexical(&config.directory)?),
                    config.folder_structure_albums.to_string(),
                )
                .await?
        } else {
            binding
        };
        if pending.is_none() && binding.applied_name == pass.album.name.as_ref() {
            continue;
        }
        let op = if let Some((op, _)) = pending {
            anyhow::ensure!(
                same_container(&op.next.scope, &pass.album.selection_scope()),
                "Pending album rename requires its recorded provider selection"
            );
            op
        } else {
            let mut ordinal = 0_u32;
            let path = loop {
                anyhow::ensure!(
                    !cancel.is_cancelled(),
                    "Album directory preparation interrupted"
                );
                let path = candidate(&root, &pass.album.name, ordinal);
                if !capture
                    .db
                    .album_directory_path_available(
                        capture.owner.clone(),
                        SelectionPath::from_path(&path),
                    )
                    .await?
                {
                    ordinal = ordinal
                        .checked_add(1)
                        .context("Album directory collision budget exhausted")?;
                    continue;
                }
                match tokio::fs::symlink_metadata(&path).await {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break path,
                    Err(error) => return Err(error.into()),
                    Ok(_) => {
                        ordinal = ordinal
                            .checked_add(1)
                            .context("Album directory collision budget exhausted")?
                    }
                }
            };
            let next = AlbumDirectoryBinding {
                path: SelectionPath::from_path(&path),
                scope: pass.album.selection_scope(),
                pass: super::generation::pass_key(pass),
                applied_name: pass.album.name.to_string(),
                generation: binding
                    .generation
                    .checked_add(1)
                    .context("Album directory generation exhausted")?,
                unclaimed: mixed,
                ..binding.clone()
            };
            let mut op = AlbumDirectoryOperation {
                format: 1,
                operation: format!("album-directory:{}", uuid::Uuid::new_v4()),
                old: binding,
                next,
                fallback: None,
                staged_directory: None,
                directories: Vec::new(),
                user_entries: Vec::new(),
                claims: Vec::new(),
            };
            let sources = capture
                .db
                .album_directory_sources(capture.owner.clone(), op.old.clone())
                .await?;
            let mut files = Vec::with_capacity(sources.len());
            let unsettled_native = capture
                .db
                .album_directory_has_unsettled_publication(capture.owner.clone(), op.old.clone())
                .await?;
            let retained_history = capture
                .db
                .album_directory_has_preserved_history(capture.owner.clone(), op.old.clone())
                .await?;
            let mut needs_files = op.old.unclaimed
                || unsettled_native
                || mixed
                || op.old.path.to_path().parent() != op.next.path.to_path().parent()
                || retained_history;
            for source in sources {
                anyhow::ensure!(
                    !cancel.is_cancelled(),
                    "Album directory preparation interrupted"
                );
                let path = source.path.to_path();
                if !path.starts_with(op.old.path.to_path()) {
                    plan_projection_route(pass, config, &mut op, &source).await?;
                    needs_files = true;
                }
                let file_root = op
                    .old
                    .additional_projection
                    .as_ref()
                    .map_or(&op.old.root, |extra| &extra.root)
                    .to_path();
                for target in [&path, &sidecar(&path)] {
                    anyhow::ensure!(
                        !crate::download::file::has_replacement_journal(&file_root, target).await?,
                        "Finish the existing file publication journal before moving its album directory"
                    );
                }
                let fingerprint = primary_layout::snapshot(&file_root, &path).await?;
                let xmp = primary_layout::snapshot(&file_root, &sidecar(&path)).await?;
                needs_files |= fingerprint.as_ref().is_none_or(|actual| {
                    data_encoding::HEXLOWER.encode(&actual.sha256) != source.local_checksum
                }) || source
                    .expected_sidecar
                    .as_ref()
                    .is_some_and(|expected| expected != &xmp);
                files.push(AlbumDirectoryFile {
                    native: source.native,
                    library: source.library,
                    child: source.child,
                    version: source.version,
                    provider_checksum: source.provider_checksum,
                    next: relocated_path(&op, &source.path)?,
                    old: source.path,
                    fingerprint,
                    sidecar: xmp,
                    prior_local_checksum: Some(source.local_checksum),
                });
            }
            if needs_files {
                primary_layout::verify_album_directory(
                    &root,
                    &op.old.path.to_path(),
                    op.old.identity,
                )
                .await?;
                op.fallback = Some(AlbumDirectoryFallback::IndependentFiles);
                if !op.old.unclaimed && !unsettled_native && !retained_history {
                    op.user_entries =
                        primary_layout::album_user_entries(&root, &op.old.path.to_path(), &files)
                            .await?;
                }
                let id = op
                    .operation
                    .strip_prefix("album-directory:")
                    .context("Invalid album operation identity")?;
                let staged = op
                    .next
                    .path
                    .to_path()
                    .with_file_name(format!(".kei-album-stage-{id}"));
                op.next.identity = create_directory(root.clone(), staged.clone()).await?;
                op.staged_directory = Some(SelectionPath::from_path(&staged));
                #[cfg(all(test, target_os = "linux"))]
                crate::test_helpers::process_death_point("album-directory-before-journal");
                for file in &mut files {
                    file.next = relocated_path(&op, &file.old)?;
                }
            } else {
                for file in &mut files {
                    file.prior_local_checksum = None;
                }
            }
            capture
                .db
                .begin_album_directory_rename(capture.owner.clone(), op.clone(), files)
                .await?;
            op
        };
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Album directory preparation interrupted"
        );
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("album-directory-planned");
        let renamed_again = op.next.applied_name != pass.album.name.as_ref();
        if !op.directories.is_empty() {
            let route_root = op
                .old
                .additional_projection
                .as_ref()
                .context("Missing external album confinement")?
                .root
                .to_path();
            // Once a route has landed the journal is moved and must be
            // retained; only a collision before the first route can still be
            // replanned from the unmoved anchor.
            for (index, route) in op.directories.iter().enumerate() {
                let moved = move_entry(
                    &capture,
                    &op.operation,
                    &route_root,
                    &route.staged.to_path(),
                    &route.next.to_path(),
                    route.identity,
                    claim_for(&op, &route.next.to_path()),
                )
                .await;
                if let Err(error) = moved {
                    if index == 0 && is_directory_collision(&error) {
                        primary_layout::verify_album_directory(
                            &route_root,
                            &route.staged.to_path(),
                            route.identity,
                        )
                        .await?;
                        cancel_unmoved_collision(&capture, &op, &root).await?;
                    }
                    return Err(error);
                }
                #[cfg(all(test, target_os = "linux"))]
                crate::test_helpers::process_death_point("album-additional-directory-published");
            }
        }
        if op.fallback.is_some() {
            primary_layout::verify_album_directory(&root, &op.old.path.to_path(), op.old.identity)
                .await?;
            if let Some(staged) = &op.staged_directory {
                let installed = move_entry(
                    &capture,
                    &op.operation,
                    &root,
                    &staged.to_path(),
                    &op.next.path.to_path(),
                    op.next.identity,
                    claim_for(&op, &op.next.path.to_path()),
                )
                .await;
                if let Err(error) = installed {
                    if op.directories.is_empty() && is_directory_collision(&error) {
                        primary_layout::verify_album_directory(
                            &root,
                            &staged.to_path(),
                            op.next.identity,
                        )
                        .await?;
                        capture
                            .db
                            .cancel_unmoved_album_directory(
                                capture.owner.clone(),
                                op.operation.clone(),
                                op.old.identity,
                            )
                            .await?;
                    }
                    return Err(error);
                }
            }
            primary_layout::verify_album_directory(
                &root,
                &op.next.path.to_path(),
                op.next.identity,
            )
            .await?;
            let files = capture
                .db
                .album_directory_files(capture.owner.clone(), op.operation.clone())
                .await?;
            // The normal selected-file owner establishes each new destination
            // independently. Source media is relinquished in place, never used
            // as replacement authority after this census.
            for entry in &op.user_entries {
                let next = relocated_path(&op, &entry.path)?.to_path();
                let moved = move_entry(
                    &capture,
                    &op.operation,
                    &root,
                    &entry.path.to_path(),
                    &next,
                    entry.identity,
                    claim_for(&op, &next),
                )
                .await;
                if let Err(error) = moved {
                    // Cancel only while every entry is still at its source and
                    // no routed destination has landed, so no user tree is
                    // stranded in the installed directory.
                    if is_directory_collision(&error)
                        && op.directories.is_empty()
                        && user_entries_unmoved(&root, &op).await?
                    {
                        cancel_unmoved_collision(&capture, &op, &root).await?;
                    }
                    return Err(error);
                }
            }
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("album-files-copied");
            capture
                .db
                .publish_album_directory(capture.owner.clone(), op.operation, files)
                .await?;
            if renamed_again || resumed_pending {
                return Box::pin(prepare(passes, config, cancel)).await;
            }
            continue;
        }
        let files = capture
            .db
            .album_directory_files(capture.owner.clone(), op.operation.clone())
            .await?;
        if tokio::fs::try_exists(op.old.path.to_path()).await? {
            primary_layout::verify_album_directory(&root, &op.old.path.to_path(), op.old.identity)
                .await?;
            for file in &files {
                if primary_layout::snapshot(&root, &file.old.to_path()).await? != file.fingerprint
                    || primary_layout::snapshot(&root, &sidecar(&file.old.to_path())).await?
                        != file.sidecar
                {
                    capture
                        .db
                        .cancel_unmoved_album_directory(
                            capture.owner.clone(),
                            op.operation.clone(),
                            op.old.identity,
                        )
                        .await?;
                    anyhow::bail!(
                        "Album source changed after planning; retry with a fresh per-file census"
                    );
                }
            }
        }
        let moved = move_entry(
            &capture,
            &op.operation,
            &root,
            &op.old.path.to_path(),
            &op.next.path.to_path(),
            op.old.identity,
            claim_for(&op, &op.next.path.to_path()),
        )
        .await;
        if let Err(error) = moved {
            if is_directory_collision(&error) {
                let root = root.clone();
                let old = op.old.path.to_path();
                let identity = tokio::task::spawn_blocking(move || {
                    file_identity(
                        &ConfinedPath::open(&root, &old, ConfinedParents::Existing)?
                            .open_directory()?,
                    )
                })
                .await??;
                capture
                    .db
                    .cancel_unmoved_album_directory(
                        capture.owner.clone(),
                        op.operation.clone(),
                        identity,
                    )
                    .await?;
            }
            return Err(error);
        }
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("album-directory-durable");
        for file in &files {
            anyhow::ensure!(
                !cancel.is_cancelled(),
                "Album directory verification interrupted"
            );
            if primary_layout::snapshot(&root, &file.next.to_path()).await? != file.fingerprint
                || primary_layout::snapshot(&root, &sidecar(&file.next.to_path())).await?
                    != file.sidecar
            {
                let restored = move_entry(
                    &capture,
                    &op.operation,
                    &root,
                    &op.next.path.to_path(),
                    &op.old.path.to_path(),
                    op.old.identity,
                    claim_for(&op, &op.old.path.to_path()),
                )
                .await;
                if let Err(error) = restored {
                    if is_directory_collision(&error) {
                        // The album already moved, so this journal is not
                        // unmoved and cannot be cancelled. Preserve the
                        // occupied name and retain the recorded transition.
                        anyhow::bail!(
                            "Album media changed during movement and its source name is occupied; recorded transition retained: {error}"
                        );
                    }
                    return Err(error);
                }
                capture
                    .db
                    .cancel_unmoved_album_directory(
                        capture.owner.clone(),
                        op.operation.clone(),
                        op.old.identity,
                    )
                    .await?;
                anyhow::bail!(
                    "Album media changed during movement; restored source for per-file replanning"
                );
            }
        }
        capture
            .db
            .publish_album_directory(capture.owner.clone(), op.operation, files)
            .await?;
        if renamed_again || resumed_pending {
            return Box::pin(prepare(passes, config, cancel)).await;
        }
    }
    Ok(())
}

async fn plan_projection_route(
    pass: &AlbumPass,
    config: &DownloadConfig,
    op: &mut AlbumDirectoryOperation,
    source: &AlbumDirectorySource,
) -> Result<()> {
    let old = source.path.to_path();
    let parent = old.parent().context("Album file has no parent")?;
    if op
        .directories
        .iter()
        .any(|route| route.old.to_path() == parent)
    {
        return Ok(());
    }
    let (capture, _, _) = pass
        .album
        .current_selection_scope()?
        .context("Missing authenticated album selection")?;
    let (root, templates) = if let Some(extra) = &op.old.additional_projection {
        (extra.root.to_path(), &extra.templates)
    } else {
        (
            op.old.root.to_path(),
            op.old
                .projection
                .as_ref()
                .context("Missing album projection recipe")?,
        )
    };
    let previous = capture
        .db
        .album_projection_routes(capture.owner.clone(), op.old.clone())
        .await?;
    let date = source
        .created
        .context("Album path has no capture-date evidence")?;
    let render = |template: &str, name: &str| {
        let template = crate::download::paths::expand_named_token(
            template,
            "{library}",
            Some(crate::download::paths::truncate_library_zone(
                &config.library,
            )),
        );
        crate::download::paths::local_download_dir(&root, &template, &date, Some(name))
    };
    let recipe = previous
        .iter()
        .find(|route| route.next.to_path() == parent)
        .map(|route| route.template.as_str())
        .or_else(|| {
            templates
                .iter()
                .find(|template| render(template, &op.old.applied_name) == parent)
                .map(String::as_str)
        })
        // Unknown legacy recipes preserve source bytes and establish selection independently.
        .unwrap_or(&config.folder_structure_albums);
    let desired = render(recipe, &op.next.applied_name);
    anyhow::ensure!(
        !op.directories.iter().any(|route| desired
            .parent()
            .is_some_and(|parent| parent.starts_with(route.next.to_path()))),
        "Nested album route destinations require disjoint templates"
    );
    let mut ordinal = 0_u32;
    let next = loop {
        let candidate = if ordinal == 0 {
            desired.clone()
        } else {
            desired.with_file_name(format!(
                "{} ({ordinal})",
                desired
                    .file_name()
                    .context("Album path has no leaf")?
                    .to_string_lossy()
            ))
        };
        let overlaps = |other: &Path| candidate.starts_with(other) || other.starts_with(&candidate);
        anyhow::ensure!(
            op.old.projection.is_some()
                || (!overlaps(&op.old.path.to_path()) && !overlaps(&op.next.path.to_path())),
            "External album route overlaps its anchor"
        );
        if !op
            .directories
            .iter()
            .any(|route| overlaps(&route.next.to_path()))
            && capture
                .db
                .album_directory_path_available(
                    capture.owner.clone(),
                    SelectionPath::from_path(&candidate),
                )
                .await?
            && !tokio::fs::try_exists(&candidate).await?
        {
            break candidate;
        }
        ordinal = ordinal
            .checked_add(1)
            .context("Album collision budget exhausted")?;
    };
    let staged = next.with_file_name(format!(
        ".kei-album-stage-{}-{}",
        op.operation
            .strip_prefix("album-directory:")
            .context("Invalid album operation")?,
        op.directories.len()
    ));
    let identity = create_directory(root, staged.clone()).await?;
    op.directories.push(AlbumDirectoryRoute {
        template: recipe.to_owned(),
        old: SelectionPath::from_path(parent),
        next: SelectionPath::from_path(&next),
        staged: SelectionPath::from_path(&staged),
        identity,
    });
    Ok(())
}

async fn prepare_projection(
    pass: &AlbumPass,
    config: &DownloadConfig,
    binding: Option<AlbumDirectoryBinding>,
    cancel: &CancellationToken,
) -> Result<()> {
    let (capture, _, _) = pass
        .album
        .current_selection_scope()?
        .context("Missing authenticated album selection")?;
    let root = crate::fs_util::absolute_lexical(&config.directory)?;
    tokio::fs::create_dir_all(&root).await?;
    let opened_root = root.clone();
    let identity = tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        let directory = crate::fs_util::open_windows_directory(&opened_root)?;
        #[cfg(unix)]
        let directory = std::fs::File::open(opened_root)?;
        anyhow::ensure!(
            directory.metadata()?.is_dir(),
            "Album projection root is not a directory"
        );
        Ok::<_, anyhow::Error>(file_identity(&directory)?)
    })
    .await??;
    let Some(binding) = binding else {
        capture
            .db
            .bind_created_album_directory(
                capture.owner.clone(),
                AlbumDirectoryBinding {
                    format: 1,
                    library: config.library.to_string(),
                    container: pass
                        .album
                        .container_id()
                        .context("Missing album identity")?
                        .to_owned(),
                    scope: pass.album.selection_scope(),
                    pass: super::generation::pass_key(pass),
                    root: SelectionPath::from_path(&root),
                    path: SelectionPath::from_path(&root),
                    identity,
                    generation: 1,
                    applied_name: pass.album.name.to_string(),
                    unclaimed: true,
                    projection: Some(vec![config.folder_structure_albums.to_string()]),
                    additional_projection: None,
                },
            )
            .await?;
        return Ok(());
    };
    anyhow::ensure!(
        binding.root.to_path() == root && binding.identity == identity,
        "Album projection root changed"
    );
    anyhow::ensure!(
        same_container(&binding.scope, &pass.album.selection_scope()),
        "Album provider identity changed"
    );
    let pending = capture
        .db
        .album_directory_operation(capture.owner.clone(), binding.clone())
        .await?;
    let resumed_pending = pending.is_some();
    if pending.is_none() && binding.applied_name == pass.album.name.as_ref() {
        capture
            .db
            .remember_album_projection(
                capture.owner.clone(),
                binding,
                config.folder_structure_albums.to_string(),
            )
            .await?;
        return Ok(());
    }
    let op = if let Some((op, _phase)) = pending {
        anyhow::ensure!(
            same_container(&op.next.scope, &pass.album.selection_scope()),
            "Resume the recorded album transition first"
        );
        op
    } else {
        let next = AlbumDirectoryBinding {
            scope: pass.album.selection_scope(),
            pass: super::generation::pass_key(pass),
            applied_name: pass.album.name.to_string(),
            generation: binding
                .generation
                .checked_add(1)
                .context("Album generation exhausted")?,
            ..binding.clone()
        };
        let mut op = AlbumDirectoryOperation {
            format: 1,
            operation: format!("album-directory:{}", uuid::Uuid::new_v4()),
            old: binding,
            next,
            fallback: Some(AlbumDirectoryFallback::IndependentFiles),
            staged_directory: None,
            directories: Vec::new(),
            user_entries: Vec::new(),
            claims: Vec::new(),
        };
        let sources = capture
            .db
            .album_directory_sources(capture.owner.clone(), op.old.clone())
            .await?;
        let mut files = Vec::with_capacity(sources.len());
        for source in sources {
            anyhow::ensure!(!cancel.is_cancelled(), "Album projection interrupted");
            let old = source.path.to_path();
            plan_projection_route(pass, config, &mut op, &source).await?;
            for path in [&old, &sidecar(&old)] {
                anyhow::ensure!(
                    !crate::download::file::has_replacement_journal(&root, path).await?,
                    "Finish the existing file publication before rerouting its album path"
                );
            }
            files.push(AlbumDirectoryFile {
                native: source.native,
                library: source.library,
                child: source.child,
                version: source.version,
                provider_checksum: source.provider_checksum,
                next: relocated_path(&op, &source.path)?,
                old: source.path,
                fingerprint: primary_layout::snapshot(&root, &old).await?,
                sidecar: primary_layout::snapshot(&root, &sidecar(&old)).await?,
                prior_local_checksum: Some(source.local_checksum),
            });
        }
        capture
            .db
            .begin_album_directory_rename(capture.owner.clone(), op.clone(), files)
            .await?;
        op
    };
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("album-projection-planned");
    for (index, route) in op.directories.iter().enumerate() {
        anyhow::ensure!(!cancel.is_cancelled(), "Album projection interrupted");
        let moved = move_entry(
            &capture,
            &op.operation,
            &root,
            &route.staged.to_path(),
            &route.next.to_path(),
            route.identity,
            claim_for(&op, &route.next.to_path()),
        )
        .await;
        if let Err(error) = moved {
            if is_directory_collision(&error) {
                primary_layout::verify_album_directory(
                    &root,
                    &route.staged.to_path(),
                    route.identity,
                )
                .await?;
                // No media publication starts before every routed directory is
                // installed. Restore only the previously installed identities.
                for previous in op.directories.iter().take(index).rev() {
                    move_entry(
                        &capture,
                        &op.operation,
                        &root,
                        &previous.next.to_path(),
                        &previous.staged.to_path(),
                        previous.identity,
                        claim_for(&op, &previous.staged.to_path()),
                    )
                    .await?;
                }
                capture
                    .db
                    .cancel_unmoved_album_directory(
                        capture.owner.clone(),
                        op.operation.clone(),
                        op.old.identity,
                    )
                    .await?;
            }
            return Err(error);
        }
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("album-projection-directory-published");
    }
    let files = capture
        .db
        .album_directory_files(capture.owner.clone(), op.operation.clone())
        .await?;
    let renamed_again = op.next.applied_name != pass.album.name.as_ref();
    let next_binding = op.next.clone();
    capture
        .db
        .publish_album_directory(capture.owner.clone(), op.operation, files)
        .await?;
    if renamed_again || resumed_pending {
        Box::pin(prepare_projection(pass, config, Some(next_binding), cancel)).await?;
    }
    Ok(())
}
