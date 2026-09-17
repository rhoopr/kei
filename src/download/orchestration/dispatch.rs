//! Sync entry point and composition of full, incremental, and recovery passes.

use std::sync::Arc;

use anyhow::Result;
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::SyncTokenError;

use super::cleanup::cleanup_orphan_part_files;
use super::config::DownloadConfig;
use super::context::backfill_asset_master_mappings_from_album_history;
use super::full::{
    download_photos_full_with_reason, download_photos_full_with_token,
    download_photos_full_with_token_policy,
};
use super::incremental::download_photos_incremental;
use super::maintenance::{has_metadata_backfill_work, run_metadata_capture_repair};
use super::models::{
    DownloadControls, DownloadOutcome, FullEnumerationReason, SMART_FOLDER_REFRESH_FAILED_REASON,
    SyncMode, SyncResult, TARGETED_ALBUM_BACKFILL_FAILED_REASON,
    block_sync_token_for_incremental_delta, clear_full_query_token_block_stats,
    merge_download_outcomes,
};
use super::recovery::append_targeted_recovery_to_sync_result;
use super::selection::{
    IncrementalRoutingDecision, determine_incremental_routing_decision,
    split_incremental_and_smart_folder_passes,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IncrementalErrorClass {
    TokenFallback,
    TransientFailure,
    StaticFallback,
}

/// Classify incremental-enumeration failures before deciding whether to fall
/// back to a full records/query pass.
///
/// Token errors that CloudKit explicitly marks unsafe fall back to full
/// enumeration. Auth and transport transients bubble up because a full pass
/// would likely hit the same service condition. Other static/decode errors
/// fall back so malformed token responses do not strand the user.
fn classify_incremental_error(error: &anyhow::Error) -> IncrementalErrorClass {
    if error
        .downcast_ref::<SyncTokenError>()
        .is_some_and(SyncTokenError::should_fallback_to_full)
    {
        return IncrementalErrorClass::TokenFallback;
    }
    if error
        .downcast_ref::<crate::auth::error::AuthError>()
        .is_some()
        || error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(is_transient_reqwest_error)
    {
        return IncrementalErrorClass::TransientFailure;
    }
    IncrementalErrorClass::StaticFallback
}

fn is_transient_reqwest_error(error: &reqwest::Error) -> bool {
    error
        .status()
        .is_some_and(|status| status == 429 || status.as_u16() >= 500)
        || error.is_timeout()
        || error.is_connect()
}

async fn targeted_backfill_snapshots_complete(
    passes: &[crate::commands::AlbumPass],
    config: &DownloadConfig,
) -> bool {
    let Some(db) = &config.state_db else {
        return false;
    };
    let container_ids: Vec<String> = passes
        .iter()
        .filter_map(|pass| pass.album.container_id().map(ToOwned::to_owned))
        .collect();
    let container_refs: Vec<&str> = container_ids.iter().map(String::as_str).collect();
    match db
        .selected_album_containers_have_complete_snapshots(&config.library, &container_refs)
        .await
    {
        Ok(complete) => complete,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to verify targeted album backfill snapshots"
            );
            false
        }
    }
}

async fn download_photos_incremental_with_targeted_album_backfill(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    backfill_pass_indices: &[usize],
    reason: FullEnumerationReason,
) -> Result<SyncResult> {
    let backfill_passes: Vec<crate::commands::AlbumPass> = backfill_pass_indices
        .iter()
        .filter_map(|index| passes.get(*index).cloned())
        .collect();
    let backfill_result = download_photos_full_with_reason(
        download_client,
        &backfill_passes,
        config,
        controls,
        shutdown_token.clone(),
        reason,
    )
    .await?;
    let backfill_failed = !matches!(backfill_result.outcome, DownloadOutcome::Success)
        || backfill_result.stats.interrupted
        || backfill_result.stats.enumeration_errors > 0
        || shutdown_token.is_cancelled()
        || !targeted_backfill_snapshots_complete(&backfill_passes, config).await;

    let SyncResult {
        outcome: backfill_outcome,
        sync_token: _,
        stats: mut combined_stats,
        full_enumeration_ran: backfill_full_enumeration_ran,
    } = backfill_result;

    if backfill_failed {
        block_sync_token_for_incremental_delta(
            &mut combined_stats,
            TARGETED_ALBUM_BACKFILL_FAILED_REASON,
        );
        return Ok(SyncResult {
            outcome: backfill_outcome,
            sync_token: None,
            stats: combined_stats,
            full_enumeration_ran: backfill_full_enumeration_ran,
        });
    }

    // Full album queries may report their own query sync-token telemetry, but
    // targeted backfill does not use that token. The zone token may advance
    // only after the following /changes/zone pass completes safely.
    clear_full_query_token_block_stats(&mut combined_stats);

    let incremental_result = if passes
        .iter()
        .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder)
    {
        download_photos_incremental_with_smart_folder_refresh(
            download_client,
            passes,
            config,
            zone_sync_token,
            controls,
            shutdown_token,
        )
        .await?
    } else {
        download_photos_incremental(
            download_client,
            passes,
            config,
            zone_sync_token,
            controls,
            shutdown_token,
        )
        .await?
    };

    let SyncResult {
        outcome: incremental_outcome,
        sync_token,
        stats: incremental_stats,
        full_enumeration_ran: incremental_full_enumeration_ran,
    } = incremental_result;
    combined_stats.accumulate(&incremental_stats);
    let outcome = merge_download_outcomes(&backfill_outcome, &incremental_outcome);
    let sync_token = (!combined_stats.sync_token_blocked)
        .then_some(sync_token)
        .flatten();

    Ok(SyncResult {
        outcome,
        sync_token,
        stats: combined_stats,
        full_enumeration_ran: backfill_full_enumeration_ran || incremental_full_enumeration_ran,
    })
}

async fn download_photos_incremental_with_smart_folder_refresh(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let (incremental_passes, smart_folder_passes) =
        split_incremental_and_smart_folder_passes(passes);

    if incremental_passes.is_empty() {
        return download_photos_full_with_token(
            download_client,
            &smart_folder_passes,
            config,
            controls,
            shutdown_token,
        )
        .await;
    }

    let incremental_result = download_photos_incremental(
        download_client,
        &incremental_passes,
        config,
        zone_sync_token,
        controls,
        shutdown_token.clone(),
    )
    .await?;

    if smart_folder_passes.is_empty() {
        return Ok(incremental_result);
    }

    let smart_folder_config =
        if config.recent.is_some() && config.recent_scope == crate::cli::RecentScope::Global {
            Arc::new(config.with_recent_scope(crate::cli::RecentScope::PerFilter))
        } else {
            Arc::clone(config)
        };

    let mut smart_folder_result = download_photos_full_with_token_policy(
        download_client,
        &smart_folder_passes,
        &smart_folder_config,
        controls,
        shutdown_token,
        false,
    )
    .await?;

    let smart_folder_refresh_failed =
        !matches!(smart_folder_result.outcome, DownloadOutcome::Success)
            || smart_folder_result.stats.interrupted
            || smart_folder_result.stats.enumeration_errors > 0;
    if !smart_folder_refresh_failed {
        // The token that matters for this mixed incremental cycle is the
        // `/changes/zone` token captured below. A selected smart-folder
        // records/query refresh can complete cleanly while still lacking a
        // full-enumeration query token, especially under bounded modes. Keep
        // refresh failures conservative, but do not let query-token telemetry
        // veto the safe incremental zone checkpoint.
        clear_full_query_token_block_stats(&mut smart_folder_result.stats);
    }

    let SyncResult {
        outcome: incremental_outcome,
        sync_token: incremental_sync_token,
        stats: mut combined_stats,
        full_enumeration_ran: incremental_full_enumeration_ran,
    } = incremental_result;
    combined_stats.accumulate(&smart_folder_result.stats);

    if smart_folder_refresh_failed {
        block_sync_token_for_incremental_delta(
            &mut combined_stats,
            SMART_FOLDER_REFRESH_FAILED_REASON,
        );
    }

    let sync_token = (!combined_stats.sync_token_blocked)
        .then_some(incremental_sync_token)
        .flatten();
    let outcome = merge_download_outcomes(&incremental_outcome, &smart_folder_result.outcome);

    Ok(SyncResult {
        outcome,
        sync_token,
        stats: combined_stats,
        full_enumeration_ran: incremental_full_enumeration_ran
            || smart_folder_result.full_enumeration_ran,
    })
}

pub async fn download_photos_with_sync(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let sync_started_at = chrono::Utc::now().timestamp();
    cleanup_orphan_part_files(&config).await;
    if matches!(config.sync_mode, SyncMode::Incremental { .. })
        && let Some(db) = &config.state_db
    {
        backfill_asset_master_mappings_from_album_history(db.as_ref()).await;
    }
    let metadata_capture_repair =
        run_metadata_capture_repair(passes, &config, controls, &shutdown_token).await;

    // Give every non-downloaded asset a fresh start this sync:
    // failed -> pending (with attempts reset), and stale attempt counts on
    // pending assets cleared so the per-sync cap starts from zero.
    if let Some(db) = &config.state_db {
        match db.prune_source_deleted_retries(Some(&config.library)).await {
            Ok(0) => {}
            Ok(count) => {
                tracing::info!(
                    count,
                    "Pruned source-deleted pending/failed assets from retry queue"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to prune source-deleted retry assets");
            }
        }
        let error_retention = if config.repair_truncated {
            crate::state::RetryErrorRetention::Preserve(
                crate::commands::reconcile::FILE_TRUNCATED_REASON,
            )
        } else {
            crate::state::RetryErrorRetention::Clear
        };
        match db
            .prepare_for_retry(Some(&config.library), error_retention)
            .await
        {
            Ok((failed, stale, _)) => {
                if failed > 0 {
                    tracing::debug!(count = failed, "Reset failed assets for retry");
                }
                if stale > 0 {
                    tracing::debug!(
                        count = stale,
                        "Cleared stale attempt counts on pending assets"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to reset assets for retry");
            }
        }
    }

    let result = match &config.sync_mode {
        SyncMode::Full => {
            download_photos_full_with_token(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            )
            .await
        }
        SyncMode::Incremental { .. } if has_metadata_backfill_work(&config).await => {
            let reason = FullEnumerationReason::MetadataBackfill;
            tracing::info!(
                full_enumeration_reason = reason.as_str(),
                "Metadata backfill requires full enumeration, skipping incremental sync"
            );
            download_photos_full_with_reason(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
                reason,
            )
            .await
        }
        SyncMode::Incremental { zone_sync_token } => {
            match determine_incremental_routing_decision(passes, &config, controls).await {
                IncrementalRoutingDecision::NeedsFull { reason } => {
                    tracing::debug!(
                        full_enumeration_reason = reason.as_str(),
                        "Selected passes are not safe for incremental routing, skipping incremental"
                    );
                    download_photos_full_with_reason(
                        download_client,
                        passes,
                        &config,
                        controls,
                        shutdown_token.clone(),
                        reason,
                    )
                    .await
                }
                IncrementalRoutingDecision::TargetedAlbumBackfill {
                    pass_indices,
                    reason,
                } => {
                    tracing::debug!(
                        full_enumeration_reason = reason.as_str(),
                        backfill_passes = pass_indices.len(),
                        "Backfilling missing album snapshots before incremental routing"
                    );
                    download_photos_incremental_with_targeted_album_backfill(
                        download_client,
                        passes,
                        &config,
                        zone_sync_token,
                        controls,
                        shutdown_token.clone(),
                        &pass_indices,
                        reason,
                    )
                    .await
                }
                IncrementalRoutingDecision::Safe => {
                    let token = zone_sync_token.clone();
                    let has_smart_folder_pass = passes
                        .iter()
                        .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder);
                    let incremental_result = if has_smart_folder_pass {
                        download_photos_incremental_with_smart_folder_refresh(
                            download_client,
                            passes,
                            &config,
                            &token,
                            controls,
                            shutdown_token.clone(),
                        )
                        .await
                    } else {
                        download_photos_incremental(
                            download_client,
                            passes,
                            &config,
                            &token,
                            controls,
                            shutdown_token.clone(),
                        )
                        .await
                    };
                    match incremental_result {
                        Ok(result) => Ok(result),
                        Err(e) => match classify_incremental_error(&e) {
                            IncrementalErrorClass::TokenFallback
                            | IncrementalErrorClass::StaticFallback => {
                                let reason = FullEnumerationReason::OtherStaticReason;
                                tracing::warn!(
                                    error = %e,
                                    full_enumeration_reason = reason.as_str(),
                                    "Incremental sync failed, falling back to full enumeration"
                                );
                                download_photos_full_with_reason(
                                    download_client,
                                    passes,
                                    &config,
                                    controls,
                                    shutdown_token.clone(),
                                    reason,
                                )
                                .await
                            }
                            IncrementalErrorClass::TransientFailure => Err(e),
                        },
                    }
                }
            }
        }
    }?;

    let mut result = append_targeted_recovery_to_sync_result(
        download_client,
        passes,
        &config,
        controls,
        shutdown_token.clone(),
        result,
    )
    .await?;

    result.stats.accumulate(&metadata_capture_repair.stats);
    if metadata_capture_repair.failures > 0 {
        result.outcome = merge_download_outcomes(
            &result.outcome,
            &DownloadOutcome::PartialFailure {
                failed_count: metadata_capture_repair.failures,
            },
        );
    }
    if metadata_capture_repair.stats.sync_token_blocked {
        result.sync_token = None;
    }

    // Pending is transient — anything still pending after a complete sync either
    // wasn't enumerated or failed silently. Skip on interrupt where pending is expected.
    if let Some(db) = &config.state_db
        && !shutdown_token.is_cancelled()
    {
        match db.promote_pending_to_failed(sync_started_at).await {
            Ok(promoted) if promoted > 0 => {
                tracing::warn!(
                    count = promoted,
                    "Promoted unresolved pending assets to failed"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to promote pending assets");
            }
            _ => {}
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests;
