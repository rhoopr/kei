//! Establish stable album directories and finish any already-journaled move.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::download::file::primary_layout;
use crate::fs_util::{ConfinedParents, ConfinedPath, file_identity};
use crate::state::db::primary_layout::directory::{
    AlbumDirectoryBinding, relocated_path, same_container,
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
        let Some((op, _phase)) = pending else {
            // Only the recorded directory is authorized for this container.
            primary_layout::verify_album_directory(
                &root,
                &binding.path.to_path(),
                binding.identity,
            )
            .await?;
            continue;
        };
        anyhow::ensure!(
            same_container(&op.next.scope, &pass.album.selection_scope()),
            "Pending album rename requires its recorded provider selection"
        );
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Album directory preparation interrupted"
        );
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("album-directory-planned");
        if !op.directories.is_empty() {
            let route_root = op
                .old
                .additional_projection
                .as_ref()
                .context("Missing external album confinement")?
                .root
                .to_path();
            // Retain the journal on collisions: earlier routes may already have landed.
            for route in &op.directories {
                primary_layout::rename_album_directory(
                    &route_root,
                    &route.staged.to_path(),
                    &route.next.to_path(),
                    route.identity,
                )
                .await?;
                #[cfg(all(test, target_os = "linux"))]
                crate::test_helpers::process_death_point("album-additional-directory-published");
            }
        }
        if op.fallback.is_some() {
            primary_layout::verify_album_directory(&root, &op.old.path.to_path(), op.old.identity)
                .await?;
            if let Some(staged) = &op.staged_directory {
                let installed = primary_layout::rename_album_directory(
                    &root,
                    &staged.to_path(),
                    &op.next.path.to_path(),
                    op.next.identity,
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
                primary_layout::move_album_user_entry(&root, entry, &next).await?;
            }
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("album-files-copied");
            capture
                .db
                .publish_album_directory(capture.owner.clone(), op.operation, files)
                .await?;
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
        let moved = primary_layout::rename_album_directory(
            &root,
            &op.old.path.to_path(),
            &op.next.path.to_path(),
            op.old.identity,
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
                primary_layout::rename_album_directory(
                    &root,
                    &op.next.path.to_path(),
                    &op.old.path.to_path(),
                    op.old.identity,
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
                anyhow::bail!(
                    "Album media changed during movement; restored source for per-file replanning"
                );
            }
        }
        capture
            .db
            .publish_album_directory(capture.owner.clone(), op.operation, files)
            .await?;
    }
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
    let Some((op, _phase)) = pending else {
        return Ok(());
    };
    anyhow::ensure!(
        same_container(&op.next.scope, &pass.album.selection_scope()),
        "Resume the recorded album transition first"
    );
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("album-projection-planned");
    for (index, route) in op.directories.iter().enumerate() {
        anyhow::ensure!(!cancel.is_cancelled(), "Album projection interrupted");
        let moved = primary_layout::rename_album_directory(
            &root,
            &route.staged.to_path(),
            &route.next.to_path(),
            route.identity,
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
                    primary_layout::rename_album_directory(
                        &root,
                        &previous.next.to_path(),
                        &previous.staged.to_path(),
                        previous.identity,
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
    capture
        .db
        .publish_album_directory(capture.owner.clone(), op.operation, files)
        .await?;
    Ok(())
}
