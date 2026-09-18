//! Metadata-capture revision repair and queued metadata maintenance.

use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use tokio_util::sync::CancellationToken;

use crate::download::metadata_rewrite::CaptureTimestampRepair;
use crate::download::pipeline::MetadataFlags;
use crate::download::{filter, metadata_rewrite};
use crate::icloud::photos::session::is_session_error as is_provider_session_error;
use crate::icloud::photos::{PhotoAsset, ProviderRecordId, RecordLookupRequest, RecordResolution};
use crate::state::VersionSizeKey;

use super::config::DownloadConfig;
use super::models::{
    DownloadControls, DownloadStore, METADATA_CAPTURE_REPAIR_FAILED_REASON, SyncStats,
    block_sync_token_for_incremental_delta,
};

/// Drain pending metadata-rewrite markers in bounded batches so a
/// `--refresh-metadata` migration finishes in its own run. Returns the number
/// of markers left failing when the drain stops.
pub(crate) async fn drain_pending_metadata_rewrites(
    db: &dyn DownloadStore,
    metadata: &crate::config::MetadataConfig,
    capture_timestamp_repair: CaptureTimestampRepair,
    library_scope: &[&str],
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> usize {
    let flags = MetadataFlags::from(metadata);
    if !flags.has_any_write() {
        return 0;
    }
    let mut offset = 0usize;
    let mut failed = 0usize;
    loop {
        let pass = metadata_rewrite::run_pending_page(
            db,
            flags,
            capture_timestamp_repair,
            Arc::clone(&temp_suffix),
            shutdown_token,
            Some(library_scope),
            offset,
        )
        .await;
        failed = failed.saturating_add(pass.failed);
        if pass.fetched == 0 {
            return failed;
        }
        tracing::debug!(
            applied = pass.applied,
            retired = pass.retired_from_selected_queue,
            failed = pass.failed,
            "Metadata rewrite batch completed"
        );
        if shutdown_token.is_cancelled() {
            return failed.max(1);
        }
        offset = offset.saturating_add(
            pass.fetched
                .saturating_sub(pass.retired_from_selected_queue),
        );
    }
}

pub(super) async fn has_metadata_backfill_work(config: &DownloadConfig) -> bool {
    let Some(db) = &config.state_db else {
        return false;
    };
    match db.has_downloaded_without_metadata_hash().await {
        Ok(needs_backfill) => needs_backfill,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to check metadata backfill state before incremental sync"
            );
            false
        }
    }
}

const METADATA_CAPTURE_BATCH: usize = 500;

#[derive(Default)]
pub(super) struct MetadataCaptureRepair {
    pub(super) stats: SyncStats,
    pub(super) failures: usize,
    pub(super) auth_errors: usize,
}

fn metadata_capture_candidate_matches(
    asset: &PhotoAsset,
    candidate: &crate::state::MetadataCaptureCandidate,
) -> bool {
    candidate.versions.iter().any(|evidence| {
        asset.versions().iter().any(|(version_size, version)| {
            VersionSizeKey::from(*version_size) == evidence.version_size
                && version.size == evidence.size_bytes
                && version.checksum.as_ref() == evidence.checksum
        })
    })
}

async fn record_metadata_capture_failure(
    db: &dyn DownloadStore,
    library: &str,
    error: &str,
    repair: &mut MetadataCaptureRepair,
) {
    repair.failures = repair.failures.saturating_add(1);
    repair.stats.metadata_capture_failures =
        repair.stats.metadata_capture_failures.saturating_add(1);
    if let Err(state_error) = db
        .record_metadata_capture_failure(library, crate::state::METADATA_CAPTURE_REVISION, error)
        .await
    {
        repair.stats.state_write_failures = repair.stats.state_write_failures.saturating_add(1);
        tracing::warn!(error = %state_error, "Failed to persist metadata-capture repair failure");
    }
}

async fn refresh_metadata_capture_candidate(
    db: &dyn DownloadStore,
    config: &DownloadConfig,
    candidate: &crate::state::MetadataCaptureCandidate,
    asset: PhotoAsset,
    repair: &mut MetadataCaptureRepair,
) {
    if let Err(error) = db
        .upsert_asset_master_mapping(&candidate.library, asset.asset_record_name(), asset.id())
        .await
    {
        let message = error.to_string();
        record_metadata_capture_failure(db, &candidate.library, &message, repair).await;
        return;
    }
    let mark_for_rewrite = MetadataFlags::from(config).has_any_write();
    let capture = filter::metadata_capture(&asset);
    match db
        .refresh_downloaded_asset_metadata(
            &candidate.library,
            &candidate.asset_id,
            (&capture, asset.created(), Some(asset.added_date())),
            mark_for_rewrite,
            false,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
    {
        Ok(updated) if updated > 0 => {
            repair.stats.metadata_capture_refreshed =
                repair.stats.metadata_capture_refreshed.saturating_add(1);
        }
        Ok(_) => {
            record_metadata_capture_failure(
                db,
                &candidate.library,
                "provider metadata matched no live downloaded catalogue row",
                repair,
            )
            .await;
        }
        Err(error) => {
            let message = error.to_string();
            record_metadata_capture_failure(db, &candidate.library, &message, repair).await;
        }
    }
}

pub(super) async fn run_metadata_capture_repair(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
    controls: DownloadControls,
    shutdown_token: &CancellationToken,
) -> MetadataCaptureRepair {
    // CONTRACT: METADATA_CAPTURE_REVISION_REPAIR_IS_DURABLE
    let mut repair = MetadataCaptureRepair::default();
    repair.stats.metadata_capture_revision = Some(crate::state::METADATA_CAPTURE_REVISION);
    if !controls.run_mode.downloads_files() || config.refresh_metadata {
        return repair;
    }
    let Some(db) = &config.state_db else {
        return repair;
    };
    let library = config.library.as_ref();
    let initial = match db
        .begin_metadata_capture_revision(library, crate::state::METADATA_CAPTURE_REVISION)
        .await
    {
        Ok(status) => status,
        Err(error) => {
            repair.failures = 1;
            repair.stats.metadata_capture_failures = 1;
            repair.stats.state_write_failures = 1;
            tracing::warn!(error = %error, library, "Could not initialize metadata-capture repair");
            block_sync_token_for_incremental_delta(
                &mut repair.stats,
                METADATA_CAPTURE_REPAIR_FAILED_REASON,
            );
            return repair;
        }
    };
    repair.stats.metadata_capture_remaining = initial.remaining_assets;
    if initial.remaining_assets == 0 {
        return repair;
    }
    let Some(provider_pass) = passes.first() else {
        record_metadata_capture_failure(
            db.as_ref(),
            library,
            "selected library has no provider pass for metadata hydration",
            &mut repair,
        )
        .await;
        block_sync_token_for_incremental_delta(
            &mut repair.stats,
            METADATA_CAPTURE_REPAIR_FAILED_REASON,
        );
        return repair;
    };
    let candidates = match db
        .get_metadata_capture_candidates(
            library,
            crate::state::METADATA_CAPTURE_REVISION,
            METADATA_CAPTURE_BATCH,
        )
        .await
    {
        Ok(candidates) => candidates,
        Err(error) => {
            let message = error.to_string();
            record_metadata_capture_failure(db.as_ref(), library, &message, &mut repair).await;
            block_sync_token_for_incremental_delta(
                &mut repair.stats,
                METADATA_CAPTURE_REPAIR_FAILED_REASON,
            );
            return repair;
        }
    };
    let mut candidates_by_id: FxHashMap<String, crate::state::MetadataCaptureCandidate> =
        candidates
            .into_iter()
            .map(|candidate| (candidate.asset_id.clone(), candidate))
            .collect();
    let requests: Vec<RecordLookupRequest> = candidates_by_id
        .values()
        .map(|candidate| match &candidate.asset_record_name {
            Some(asset_record_name) => RecordLookupRequest::paired(
                ProviderRecordId::new(candidate.asset_id.as_str()),
                ProviderRecordId::new(candidate.master_record_name.as_str()),
                ProviderRecordId::new(asset_record_name.as_str()),
            ),
            None => RecordLookupRequest::master_only(
                ProviderRecordId::new(candidate.asset_id.as_str()),
                ProviderRecordId::new(candidate.master_record_name.as_str()),
            ),
        })
        .collect();
    let resolutions = provider_pass.album.resolve_records(&requests).await;
    repair.stats.rate_limited = repair
        .stats
        .rate_limited
        .saturating_add(resolutions.rate_limit_observations);
    let mut legacy_masters = FxHashSet::default();
    for (state_id, resolution) in resolutions.results {
        if shutdown_token.is_cancelled() {
            break;
        }
        let Some(candidate) = candidates_by_id.remove(state_id.as_str()) else {
            continue;
        };
        match resolution {
            RecordResolution::Present(asset) => {
                refresh_metadata_capture_candidate(
                    db.as_ref(),
                    config,
                    &candidate,
                    asset,
                    &mut repair,
                )
                .await;
            }
            RecordResolution::MasterPresent => {
                legacy_masters.insert(candidate.master_record_name.clone());
                candidates_by_id.insert(candidate.asset_id.clone(), candidate);
            }
            RecordResolution::Deleted {
                deleted_at,
                master_family,
            } => {
                let result = if master_family {
                    db.resolve_master_family_source_deleted_affected(
                        &candidate.library,
                        &candidate.master_record_name,
                        deleted_at,
                    )
                    .await
                } else {
                    db.resolve_source_deleted_affected(
                        &candidate.library,
                        &candidate.asset_id,
                        deleted_at,
                    )
                    .await
                };
                match result {
                    Ok(_) => {}
                    Err(error) => {
                        let message = error.to_string();
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            &message,
                            &mut repair,
                        )
                        .await;
                    }
                }
            }
            RecordResolution::AssetPresent { .. } | RecordResolution::Unknown => {
                record_metadata_capture_failure(
                    db.as_ref(),
                    &candidate.library,
                    "provider lookup did not return a complete photo record",
                    &mut repair,
                )
                .await;
            }
            RecordResolution::TransientFailure(error) => {
                repair.auth_errors += usize::from(error.is_authentication());
                let message = error.to_string();
                record_metadata_capture_failure(
                    db.as_ref(),
                    &candidate.library,
                    &message,
                    &mut repair,
                )
                .await;
            }
        }
    }

    if !legacy_masters.is_empty() && !shutdown_token.is_cancelled() && repair.auth_errors == 0 {
        match provider_pass
            .album
            .hydrate_matching_master_assets_from_changes(&legacy_masters, shutdown_token)
            .await
        {
            Ok(assets) => {
                let mut assets_by_master: FxHashMap<String, Vec<PhotoAsset>> = FxHashMap::default();
                for asset in assets {
                    assets_by_master
                        .entry(asset.id().to_owned())
                        .or_default()
                        .push(asset);
                }
                let legacy_ids: Vec<String> = candidates_by_id
                    .iter()
                    .filter(|(_, candidate)| legacy_masters.contains(&candidate.master_record_name))
                    .map(|(state_id, _)| state_id.clone())
                    .collect();
                for state_id in legacy_ids {
                    let Some(candidate) = candidates_by_id.remove(&state_id) else {
                        continue;
                    };
                    let mut matches = assets_by_master
                        .remove(&candidate.master_record_name)
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|asset| metadata_capture_candidate_matches(asset, &candidate));
                    let Some(asset) = matches.next() else {
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            "no current provider child matched durable catalogue evidence",
                            &mut repair,
                        )
                        .await;
                        continue;
                    };
                    if matches.next().is_some() {
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            "multiple provider children matched durable catalogue evidence",
                            &mut repair,
                        )
                        .await;
                        continue;
                    }
                    match db
                        .claim_legacy_master_state_owner(
                            &candidate.library,
                            &candidate.master_record_name,
                            asset.asset_record_name(),
                        )
                        .await
                    {
                        Ok(true) => {
                            let asset = asset.with_state_record_name(Arc::from(state_id));
                            refresh_metadata_capture_candidate(
                                db.as_ref(),
                                config,
                                &candidate,
                                asset,
                                &mut repair,
                            )
                            .await;
                        }
                        Ok(false) => {
                            record_metadata_capture_failure(
                                db.as_ref(),
                                &candidate.library,
                                "a different provider child owns the legacy catalogue row",
                                &mut repair,
                            )
                            .await;
                        }
                        Err(error) => {
                            let message = error.to_string();
                            record_metadata_capture_failure(
                                db.as_ref(),
                                &candidate.library,
                                &message,
                                &mut repair,
                            )
                            .await;
                        }
                    }
                }
            }
            Err(error) => {
                repair.auth_errors += usize::from(is_provider_session_error(&error));
                let message = error.to_string();
                let unresolved: Vec<String> = candidates_by_id
                    .iter()
                    .filter(|(_, candidate)| legacy_masters.contains(&candidate.master_record_name))
                    .map(|(state_id, _)| state_id.clone())
                    .collect();
                for state_id in unresolved {
                    if let Some(candidate) = candidates_by_id.remove(&state_id) {
                        record_metadata_capture_failure(
                            db.as_ref(),
                            &candidate.library,
                            &message,
                            &mut repair,
                        )
                        .await;
                    }
                }
            }
        }
    }

    if shutdown_token.is_cancelled() {
        repair.stats.interrupted = true;
    } else if repair.auth_errors == 0 {
        for (_, candidate) in candidates_by_id {
            record_metadata_capture_failure(
                db.as_ref(),
                &candidate.library,
                "provider lookup omitted the requested catalogue identity",
                &mut repair,
            )
            .await;
        }
    }
    match db
        .complete_metadata_capture_revision(library, crate::state::METADATA_CAPTURE_REVISION)
        .await
    {
        Ok(status) => {
            repair.stats.metadata_capture_progressed =
                status.remaining_assets < initial.remaining_assets;
            repair.stats.metadata_capture_remaining = status.remaining_assets;
        }
        Err(error) => {
            repair.stats.state_write_failures = repair.stats.state_write_failures.saturating_add(1);
            repair.failures = repair.failures.saturating_add(1);
            repair.stats.metadata_capture_failures =
                repair.stats.metadata_capture_failures.saturating_add(1);
            tracing::warn!(error = %error, library, "Could not finalize metadata-capture repair state");
        }
    }
    if repair.failures > 0 || repair.stats.interrupted {
        block_sync_token_for_incremental_delta(
            &mut repair.stats,
            METADATA_CAPTURE_REPAIR_FAILED_REASON,
        );
    }
    repair
}

#[cfg(test)]
mod tests;
