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
    preview_claims: std::sync::Mutex<rustc_hash::FxHashSet<String>>,
}
impl LayoutSession {
    pub(in crate::download) async fn load(db: &dyn DownloadStore) -> Result<Self> {
        let mut receipts: rustc_hash::FxHashMap<(String, String), Vec<DownloadedFileRecord>> =
            rustc_hash::FxHashMap::default();
        for record in db.get_primary_layout_receipts().await? {
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
    if config.edited_naming == EditedNaming::Suffix && binding.is_none() {
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
    if binding.as_ref().is_some_and(|b| {
        b.decision == decision(&observed, config).unwrap_or_default() && b.policy == policy(config)
    }) {
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
        && binding.decision == desired
        && binding.policy == policy(config)
    {
        for file in &binding.files {
            validate_file(&config.directory, file).await?;
        }
        return Ok(Some((asset, LayoutOutcome::default())));
    }
    let pending = db
        .primary_layout_operations(config.library.to_string())
        .await?;
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
        let legacy = DownloadConfig {
            edited_naming: EditedNaming::Suffix,
            ..config.clone()
        };
        let derived = filter::derive_expected_paths(asset, &legacy);
        for record in session.receipts(
            asset.source_zone().unwrap_or(&config.library),
            asset.state_id(),
        ) {
            let (Some(path), Some(checksum)) = (&record.local_path, &record.local_checksum) else {
                continue;
            };
            let Some(expected) = derived
                .iter()
                .find(|p| p.version_size == record.version_size)
            else {
                continue;
            };
            if !filter::stored_path_matches_download_family(
                asset.state_id(),
                expected,
                &derived,
                &legacy,
                path,
            ) {
                continue;
            }
            let Some(actual) = snapshot(&config.directory, path).await? else {
                continue;
            };
            anyhow::ensure!(
                data_encoding::HEXLOWER.encode(&actual.sha256) == *checksum,
                "Legacy layout media changed locally"
            );
            anyhow::ensure!(
                session
                    .owners(path)?
                    .all(|r| r.library.as_ref() == record.library
                        && r.asset_id.as_ref() == record.id),
                "Ambiguous legacy primary path owner"
            );
            if oldfiles.iter().all(|f| f.path.to_path() != *path) {
                oldfiles.push(
                    file_receipt(
                        &config.directory,
                        path,
                        record.version_size.as_str(),
                        &record.checksum,
                        record.download_checksum.clone(),
                    )
                    .await?,
                );
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
            .with_file_name(format!(".retired-{operation}"));
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
                    found = Some(
                        file_receipt(
                            &config.directory,
                            path,
                            record.version_size.as_str(),
                            &receipt.checksum,
                            receipt.download_checksum.clone(),
                        )
                        .await?,
                    );
                    break;
                }
            }
            found
        };
        let transform = metadata_decision(asset, config, record.version_size, &record.checksum)?;
        let prepared = if let Some(source) = source {
            let media = copy_verified(
                &config.directory,
                &source.path.to_path(),
                &stage,
                &source.fingerprint,
            )
            .await?;
            if let Some(xmp) = &source.sidecar {
                copy_verified(
                    &config.directory,
                    &sidecar(&source.path.to_path()),
                    &sidecar(&stage),
                    xmp,
                )
                .await?;
            }
            if source.metadata_decision.as_deref() != Some(transform.as_str()) {
                let flags = crate::download::pipeline::MetadataFlags::from(config);
                let fingerprint = crate::download::file::ExistingFileFingerprint {
                    size: media.size,
                    sha256: media.sha256,
                };
                let written = crate::download::metadata_rewrite::write_download_metadata(
                    crate::download::metadata_rewrite::MetadataWriteRequest {
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
        })
        .collect();
    paths.sort();
    paths.dedup();
    let mut guards = Vec::new();
    for path in paths {
        guards.push(file::lock_download_destination(&path, cancel).await?);
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
        return Ok(None);
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
    if config.edited_naming == EditedNaming::Suffix && binding.is_none() {
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
        let protected =
            crate::download::legacy_preservation::protected_replacement_paths(Some(db)).await?;
        file::recover_conditional_replacements(&op.root.to_path(), &protected).await?;
        if op.phase == "publishing" {
            finish(db, &mut op, cancel, &mut LayoutOutcome::default()).await?;
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
            if family(&current, &effective)? != op.family {
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
