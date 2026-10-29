//! Owns opt-in current-primary layout transitions across selected renditions.

use crate::download::{DownloadConfig, DownloadStore, file, filter, planner};
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
fn policy(config: &DownloadConfig) -> &'static str {
    if config.edited_naming == EditedNaming::Primary {
        "primary"
    } else {
        "suffix"
    }
}

fn family(asset: &PhotoAsset, config: &DownloadConfig) -> Result<String> {
    let parent = crate::download::paths::local_download_dir(
        &config.directory,
        &config.folder_structure,
        &asset.created_local(),
        config.album_name.as_deref(),
    );
    Ok(digest(serde_json::to_vec(&serde_json::json!([
        1,
        asset.source_zone().unwrap_or(&config.library),
        asset.state_id(),
        config
            .primary_layout_pass
            .as_deref()
            .unwrap_or(config.pass_label()),
        SelectionPath::from_path(&std::path::absolute(parent)?)
    ]))?))
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
        format!("{:?}", config.capture_timestamp_repair),
        asset.created(),
        filter::build_selected_payload(asset, config, version)
    ]))?))
}

async fn file_receipt(
    root: &Path,
    path: &Path,
    version: &str,
    checksum: &str,
    source_checksum: Option<String>,
) -> Result<LayoutFile> {
    Ok(LayoutFile {
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
    let key = family(&observed, config)?;
    let result = process_asset_inner(client, observed, config, session, cancel).await;
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
    let family = family(&observed, config)?;
    let binding = db.primary_layout_binding(family.clone()).await?;
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
    let pending = db
        .primary_layout_operations(config.library.to_string())
        .await?;
    let has_pending = pending.iter().any(|op| op.family == family);
    if !has_pending
        && !config.refresh_metadata
        && binding.as_ref().is_some_and(|b| {
            b.decision == decision(&observed, config).unwrap_or_default()
                && b.policy == policy(config)
        })
    {
        for file in &binding.as_ref().context("Missing committed binding")?.files {
            validate_file(&config.directory, file).await?;
        }
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
        && !config.refresh_metadata
        && binding.decision == desired
        && binding.policy == policy(config)
    {
        for file in &binding.files {
            validate_file(&config.directory, file).await?;
        }
        return Ok(Some((asset, LayoutOutcome::default())));
    }
    let mut op = if let Some(op) = pending.into_iter().find(|op| op.family == family) {
        if op.decision == desired {
            op
        } else {
            if op.phase == "publishing" {
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
            anyhow::ensure!(
                data_encoding::HEXLOWER.encode(&actual.sha256) == checksum,
                "Legacy layout media changed locally"
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
                let file = file_receipt(
                    &config.directory,
                    path,
                    record.version_size.as_str(),
                    &record.checksum,
                    record.download_checksum.clone(),
                )
                .await?;
                anyhow::ensure!(
                    file.fingerprint == actual,
                    "Legacy layout media changed during ownership verification"
                );
                oldfiles.push(file);
            }
        }
    }
    for file in &oldfiles {
        validate_file(&config.directory, file).await?;
    }
    let paths = filter::derive_expected_paths(asset, config);
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
            if oldfiles.iter().all(|f| f.path.to_path() != derived.path)
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
        let destination = qualified_path(&derived.path, &qualifier, derived.naming_role)?;
        let target = snapshot(&config.directory, &destination).await?;
        let old = oldfiles
            .iter()
            .find(|f| f.path.to_path() == destination)
            .cloned();
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
        // Every occupied owned target retains exact old media and sidecar evidence, even for metadata-only changes.
        let preserved_path = old.as_ref().map(|old| preservation_path(old, &family));
        destinations.push(destination.clone());
        members.push(LayoutMember {
            record: Some(record),
            destination: Some(SelectionPath::from_path(&destination)),
            path_key: Some(path_key(&destination)?),
            stage: Some(SelectionPath::from_path(&stage)),
            archive_source: None,
            old,
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
    if config.edited_naming == EditedNaming::Primary {
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
        if old.archive_original {
            members.push(LayoutMember {
                record: None,
                destination: None,
                path_key: None,
                stage: None,
                archive_source: None,
                old: Some(old.clone()),
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
        require_created_entry: !config.allows_existing_mutation(),
        url: url.into(),
        download_path: path.to_owned(),
        replacement_fingerprint: None,
        publication_binding: None,
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
            let fingerprint = file::primary_layout::copy_verified_create_only(
                &config.directory,
                &source.path.to_path(),
                &stage,
                &source.fingerprint,
            )
            .await?;
            let xmp = if let Some(xmp) = &source.sidecar {
                Some(
                    file::primary_layout::copy_verified_create_only(
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
        let owned = op
            .members
            .iter()
            .filter_map(|m| m.old.as_ref())
            .find(|old| {
                old.version == record.version_size.as_str()
                    && old.provider_checksum == record.checksum.as_ref()
            })
            .cloned();
        let source = if let Some(source) = owned {
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
                    let candidate = file_receipt(
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
                    found = Some(candidate);
                    break;
                }
            }
            found
        };
        let transform = metadata_decision(asset, config, record.version_size, &record.checksum)?;
        let prepared = if let Some(source) = source {
            let media = file::primary_layout::copy_verified_create_only(
                &config.directory,
                &source.path.to_path(),
                &stage,
                &source.fingerprint,
            )
            .await?;
            if let Some(xmp) = &source.sidecar {
                file::primary_layout::copy_verified_create_only(
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
                let written = crate::download::metadata_rewrite::write_download_metadata(
                    crate::download::metadata_rewrite::MetadataWriteRequest {
                        sidecar_namespace: None,
                        require_created_entry: source.sidecar.is_none(),
                        final_path: &stage,
                        embed_path: Some(&stage),
                        expected_embed_fingerprint: Some(fingerprint),
                        source_checksum: source.source_checksum.as_deref(),
                        sidecar_path: Some(&stage),
                        payload: filter::build_selected_payload(asset, config, record.version_size),
                        created_local: asset.created_local(),
                        flags,
                        capture_timestamp_repair: config.capture_timestamp_repair,
                        temp_suffix: &config.temp_suffix,
                    },
                )
                .await;
                anyhow::ensure!(
                    !written.any_failed() && !written.embed_input_changed,
                    "Prepared primary metadata remains incomplete"
                );
            }
            file_receipt(
                &config.directory,
                &stage,
                record.version_size.as_str(),
                &record.checksum,
                source.source_checksum,
            )
            .await?
        } else {
            anyhow::ensure!(
                !cancel.is_cancelled(),
                "Primary layout interrupted before preparation"
            );
            let mut task = task_for(
                asset,
                config,
                &expected.url,
                &expected.checksum,
                expected.size,
                expected.version_size,
                &stage,
            );
            task.require_created_entry = true;
            let context = crate::download::pipeline::DownloadSingleContext {
                temp_suffix: &config.temp_suffix,
                state_db: Some(db),
                rate_limit_counter: None,
                bandwidth_limiter: config.bandwidth_limiter.as_ref(),
                shutdown_token: cancel,
                mode: crate::personality::Mode::Off,
            };
            let (metadata_ok, checksum, source_checksum, bytes, disk, _) =
                crate::download::pipeline::download_single_task(
                    client,
                    &task,
                    &config.retry,
                    crate::download::pipeline::MetadataFlags::from(config),
                    context,
                )
                .await?;
            anyhow::ensure!(
                metadata_ok,
                "Primary layout metadata preparation is incomplete"
            );
            let prepared = file_receipt(
                &config.directory,
                &stage,
                record.version_size.as_str(),
                &record.checksum,
                source_checksum,
            )
            .await?;
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
        if member.preserved.is_some() {
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
                    Some(ManagedPrimaryAuthorization::preserved(
                        op.operation.clone(),
                        old.fingerprint.clone(),
                        preserved.path.to_path(),
                        preserved.fingerprint.clone(),
                    )?)
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
                    file::primary_layout::retire(&root, &xmp_path, &retired, old).await?;
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
            )
            .await?;
            if let Some(xmp) = &old.sidecar {
                file::primary_layout::retire(
                    &root,
                    &sidecar(&old.path.to_path()),
                    &sidecar(&retirement.to_path()),
                    xmp,
                )
                .await?;
            }
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
    let key = family(asset, config)?;
    let binding = if let Some(db) = config.state_db.as_deref() {
        db.primary_layout_binding(key.clone()).await?
    } else {
        None
    };
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
    if let Some(db) = config.state_db.as_deref()
        && db
            .primary_layout_operations(config.library.to_string())
            .await?
            .iter()
            .any(|op| op.family == key)
    {
        tracing::info!(family=%key,"[PLAN] Recorded primary layout recovery must finish before applying this plan");
    }
    let unchanged = binding
        .as_ref()
        .is_some_and(|binding| binding.decision == desired && binding.policy == policy(config));
    let qualifier = if unchanged {
        let binding = binding.as_ref().context("Missing preview binding")?;
        for file in &binding.files {
            validate_file(&config.directory, file).await?;
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
        let target = qualified_path(&path.path, &qualifier, path.naming_role)?;
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
    let key = family(asset, config)?;
    let binding = db.primary_layout_binding(key.clone()).await?;
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
    let op = db
        .primary_layout_operations(config.library.to_string())
        .await?
        .into_iter()
        .find(|o| o.family == key);
    let op = if let Some(mut op) = op {
        if op.decision == desired {
            Some(op)
        } else {
            if op.phase == "publishing" {
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
    } else if binding
        .as_ref()
        .is_some_and(|b| b.decision == desired && b.policy == policy(config))
    {
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
        let target = qualified_path(&path.path, qualifier, path.naming_role)?;
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

/// Operation-owned private namespace; unknown entries and interrupted attempts are retained.
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
struct EmbeddedScratch {
    directory: crate::fs_util::ConfinedPath,
    copy_root: PathBuf,
    input: crate::fs_util::ConfinedPath,
    #[cfg(not(target_os = "macos"))]
    anchored_directory: PathBuf,
    anchored_input: PathBuf,
}

#[cfg(target_os = "linux")]
impl EmbeddedScratch {
    async fn create(
        root: &Path,
        stage: &Path,
        parent: Option<crate::fs_util::ConfinedPath>,
    ) -> Result<Self> {
        let root = root.to_owned();
        let stage = stage.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::DirBuilderExt;
            let parent_path = stage.parent().context("Recorded stage has no parent")?;
            let directory_path =
                parent_path.join(format!(".kei-embedded-attempt-{}", uuid::Uuid::new_v4()));
            let directory = if let Some(parent) = parent {
                parent.sibling(&directory_path)?
            } else {
                crate::fs_util::ConfinedPath::open(
                    &root,
                    &directory_path,
                    crate::fs_util::ConfinedParents::Create,
                )?
            };
            directory.validate_namespace()?;
            let anchored_directory =
                PathBuf::from(format!("/proc/self/fd/{}", directory.parent_fd()))
                    .join(directory_path.file_name().context("Missing attempt name")?);
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&anchored_directory)?;
            directory.sync_parent()?;
            let input = directory.child(
                stage
                    .file_name()
                    .context("Recorded stage has no filename")?,
            )?;
            input.validate_namespace()?;
            let anchored_input = PathBuf::from(format!("/proc/self/fd/{}", input.parent_fd()))
                .join(
                    stage
                        .file_name()
                        .context("Recorded stage has no filename")?,
                );
            Ok(Self {
                directory,
                copy_root: root,
                input,
                anchored_directory,
                anchored_input,
            })
        })
        .await?
    }

    async fn cleanup(
        self,
        expected: &crate::state::db::primary_layout::LayoutFingerprint,
    ) -> Result<()> {
        let expected = expected.clone();
        tokio::task::spawn_blocking(move || {
            self.input.validate_namespace()?;
            self.input.validate_identity(expected.identity)?;
            self.input.remove_regular(expected.identity)?;
            self.input.sync_parent()?;
            self.directory.validate_namespace()?;
            let held = std::fs::File::open(format!("/proc/self/fd/{}", self.input.parent_fd()))?;
            let metadata = std::fs::symlink_metadata(&self.anchored_directory)?;
            anyhow::ensure!(
                metadata.file_type().is_dir(),
                "Embedded attempt directory changed"
            );
            let current = std::fs::File::open(&self.anchored_directory)?;
            anyhow::ensure!(
                crate::fs_util::file_identity(&held)? == crate::fs_util::file_identity(&current)?,
                "Embedded attempt directory changed identity"
            );
            self.input.validate_namespace()?;
            // Remove only an empty directory created by this attempt; never
            // recursively remove entries a different writer may have added.
            std::fs::remove_dir(&self.anchored_directory)?;
            self.directory.sync_parent()?;
            Ok(())
        })
        .await?
    }
}

#[cfg(target_os = "windows")]
impl EmbeddedScratch {
    async fn create(
        root: &Path,
        stage: &Path,
        parent: Option<crate::fs_util::ConfinedPath>,
    ) -> Result<Self> {
        let root = root.to_owned();
        let stage = stage.to_owned();
        tokio::task::spawn_blocking(move || {
            let parent_path = stage.parent().context("Recorded stage has no parent")?;
            let directory_path =
                parent_path.join(format!(".kei-embedded-attempt-{}", uuid::Uuid::new_v4()));
            let directory = if let Some(parent) = parent {
                parent.sibling(&directory_path)?
            } else {
                crate::fs_util::ConfinedPath::open(
                    &root,
                    &directory_path,
                    crate::fs_util::ConfinedParents::Create,
                )?
            };
            std::fs::create_dir(&directory_path)?;
            let input_path = directory_path.join(
                stage
                    .file_name()
                    .context("Recorded stage has no filename")?,
            );
            // ConfinedPath retains the directory chain without delete sharing.
            let input = crate::fs_util::ConfinedPath::open(
                &root,
                &input_path,
                crate::fs_util::ConfinedParents::Existing,
            )?;
            Ok(Self {
                directory,
                copy_root: root,
                input,
                anchored_directory: directory_path,
                anchored_input: input_path,
            })
        })
        .await?
    }

    async fn cleanup(
        self,
        expected: &crate::state::db::primary_layout::LayoutFingerprint,
    ) -> Result<()> {
        let expected = expected.clone();
        tokio::task::spawn_blocking(move || {
            self.input.validate_identity(expected.identity)?;
            self.input.remove_regular(expected.identity)?;
            self.input.sync_parent()?;
            // The parent chain remains locked while releasing the attempt handle.
            drop(self.input);
            std::fs::remove_dir(&self.anchored_directory)?;
            self.directory.sync_parent()?;
            Ok(())
        })
        .await?
    }
}

#[cfg(target_os = "macos")]
impl EmbeddedScratch {
    async fn create(
        _root: &Path,
        stage: &Path,
        _parent: Option<crate::fs_util::ConfinedPath>,
    ) -> Result<Self> {
        let stage = stage.to_owned();
        tokio::task::spawn_blocking(move || {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            let root = std::fs::canonicalize(std::env::temp_dir())?;
            let metadata = std::fs::metadata(&root)?;
            // SAFETY: geteuid has no preconditions.
            anyhow::ensure!(
                metadata.is_dir()
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.permissions().mode() & 0o077 == 0,
                "System temporary root is not private to this user"
            );
            let directory_path =
                root.join(format!(".kei-embedded-attempt-{}", uuid::Uuid::new_v4()));
            let directory = crate::fs_util::ConfinedPath::open(
                &root,
                &directory_path,
                crate::fs_util::ConfinedParents::Existing,
            )?;
            directory.create_private_directory()?;
            let input = directory.child(
                stage
                    .file_name()
                    .context("Recorded stage has no filename")?,
            )?;
            let anchored_input = input.path().to_owned();
            Ok(Self {
                directory,
                copy_root: root,
                input,
                anchored_input,
            })
        })
        .await?
    }

    async fn cleanup(
        self,
        expected: &crate::state::db::primary_layout::LayoutFingerprint,
    ) -> Result<()> {
        let expected = expected.clone();
        tokio::task::spawn_blocking(move || {
            self.input.validate_namespace()?;
            self.input.remove_regular(expected.identity)?;
            self.input.sync_parent()?;
            self.directory.remove_empty_directory()?;
            Ok(())
        })
        .await?
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
async fn prepare_recorded_embedded_stage(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    record: &crate::state::AssetRecord,
    source: &LayoutFile,
    stage: &Path,
    destination: crate::fs_util::ConfinedPath,
    cancel: &CancellationToken,
) -> Result<crate::state::db::primary_layout::LayoutFingerprint> {
    // Avoid another scratch attempt for a known collision. Publication still
    // uses exclusive creation to handle concurrent writers.
    anyhow::ensure!(
        snapshot(&config.directory, stage).await?.is_none(),
        "Recorded embedded stage is occupied"
    );
    anyhow::ensure!(
        snapshot(&config.directory, &sidecar(stage))
            .await?
            .is_none(),
        "Recorded embedded stage sidecar is occupied"
    );
    destination.validate_namespace()?;
    validate_file(&config.directory, source).await?;
    let scratch =
        EmbeddedScratch::create(&config.directory, stage, Some(destination.sibling(stage)?))
            .await?;
    let source_path = source.path.to_path();
    let source_root = file::reconciliation_source_root(&config.directory, &source_path)?;
    let input = file::primary_layout::copy_verified_between_roots(
        &source_root,
        &source_path,
        &scratch.copy_root,
        scratch.input.path(),
        &source.fingerprint,
    )
    .await?;
    #[cfg(all(
        test,
        any(target_os = "linux", target_os = "windows", target_os = "macos")
    ))]
    crate::test_helpers::process_death_point("recorded-embed-scratch");
    anyhow::ensure!(
        !cancel.is_cancelled(),
        "Recorded embedded refresh interrupted"
    );
    scratch.input.validate_namespace()?;
    let flags = crate::download::pipeline::MetadataFlags::from(config);
    let fingerprint = crate::download::file::ExistingFileFingerprint {
        size: input.size,
        sha256: input.sha256,
    };
    let result = crate::download::metadata_rewrite::prepare_embed_metadata(
        &scratch.anchored_input,
        Some(fingerprint),
        filter::build_selected_payload(asset, config, record.version_size),
        asset.created_local(),
        flags,
        config.capture_timestamp_repair,
        &config.temp_suffix,
    )
    .await;
    scratch.input.validate_namespace()?;
    anyhow::ensure!(
        snapshot(&config.directory, scratch.input.path())
            .await?
            .as_ref()
            == Some(&input),
        "Embedded scratch input changed"
    );
    anyhow::ensure!(
        !cancel.is_cancelled(),
        "Recorded embedded refresh interrupted"
    );
    let stage_file = match result {
        crate::download::metadata_rewrite::EmbedPrepareResult::Prepared(prepared) => {
            let input_capability = scratch.input.sibling(scratch.input.path())?;
            let (prepared, (output_path, witness)) = tokio::task::spawn_blocking(move || {
                let witness = prepared.retained_output_witness(&input_capability)?;
                anyhow::Ok((prepared, witness))
            })
            .await??;
            let staged = file::primary_layout::copy_verified_to_confined(
                &scratch.copy_root,
                &output_path,
                destination,
                &witness,
            )
            .await?;
            drop(prepared);
            staged
        }
        crate::download::metadata_rewrite::EmbedPrepareResult::NoWrite => {
            file::primary_layout::copy_verified_to_confined(
                &scratch.copy_root,
                scratch.input.path(),
                destination,
                &input,
            )
            .await?
        }
        crate::download::metadata_rewrite::EmbedPrepareResult::Failed => {
            anyhow::bail!("Recorded embedded metadata preparation failed")
        }
        crate::download::metadata_rewrite::EmbedPrepareResult::InputChanged => {
            anyhow::bail!("Recorded embedded metadata input changed")
        }
    };
    scratch.cleanup(&input).await?;
    Ok(stage_file)
}

/// Prepare only stages created by this attempt. Receipts stay in memory until
/// the provider is reconfirmed; an interrupted occupied stage remains pending.
async fn prepare_recorded_refresh(
    asset: &PhotoAsset,
    config: &DownloadConfig,
    session: &LayoutSession,
    op: &mut LayoutOperation,
    cancel: &CancellationToken,
) -> Result<Vec<LayoutFile>> {
    let db = config
        .state_db
        .as_deref()
        .context("Missing layout database")?;
    let flags = crate::download::pipeline::MetadataFlags::from(config);
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    anyhow::ensure!(
        !flags.any_embed(),
        "Recorded embedded refresh is unavailable on this platform"
    );
    let selected = filter::derive_expected_paths(asset, config);
    let mut sources = Vec::new();
    for index in 0..op.members.len() {
        let member = op
            .members
            .get(index)
            .context("Recorded refresh member disappeared")?;
        if member.prepared.is_some() || member.stage.is_none() {
            continue;
        }
        anyhow::ensure!(!cancel.is_cancelled(), "Recorded refresh interrupted");
        let stage = member
            .stage
            .as_ref()
            .context("Missing refresh stage")?
            .to_path();
        let source = if let Some(source) = &member.archive_source {
            source.clone()
        } else {
            let record = member
                .record
                .as_ref()
                .context("Missing recorded refresh member")?;
            selected
                .iter()
                .find(|p| {
                    p.version_size == record.version_size
                        && p.checksum.as_ref() == record.checksum.as_ref()
                })
                .context("Recorded refresh selected resource changed")?;
            let mut source = op
                .members
                .iter()
                .filter_map(|m| m.old.as_ref())
                .find(|old| {
                    old.version == record.version_size.as_str()
                        && old.provider_checksum == record.checksum.as_ref()
                })
                .cloned();
            if source.is_none() {
                for receipt in session.receipts(&op.library, &op.child).filter(|r| {
                    r.version_size == record.version_size && r.checksum == record.checksum.as_ref()
                }) {
                    let (Some(path), Some(checksum)) =
                        (&receipt.local_path, &receipt.local_checksum)
                    else {
                        continue;
                    };
                    let Some(actual) = snapshot(&config.directory, path).await? else {
                        continue;
                    };
                    if data_encoding::HEXLOWER.encode(&actual.sha256) != *checksum {
                        continue;
                    }
                    let candidate = file_receipt(
                        &config.directory,
                        path,
                        record.version_size.as_str(),
                        &receipt.checksum,
                        receipt.download_checksum.clone(),
                    )
                    .await?;
                    anyhow::ensure!(
                        candidate.fingerprint == actual,
                        "Retained refresh source changed"
                    );
                    anyhow::ensure!(
                        candidate.sidecar.is_none(),
                        "Retained receipt does not establish ownership of an unrecorded sidecar"
                    );
                    source = Some(candidate);
                    break;
                }
            }
            source.context("Recorded refresh has no verified local source")?
        };
        validate_file(&config.directory, &source).await?;
        anyhow::ensure!(
            !matches!(
                db.source_metadata_recovery(&source.path.to_path()).await?,
                crate::state::db::SourceMetadataRecovery::ReadOnly
                    | crate::state::db::SourceMetadataRecovery::ProtectedLegacy
            ),
            "Recorded refresh source is read-only"
        );
        let stage_cap = crate::fs_util::ConfinedPath::open(
            &config.directory,
            &stage,
            crate::fs_util::ConfinedParents::Create,
        )?;
        validate_file(&config.directory, &source).await?;
        let media = if flags.any_embed() && member.archive_source.is_none() {
            #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
            {
                prepare_recorded_embedded_stage(
                    asset,
                    config,
                    member
                        .record
                        .as_ref()
                        .context("Missing embedded refresh record")?,
                    &source,
                    &stage,
                    stage_cap.sibling(&stage)?,
                    cancel,
                )
                .await?
            }
            #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
            {
                anyhow::bail!("Recorded embedded refresh is unavailable on this platform")
            }
        } else {
            file::primary_layout::copy_verified_to_confined(
                &config.directory,
                &source.path.to_path(),
                stage_cap.sibling(&stage)?,
                &source.fingerprint,
            )
            .await?
        };
        let source_xmp = sidecar(&source.path.to_path());
        let xmp = if member.archive_source.is_some()
            || !flags.contains(crate::download::pipeline::MetadataFlags::XMP_SIDECAR)
        {
            if let Some(expected) = &source.sidecar {
                Some(
                    file::primary_layout::copy_verified_to_confined(
                        &config.directory,
                        &source_xmp,
                        stage_cap.sibling(&sidecar(&stage))?,
                        expected,
                    )
                    .await?,
                )
            } else {
                None
            }
        } else {
            #[cfg(feature = "xmp")]
            {
                let record = member
                    .record
                    .as_ref()
                    .context("Missing recorded refresh member")?;
                #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
                {
                    let planning = EmbeddedScratch::create(
                        &config.directory,
                        &stage,
                        Some(stage_cap.sibling(&stage)?),
                    )
                    .await?;
                    let planning_input = file::primary_layout::copy_verified_between_roots(
                        &config.directory,
                        &stage,
                        &planning.copy_root,
                        planning.input.path(),
                        &media,
                    )
                    .await?;
                    let result = crate::download::metadata_rewrite::create_layout_sidecar(
                        &config.directory,
                        &planning.anchored_input,
                        stage_cap.sibling(&sidecar(&stage))?,
                        source.sidecar.as_ref().map(|f| (source_xmp.as_path(), f)),
                        filter::build_selected_payload(asset, config, record.version_size),
                        asset.created_local(),
                        source.source_checksum.as_deref(),
                        &config.temp_suffix,
                    )
                    .await?;
                    planning.cleanup(&planning_input).await?;
                    result
                }
                #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
                {
                    crate::download::metadata_rewrite::create_layout_sidecar(
                        &config.directory,
                        &stage,
                        stage_cap.sibling(&sidecar(&stage))?,
                        source.sidecar.as_ref().map(|f| (source_xmp.as_path(), f)),
                        filter::build_selected_payload(asset, config, record.version_size),
                        asset.created_local(),
                        source.source_checksum.as_deref(),
                        &config.temp_suffix,
                    )
                    .await?
                }
            }
            #[cfg(not(feature = "xmp"))]
            {
                anyhow::bail!("XMP sidecar support is unavailable")
            }
        };
        let mut prepared = source.clone();
        prepared.path = SelectionPath::from_path(&stage);
        prepared.fingerprint = media;
        prepared.sidecar = xmp;
        if let Some(record) = &member.record {
            let expected = selected
                .iter()
                .find(|p| {
                    p.version_size == record.version_size
                        && p.checksum.as_ref() == record.checksum.as_ref()
                })
                .context("Recorded refresh selected resource changed")?;
            prepared.archive_original = expected.naming_role == filter::NamingRole::OriginalArchive;
            prepared.metadata_decision = Some(metadata_decision(
                asset,
                config,
                record.version_size,
                &record.checksum,
            )?);
        } else {
            prepared.archive_original = true;
        }
        validate_file(&config.directory, &source).await?;
        validate_file(&config.directory, &prepared).await?;
        sources.push(source);
        op.members
            .get_mut(index)
            .context("Recorded refresh member disappeared")?
            .prepared = Some(prepared);
    }
    Ok(sources)
}

pub(super) async fn recover(
    _client: &reqwest::Client,
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    cancel: &CancellationToken,
) -> Result<()> {
    let checked = config.check_ownership().await?;
    if !checked.allows_existing_mutation() {
        tracing::warn!(
            "Recorded layout operations remain pending while historical ownership is incomplete"
        );
        return Ok(());
    }
    let Some(db) = config.state_db.as_deref() else {
        return Ok(());
    };
    let operations = db
        .primary_layout_operations(config.library.to_string())
        .await?;
    if operations.is_empty() {
        return Ok(());
    }
    for mut op in operations {
        if op.phase == "publishing" {
            // Publication may already have consumed a stage. Its existing
            // recovery handles installed links and repeats the catalogue guard.
            db.save_primary_layout(op.clone()).await?;
            let protected =
                crate::download::legacy_preservation::protected_replacement_paths(Some(db)).await?;
            if file::recover_conditional_replacements(&op.root.to_path(), &protected).await? > 0 {
                tracing::warn!("Recorded layout retains protected replacement journals");
                continue;
            }
            finish(db, &mut op, cancel, &mut LayoutOutcome::default()).await?;
            continue;
        }
        if op.phase == "planned"
            && op.refresh_metadata
            && op
                .members
                .iter()
                .any(|member| member.stage.is_some() && member.prepared.is_none())
        {
            let mut matches = passes
                .iter()
                .filter(|pass| super::generation::pass_key(pass) == op.pass);
            let pass = matches
                .next()
                .context("Recorded refresh has no matching current pass")?;
            anyhow::ensure!(
                matches.next().is_none(),
                "Recorded refresh pass is ambiguous"
            );
            let current_config = config.with_pass(pass);
            anyhow::ensure!(
                SelectionPath::from_path(&std::path::absolute(&current_config.directory)?)
                    == op.root
                    && policy(&current_config) == op.policy
                    && crate::download::pipeline::MetadataFlags::from(&current_config).bits()
                        == op.metadata_flags,
                "Recorded refresh options or root changed"
            );
            let asset = pass
                .album
                .confirm_layout_asset(&op.asset_record_name, &op.library)
                .await?
                .asset;
            anyhow::ensure!(
                asset.state_id() == op.child
                    && family(&asset, &current_config)? == op.family
                    && decision(&asset, &current_config)? == op.decision,
                "Recorded refresh source changed"
            );
            for member in &op.members {
                for file in [
                    &member.old,
                    &member.archive_source,
                    &member.prepared,
                    &member.preserved,
                    &member.installed,
                ]
                .into_iter()
                .flatten()
                {
                    validate_file(&op.root.to_path(), file).await?;
                    anyhow::ensure!(
                        !matches!(
                            db.source_metadata_recovery(&file.path.to_path()).await?,
                            crate::state::db::SourceMetadataRecovery::ReadOnly
                                | crate::state::db::SourceMetadataRecovery::ProtectedLegacy
                        ),
                        "Recorded refresh source is read-only"
                    );
                }
            }
            db.save_primary_layout(op.clone()).await?;
            let session = LayoutSession::load(db).await?;
            let sources =
                prepare_recorded_refresh(&asset, &current_config, &session, &mut op, cancel)
                    .await?;
            let confirmed = confirm(&asset, &current_config).await?;
            anyhow::ensure!(
                decision(&confirmed, &current_config)? == op.decision,
                "Provider changed during recorded refresh preparation"
            );
            for member in &op.members {
                for file in [&member.old, &member.archive_source, &member.prepared]
                    .into_iter()
                    .flatten()
                {
                    validate_file(&op.root.to_path(), file).await?;
                    anyhow::ensure!(
                        !matches!(
                            db.source_metadata_recovery(&file.path.to_path()).await?,
                            crate::state::db::SourceMetadataRecovery::ReadOnly
                                | crate::state::db::SourceMetadataRecovery::ProtectedLegacy
                        ),
                        "Recorded refresh source became read-only"
                    );
                }
            }
            for source in &sources {
                validate_file(&op.root.to_path(), source).await?;
                anyhow::ensure!(
                    !matches!(
                        db.source_metadata_recovery(&source.path.to_path()).await?,
                        crate::state::db::SourceMetadataRecovery::ReadOnly
                            | crate::state::db::SourceMetadataRecovery::ProtectedLegacy
                    ),
                    "Recorded refresh source became read-only"
                );
            }
            op.phase = "prepared".into();
            db.save_primary_layout(op.clone()).await?;
            finish(db, &mut op, cancel, &mut LayoutOutcome::default()).await?;
            continue;
        }
        if !matches!(op.phase.as_str(), "planned" | "prepared" | "preserved")
            || op
                .members
                .iter()
                .any(|member| member.stage.is_some() && member.prepared.is_none())
        {
            tracing::warn!(
                reason = "retained_layout_operation",
                "Interrupted layout work waits for recorded preparation evidence"
            );
            continue;
        }
        // Only recorded files and destinations participate. Rehash every
        // prepared member before preservation/publication can change any file.
        for member in &op.members {
            if let Some(prepared) = &member.prepared {
                anyhow::ensure!(
                    member.stage.as_ref() == Some(&prepared.path) && member.destination.is_some(),
                    "Recorded layout preparation has a different native stage"
                );
                validate_file(&op.root.to_path(), prepared).await?;
            }
            for file in [&member.preserved, &member.installed].into_iter().flatten() {
                validate_file(&op.root.to_path(), file).await?;
            }
        }
        if op.phase == "planned" {
            op.phase = "prepared".into();
        }
        // Even a same-phase save validates current catalogue facts atomically.
        // A changed pass/root never cancels the operation or invents a successor.
        db.save_primary_layout(op.clone()).await?;
        #[cfg(all(test, target_os = "linux"))]
        crate::test_helpers::process_death_point("layout-recovery-prepared");
        let protected =
            crate::download::legacy_preservation::protected_replacement_paths(Some(db)).await?;
        if file::recover_conditional_replacements(&op.root.to_path(), &protected).await? > 0 {
            tracing::warn!("Recorded layout retains protected replacement journals");
            continue;
        }
        finish(db, &mut op, cancel, &mut LayoutOutcome::default()).await?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
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
        prepared_layout_in_library(db, root, filename, "PrimarySync").await
    }

    pub(crate) async fn prepared_layout_in_library(
        db: &Arc<SqliteStateDb>,
        root: &Path,
        filename: &str,
        library: &str,
    ) -> LayoutOperation {
        prepared_layout_with_refresh(db, root, filename, library, false).await
    }

    pub(crate) async fn seed_unprepared_refresh(
        config: &DownloadConfig,
        pass: &crate::commands::AlbumPass,
    ) -> LayoutOperation {
        let config = config.with_pass(pass).check_ownership().await.unwrap();
        let asset = pass
            .album
            .confirm_layout_asset("asset-501", "PrimarySync")
            .await
            .unwrap()
            .asset;
        let db = config.state_db.as_deref().unwrap();
        let session = LayoutSession::load(db).await.unwrap();
        let binding = db
            .primary_layout_binding(family(&asset, &config).unwrap())
            .await
            .unwrap();
        let op = plan(
            &asset,
            &config,
            &session,
            binding.as_ref(),
            family(&asset, &config).unwrap(),
            decision(&asset, &config).unwrap(),
            true,
        )
        .await
        .unwrap();
        assert!(op.refresh_metadata);
        assert!(
            op.members
                .iter()
                .filter_map(|m| m.stage.as_ref())
                .all(|p| !p.to_path().exists())
        );
        op
    }

    pub(crate) async fn prepared_layout_with_refresh(
        db: &Arc<SqliteStateDb>,
        root: &Path,
        filename: &str,
        library: &str,
        refresh_metadata: bool,
    ) -> LayoutOperation {
        let asset = TestPhotoAsset::new(filename)
            .filename(filename)
            .orig_size(PREPARED.len() as u64)
            .orig_checksum("synthetic-provider-checksum")
            .build()
            .with_source_zone(library.into());
        let config = DownloadConfig {
            directory: Arc::from(root),
            library: Arc::from(library),
            folder_structure: String::new(),
            edited_naming: EditedNaming::Primary,
            refresh_metadata,
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
    mod normal_boundary_tests;
}
