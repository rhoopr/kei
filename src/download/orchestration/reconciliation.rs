//! Local catalog path reconciliation and durable destination reservations.

use std::sync::Arc;

use anyhow::Result;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio_util::sync::CancellationToken;

use crate::download::filter::DownloadTask;
#[cfg(feature = "xmp")]
use crate::download::metadata_rewrite;
use crate::download::retry::PendingRetryTarget;
use crate::download::{file, filter, pipeline, planner};
use crate::icloud::photos::{ProviderRecordId, RecordLookupRequest, RecordResolution};

use super::config::DownloadConfig;
use super::context::RecordedLocalFile;
use super::models::{DownloadStore, SyncStats};
use super::url_refresh::RetryTaskKey;

#[derive(Debug, Default)]
pub(crate) struct PathReconciliationResult {
    pub(crate) complete: bool,
    pub(crate) stats: SyncStats,
}

async fn requeue_missing_catalog_file(
    db: &dyn DownloadStore,
    task: &DownloadTask,
    stats: &mut SyncStats,
) {
    if let Err(error) = db
        .mark_failed(
            &task.library,
            &task.asset_id,
            task.version_size.as_str(),
            crate::commands::reconcile::FILE_MISSING_REASON,
        )
        .await
    {
        stats.state_write_failures += 1;
        tracing::warn!(asset_id = %task.asset_id, %error, "Path reconciliation could not requeue a missing catalog file");
    }
}

pub(crate) async fn reconcile_catalog_paths(
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    shutdown_token: CancellationToken,
) -> Result<PathReconciliationResult> {
    let Some(db) = &config.state_db else {
        return Ok(PathReconciliationResult::default());
    };
    let Some(provider_pass) = passes.first() else {
        return Ok(PathReconciliationResult::default());
    };

    let mut records = Vec::new();
    let mut offset = 0u64;
    const PAGE_SIZE: u32 = 1_000;
    loop {
        let page = db.get_downloaded_page(offset, PAGE_SIZE).await?;
        let page_len = page.len();
        records.extend(page.into_iter().filter(|record| {
            record.library.as_ref() == config.library.as_ref() && !record.metadata.is_deleted
        }));
        if page_len < PAGE_SIZE as usize {
            break;
        }
        offset = offset.saturating_add(u64::from(PAGE_SIZE));
    }
    if records.is_empty() {
        return Ok(PathReconciliationResult {
            complete: true,
            stats: SyncStats::default(),
        });
    }

    let mut targets: FxHashSet<PendingRetryTarget> = records
        .iter()
        .map(PendingRetryTarget::from_record)
        .collect();
    let mut requests = Vec::new();
    let mut seen_requests = FxHashSet::default();
    let mut master_by_state_id = FxHashMap::default();
    for record in &records {
        let mapped_master = db
            .get_master_record_name_for_asset(&config.library, &record.id)
            .await?;
        let master = mapped_master
            .as_deref()
            .unwrap_or(record.id.as_ref())
            .to_owned();
        let asset_record_names = if mapped_master.is_some() {
            vec![record.id.to_string()]
        } else {
            db.get_asset_record_names_for_master(&config.library, &master)
                .await?
        };
        master_by_state_id.insert(record.id.to_string(), master.clone());
        for asset_record_name in asset_record_names {
            let key = (
                record.id.to_string(),
                master.clone(),
                asset_record_name.clone(),
            );
            if seen_requests.insert(key.clone()) {
                requests.push(RecordLookupRequest::paired(
                    ProviderRecordId::new(key.0),
                    ProviderRecordId::new(key.1),
                    ProviderRecordId::new(key.2),
                ));
            }
        }
    }

    let pass_configs: Vec<Arc<DownloadConfig>> = passes
        .iter()
        .map(|pass| Arc::new(config.with_pass(pass)))
        .collect();
    let records_by_target: FxHashMap<PendingRetryTarget, &crate::state::AssetRecord> = records
        .iter()
        .map(|record| (PendingRetryTarget::from_record(record), record))
        .collect();
    let mut albums_by_asset: FxHashMap<String, FxHashSet<String>> = FxHashMap::default();
    for (asset_id, album_name) in db.get_all_asset_albums(&config.library).await? {
        albums_by_asset
            .entry(asset_id)
            .or_default()
            .insert(album_name);
    }
    let album_container_ids: Vec<String> = passes
        .iter()
        .filter(|pass| pass.kind == crate::commands::PassKind::Album)
        .filter_map(|pass| pass.album.container_id().map(ToOwned::to_owned))
        .collect();
    let album_pass_count = passes
        .iter()
        .filter(|pass| pass.kind == crate::commands::PassKind::Album)
        .count();
    let album_container_refs: Vec<&str> = album_container_ids.iter().map(String::as_str).collect();
    let album_membership_complete = album_container_ids.len() == album_pass_count
        && (album_container_refs.is_empty()
            || db
                .selected_album_containers_have_complete_snapshots(
                    &config.library,
                    &album_container_refs,
                )
                .await?);
    let selection_complete = album_membership_complete
        && !passes
            .iter()
            .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder);
    let batch = provider_pass.album.resolve_records(&requests).await;
    let catalog_paths = db.get_reconciliation_catalog_paths().await?;
    let reservations = db.get_reconciliation_reservations().await?;
    let mut task_planner =
        match planner::TaskPlanner::for_reconciliation(catalog_paths, reservations) {
            Ok(planner) => planner,
            Err(error) => {
                tracing::warn!(%error, "Path reconciliation rejected an unsafe recorded path");
                return Ok(PathReconciliationResult {
                    stats: SyncStats {
                        failed: 1,
                        ..SyncStats::default()
                    },
                    ..PathReconciliationResult::default()
                });
            }
        };
    let mut tasks = Vec::new();
    let mut task_keys = FxHashSet::default();
    let mut stats = SyncStats::default();
    for (state_id, resolution) in batch.results {
        if shutdown_token.is_cancelled() {
            break;
        }
        match resolution {
            RecordResolution::Present(asset) => {
                targets.retain(|target| target.asset_id.as_ref() != state_id.as_str());
                let known_albums = albums_by_asset.get(state_id.as_str());
                for (pass, pass_config) in passes.iter().zip(&pass_configs) {
                    let selected = match pass.kind {
                        crate::commands::PassKind::Album => known_albums
                            .is_some_and(|albums| albums.contains(pass.album.name.as_ref())),
                        crate::commands::PassKind::Unfiled => {
                            known_albums.is_none_or(FxHashSet::is_empty)
                        }
                        crate::commands::PassKind::SmartFolder => false,
                    };
                    if !selected {
                        continue;
                    }
                    if filter::is_asset_filtered(&asset, pass_config.as_ref()).is_some() {
                        continue;
                    }
                    let mut unsafe_destination = false;
                    for expected in filter::expected_paths_for(&asset, pass_config.as_ref()) {
                        let target = PendingRetryTarget {
                            library: Arc::clone(&pass_config.library),
                            asset_id: Arc::from(state_id.as_str()),
                            version_size: expected.version_size,
                        };
                        let Some(record) = records_by_target.get(&target) else {
                            continue;
                        };
                        if let Err(error) = file::validate_reconciliation_paths(
                            &pass_config.directory,
                            record.local_path.as_deref(),
                            &expected.path,
                        )
                        .await
                        {
                            stats.failed += 1;
                            unsafe_destination = true;
                            tracing::warn!(%error, "Path reconciliation rejected an unsafe destination");
                        }
                    }
                    if unsafe_destination {
                        continue;
                    }
                    let plan = match task_planner
                        .plan_reconciliation_asset(&asset, pass_config)
                        .await
                    {
                        Ok(plan) => plan,
                        Err(error) => {
                            stats.failed += 1;
                            tracing::warn!(%error, "Path reconciliation could not reserve planned paths");
                            return Ok(PathReconciliationResult {
                                complete: false,
                                stats,
                            });
                        }
                    };
                    if plan.filter_reason.is_some() {
                        continue;
                    }
                    for task in plan.tasks {
                        let target = PendingRetryTarget::from_task(&task);
                        if let Some(record) = records_by_target.get(&target)
                            && record.checksum.as_ref() == task.checksum.as_ref()
                            && record.size_bytes == task.size
                            && let Some(path) = record.local_path.clone()
                        {
                            let recorded_file = RecordedLocalFile {
                                path,
                                local_checksum: record
                                    .local_checksum
                                    .clone()
                                    .map(String::into_boxed_str),
                                download_checksum: record
                                    .download_checksum
                                    .clone()
                                    .map(String::into_boxed_str),
                            };
                            if pipeline::recorded_current_path_exists(
                                pass_config,
                                &asset,
                                task.version_size,
                                &mut task_planner,
                                &recorded_file,
                            )
                            .await
                            .is_some()
                            {
                                continue;
                            }
                        }
                        let key = RetryTaskKey::from(&task);
                        if task_keys.insert(key) {
                            tasks.push(task);
                        }
                    }
                }
            }
            RecordResolution::Deleted {
                deleted_at,
                master_family,
            } => {
                let id = state_id.as_str();
                if master_family {
                    let master = master_by_state_id.get(id).map(String::as_str).unwrap_or(id);
                    db.resolve_master_family_source_deleted_affected(
                        &config.library,
                        master,
                        deleted_at,
                    )
                    .await?;
                } else {
                    db.resolve_source_deleted_affected(&config.library, id, deleted_at)
                        .await?;
                }
                targets.retain(|target| target.asset_id.as_ref() != id);
            }
            RecordResolution::AssetPresent { .. }
            | RecordResolution::MasterPresent
            | RecordResolution::Unknown
            | RecordResolution::TransientFailure(_) => {}
        }
    }

    if let Err(error) = db
        .reserve_reconciliation_paths(task_planner.reconciliation_reservations())
        .await
    {
        stats.state_write_failures += 1;
        tracing::warn!(%error, "Could not persist reconciliation destinations before publication");
        return Ok(PathReconciliationResult {
            complete: false,
            stats,
        });
    }

    let mut deferred_to_pending_retry = false;
    for task in tasks {
        let target = PendingRetryTarget::from_task(&task);
        let Some(record) = records_by_target.get(&target) else {
            tracing::debug!(
                version_size = task.version_size.as_str(),
                "Path reconciliation left newly selected version to normal download"
            );
            continue;
        };
        let Some(source_path) = record.local_path.as_deref() else {
            deferred_to_pending_retry = true;
            requeue_missing_catalog_file(db.as_ref(), &task, &mut stats).await;
            tracing::debug!(asset_id = %task.asset_id, "Path reconciliation deferred catalog row without a local file to targeted retry");
            continue;
        };
        match tokio::fs::try_exists(source_path).await {
            Ok(true) => {}
            Ok(false) => {
                deferred_to_pending_retry = true;
                requeue_missing_catalog_file(db.as_ref(), &task, &mut stats).await;
                tracing::debug!(asset_id = %task.asset_id, path = %source_path.display(), "Path reconciliation deferred missing local file to targeted retry");
                continue;
            }
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(asset_id = %task.asset_id, path = %source_path.display(), %error, "Path reconciliation could not inspect the catalog file");
                continue;
            }
        }
        match file::copy_local_file_no_replace(
            &config.directory,
            source_path,
            &task.download_path,
            &config.temp_suffix,
        )
        .await
        {
            Ok(Some(copy)) => {
                // Equivalent root spellings only change the durable path. Do
                // not rewrite timestamps or sidecars on the existing source.
                let same_path = copy.is_same_path();
                if !same_path
                    && let Err(error) = copy.set_capture_time(task.created_local.timestamp()).await
                {
                    stats.failed += 1;
                    tracing::warn!(%error, "Could not restore reconciled capture mtime");
                    continue;
                }
                #[cfg(feature = "xmp")]
                let sidecar = if !same_path && config.metadata.xmp_sidecar {
                    match metadata_rewrite::write_reconciled_sidecar(
                        Arc::clone(&copy),
                        Arc::clone(&task.metadata),
                        task.created_local,
                        config.temp_suffix.to_string(),
                    )
                    .await
                    {
                        Ok(receipt) => Some(receipt),
                        Err(error) => {
                            stats.exif_failures += 1;
                            tracing::warn!(%error, "Could not preserve reconciled XMP sidecar");
                            continue;
                        }
                    }
                } else {
                    None
                };
                if let Err(error) = copy.validate().await {
                    stats.failed += 1;
                    tracing::warn!(%error, "Reconciliation changed before state finalization");
                    continue;
                }
                #[cfg(feature = "xmp")]
                if let Some(sidecar) = &sidecar
                    && let Err(error) = sidecar.validate().await
                {
                    stats.exif_failures += 1;
                    tracing::warn!(%error, "Reconciled sidecar changed before state finalization");
                    continue;
                }
                if let Err(error) = db
                    .mark_downloaded(
                        &task.library,
                        &task.asset_id,
                        task.version_size.as_str(),
                        &task.download_path,
                        copy.checksum(),
                        None,
                    )
                    .await
                {
                    stats.state_write_failures += 1;
                    tracing::warn!(asset_id = %task.asset_id, %error, "Failed to persist reconciled local path");
                } else {
                    stats.downloaded += 1;
                    if matches!(
                        task.media_type,
                        crate::state::MediaType::Photo | crate::state::MediaType::LivePhotoImage
                    ) {
                        stats.photos_downloaded += 1;
                    } else {
                        stats.videos_downloaded += 1;
                    }
                    if !same_path {
                        stats.disk_bytes_written =
                            stats.disk_bytes_written.saturating_add(record.size_bytes);
                    }
                }
            }
            Ok(None) => {
                stats.failed += 1;
                tracing::warn!(asset_id = %task.asset_id, path = %task.download_path.display(), "Path reconciliation found conflicting destination bytes");
            }
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(asset_id = %task.asset_id, %error, "Failed to copy existing media into reconciled path");
            }
        }
    }
    stats.interrupted = shutdown_token.is_cancelled();
    let complete = batch.complete
        && selection_complete
        && targets.is_empty()
        && !deferred_to_pending_retry
        && stats.failed == 0
        && stats.exif_failures == 0
        && stats.state_write_failures == 0
        && !stats.interrupted;
    Ok(PathReconciliationResult { complete, stats })
}

#[cfg(test)]
mod tests;
