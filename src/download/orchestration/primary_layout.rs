//! Owns recorded current-primary layout transitions across selected renditions.

use crate::download::{DownloadConfig, DownloadStore, file, filter, planner};
pub(super) mod membership;

use crate::icloud::photos::PhotoAsset;
use crate::state::db::primary_layout::{
    LayoutBinding, LayoutFile, LayoutMember, LayoutOperation, path_key,
};
use crate::state::db::provider_selection::SelectionPath;
use crate::state::{DownloadedFileRecord, ReconciliationCatalogPath, VersionSizeKey};
use crate::types::EditedNaming;
use anyhow::{Context, Result};
use file::primary_layout::{ManagedPrimaryAuthorization, copy_verified, snapshot};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub(in crate::download) struct LayoutOutcome {
    pub(in crate::download) downloaded: usize,
    pub(in crate::download) photos_downloaded: usize,
    pub(in crate::download) videos_downloaded: usize,
    pub(in crate::download) downloaded_assets: Vec<crate::download::recap::RecapAsset>,
    pub(in crate::download) network_bytes: u64,
    pub(in crate::download) disk_bytes: u64,
    pub(in crate::download) preserved: usize,
}

#[derive(Debug, Default)]
pub(in crate::download) struct LayoutSession {
    receipts: rustc_hash::FxHashMap<(String, String), Vec<DownloadedFileRecord>>,
    owners: rustc_hash::FxHashMap<String, Vec<ReconciliationCatalogPath>>,
    history_paths: rustc_hash::FxHashSet<String>,
    preview_claims: std::sync::Mutex<rustc_hash::FxHashSet<String>>,
}
impl LayoutSession {
    pub(in crate::download) async fn load(db: &dyn DownloadStore) -> Result<Self> {
        let mut receipts: rustc_hash::FxHashMap<(String, String), Vec<DownloadedFileRecord>> =
            rustc_hash::FxHashMap::default();
        let mut history_paths = rustc_hash::FxHashSet::default();
        for record in db.get_primary_layout_receipts().await? {
            if record.is_preserved_history
                && let Some(path) = &record.local_path
            {
                history_paths.insert(path_key(path)?);
            }
            receipts
                .entry((record.library.clone(), record.id.clone()))
                .or_default()
                .push(record);
        }
        let mut owners: rustc_hash::FxHashMap<String, Vec<ReconciliationCatalogPath>> =
            rustc_hash::FxHashMap::default();
        for record in db.get_reconciliation_catalog_paths().await? {
            owners
                .entry(path_key(&record.path)?)
                .or_default()
                .push(record);
        }
        Ok(Self {
            receipts,
            owners,
            history_paths,
            preview_claims: Default::default(),
        })
    }
    fn receipts(&self, library: &str, child: &str) -> impl Iterator<Item = &DownloadedFileRecord> {
        self.receipts
            .get(&(library.to_owned(), child.to_owned()))
            .into_iter()
            .flatten()
    }
    fn owners(&self, path: &Path) -> Result<impl Iterator<Item = &ReconciliationCatalogPath>> {
        Ok(self.owners.get(&path_key(path)?).into_iter().flatten())
    }
}

fn digest(value: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(value.as_ref()))
}
fn sidecar(path: &Path) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(".xmp");
    path.into()
}

/// Root changes must not turn a journal-owned source into ordinary copy input.
/// Inspect exact recorded targets before provider planning or new layout claims.
pub(super) async fn validate_recorded_sources(config: &DownloadConfig) -> Result<()> {
    let Some(db) = config.state_db.as_deref() else {
        return Ok(());
    };
    for record in db.get_primary_layout_receipts().await? {
        let Some(path) = record.local_path else {
            continue;
        };
        if !matches!(
            db.source_metadata_recovery(&path).await?,
            crate::state::db::SourceMetadataRecovery::ManagedPrimary
        ) {
            continue;
        }
        let parent = path
            .parent()
            .context("Recorded managed source has no parent")?;
        for target in [&path, &sidecar(&path)] {
            match file::has_replacement_journal(parent, target).await {
                Ok(false) => {}
                Ok(true) => {
                    anyhow::bail!("Recorded managed source has unfinished replacement evidence")
                }
                Err(error)
                    if super::reconciliation::classify_reconciliation_io_error(&error)
                        == Some(std::io::ErrorKind::NotFound) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}
fn policy(config: &DownloadConfig) -> &'static str {
    if config.edited_naming == EditedNaming::Primary {
        "primary"
    } else {
        "suffix"
    }
}

// Established layout and unfinished publication are durable facts, not a
// request to move files when the default naming policy changes. A pending
// operation takes precedence because it may already have published members.
fn recorded_layout_config(
    config: &DownloadConfig,
    binding: Option<&LayoutBinding>,
    operation: Option<&LayoutOperation>,
) -> Result<Option<Box<DownloadConfig>>> {
    let recorded = operation
        .map(|operation| operation.policy.as_str())
        .or_else(|| binding.map(|binding| binding.policy.as_str()));
    let naming = match recorded {
        None => return Ok(None),
        Some("primary") => EditedNaming::Primary,
        Some("suffix") => EditedNaming::Suffix,
        Some(_) => {
            anyhow::bail!("Unrecognized recorded layout policy; preserving its files and journal")
        }
    };
    if naming == config.edited_naming {
        return Ok(None);
    }
    let mut recorded_config = config.clone();
    recorded_config.edited_naming = naming;
    Ok(Some(Box::new(recorded_config)))
}

fn family(asset: &PhotoAsset, config: &DownloadConfig) -> Result<String> {
    let parent = crate::download::paths::local_download_dir(
        &config.directory,
        &config.folder_structure,
        &asset.created_local(),
        config.album_name.as_deref(),
    );
    family_in_parent(asset, config, &parent)
}

fn family_in_parent(asset: &PhotoAsset, config: &DownloadConfig, parent: &Path) -> Result<String> {
    Ok(crate::state::db::primary_layout::family_key(
        asset.source_zone().unwrap_or(&config.library),
        asset.state_id(),
        config
            .primary_layout_pass
            .as_deref()
            .unwrap_or(config.pass_label()),
        parent,
    )?)
}

async fn recorded_family(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
) -> Result<String> {
    let requested = family(asset, config)?;
    let Some(db) = config.state_db.as_deref() else {
        return Ok(requested);
    };
    if db
        .primary_layout_binding(requested.clone())
        .await?
        .is_some()
    {
        return Ok(requested);
    }
    let mut candidates = rustc_hash::FxHashSet::default();
    for record in session.receipts(
        asset.source_zone().unwrap_or(&config.library),
        asset.state_id(),
    ) {
        if record.is_preserved_history {
            continue;
        }
        let Some(path) = &record.local_path else {
            continue;
        };
        if crate::fs_util::reserved_path_under_root(&config.directory, path).is_err() {
            continue;
        }
        let Some(parent) = path.parent() else {
            continue;
        };
        let key = family_in_parent(asset, config, parent)?;
        if db.primary_layout_binding(key.clone()).await?.is_some() {
            candidates.insert(key);
        }
    }
    for operation in db
        .primary_layout_operations(config.library.to_string())
        .await?
    {
        if operation.child != asset.state_id()
            || operation.pass
                != config
                    .primary_layout_pass
                    .as_deref()
                    .unwrap_or(config.pass_label())
        {
            continue;
        }
        for member in &operation.members {
            let Some(destination) = &member.destination else {
                continue;
            };
            let path = destination.to_path();
            if crate::fs_util::reserved_path_under_root(&config.directory, &path).is_err() {
                continue;
            }
            let Some(parent) = path.parent() else {
                continue;
            };
            if family_in_parent(asset, config, parent)? == operation.family {
                candidates.insert(operation.family.clone());
            }
        }
    }
    // Multiple historical copies are retained; they do not identify one
    // established destination for this pass.
    Ok(if candidates.len() == 1 {
        candidates.into_iter().next().unwrap_or(requested)
    } else {
        requested
    })
}
fn decision(asset: &PhotoAsset, config: &DownloadConfig) -> Result<String> {
    let paths = filter::derive_expected_paths(asset, config);
    let facts: Vec<_> = paths
        .iter()
        .map(|p| {
            serde_json::json!([
                p.version_size.as_str(),
                p.checksum,
                p.size,
                SelectionPath::from_path(&p.path),
                filter::metadata_for_selected_version(asset, config, p.version_size).metadata_hash,
                filter::build_selected_payload(asset, config, p.version_size)
            ])
        })
        .collect();
    Ok(digest(serde_json::to_vec(&serde_json::json!([
        1,
        super::config::hash_download_config(config),
        crate::download::pipeline::MetadataFlags::from(config).bits(),
        crate::download::pipeline::MetadataFlags::POLICY_REVISION,
        format!("{:?}", config.capture_timestamp_repair),
        asset.asset_record_name(),
        asset.created(),
        facts
    ]))?))
}

fn metadata_decision(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    version: VersionSizeKey,
    checksum: &str,
) -> Result<String> {
    Ok(digest(serde_json::to_vec(&serde_json::json!([
        1,
        version.as_str(),
        checksum,
        crate::download::pipeline::MetadataFlags::from(config).bits(),
        crate::download::pipeline::MetadataFlags::POLICY_REVISION,
        format!("{:?}", config.capture_timestamp_repair),
        asset.created(),
        filter::build_selected_payload(asset, config, version)
    ]))?))
}

async fn load_source_fields(
    db: &dyn DownloadStore,
    library: &str,
    id: &str,
    file: &mut LayoutFile,
) -> Result<()> {
    if let Some(receipt) = db
        .embedded_metadata_receipt(library, id, &file.version, &file.path.to_path(), None)
        .await?
    {
        let checksum = data_encoding::HEXLOWER.encode(&file.fingerprint.sha256);
        if checksum == receipt.output_checksum {
            *file.embedded_fields = receipt.written;
        } else if !receipt.published && checksum == receipt.input_checksum {
            *file.embedded_fields = receipt.previous;
        }
    }
    Ok(())
}

async fn file_receipt(
    root: &Path,
    path: &Path,
    version: &str,
    checksum: &str,
    source_checksum: Option<String>,
) -> Result<LayoutFile> {
    Ok(LayoutFile {
        local_preserved: false,
        local_modified: None,
        embedded_fields: Default::default(),
        path: SelectionPath::from_path(path),
        version: version.to_owned(),
        provider_checksum: checksum.to_owned(),
        fingerprint: snapshot(root, path).await?.context("Missing layout file")?,
        source_checksum,
        sidecar: snapshot(root, &sidecar(path)).await?,
        archive_original: false,
        metadata_decision: None,
    })
}
async fn validate_file(root: &Path, file: &LayoutFile) -> Result<()> {
    if let Some(expected) = file.local_modified {
        let retained = file::retain_pending_file(root, &file.path.to_path())
            .await?
            .context("Missing local layout file")?;
        anyhow::ensure!(
            retained.modified.map(chrono::DateTime::<chrono::Utc>::from) == Some(expected),
            "Owned layout modification time changed locally"
        );
    }
    anyhow::ensure!(
        snapshot(root, &file.path.to_path()).await?.as_ref() == Some(&file.fingerprint),
        "Owned layout file changed locally"
    );
    anyhow::ensure!(
        snapshot(root, &sidecar(&file.path.to_path())).await? == file.sidecar,
        "Owned layout sidecar changed locally"
    );
    Ok(())
}

pub(super) async fn validate_cancelled_selection_layouts(
    operations: &[LayoutOperation],
) -> Result<()> {
    for op in operations {
        anyhow::ensure!(
            matches!(op.phase.as_str(), "planned" | "prepared" | "preserved"),
            "Successor cannot retire a publishing journal"
        );
        for member in &op.members {
            anyhow::ensure!(
                member.installed.is_none(),
                "Successor cannot retire installed journal members"
            );
            for file in [
                &member.old,
                &member.prepared,
                &member.preserved,
                &member.archive_source,
            ]
            .into_iter()
            .flatten()
            {
                validate_file(&op.root.to_path(), file)
                    .await
                    .context("Cancelled layout input changed")?;
                for target in [file.path.to_path(), sidecar(&file.path.to_path())] {
                    anyhow::ensure!(
                        !file::has_replacement_journal(&op.root.to_path(), &target).await?,
                        "Cancelled layout retains unfinished replacement evidence"
                    );
                }
            }
        }
    }
    Ok(())
}

fn qualified_path(path: &Path, qualifier: &str, role: filter::NamingRole) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("Layout filename is not Unicode")?;
    let base = if role == filter::NamingRole::OriginalArchive {
        let (stem, ext) = name
            .rsplit_once('.')
            .context("Archive original has no extension")?;
        let stem = stem
            .strip_suffix("_original")
            .context("Archive original has no terminal role")?;
        format!("{stem}.{ext}")
    } else {
        name.to_owned()
    };
    Ok(path.with_file_name(crate::download::paths::role_filename(
        &base,
        qualifier,
        role.suffix(),
    )))
}

fn bound_destination(
    derived: &filter::DerivedPath,
    qualifier: &str,
    binding: Option<&LayoutBinding>,
) -> Result<PathBuf> {
    let destination = qualified_path(&derived.path, qualifier, derived.naming_role)?;
    if let Some(file) = binding.and_then(|binding| {
        binding.files.iter().find(|file| {
            file.version == derived.version_size.as_str()
                && file.archive_original
                    == (derived.naming_role == filter::NamingRole::OriginalArchive)
                && (file.provider_checksum == derived.checksum.as_ref()
                    || file.path.to_path().extension() == destination.extension())
        })
    }) {
        return Ok(file.path.to_path());
    }
    if let Some(file) =
        binding.and_then(|binding| binding.files.iter().find(|file| !file.archive_original))
    {
        let recorded = file.path.to_path();
        let parent = recorded.parent().context("Recorded layout has no parent")?;
        return Ok(parent.join(
            destination
                .file_name()
                .context("Layout destination has no filename")?,
        ));
    }
    Ok(destination)
}

/// A source observation does not authorize a revert. Confirmation belongs to
/// the Photos adapter and uses its complete current-resource projection.
async fn confirm(asset: &PhotoAsset, config: &DownloadConfig) -> Result<PhotoAsset> {
    let source = config
        .primary_layout_source
        .as_ref()
        .context("Primary layout has no current provider source")?;
    let confirmed = source
        .confirm_layout_asset(
            asset.asset_record_name(),
            asset.source_zone().unwrap_or(&config.library),
        )
        .await?;
    anyhow::ensure!(
        confirmed.asset.source_zone() == asset.source_zone() || asset.source_zone().is_none(),
        "Primary layout provider scope changed"
    );
    Ok(confirmed.asset.with_state_record_name(asset.state_id_arc()))
}

/// The ordinary whole-asset metadata refresher must not create obligations
/// for slots whose selected-resource metadata is owned by the layout journal.
pub(in crate::download) async fn owns_metadata(
    asset: &PhotoAsset,
    config: &DownloadConfig,
) -> Result<bool> {
    if !config.primary_layout_active {
        return Ok(false);
    }
    let db = config
        .state_db
        .as_deref()
        .context("Managed layout requires a database")?;
    // The ordinary refresh touches every stored path for this provider child.
    // A base config has no concrete pass key, so a family miss cannot establish
    // that all of those paths are ordinary. Claims also cover pending bindings.
    if db
        .primary_layout_owns_asset(
            asset.source_zone().unwrap_or(&config.library),
            asset.state_id(),
        )
        .await?
    {
        return Ok(true);
    }
    Ok(config.edited_naming == EditedNaming::Primary
        && filter::is_asset_filtered(asset, config).is_none())
}

/// Returns None for an ordinary suffix family that has never been managed.
pub(in crate::download) async fn process_asset(
    client: &reqwest::Client,
    observed: PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
    cancel: &CancellationToken,
) -> Result<Option<(PhotoAsset, LayoutOutcome)>> {
    let mut current_config;
    let config = if config.selection_run.is_some() {
        let db = config
            .state_db
            .as_deref()
            .context("Managed layout requires a database")?;
        let rows = db
            .get_asset_groupings(
                observed.source_zone().unwrap_or(&config.library),
                &[observed.state_id()],
            )
            .await?;
        let mut groupings = crate::download::AssetGroupings::default();
        for (asset, album) in rows.albums {
            groupings.albums.entry(asset).or_default().push(album);
        }
        for (asset, person) in rows.people {
            groupings.people.entry(asset).or_default().push(person);
        }
        current_config = config.clone();
        current_config.asset_groupings = Arc::new(groupings);
        &current_config
    } else {
        config
    };
    let key = recorded_family(&observed, config, session).await?;
    let result = async {
        let result = process_asset_inner(client, observed, config, session, cancel).await?;
        if config.selection_run.is_some()
            && let Some((asset, _)) = &result
            && let Some(db) = config.state_db.as_deref()
            && let Some(binding) = db.primary_layout_binding(key.clone()).await?
        {
            anyhow::ensure!(
                acknowledge_binding(asset, config, &binding, cancel).await?,
                "Managed layout has not completed the selected metadata recipe"
            );
        }
        Ok(result)
    }
    .await;
    if result.is_err()
        && let Some(db) = config.state_db.as_deref()
    {
        for op in db
            .primary_layout_operations(config.library.to_string())
            .await?
        {
            if op.family == key {
                db.hold_primary_layout(op.operation).await?;
            }
        }
    }
    result
}

async fn acknowledge_binding(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    binding: &LayoutBinding,
    cancel: &CancellationToken,
) -> Result<bool> {
    let mut paths: Vec<_> = binding
        .files
        .iter()
        .map(|file| file.path.to_path())
        .collect();
    paths.sort();
    paths.dedup();
    let mut guards = Vec::new();
    for path in &paths {
        guards.push(file::lock_download_destination(path, cancel).await?);
    }
    for file in &binding.files {
        if validate_file(&config.directory, file).await.is_err() {
            return Ok(false);
        }
        if config.selection_run.is_some()
            && filter::derive_expected_paths(asset, config)
                .iter()
                .any(|selected| selected.version_size.as_str() == file.version)
            && file.metadata_decision.as_deref()
                != Some(
                    metadata_decision(
                        asset,
                        config,
                        VersionSizeKey::from_str(&file.version)
                            .context("Invalid managed rendition")?,
                        &file.provider_checksum,
                    )?
                    .as_str(),
                )
        {
            return Ok(false);
        }
    }
    anyhow::ensure!(
        !cancel.is_cancelled(),
        "Primary layout interrupted before acknowledgement"
    );
    if let Some(run) = &config.selection_run {
        anyhow::ensure!(
            run.root.spec.metadata_flags
                == crate::download::pipeline::MetadataFlags::from(config).bits(),
            "Managed metadata flags differ from selected intent"
        );
        run.capture
            .db
            .reuse_layout_selection_publication(
                run.capture.owner.clone(),
                run.root.id.clone(),
                binding.clone(),
            )
            .await?;
    }
    Ok(true)
}

fn binding_matches_selection(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    binding: &LayoutBinding,
) -> bool {
    let selected = filter::derive_expected_paths(asset, config);
    !selected.is_empty()
        && binding
            .files
            .iter()
            .filter(|file| !file.archive_original)
            .count()
            == selected.len()
        && selected.iter().all(|current| {
            binding.files.iter().any(|file| {
                file.version == current.version_size.as_str()
                    && file.provider_checksum == current.checksum.as_ref()
                    && file.path.to_path() == current.path
                    && metadata_decision(asset, config, current.version_size, &current.checksum)
                        .is_ok_and(|decision| {
                            file.metadata_decision.as_deref() == Some(decision.as_str())
                        })
            })
        })
}

async fn process_asset_inner(
    client: &reqwest::Client,
    observed: PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
    cancel: &CancellationToken,
) -> Result<Option<(PhotoAsset, LayoutOutcome)>> {
    let db = config
        .state_db
        .as_deref()
        .context("Primary naming requires an owned state database")?;
    let family = recorded_family(&observed, config, session).await?;
    let binding = db.primary_layout_binding(family.clone()).await?;
    let pending = db
        .primary_layout_operations(config.library.to_string())
        .await?;
    let recorded_config = recorded_layout_config(
        config,
        binding.as_ref(),
        pending.iter().find(|op| op.family == family),
    )?;
    let config = recorded_config.as_deref().unwrap_or(config);
    if config.edited_naming == EditedNaming::Suffix
        && binding.is_none()
        && !db
            .primary_layout_owns_asset(
                observed.source_zone().unwrap_or(&config.library),
                observed.state_id(),
            )
            .await?
    {
        return Ok(None);
    }
    if binding.is_none() && filter::derive_expected_paths(&observed, config).is_empty() {
        return Ok(Some((observed, LayoutOutcome::default())));
    }
    if filter::is_asset_filtered(&observed, config).is_some() {
        return Ok(None);
    }
    if cancel.is_cancelled() {
        anyhow::bail!("Primary layout interrupted")
    }
    let has_pending = pending.iter().any(|op| op.family == family);
    let pending_op = pending.iter().find(|op| op.family == family);
    let (repairing, repair_input_changed) =
        repair_plan_status(&observed, config, binding.as_ref(), pending_op).await?;
    if !has_pending
        && !repairing
        && !config.refresh_metadata
        && binding.as_ref().is_some_and(|b| {
            (b.decision == decision(&observed, config).unwrap_or_default()
                || binding_matches_selection(&observed, config, b))
                && b.policy == policy(config)
        })
        && acknowledge_binding(
            &observed,
            config,
            binding.as_ref().context("Missing committed binding")?,
            cancel,
        )
        .await?
    {
        return Ok(Some((observed, LayoutOutcome::default())));
    }
    let asset = confirm(&observed, config).await?;
    // Advertised malformed adjusted resources are unknown, not absence.
    anyhow::ensure!(
        !asset.malformed_resources().iter().any(|r| matches!(
            r.version_size,
            crate::types::AssetVersionSize::Adjusted | crate::types::AssetVersionSize::LiveAdjusted
        )),
        "Primary layout waits for usable adjusted resource evidence"
    );
    let desired = decision(&asset, config)?;
    if let Some(binding) = &binding
        && !has_pending
        && !repairing
        && !config.refresh_metadata
        && (binding.decision == desired || binding_matches_selection(&asset, config, binding))
        && binding.policy == policy(config)
        && acknowledge_binding(&asset, config, binding, cancel).await?
    {
        return Ok(Some((asset, LayoutOutcome::default())));
    }
    let mut op = if let Some(op) = pending.into_iter().find(|op| op.family == family) {
        if op.decision == desired && !repair_input_changed {
            op
        } else {
            if op.phase == "publishing" {
                anyhow::ensure!(
                    !op.is_membership_retirement(),
                    "Membership retirement requires completed current source evidence"
                );
                finish(db, &mut op.clone(), cancel, &mut LayoutOutcome::default()).await?;
            } else {
                db.cancel_primary_layout(op.operation).await?;
            }
            let binding = db.primary_layout_binding(family.clone()).await?;
            plan(
                &asset,
                config,
                session,
                binding.as_ref(),
                family,
                desired,
                true,
            )
            .await?
        }
    } else {
        plan(
            &asset,
            config,
            session,
            binding.as_ref(),
            family,
            desired,
            true,
        )
        .await?
    };
    let mut outcome = LayoutOutcome::default();
    if op.phase == "planned" || op.phase == "prepared" {
        prepare(
            client,
            &asset,
            config,
            session,
            &mut op,
            cancel,
            &mut outcome,
        )
        .await?;
        op.phase = "prepared".into();
        db.save_primary_layout(op.clone()).await?;
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("layout-prepared");
    }
    let current = confirm(&asset, config).await?;
    anyhow::ensure!(
        decision(&current, config)? == op.decision,
        "Provider changed during primary layout preparation"
    );
    finish(db, &mut op, cancel, &mut outcome).await?;
    tracing::info!(family=%op.family,preservation_receipts=outcome.preserved,network_downloads=outcome.downloaded,network_bytes=outcome.network_bytes,"Primary layout committed with independently preserved prior contents");
    Ok(Some((asset, outcome)))
}

pub(in crate::download) async fn has_confirmed_repair(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    tasks: &[filter::DownloadTask],
) -> Result<bool> {
    for task in tasks {
        if confirmed_damage(
            asset,
            config,
            &SelectionPath::from_path(&task.download_path),
        )
        .await?
        .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn repair_plan_status(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    binding: Option<&LayoutBinding>,
    op: Option<&LayoutOperation>,
) -> Result<(bool, bool)> {
    let mut repairing = false;
    let mut changed = false;
    if binding.is_none() {
        // A suffix-to-primary transition can inherit a confirmed ordinary
        // copy before the first layout binding exists. Resolve its selected
        // target without creating a journal ahead of admission.
        for selected in filter::derive_expected_paths(asset, config) {
            if confirmed_damage(asset, config, &SelectionPath::from_path(&selected.path))
                .await?
                .is_some()
            {
                return Ok((true, false));
            }
        }
    }
    if let Some(binding) = binding {
        for file in &binding.files {
            if let Some((checksum, size)) = confirmed_damage(asset, config, &file.path).await? {
                repairing = true;
                changed |= op.is_some_and(|op| {
                    !op.members
                        .iter()
                        .filter_map(|member| member.old.as_ref())
                        .any(|old| {
                            old.path == file.path
                                && old.fingerprint.size == size
                                && data_encoding::HEXLOWER.encode(&old.fingerprint.sha256)
                                    == checksum
                        })
                });
            }
        }
    }
    Ok((repairing, changed))
}

async fn confirmed_damage(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    path: &SelectionPath,
) -> Result<Option<(String, u64)>> {
    let (Some(run), Some(key)) = (&config.selection_run, &config.selection_pass) else {
        return Ok(None);
    };
    for current in filter::derive_expected_paths(asset, config) {
        if let Some(confirmation) = run
            .capture
            .db
            .retained_integrity_confirmation(
                run.capture.owner.clone(),
                run.root.spec.scope.clone(),
                key.clone(),
                (asset.asset_record_name().to_owned(), asset.id().to_owned()),
                (
                    path.clone(),
                    current.version_size.as_str().to_owned(),
                    current.checksum.to_string(),
                    current.size,
                ),
            )
            .await?
        {
            return Ok(Some(confirmation));
        }
    }
    Ok(None)
}

async fn plan(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
    binding: Option<&LayoutBinding>,
    family: String,
    desired: String,
    persist: bool,
) -> Result<LayoutOperation> {
    let db = config.state_db.as_deref();
    anyhow::ensure!(!persist || db.is_some(), "Missing primary layout database");
    let mut oldfiles = binding.map_or(Vec::new(), |b| b.files.clone());
    if binding.is_none() {
        let parent = crate::download::paths::local_download_dir(
            &config.directory,
            &config.folder_structure,
            &asset.created_local(),
            config.album_name.as_deref(),
        );
        for record in session.receipts(
            asset.source_zone().unwrap_or(&config.library),
            asset.state_id(),
        ) {
            let Some(path) = &record.local_path else {
                continue;
            };
            // Each durable per-path receipt pins its own historical filename,
            // rendition and provider/local checksums. Neither current provider
            // selection nor the latest catalogue filename can census earlier
            // edits or extensions. Only this pass's parent is eligible; other
            // roots and private history remain retained reuse sources.
            if record.is_preserved_history
                || path.parent() != Some(parent.as_path())
                || session.history_paths.contains(&path_key(path)?)
                || if let Some(run) = &config.selection_run {
                    run.album_path_claimed(config, path).await?
                } else {
                    false
                }
            {
                continue;
            }
            let Some(actual) = snapshot(&config.directory, path).await? else {
                continue;
            };
            let checksum = record
                .local_checksum
                .as_deref()
                .context("Legacy layout media has no trusted local checksum")?;
            anyhow::ensure!(
                !record.checksum.is_empty(),
                "Legacy layout media has no trusted provider rendition"
            );
            let locally_changed = data_encoding::HEXLOWER.encode(&actual.sha256) != checksum;
            anyhow::ensure!(
                !locally_changed
                    || filter::derive_expected_paths(asset, config)
                        .iter()
                        .any(|derived| derived.version_size == record.version_size
                            && derived.path == *path),
                "Changed legacy media cannot be relocated to another rendition slot"
            );
            anyhow::ensure!(
                session.owners(path)?.any(|owner| {
                    owner.library.as_ref() == record.library
                        && owner.asset_id.as_ref() == record.id
                        && owner.version_size == record.version_size
                }) && session
                    .owners(path)?
                    .all(|owner| owner.library.as_ref() == record.library
                        && owner.asset_id.as_ref() == record.id),
                "Ambiguous legacy primary path owner"
            );
            if oldfiles.iter().all(|f| f.path.to_path() != *path) {
                let mut file = file_receipt(
                    &config.directory,
                    path,
                    record.version_size.as_str(),
                    &record.checksum,
                    record.download_checksum.clone(),
                )
                .await?;
                file.local_preserved = locally_changed;
                if locally_changed {
                    file.source_checksum = None;
                }
                anyhow::ensure!(
                    file.fingerprint == actual,
                    "Legacy layout media changed during ownership verification"
                );
                if let Some(db) = db {
                    load_source_fields(db, &record.library, &record.id, &mut file).await?;
                }
                oldfiles.push(file);
            }
        }
    }
    let mut repairs = Vec::new();
    let mut present = Vec::with_capacity(oldfiles.len());
    for mut old in oldfiles {
        let confirmed = confirmed_damage(asset, config, &old.path).await?;
        let Some(current) =
            file::retain_pending_file(&config.directory, &old.path.to_path()).await?
        else {
            anyhow::ensure!(confirmed.is_none(), "Confirmed managed damage disappeared");
            present.push(old);
            continue;
        };
        let changed = current.fingerprint.sha256 != old.fingerprint.sha256
            || current.fingerprint.size != old.fingerprint.size;
        if let Some((checksum, size)) = confirmed {
            anyhow::ensure!(
                current.fingerprint.size == size
                    && data_encoding::HEXLOWER.encode(&current.fingerprint.sha256) == checksum,
                "Confirmed damaged destination changed; replacement refused"
            );
            repairs.push(old.path.clone());
        }
        if changed || old.local_preserved {
            let mut observed = file_receipt(
                &config.directory,
                &old.path.to_path(),
                &old.version,
                &old.provider_checksum,
                None,
            )
            .await?;
            observed.local_preserved = true;
            observed.local_modified = current.modified.map(chrono::DateTime::<chrono::Utc>::from);
            observed.archive_original = old.archive_original;
            if !changed {
                observed.embedded_fields = old.embedded_fields;
            }
            old = observed;
        } else {
            old.local_modified = None;
        }
        current.validate().await?;
        validate_file(&config.directory, &old).await?;
        present.push(old);
    }
    oldfiles = present;
    let paths = filter::derive_expected_paths(asset, config);
    let mut missingfiles = Vec::new();
    let mut present = Vec::with_capacity(oldfiles.len());
    for file in oldfiles {
        if snapshot(&config.directory, &file.path.to_path())
            .await?
            .is_none()
        {
            let selected = paths
                .iter()
                .find(|path| path.version_size.as_str() == file.version);
            anyhow::ensure!(
                selected.is_some_and(|path| bound_destination(
                    path,
                    binding.map_or("", |b| b.qualifier.as_str()),
                    binding
                )
                .ok()
                .as_ref()
                    == Some(&file.path.to_path())),
                "Missing retained media has no matching current provider rendition"
            );
            anyhow::ensure!(
                snapshot(&config.directory, &sidecar(&file.path.to_path())).await? == file.sidecar,
                "Missing media sidecar changed locally"
            );
            missingfiles.push(file);
        } else {
            validate_file(&config.directory, &file).await?;
            present.push(file);
        }
    }
    let oldfiles = present;
    let preview_claims = if persist {
        Default::default()
    } else {
        session
            .preview_claims
            .lock()
            .map_err(|_error| anyhow::anyhow!("Preview path index poisoned"))?
            .clone()
    };
    let mut qualifier = binding.map_or(String::new(), |b| b.qualifier.clone());
    if binding.is_none() {
        for derived in &paths {
            if config
                .selection_run
                .as_ref()
                .is_some_and(|run| run.colliding_album_parent(asset, config))
                || oldfiles.iter().all(|f| f.path.to_path() != derived.path)
                    && (snapshot(&config.directory, &derived.path).await?.is_some()
                        || session.owners(&derived.path)?.any(|owner| {
                            owner.library.as_ref() != asset.source_zone().unwrap_or(&config.library)
                                || owner.asset_id.as_ref() != asset.state_id()
                        })
                        || preview_claims.contains(&path_key(&derived.path)?)
                        || if let Some(db) = db {
                            db.guard_primary_slot(
                                asset.source_zone().unwrap_or(&config.library),
                                asset.state_id(),
                                derived.version_size.as_str(),
                                &derived.checksum,
                                &derived.path,
                            )
                            .await
                            .is_err()
                        } else {
                            false
                        })
            {
                qualifier = format!(
                    "-{}",
                    digest(serde_json::to_vec(&serde_json::json!([
                        family,
                        asset.state_id()
                    ]))?)
                );
                break;
            }
        }
    }
    let operation = uuid::Uuid::new_v4().to_string();
    let mut members = Vec::new();
    let mut destinations = Vec::new();
    for derived in paths {
        let destination = bound_destination(&derived, &qualifier, binding)?;
        let target = snapshot(&config.directory, &destination).await?;
        let old = oldfiles
            .iter()
            .find(|f| f.path.to_path() == destination)
            .cloned();
        if !repairs.is_empty() && !repairs.iter().any(|path| path.to_path() == destination) {
            let unchanged =
                old.context("Integrity repair cannot introduce another managed destination")?;
            destinations.push(destination);
            members.push(LayoutMember {
                missing: None,
                record: None,
                destination: None,
                path_key: None,
                stage: None,
                old: None,
                archive_source: None,
                preserved_path: None,
                retirement: None,
                prepared: None,
                preserved: None,
                installed: Some(unchanged),
            });
            continue;
        }
        anyhow::ensure!(
            target.is_none() || old.is_some(),
            "Foreign file occupies a reserved primary family path"
        );
        let stage = destination
            .parent()
            .context("Primary has no parent")?
            .join(".kei-history")
            .join(&family)
            .join(format!(".prepared-{operation}"))
            .join(
                destination
                    .file_name()
                    .context("Missing primary filename")?,
            );
        let task = task_for(
            asset,
            config,
            &derived.url,
            &derived.checksum,
            derived.size,
            derived.version_size,
            &destination,
        );
        let record = planner::pending_record_for_task(config, asset, &task);
        if repairs.is_empty()
            && derived.path.parent() == destination.parent()
            && crate::download::pipeline::MetadataFlags::from(config).bits() == 0
            && !config.refresh_metadata
            && let Some(existing) = old.as_ref().filter(|old| {
                old.version == derived.version_size.as_str()
                    && old.provider_checksum == derived.checksum.as_ref()
            })
        {
            // A catalogue/grouping refresh with no file metadata recipe does
            // not authorize republishing an independently verified sibling.
            let mut retained = existing.clone();
            retained.metadata_decision = Some(metadata_decision(
                asset,
                config,
                derived.version_size,
                &derived.checksum,
            )?);
            destinations.push(destination.clone());
            members.push(LayoutMember {
                missing: None,
                record: Some(record),
                destination: Some(SelectionPath::from_path(&destination)),
                path_key: Some(path_key(&destination)?),
                stage: Some(SelectionPath::from_path(&destination)),
                old: None,
                archive_source: None,
                preserved_path: None,
                retirement: None,
                prepared: Some(retained.clone()),
                preserved: None,
                installed: Some(retained),
            });
            continue;
        }
        // Every occupied owned target retains exact old media and sidecar evidence, even for metadata-only changes.
        let missing = missingfiles
            .iter()
            .find(|file| file.path.to_path() == destination)
            .cloned();
        let preserved_path = old
            .as_ref()
            .or(missing.as_ref())
            .map(|old| preservation_path(old, &family));
        destinations.push(destination.clone());
        members.push(LayoutMember {
            record: Some(record),
            destination: Some(SelectionPath::from_path(&destination)),
            path_key: Some(path_key(&destination)?),
            stage: Some(SelectionPath::from_path(&stage)),
            archive_source: None,
            old,
            missing,
            preserved_path,
            retirement: None,
            prepared: None,
            preserved: None,
            installed: None,
        });
    }
    // A previously owned original must remain filterable even when a later
    // selection requests only adjusted media. This retains local bytes without
    // adding a provider selection or download obligation.
    if config.edited_naming == EditedNaming::Primary && repairs.is_empty() {
        for old in &oldfiles {
            let adjusted = match old.version.as_str() {
                "original" => "adjusted",
                "live_original" => "live_adjusted",
                _ => continue,
            };
            if old.archive_original
                || !members.iter().any(|member| {
                    member
                        .record
                        .as_ref()
                        .is_some_and(|record| record.version_size.as_str() == adjusted)
                })
                || members.iter().any(|member| {
                    member
                        .record
                        .as_ref()
                        .is_some_and(|record| record.version_size.as_str() == old.version)
                })
            {
                continue;
            }
            if oldfiles
                .iter()
                .any(|file| file.version == old.version && file.archive_original)
            {
                continue;
            }
            let source_path = old.path.to_path();
            let filename = source_path
                .file_name()
                .and_then(|name| name.to_str())
                .context("Original archive filename is not Unicode")?;
            let destination = source_path.with_file_name(crate::download::paths::role_filename(
                filename,
                "",
                "_original",
            ));
            anyhow::ensure!(
                snapshot(&config.directory, &destination).await?.is_none(),
                "Foreign file occupies retained original archive"
            );
            let stage = destination
                .parent()
                .context("Archive has no parent")?
                .join(".kei-history")
                .join(&family)
                .join(format!(".prepared-{operation}"))
                .join(
                    destination
                        .file_name()
                        .context("Missing archive filename")?,
                );
            members.insert(
                0,
                LayoutMember {
                    record: None,
                    destination: Some(SelectionPath::from_path(&destination)),
                    path_key: Some(path_key(&destination)?),
                    stage: Some(SelectionPath::from_path(&stage)),
                    archive_source: Some(old.clone()),
                    old: None,
                    missing: None,
                    preserved_path: None,
                    retirement: None,
                    prepared: None,
                    preserved: None,
                    installed: None,
                },
            );
            destinations.push(destination);
        }
    }
    // Retire owned aliases only after preserving their exact media and sidecar.
    for old in oldfiles {
        if destinations.iter().any(|p| *p == old.path.to_path()) {
            continue;
        }
        if !repairs.is_empty() {
            members.push(LayoutMember {
                missing: None,
                record: None,
                destination: None,
                path_key: None,
                stage: None,
                old: None,
                archive_source: None,
                preserved_path: None,
                retirement: None,
                prepared: None,
                preserved: None,
                installed: Some(old),
            });
            continue;
        }
        if old.archive_original {
            members.push(LayoutMember {
                record: None,
                destination: None,
                path_key: None,
                stage: None,
                archive_source: None,
                old: Some(old.clone()),
                missing: None,
                preserved_path: Some(preservation_path(&old, &family)),
                retirement: None,
                prepared: None,
                preserved: None,
                installed: Some(old),
            });
            continue;
        }
        let preserved_path = preservation_path(&old, &family);
        // The same contents may become current and retire again after a revert.
        // Their immutable history path remains stable, but each consumed inode
        // needs its own private slot pinned by this operation's journal.
        let retirement = preserved_path
            .to_path()
            .with_file_name(format!(".retired-{operation}-{}", members.len()));
        members.push(LayoutMember {
            record: None,
            destination: None,
            path_key: None,
            stage: None,
            archive_source: None,
            old: Some(old),
            missing: None,
            preserved_path: Some(preserved_path),
            retirement: Some(SelectionPath::from_path(&retirement)),
            prepared: None,
            preserved: None,
            installed: None,
        });
    }
    anyhow::ensure!(
        !members.is_empty(),
        "Primary layout has no selected or retained files"
    );
    let op = LayoutOperation {
        operation,
        family,
        source_library: config.library.to_string(),
        library: asset.source_zone().unwrap_or(&config.library).to_owned(),
        child: asset.state_id().to_owned(),
        metadata_flags: crate::download::pipeline::MetadataFlags::from(config).bits(),
        refresh_metadata: config.refresh_metadata,
        asset_record_name: asset.asset_record_name().to_owned(),
        pass: config
            .primary_layout_pass
            .clone()
            .unwrap_or_else(|| config.pass_label().into()),
        root: SelectionPath::from_path(&std::path::absolute(&config.directory)?),
        generation: binding.map_or(0, |b| b.generation),
        decision: desired,
        policy: policy(config).into(),
        qualifier,
        phase: "planned".into(),
        members,
    };
    if persist {
        db.context("Missing primary layout database")?
            .begin_primary_layout(op.clone())
            .await?;
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("layout-planned");
    } else {
        let mut claims = session
            .preview_claims
            .lock()
            .map_err(|_error| anyhow::anyhow!("Preview path index poisoned"))?;
        for path in destinations {
            claims.insert(path_key(&path)?);
        }
    }
    Ok(op)
}

fn preservation_path(old: &LayoutFile, family: &str) -> SelectionPath {
    let path = old.path.to_path();
    let revision = digest(
        serde_json::to_vec(&serde_json::json!([
            old.version,
            old.provider_checksum,
            old.fingerprint.sha256,
            old.sidecar.as_ref().map(|s| s.sha256)
        ]))
        .unwrap_or_default(),
    );
    SelectionPath::from_path(
        &path
            .parent()
            .unwrap_or(Path::new("."))
            .join(".kei-history")
            .join(family)
            .join(revision)
            .join(path.file_name().unwrap_or_default()),
    )
}

fn task_for(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    url: &str,
    checksum: &str,
    size: u64,
    version: VersionSizeKey,
    path: &Path,
) -> filter::DownloadTask {
    filter::DownloadTask {
        url: url.into(),
        download_path: path.to_owned(),
        replacement_fingerprint: None,
        pending_cross_parent_root: None,
        checksum: checksum.into(),
        asset_id: asset.state_id_arc(),
        asset_record_name: asset.asset_record_name_arc(),
        library: Arc::from(asset.source_zone().unwrap_or(&config.library)),
        metadata: filter::build_selected_payload(asset, config, version),
        size,
        created_local: asset.created_local(),
        version_size: version,
        media_type: filter::determine_media_type(version, asset),
    }
}

async fn prepare(
    client: &reqwest::Client,
    asset: &PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
    op: &mut LayoutOperation,
    cancel: &CancellationToken,
    outcome: &mut LayoutOutcome,
) -> Result<()> {
    let db = config
        .state_db
        .as_deref()
        .context("Missing layout database")?;
    let selected = filter::derive_expected_paths(asset, config);
    for index in 0..op.members.len() {
        let member = op.members.get(index).context("Layout member disappeared")?;
        if member.prepared.is_some() {
            continue;
        }
        if let Some(source) = &member.archive_source {
            let stage = member
                .stage
                .as_ref()
                .context("Missing retained original stage")?
                .to_path();
            let fingerprint = copy_verified(
                &config.directory,
                &source.path.to_path(),
                &stage,
                &source.fingerprint,
            )
            .await?;
            let xmp = if let Some(xmp) = &source.sidecar {
                Some(
                    copy_verified(
                        &config.directory,
                        &sidecar(&source.path.to_path()),
                        &sidecar(&stage),
                        xmp,
                    )
                    .await?,
                )
            } else {
                None
            };
            op.members
                .get_mut(index)
                .context("Layout member disappeared")?
                .prepared = Some(LayoutFile {
                path: SelectionPath::from_path(&stage),
                fingerprint,
                sidecar: xmp,
                archive_original: true,
                ..source.clone()
            });
            db.save_primary_layout(op.clone()).await?;
            continue;
        }
        let Some(record) = &member.record else {
            continue;
        };
        if member.prepared.is_some() {
            continue;
        }
        let stage = member
            .stage
            .as_ref()
            .context("Missing layout stage")?
            .to_path();
        let target = member
            .destination
            .as_ref()
            .context("Missing layout destination")?
            .to_path();
        let expected = selected
            .iter()
            .find(|p| {
                p.version_size == record.version_size
                    && p.checksum.as_ref() == record.checksum.as_ref()
            })
            .context("Selected layout resource changed")?;
        let repairing = confirmed_damage(asset, config, &SelectionPath::from_path(&target))
            .await?
            .is_some();
        let owned = op
            .members
            .iter()
            .filter_map(|m| m.old.as_ref())
            .find(|old| {
                !old.local_preserved
                    && !repairing
                    && old.version == record.version_size.as_str()
                    && old.provider_checksum == record.checksum.as_ref()
            })
            .cloned();
        let local_source = member
            .old
            .as_ref()
            .filter(|old| {
                old.local_preserved
                    && asset
                        .media_modified_at(record.version_size)
                        .zip(old.local_modified)
                        .is_none_or(|(cloud, local)| cloud <= local)
            })
            .cloned();
        let source = if repairing {
            None
        } else if let Some(source) = local_source {
            Some(source)
        } else if let Some(source) = owned {
            Some(source)
        } else {
            let mut found = None;
            for receipt in session.receipts(&op.library, &op.child).filter(|r| {
                r.version_size == record.version_size && r.checksum == record.checksum.as_ref()
            }) {
                let (Some(path), Some(checksum)) = (&receipt.local_path, &receipt.local_checksum)
                else {
                    continue;
                };
                let Some(actual) = snapshot(&config.directory, path).await? else {
                    continue;
                };
                if data_encoding::HEXLOWER.encode(&actual.sha256) == *checksum {
                    #[cfg(test)]
                    crate::test_helpers::primary_source_snapshot_point(path).await?;
                    let mut candidate = file_receipt(
                        &config.directory,
                        path,
                        record.version_size.as_str(),
                        &receipt.checksum,
                        receipt.download_checksum.clone(),
                    )
                    .await?;
                    anyhow::ensure!(
                        candidate.fingerprint == actual,
                        "Retained primary source changed while proving its receipt"
                    );
                    load_source_fields(db, &op.library, &op.child, &mut candidate).await?;
                    found = Some(candidate);
                    break;
                }
            }
            found
        };
        let transform = metadata_decision(asset, config, record.version_size, &record.checksum)?;
        let prepared = if let Some(source) = source {
            let mut embedded_fields = source.embedded_fields.clone();
            let media = copy_verified(
                &config.directory,
                &source.path.to_path(),
                &stage,
                &source.fingerprint,
            )
            .await?;
            if source.local_preserved {
                if let Some(modified) = source.local_modified {
                    file::primary_layout::keep_modified(&config.directory, &stage, modified.into())
                        .await?;
                } else {
                    anyhow::ensure!(
                        crate::download::pipeline::MetadataFlags::from(config).is_empty(),
                        "Preserved layout metadata requires a known local modification time"
                    );
                }
            }
            if let Some(xmp) = &source.sidecar {
                copy_verified(
                    &config.directory,
                    &sidecar(&source.path.to_path()),
                    &sidecar(&stage),
                    xmp,
                )
                .await?;
            }
            if op.refresh_metadata
                || source.metadata_decision.as_deref() != Some(transform.as_str())
            {
                let flags = crate::download::pipeline::MetadataFlags::from(config);
                let fingerprint = crate::download::file::ExistingFileFingerprint {
                    size: media.size,
                    sha256: media.sha256,
                };
                let mut payload =
                    filter::build_selected_payload(asset, config, record.version_size);
                Arc::make_mut(&mut payload)
                    .prior_embedded
                    .clone_from(&source.embedded_fields);
                let written = crate::download::metadata_rewrite::write_download_metadata(
                    crate::download::metadata_rewrite::MetadataWriteRequest {
                        final_path: &stage,
                        embed_path: Some(&stage),
                        expected_embed_fingerprint: Some(fingerprint),
                        source_checksum: source.source_checksum.as_deref(),
                        sidecar_path: Some(&stage),
                        payload,
                        created_local: asset.created_local(),
                        flags,
                        capture_timestamp_repair: config.capture_timestamp_repair,
                        temp_suffix: &config.temp_suffix,
                    },
                )
                .await;
                if written.storage_failed {
                    return Err(std::io::Error::other(
                        "Primary layout metadata storage unavailable",
                    )
                    .into());
                }
                anyhow::ensure!(
                    !written.any_failed() && !written.embed_input_changed,
                    "Prepared primary metadata remains incomplete"
                );
                embedded_fields = written.embedded_fields;
            }
            let mut prepared = file_receipt(
                &config.directory,
                &stage,
                record.version_size.as_str(),
                &record.checksum,
                source.source_checksum,
            )
            .await?;
            prepared.local_preserved = source.local_preserved;
            prepared.local_modified = source
                .local_preserved
                .then_some(source.local_modified)
                .flatten();
            prepared.embedded_fields = embedded_fields;
            prepared
        } else {
            anyhow::ensure!(
                !cancel.is_cancelled(),
                "Primary layout interrupted before preparation"
            );
            let task = task_for(
                asset,
                config,
                &expected.url,
                &expected.checksum,
                expected.size,
                expected.version_size,
                &stage,
            );
            let context = crate::download::pipeline::DownloadSingleContext {
                temp_suffix: &config.temp_suffix,
                state_db: Some(db),
                rate_limit_counter: None,
                bandwidth_limiter: config.bandwidth_limiter.as_ref(),
                shutdown_token: cancel,
                mode: crate::personality::Mode::Off,
            };
            let (
                metadata_ok,
                checksum,
                source_checksum,
                bytes,
                disk,
                _,
                embedded_fields,
                metadata_storage_failed,
            ) = crate::download::pipeline::download_single_task(
                client,
                &task,
                &config.retry,
                crate::download::pipeline::MetadataFlags::from(config),
                context,
            )
            .await?;
            if metadata_storage_failed {
                return Err(
                    std::io::Error::other("Primary layout metadata storage unavailable").into(),
                );
            }
            anyhow::ensure!(
                metadata_ok,
                "Primary layout metadata preparation is incomplete"
            );
            let mut prepared = file_receipt(
                &config.directory,
                &stage,
                record.version_size.as_str(),
                &record.checksum,
                source_checksum,
            )
            .await?;
            prepared.embedded_fields = embedded_fields;
            anyhow::ensure!(
                data_encoding::HEXLOWER.encode(&prepared.fingerprint.sha256) == checksum,
                "Prepared primary checksum changed"
            );
            outcome.downloaded += 1;
            if task.media_type.is_photo_like() {
                outcome.photos_downloaded += 1;
            } else if task.media_type.is_video_like() {
                outcome.videos_downloaded += 1;
            }
            outcome.downloaded_assets.push(task.to_recap_asset());
            outcome.network_bytes += bytes;
            outcome.disk_bytes += disk;
            prepared
        };
        let _target = target;
        let mut prepared = prepared;
        prepared.archive_original = expected.naming_role == filter::NamingRole::OriginalArchive;
        prepared.metadata_decision = Some(transform);
        op.members
            .get_mut(index)
            .context("Layout member disappeared")?
            .prepared = Some(prepared);
        db.save_primary_layout(op.clone()).await?;
    }
    Ok(())
}

pub(in crate::download) async fn finish(
    db: &dyn DownloadStore,
    op: &mut LayoutOperation,
    cancel: &CancellationToken,
    outcome: &mut LayoutOutcome,
) -> Result<()> {
    let root = op.root.to_path();
    let mut paths: Vec<_> = op
        .members
        .iter()
        .flat_map(|member| {
            member
                .destination
                .as_ref()
                .map(SelectionPath::to_path)
                .into_iter()
                .chain(member.old.as_ref().map(|old| old.path.to_path()))
                .chain(member.preserved_path.as_ref().map(SelectionPath::to_path))
                .chain(member.retirement.as_ref().map(SelectionPath::to_path))
        })
        .collect();
    paths.sort();
    paths.dedup();
    let mut guards = Vec::new();
    for path in &paths {
        guards.push(file::lock_download_destination(path, cancel).await?);
    }
    let protected: rustc_hash::FxHashSet<_> =
        crate::download::legacy_preservation::protected_replacement_paths(Some(db))
            .await?
            .iter()
            .map(|path| crate::fs_util::confined_path_key(path))
            .collect::<std::io::Result<_>>()?;
    for path in &paths {
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".xmp");
        anyhow::ensure!(
            !protected.contains(&crate::fs_util::confined_path_key(path)?)
                && !protected.contains(&crate::fs_util::confined_path_key(Path::new(&sidecar))?),
            "Primary layout overlaps protected legacy originals"
        );
    }

    // Preserve every member before the first visible change, including sidecars.
    for index in 0..op.members.len() {
        let member = op.members.get(index).context("Layout member disappeared")?;
        if let Some(missing) = &member.missing
            && let Some(expected) = &missing.sidecar
        {
            let preserved = sidecar(
                &member
                    .preserved_path
                    .as_ref()
                    .context("Missing sidecar history path")?
                    .to_path(),
            );
            if let Some(actual) = snapshot(&root, &preserved).await? {
                anyhow::ensure!(
                    file::primary_layout::bytes_match(expected, &actual)
                        && actual.identity != expected.identity,
                    "Missing media sidecar history changed"
                );
            } else {
                anyhow::ensure!(
                    op.phase != "publishing",
                    "Missing durable sidecar preservation"
                );
                copy_verified(
                    &root,
                    &sidecar(&missing.path.to_path()),
                    &preserved,
                    expected,
                )
                .await?;
            }
        }
        if let Some(preserved) = &member.preserved {
            validate_file(&root, preserved).await?;
            continue;
        }
        let Some(old) = &member.old else { continue };
        validate_file(&root, old).await?;
        let path = member
            .preserved_path
            .as_ref()
            .context("Missing archive reservation")?
            .to_path();
        let media = copy_verified(&root, &old.path.to_path(), &path, &old.fingerprint).await?;
        if let Some(modified) = old.local_modified {
            file::primary_layout::keep_modified(&root, &path, modified.into()).await?;
        }
        let xmp = if let Some(xmp) = &old.sidecar {
            Some(copy_verified(&root, &sidecar(&old.path.to_path()), &sidecar(&path), xmp).await?)
        } else {
            None
        };
        op.members
            .get_mut(index)
            .context("Layout member disappeared")?
            .preserved = Some(LayoutFile {
            path: SelectionPath::from_path(&path),
            fingerprint: media,
            sidecar: xmp,
            ..old.clone()
        });
        db.save_primary_layout(op.clone()).await?;
        outcome.preserved += 1;
    }
    if op.phase != "publishing" {
        op.phase = "preserved".into();
        db.save_primary_layout(op.clone()).await?;
        #[cfg(all(test, target_os = "linux"))]
        if op.is_membership_retirement() {
            crate::test_helpers::process_death_point("membership-layout-preserved");
        }
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("layout-preserved");
        anyhow::ensure!(
            !cancel.is_cancelled(),
            "Primary layout interrupted before publication"
        );
        op.phase = "publishing".into();
        db.save_primary_layout(op.clone()).await?;
    }
    for index in 0..op.members.len() {
        let member = op.members.get(index).context("Layout member disappeared")?;
        if member.installed.is_some() {
            validate_file(
                &root,
                member
                    .installed
                    .as_ref()
                    .context("Missing installed member")?,
            )
            .await?;
            continue;
        }
        if let (Some(target), Some(prepared)) = (&member.destination, &member.prepared) {
            let target = target.to_path();
            file::primary_layout::recover_prepared_link(
                &root,
                &prepared.path.to_path(),
                &target,
                &prepared.fingerprint,
            )
            .await?;
            let existing = snapshot(&root, &target).await?;
            let media = if existing
                .as_ref()
                .is_some_and(|f| f == &prepared.fingerprint)
            {
                existing.context("Missing installed primary")?
            } else {
                let authorization = if let Some(old) = &member.old {
                    let preserved = member
                        .preserved
                        .as_ref()
                        .context("Missing preservation proof")?;
                    Some(
                        ManagedPrimaryAuthorization::preserved(
                            op.operation.clone(),
                            old.fingerprint.clone(),
                            preserved.path.to_path(),
                            preserved.fingerprint.clone(),
                        )?
                        .with_modified(old.local_modified.map(Into::into)),
                    )
                } else {
                    None
                };
                file::primary_layout::publish(
                    &root,
                    &prepared.path.to_path(),
                    &target,
                    &prepared.fingerprint,
                    authorization,
                )
                .await?
            };
            let xmp_path = sidecar(&target);
            let xmp = if let Some(prepared_xmp) = &prepared.sidecar {
                file::primary_layout::recover_prepared_link(
                    &root,
                    &sidecar(&prepared.path.to_path()),
                    &xmp_path,
                    prepared_xmp,
                )
                .await?;
                let existing = snapshot(&root, &xmp_path).await?;
                if existing.as_ref().is_some_and(|f| f == prepared_xmp) {
                    existing
                } else {
                    let authorization =
                        if let Some(old) = member.old.as_ref().and_then(|o| o.sidecar.as_ref()) {
                            let preserved = member
                                .preserved
                                .as_ref()
                                .context("Missing sidecar preservation")?;
                            Some(ManagedPrimaryAuthorization::preserved(
                                op.operation.clone(),
                                old.clone(),
                                sidecar(&preserved.path.to_path()),
                                preserved
                                    .sidecar
                                    .clone()
                                    .context("Missing preserved sidecar")?,
                            )?)
                        } else if let Some(old) = member
                            .missing
                            .as_ref()
                            .and_then(|missing| missing.sidecar.as_ref())
                        {
                            let path = sidecar(
                                &member
                                    .preserved_path
                                    .as_ref()
                                    .context("Missing sidecar history path")?
                                    .to_path(),
                            );
                            let preserved = snapshot(&root, &path)
                                .await?
                                .context("Missing preserved orphan sidecar")?;
                            Some(ManagedPrimaryAuthorization::preserved(
                                op.operation.clone(),
                                old.clone(),
                                path,
                                preserved,
                            )?)
                        } else {
                            None
                        };
                    Some(
                        file::primary_layout::publish(
                            &root,
                            &sidecar(&prepared.path.to_path()),
                            &xmp_path,
                            prepared_xmp,
                            authorization,
                        )
                        .await?,
                    )
                }
            } else {
                if let Some(old) = member.old.as_ref().and_then(|o| o.sidecar.as_ref()) {
                    let preserved = member
                        .preserved
                        .as_ref()
                        .context("Missing sidecar preservation")?;
                    let retired = preserved
                        .path
                        .to_path()
                        .with_file_name(format!(".retired-sidecar-{}-{index}", op.operation));
                    file::primary_layout::retire(&root, &xmp_path, &retired, old, None).await?;
                } else if let Some(old) = member
                    .missing
                    .as_ref()
                    .and_then(|missing| missing.sidecar.as_ref())
                {
                    let retired = member
                        .preserved_path
                        .as_ref()
                        .context("Missing sidecar history path")?
                        .to_path()
                        .with_file_name(format!(".retired-sidecar-{}-{index}", op.operation));
                    file::primary_layout::retire(&root, &xmp_path, &retired, old, None).await?;
                } else {
                    anyhow::ensure!(
                        snapshot(&root, &xmp_path).await?.is_none(),
                        "Foreign sidecar occupies primary layout target"
                    );
                }
                None
            };
            op.members
                .get_mut(index)
                .context("Layout member disappeared")?
                .installed = Some(LayoutFile {
                path: SelectionPath::from_path(&target),
                fingerprint: media,
                sidecar: xmp,
                ..prepared.clone()
            });
            db.save_primary_layout(op.clone()).await?;
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("layout-member-published");
        }
    }
    for member in &op.members {
        if let (Some(old), Some(retirement)) = (&member.old, &member.retirement) {
            file::primary_layout::retire(
                &root,
                &old.path.to_path(),
                &retirement.to_path(),
                &old.fingerprint,
                Some(file::primary_layout::RetirementCompanion {
                    path: &sidecar(&old.path.to_path()),
                    retired: &sidecar(&retirement.to_path()),
                    expected: old.sidecar.as_ref(),
                }),
            )
            .await?;
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("layout-member-retired");
        }
        if let Some(file) = &member.installed {
            validate_file(&root, file).await?;
        }
        if let Some(file) = &member.preserved {
            validate_file(&root, file).await?;
        }
    }
    db.commit_primary_layout(op.clone()).await?;
    op.phase = "committed".into();
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("layout-committed");
    Ok(())
}

/// Use the same ownership, collision, and history plan without reserving or
/// changing any file or durable receipt. Provider confirmation is read-only.
pub(in crate::download) async fn preview_asset(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
) -> Result<Option<Vec<filter::DownloadTask>>> {
    let key = recorded_family(asset, config, session).await?;
    let binding = if let Some(db) = config.state_db.as_deref() {
        db.primary_layout_binding(key.clone()).await?
    } else {
        None
    };
    let pending = if let Some(db) = config.state_db.as_deref() {
        db.primary_layout_operations(config.library.to_string())
            .await?
    } else {
        Vec::new()
    };
    let recorded_config = recorded_layout_config(
        config,
        binding.as_ref(),
        pending.iter().find(|op| op.family == key),
    )?;
    let config = recorded_config.as_deref().unwrap_or(config);
    if config.edited_naming == EditedNaming::Suffix && binding.is_none() {
        let managed = if let Some(db) = config.state_db.as_deref() {
            db.primary_layout_owns_asset(
                asset.source_zone().unwrap_or(&config.library),
                asset.state_id(),
            )
            .await?
        } else {
            false
        };
        if !managed {
            return Ok(None);
        }
    }
    if filter::derive_expected_paths(asset, config).is_empty() {
        return Ok(Some(Vec::new()));
    }
    let current = confirm(asset, config).await?;
    anyhow::ensure!(
        !current
            .malformed_resources()
            .iter()
            .any(|resource| matches!(
                resource.version_size,
                crate::types::AssetVersionSize::Adjusted
                    | crate::types::AssetVersionSize::LiveAdjusted
            )),
        "Primary layout preview waits for usable adjusted resources"
    );
    let desired = decision(&current, config)?;
    if pending.iter().any(|op| op.family == key) {
        tracing::info!(family=%key,"[PLAN] Recorded primary layout recovery must finish before applying this plan");
    }
    let unchanged = binding
        .as_ref()
        .is_some_and(|binding| binding.decision == desired && binding.policy == policy(config));
    let qualifier = if unchanged {
        let binding = binding.as_ref().context("Missing preview binding")?;
        for file in &binding.files {
            if let Some(retained) = pending
                .iter()
                .filter(|op| op.family == key)
                .flat_map(|op| &op.members)
                .filter_map(|member| member.installed.as_ref().or(member.old.as_ref()))
                .find(|retained| retained.path == file.path)
            {
                validate_file(&config.directory, retained)
                    .await
                    .context("Preview journal input changed")?;
            } else if let Some((checksum, size)) =
                confirmed_damage(&current, config, &file.path).await?
            {
                let actual = snapshot(&config.directory, &file.path.to_path())
                    .await?
                    .context("Confirmed managed damage disappeared")?;
                anyhow::ensure!(
                    actual.size == size
                        && data_encoding::HEXLOWER.encode(&actual.sha256) == checksum,
                    "Confirmed damaged destination changed; replacement refused"
                );
            } else {
                validate_file(&config.directory, file)
                    .await
                    .context("Preview committed input changed")?;
            }
        }
        binding.qualifier.clone()
    } else {
        let op = plan(
            &current,
            config,
            session,
            binding.as_ref(),
            key,
            desired,
            false,
        )
        .await?;
        for member in &op.members {
            if let (Some(old), Some(history)) = (&member.old, &member.preserved_path) {
                tracing::info!(source=%old.path.to_path().display(),history=%history.to_path().display(),sidecar=old.sidecar.is_some(),"[PLAN] Preserve previous media and sidecar independently before handover");
            }
            if let (Some(source), Some(destination)) = (&member.archive_source, &member.destination)
            {
                tracing::info!(source=%source.path.to_path().display(),original=%destination.to_path().display(),"[PLAN] Retain original archive");
            }
            if let Some(retirement) = &member.retirement {
                tracing::info!(retirement=%retirement.to_path().display(),"[PLAN] Retire preserved obsolete alias");
            }
        }
        op.qualifier
    };
    let mut tasks = Vec::new();
    for path in filter::derive_expected_paths(&current, config) {
        let target = bound_destination(&path, &qualifier, binding.as_ref())?;
        tracing::info!(version=path.version_size.as_str(),path=%target.display(),"[PLAN] Current selected rendition destination");
        tasks.push(task_for(
            &current,
            config,
            &path.url,
            &path.checksum,
            path.size,
            path.version_size,
            &target,
        ));
    }
    Ok(Some(tasks))
}

/// Resolve and journal the complete family before ordinary queue admission.
pub(in crate::download) async fn plan_asset(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
) -> Result<Option<Vec<filter::DownloadTask>>> {
    let db = config
        .state_db
        .as_deref()
        .context("Primary naming requires an owned state database")?;
    let key = recorded_family(asset, config, session).await?;
    let binding = db.primary_layout_binding(key.clone()).await?;
    let op = db
        .primary_layout_operations(config.library.to_string())
        .await?
        .into_iter()
        .find(|op| op.family == key);
    let recorded_config = recorded_layout_config(config, binding.as_ref(), op.as_ref())?;
    let config = recorded_config.as_deref().unwrap_or(config);
    if binding.is_none()
        && config.edited_naming == EditedNaming::Primary
        && filter::derive_expected_paths(asset, config).is_empty()
    {
        return Ok(Some(Vec::new()));
    }
    if config.edited_naming == EditedNaming::Suffix
        && binding.is_none()
        && !db
            .primary_layout_owns_asset(
                asset.source_zone().unwrap_or(&config.library),
                asset.state_id(),
            )
            .await?
    {
        return Ok(None);
    }
    let observed_decision = decision(asset, config)?;
    let current = if binding
        .as_ref()
        .is_some_and(|b| b.decision == observed_decision && b.policy == policy(config))
    {
        asset.clone()
    } else {
        confirm(asset, config).await?
    };
    anyhow::ensure!(
        crate::icloud::photos::same_selected_facts(asset, &current),
        "Observed primary layout facts changed before queue admission"
    );
    anyhow::ensure!(
        !current
            .malformed_resources()
            .iter()
            .any(|resource| matches!(
                resource.version_size,
                crate::types::AssetVersionSize::Adjusted
                    | crate::types::AssetVersionSize::LiveAdjusted
            )),
        "Primary layout waits for usable adjusted resources"
    );
    let desired = decision(&current, config)?;
    // Render the already bound targets first. Only execution after current
    // selection admission may create or replan a repair journal.
    let (repairing, _) = repair_plan_status(&current, config, binding.as_ref(), None).await?;
    let op = if repairing {
        op
    } else if let Some(mut op) = op {
        if op.decision == desired {
            Some(op)
        } else {
            if op.phase == "publishing" {
                anyhow::ensure!(
                    !op.is_membership_retirement(),
                    "Membership retirement requires completed current source evidence"
                );
                finish(
                    db,
                    &mut op,
                    &CancellationToken::new(),
                    &mut LayoutOutcome::default(),
                )
                .await?;
            } else {
                db.cancel_primary_layout(op.operation.clone()).await?;
            }
            let binding = db.primary_layout_binding(key.clone()).await?;
            Some(
                plan(
                    &current,
                    config,
                    session,
                    binding.as_ref(),
                    key,
                    desired,
                    true,
                )
                .await?,
            )
        }
    } else if binding.as_ref().is_some_and(|b| {
        (b.decision == desired || binding_matches_selection(&current, config, b))
            && b.policy == policy(config)
    }) {
        None
    } else {
        Some(
            plan(
                &current,
                config,
                session,
                binding.as_ref(),
                key,
                desired,
                true,
            )
            .await?,
        )
    };
    let paths = filter::derive_expected_paths(&current, config);
    let qualifier = op
        .as_ref()
        .map(|o| o.qualifier.as_str())
        .or_else(|| binding.as_ref().map(|b| b.qualifier.as_str()))
        .unwrap_or("");
    let mut tasks = Vec::new();
    for path in paths {
        let target = bound_destination(&path, qualifier, binding.as_ref())?;
        tasks.push(task_for(
            &current,
            config,
            &path.url,
            &path.checksum,
            path.size,
            path.version_size,
            &target,
        ));
    }
    Ok(Some(tasks))
}

pub(super) async fn recover(
    client: &reqwest::Client,
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    cancel: &CancellationToken,
) -> Result<()> {
    let Some(db) = config.state_db.as_deref() else {
        return Ok(());
    };
    let operations = db
        .primary_layout_operations(config.library.to_string())
        .await?;
    if operations.is_empty() {
        return Ok(());
    }
    let session = LayoutSession::load(db).await?;
    for mut op in operations {
        if op.is_membership_retirement() {
            continue;
        }
        let protected =
            crate::download::legacy_preservation::protected_replacement_paths(Some(db)).await?;
        file::recover_conditional_replacements(&op.root.to_path(), &protected).await?;
        if op.phase == "publishing" {
            finish(db, &mut op, cancel, &mut LayoutOutcome::default()).await?;
            continue;
        }
        let mut selected = false;
        for pass in passes {
            if let Some((capture, _, _)) = pass.album.owned_selection_scope()? {
                let concrete = Arc::clone(&capture.db) as Arc<dyn DownloadStore>;
                anyhow::ensure!(
                    config
                        .state_db
                        .as_ref()
                        .is_some_and(|store| Arc::ptr_eq(store, &concrete)),
                    "Managed selection store mismatch"
                );
                selected |= capture
                    .db
                    .has_selected_layout_obligation(
                        capture.owner,
                        op.library.clone(),
                        op.child.clone(),
                    )
                    .await?;
            }
        }
        if selected {
            // Fresh current selection must bind a replacement resource before
            // this journal can stop owning the old native obligation.
            continue;
        }
        let mut matched = false;
        for pass in passes {
            let effective = config.with_pass(pass);
            if effective.primary_layout_pass.as_deref() != Some(&op.pass) {
                continue;
            }
            let current = pass
                .album
                .confirm_layout_asset(&op.asset_record_name, &op.library)
                .await?
                .asset
                .with_state_record_name(Arc::from(op.child.as_str()));
            if recorded_family(&current, &effective, &session).await? != op.family {
                db.cancel_primary_layout(op.operation.clone()).await?;
                matched = true;
                break;
            }
            process_asset(client, current, &effective, &session, cancel).await?;
            matched = true;
            break;
        }
        if !matched {
            db.cancel_primary_layout(op.operation.clone()).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DownloadConfig, EditedNaming, LayoutOperation, LayoutOutcome, LayoutSession, decision,
        digest, family, file_receipt, finish, plan, recover, sidecar,
    };
    use crate::state::SqliteStateDb;
    use crate::test_helpers::{TestAssetRecord, TestPhotoAsset};
    use rusqlite::types::Value;
    use std::path::Path;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    const PREPARED: &[u8] = b"prepared current rendition";
    const LEGACY: &[u8] = b"ambiguous legacy original";

    async fn prepared_layout(
        db: &Arc<SqliteStateDb>,
        root: &Path,
        filename: &str,
    ) -> LayoutOperation {
        let asset = TestPhotoAsset::new(filename)
            .filename(filename)
            .orig_size(PREPARED.len() as u64)
            .orig_checksum("synthetic-provider-checksum")
            .build();
        let config = DownloadConfig {
            directory: Arc::from(root),
            folder_structure: String::new(),
            edited_naming: EditedNaming::Primary,
            state_db: Some(db.clone()),
            ..DownloadConfig::test_default()
        };
        let session = LayoutSession::load(db.as_ref()).await.unwrap();
        let mut op = plan(
            &asset,
            &config,
            &session,
            None,
            family(&asset, &config).unwrap(),
            decision(&asset, &config).unwrap(),
            true,
        )
        .await
        .unwrap();
        assert_eq!(op.members.len(), 1);
        let member = &mut op.members[0];
        let stage = member.stage.as_ref().unwrap().to_path();
        tokio::fs::create_dir_all(stage.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&stage, PREPARED).await.unwrap();
        member.prepared = Some(
            file_receipt(
                root,
                &stage,
                "original",
                "synthetic-provider-checksum",
                Some(digest(PREPARED)),
            )
            .await
            .unwrap(),
        );
        op.phase = "prepared".into();
        db.save_primary_layout(op.clone()).await.unwrap();
        op
    }

    fn durable_state(db: &SqliteStateDb) -> Vec<Vec<Vec<Value>>> {
        let conn = db
            .acquire_lock("snapshot protected layout fixture")
            .unwrap();
        [
            "assets",
            "asset_metadata_paths",
            "asset_master_mappings",
            "metadata",
            "primary_layout_operations",
            "primary_layout_members",
            "primary_layout_claims",
            "primary_layout_preserved",
            "primary_layout_bindings",
            "primary_layout_superseded_paths",
            "asset_metadata_capture_revisions",
        ]
        .iter()
        .map(|table| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |row| {
                (0..columns).map(|column| row.get(column)).collect()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        })
        .collect()
    }

    #[tokio::test]
    async fn primary_layout_finish_protected_overlap_preserves_state_and_independent_progress() {
        assert_protected_overlap(false).await;
    }

    #[tokio::test]
    async fn primary_layout_recovery_stops_before_later_independent_operations() {
        assert_protected_overlap(true).await;
    }

    async fn assert_protected_overlap(coordinator: bool) {
        for sidecar_collision in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let database = dir.path().join("state.db");
            let db = Arc::new(SqliteStateDb::open(&database).await.unwrap());
            let mut operations = [
                prepared_layout(&db, dir.path(), "current.jpg").await,
                prepared_layout(&db, dir.path(), "independent.jpg").await,
            ];
            if coordinator {
                for op in &mut operations {
                    for phase in ["preserved", "publishing"] {
                        op.phase = phase.into();
                        db.save_primary_layout(op.clone()).await.unwrap();
                    }
                }
            }
            // Recovery orders its durable operations by their immutable IDs.
            operations.sort_by(|a, b| a.operation.cmp(&b.operation));
            let [op, mut independent] = operations;
            let destination = op.members[0].destination.as_ref().unwrap().to_path();
            let stage = op.members[0].stage.as_ref().unwrap().to_path();
            let independent_path = independent.members[0]
                .destination
                .as_ref()
                .unwrap()
                .to_path();
            let independent_stage = independent.members[0].stage.as_ref().unwrap().to_path();
            let protected = if sidecar_collision {
                sidecar(&destination)
            } else {
                destination.clone()
            };
            tokio::fs::write(&protected, LEGACY).await.unwrap();
            db.upsert_seen(&TestAssetRecord::new("legacy-master").build())
                .await
                .unwrap();
            db.upsert_seen(&TestAssetRecord::new("retry-debt").build())
                .await
                .unwrap();
            db.acquire_lock("seed retained retry debt").unwrap().execute_batch(
                "UPDATE assets SET status='failed',download_attempts=2,last_error='retained retry' WHERE id='retry-debt';",
            ).unwrap();
            for child in ["legacy-child-one", "legacy-child-two"] {
                db.upsert_asset_master_mapping("PrimarySync", child, "legacy-master")
                    .await
                    .unwrap();
            }
            // Simulate a legacy original discovered after the layout was prepared.
            db.acquire_lock("seed late protected overlap").unwrap().execute(
                "UPDATE assets SET status='downloaded',local_path=?1,local_checksum=?2 WHERE id='legacy-master'",
                rusqlite::params![protected.to_str().unwrap(), digest(LEGACY)],
            ).unwrap();
            db.set_metadata("sync_token:PrimarySync", "retained")
                .await
                .unwrap();
            let before = durable_state(&db);
            drop(db);

            for _ in 0..2 {
                let db = Arc::new(SqliteStateDb::open(&database).await.unwrap());
                let mut pending = db
                    .primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap();
                assert_eq!(pending.len(), 2);
                let mut outcome = LayoutOutcome::default();
                let error = if coordinator {
                    let config = DownloadConfig {
                        state_db: Some(db.clone()),
                        ..DownloadConfig::test_default()
                    };
                    recover(
                        &reqwest::Client::new(),
                        &[],
                        &config,
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap_err()
                } else {
                    finish(
                        db.as_ref(),
                        &mut pending[0],
                        &CancellationToken::new(),
                        &mut outcome,
                    )
                    .await
                    .unwrap_err()
                };
                assert_eq!(tokio::fs::read(&protected).await.unwrap(), LEGACY);
                assert_eq!(tokio::fs::read(&stage).await.unwrap(), PREPARED);
                assert_eq!(tokio::fs::read(&independent_stage).await.unwrap(), PREPARED);
                assert!(!independent_path.exists());
                assert_eq!(destination.exists(), !sidecar_collision);
                assert_eq!(durable_state(&db), before);
                assert_eq!(outcome.downloaded, 0);
                assert_eq!(outcome.preserved, 0);
                assert!(
                    error
                        .to_string()
                        .contains("Primary layout overlaps protected legacy originals")
                );
            }

            let db = Arc::new(SqliteStateDb::open(&database).await.unwrap());
            finish(
                db.as_ref(),
                &mut independent,
                &CancellationToken::new(),
                &mut LayoutOutcome::default(),
            )
            .await
            .unwrap();
            assert_eq!(tokio::fs::read(&independent_path).await.unwrap(), PREPARED);
            assert!(
                db.primary_layout_binding(independent.family)
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(tokio::fs::read(&protected).await.unwrap(), LEGACY);
            assert_eq!(
                db.primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap()
                    .len(),
                1
            );
            if coordinator {
                let after_independent = durable_state(&db);
                drop(db);
                let db = Arc::new(SqliteStateDb::open(&database).await.unwrap());
                let config = DownloadConfig {
                    state_db: Some(db.clone()),
                    ..DownloadConfig::test_default()
                };
                assert!(
                    recover(
                        &reqwest::Client::new(),
                        &[],
                        &config,
                        &CancellationToken::new()
                    )
                    .await
                    .is_err()
                );
                assert_eq!(durable_state(&db), after_independent);
                assert_eq!(tokio::fs::read(&protected).await.unwrap(), LEGACY);
                assert_eq!(tokio::fs::read(&stage).await.unwrap(), PREPARED);
                assert_eq!(tokio::fs::read(&independent_path).await.unwrap(), PREPARED);
            }
        }
    }
}
