//! Sync entry point and composition of full, incremental, and recovery passes.

use std::sync::Arc;

use anyhow::Result;
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::icloud::photos::SyncTokenError;
use crate::icloud::photos::session::is_session_error as is_provider_session_error;

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
    SyncMode, SyncResult, SyncStats, TARGETED_ALBUM_BACKFILL_FAILED_REASON,
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
    SessionExpired,
    TransientFailure,
    StaticFallback,
    CaptureRefused,
}

/// Classify incremental-enumeration failures before deciding whether to fall
/// back to a full records/query pass.
///
/// Token errors that CloudKit explicitly marks unsafe fall back to full
/// enumeration. Auth and transport transients bubble up because a full pass
/// would likely hit the same service condition. Other static/decode errors
/// fall back so malformed token responses do not strand the user.
fn classify_incremental_error(error: &anyhow::Error) -> IncrementalErrorClass {
    if is_provider_session_error(error) {
        return IncrementalErrorClass::SessionExpired;
    }
    if error
        .downcast_ref::<crate::icloud::photos::error::ShadowPageError>()
        .is_some()
    {
        return IncrementalErrorClass::CaptureRefused;
    }
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

/// An activation state refusal cannot start another rank inventory. All other
/// errors retain the existing session, token, transport and decode classification.
fn classify_selection_incremental_error(error: &anyhow::Error) -> IncrementalErrorClass {
    if error
        .downcast_ref::<crate::state::error::StateError>()
        .is_some()
    {
        IncrementalErrorClass::CaptureRefused
    } else {
        classify_incremental_error(error)
    }
}

fn is_transient_reqwest_error(error: &reqwest::Error) -> bool {
    error
        .status()
        .is_some_and(|status| status == 429 || status.as_u16() >= 500)
        || error.is_timeout()
        || error.is_connect()
}

/// Apply one incremental failure decision and preserve its checkpoint evidence.
/// Captured selections retain their stricter state-refusal boundary and their
/// existing inventory owner; ordinary selections retain their full-pass reason.
async fn finish_incremental_attempt(
    attempt: Result<SyncResult>,
    client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    cancel: CancellationToken,
) -> Result<SyncResult> {
    let error = match attempt {
        Ok(result) => return Ok(result),
        Err(error) => error,
    };
    let class = if config.selection_context.is_some() {
        classify_selection_incremental_error(&error)
    } else {
        classify_incremental_error(&error)
    };
    match class {
        IncrementalErrorClass::SessionExpired
        | IncrementalErrorClass::TransientFailure
        | IncrementalErrorClass::CaptureRefused => return Err(error),
        IncrementalErrorClass::TokenFallback | IncrementalErrorClass::StaticFallback => {}
    }
    let mut result = if config.selection_context.is_some() {
        Box::pin(super::generation::inventory(
            client, passes, config, controls, cancel,
        ))
        .await?
    } else {
        let reason = FullEnumerationReason::OtherStaticReason;
        tracing::warn!(
            error = %error,
            full_enumeration_reason = reason.as_str(),
            "Incremental sync failed, falling back to full enumeration"
        );
        download_photos_full_with_reason(client, passes, config, controls, cancel, reason).await?
    };
    result.checkpoint.retained_token_rejected = class == IncrementalErrorClass::TokenFallback;
    Ok(result)
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
    let mut backfill_result = download_photos_full_with_reason(
        download_client,
        &backfill_passes,
        config,
        controls,
        shutdown_token.clone(),
        reason,
    )
    .await?;
    let backfill_failed = !matches!(backfill_result.outcome, DownloadOutcome::Success)
        || backfill_result.checkpoint.interrupted
        || backfill_result.checkpoint.enumeration_errors > 0
        || shutdown_token.is_cancelled()
        || !targeted_backfill_snapshots_complete(&backfill_passes, config).await;

    if backfill_failed {
        backfill_result.block_incremental_token(TARGETED_ALBUM_BACKFILL_FAILED_REASON);
        backfill_result.sync_token = None;
        return Ok(backfill_result);
    }

    // Full album queries may report their own query sync-token telemetry, but
    // targeted backfill does not use that token. The zone token may advance
    // only after the following /changes/zone pass completes safely.
    backfill_result.clear_full_query_token_block();

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

    backfill_result.outcome =
        merge_download_outcomes(&backfill_result.outcome, &incremental_result.outcome);
    backfill_result.accumulate(&incremental_result);
    backfill_result.sync_token = (!backfill_result.checkpoint.sync_token_blocked)
        .then_some(incremental_result.sync_token)
        .flatten();
    Ok(backfill_result)
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

    // Smart queries establish selection, not source replay. With no album or
    // unfiled pass, use the smart pass only to access its zone change stream.
    // Incremental routing has no destinations for SmartFolder passes, so media
    // admission remains owned by the selected query below.
    let delta_passes = if incremental_passes.is_empty() {
        &smart_folder_passes
    } else {
        &incremental_passes
    };

    let mut incremental_result = download_photos_incremental(
        download_client,
        delta_passes,
        config,
        zone_sync_token,
        controls,
        shutdown_token.clone(),
    )
    .await?;

    if smart_folder_passes.is_empty() {
        return Ok(incremental_result);
    }

    let mut smart_folder_result = download_photos_full_with_token_policy(
        download_client,
        &smart_folder_passes,
        config,
        controls,
        shutdown_token,
        incremental_passes.is_empty(),
    )
    .await?;

    let smart_folder_refresh_failed =
        !matches!(smart_folder_result.outcome, DownloadOutcome::Success)
            || smart_folder_result.checkpoint.interrupted
            || smart_folder_result.checkpoint.enumeration_errors > 0;
    if !incremental_passes.is_empty() && !smart_folder_refresh_failed {
        // A mixed cycle's smart query is a subset refresh; the independently
        // selected incremental passes own its zone checkpoint. A sole-smart
        // query establishes all selection evidence and must retain its token
        // hold, even when the source replay has a usable successor.
        smart_folder_result.clear_full_query_token_block();
    }

    if incremental_passes.is_empty() {
        // This query owns the whole selection, its bounded local path proof,
        // and comparable inventory. Source replay may veto that evidence, but
        // its token is the only candidate for the source checkpoint.
        smart_folder_result.outcome =
            merge_download_outcomes(&incremental_result.outcome, &smart_folder_result.outcome);
        smart_folder_result.accumulate(&incremental_result);
        if smart_folder_refresh_failed {
            smart_folder_result.block_incremental_token(SMART_FOLDER_REFRESH_FAILED_REASON);
        }
        smart_folder_result.sync_token = (!smart_folder_result.checkpoint.sync_token_blocked)
            .then_some(incremental_result.sync_token)
            .flatten();
        return Ok(smart_folder_result);
    }

    incremental_result.outcome =
        merge_download_outcomes(&incremental_result.outcome, &smart_folder_result.outcome);
    incremental_result.accumulate(&smart_folder_result);

    if smart_folder_refresh_failed {
        incremental_result.block_incremental_token(SMART_FOLDER_REFRESH_FAILED_REASON);
    }

    if incremental_result.checkpoint.sync_token_blocked {
        incremental_result.sync_token = None;
    }
    Ok(incremental_result)
}

/// Reuse the ordinary delta and inventory owners. Query tokens are EOF evidence
/// only; the retained source cursor and terminal delta successors own replay.
async fn reconcile_inventory(
    client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    prior: &str,
    controls: DownloadControls,
    cancel: CancellationToken,
) -> Result<SyncResult> {
    let classify = |error: &anyhow::Error| {
        if config.selection_context.is_some() {
            classify_selection_incremental_error(error)
        } else {
            classify_incremental_error(error)
        }
    };
    let mut rejected = false;
    let first =
        match download_photos_incremental(client, passes, config, prior, controls, cancel.clone())
            .await
        {
            Ok(mut first) => {
                if !first.checkpoint.completed_delta_replay
                    || first.checkpoint.delta_successor.is_none()
                    || matches!(first.outcome, DownloadOutcome::SessionExpired { .. })
                    || first.checkpoint.interrupted
                    || cancel.is_cancelled()
                {
                    first.sync_token = None;
                    first.block_incremental_token("inventory_reconciliation_required");
                    return Ok(first);
                }
                Some(first)
            }
            Err(error) => match classify(&error) {
                class @ (IncrementalErrorClass::TokenFallback
                | IncrementalErrorClass::StaticFallback) => {
                    rejected = class == IncrementalErrorClass::TokenFallback;
                    None
                }
                IncrementalErrorClass::SessionExpired
                | IncrementalErrorClass::TransientFailure
                | IncrementalErrorClass::CaptureRefused => return Err(error),
            },
        };
    let mut inventory = if config.selection_context.is_some() {
        Box::pin(super::generation::inventory(
            client,
            passes,
            config,
            controls,
            cancel.clone(),
        ))
        .await?
    } else {
        download_photos_full_with_token(client, passes, config, controls, cancel.clone()).await?
    };
    // Current delta failures veto qualification, not independently identified
    // healthy inventory work. The retained source cursor still covers any
    // observation that the private generation owner has not admitted yet.
    let inventory_complete = first.as_ref().is_none_or(reconciliation_work_complete)
        && (inventory.sync_token.take().is_some()
            || inventory.checkpoint.bounded_path_selection_complete)
        && reconciliation_work_complete(&inventory)
        && (!inventory.checkpoint.sync_token_blocked
            || inventory.checkpoint.bounded_path_selection_complete);
    let successor = first
        .as_ref()
        .and_then(|first| first.checkpoint.delta_successor.clone());
    if let Some(mut first) = first {
        inventory.outcome = merge_download_outcomes(&first.outcome, &inventory.outcome);
        // Only the inventory's snapshots can establish absence. Earlier incidental
        // selection work must not complete a different generation's snapshots.
        first.deferred_album_snapshots.clear();
        inventory.accumulate(&first);
    }
    if inventory_complete
        && !cancel.is_cancelled()
        && let Some(successor) = successor
    {
        match download_photos_incremental(
            client,
            passes,
            config,
            &successor,
            controls,
            cancel.clone(),
        )
        .await
        {
            Ok(mut replay) => {
                let qualified = replay.checkpoint.delta_raw_records_observed == Some(false)
                    && reconciliation_work_complete(&replay)
                    && (!replay.checkpoint.sync_token_blocked
                        || replay.checkpoint.bounded_path_selection_complete)
                    && replay.checkpoint.delta_successor.is_some()
                    && !cancel.is_cancelled();
                inventory.sync_token = replay.sync_token.take();
                replay.deferred_album_snapshots.clear();
                inventory.outcome = merge_download_outcomes(&inventory.outcome, &replay.outcome);
                inventory.accumulate(&replay);
                inventory.checkpoint.inventory_reconciliation_complete = qualified;
            }
            Err(error) => match classify(&error) {
                class @ (IncrementalErrorClass::TokenFallback
                | IncrementalErrorClass::StaticFallback) => {
                    // The selected inventory already ran. Do not repeat it or use
                    // its query token as a replacement for the rejected successor.
                    rejected = class == IncrementalErrorClass::TokenFallback;
                }
                IncrementalErrorClass::SessionExpired
                | IncrementalErrorClass::TransientFailure
                | IncrementalErrorClass::CaptureRefused => return Err(error),
            },
        }
    }
    // As with ordinary fallback, only completed inventory can reach the cycle's
    // durable retained-checkpoint hold. It never qualifies this request.
    inventory.checkpoint.retained_token_rejected = rejected && inventory_complete;
    if !inventory.checkpoint.inventory_reconciliation_complete {
        inventory.sync_token = None;
        inventory.block_incremental_token("inventory_reconciliation_required");
    }
    Ok(inventory)
}

fn reconciliation_work_complete(result: &SyncResult) -> bool {
    matches!(result.outcome, DownloadOutcome::Success)
        && result.stats.failed == 0
        && result.stats.exif_failures == 0
        && result.checkpoint.state_write_failures == 0
        && result.checkpoint.enumeration_errors == 0
        && !result.checkpoint.enumeration_incomplete
        && !result.checkpoint.identity_incomplete
        && !result.checkpoint.interrupted
}

pub async fn download_photos_with_sync(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    anyhow::ensure!(
        !controls.run_mode.downloads_files() || config.state_db.is_some(),
        "Writable sync requires a state database; use dry-run for a state-free preview"
    );
    // Bound the complete cycle's new selection/replay state at its owner,
    // preserving one opaque public future and avoiding caller-specific boxing.
    Box::pin(download_photos_with_sync_inner(
        download_client,
        passes,
        config,
        controls,
        shutdown_token,
    ))
    .await
}

async fn download_photos_with_sync_inner(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: Arc<DownloadConfig>,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    if config.provisional {
        // Do not turn incomplete listings/lookups into deletion, membership,
        // generation activation, retry retirement, or incremental evidence.
        // Managed replacement needs independently certified current coverage.
        let managed = config.edited_naming == crate::types::EditedNaming::Primary
            || match &config.state_db {
                Some(db) => db.has_primary_layouts(&config.library).await?,
                None => false,
            };
        anyhow::ensure!(
            !managed || config.indexing_inventory_only,
            "Unqualified indexing recovery cannot certify managed primary replacement coverage"
        );
        return download_photos_full_with_token_policy(
            download_client,
            passes,
            &config,
            controls,
            shutdown_token,
            false,
        )
        .await;
    }
    let primary_layout_requested = config.edited_naming == crate::types::EditedNaming::Primary
        || match &config.state_db {
            Some(db) => db.has_primary_layouts(&config.library).await?,
            None => false,
        };
    let primary_layout_active = controls.run_mode.downloads_files() && primary_layout_requested;
    let primary_layout_preview = !controls.run_mode.downloads_files() && primary_layout_requested;
    let config = Arc::new(DownloadConfig {
        complete_path_selection: config.complete_path_selection || config.inventory_reconciliation,
        primary_layout_active,
        primary_layout_preview,
        ..config.as_ref().clone()
    });
    let sync_started_at = chrono::Utc::now().timestamp();
    super::generation::freeze_before_sync(passes, &config, controls).await?;
    if matches!(controls.run_mode, super::models::DownloadRunMode::Download) {
        let protected = crate::download::legacy_preservation::protected_replacement_paths(
            config.state_db.as_deref(),
        )
        .await?;
        crate::download::file::recover_conditional_replacements(&config.directory, &protected)
            .await?;
    }
    if primary_layout_active {
        super::primary_layout::recover(download_client, passes, &config, &shutdown_token).await?;
    }
    if controls.run_mode.downloads_files() {
        cleanup_orphan_part_files(&config).await;
    }
    if matches!(config.sync_mode, SyncMode::Incremental { .. })
        && let Some(db) = &config.state_db
    {
        backfill_asset_master_mappings_from_album_history(db.as_ref()).await;
    }
    let metadata_capture_repair =
        run_metadata_capture_repair(passes, &config, controls, &shutdown_token).await;
    if metadata_capture_repair.auth_errors > 0 {
        return Ok(SyncResult {
            deferred_album_snapshots: Vec::new(),
            retry_exclusions: Default::default(),
            outcome: DownloadOutcome::SessionExpired {
                auth_error_count: metadata_capture_repair.auth_errors,
            },
            sync_token: None,
            stats: metadata_capture_repair.stats,
            checkpoint: metadata_capture_repair.checkpoint,
            full_enumeration_ran: false,
            inventory: None,
        });
    }

    let config =
        if let Some(context) = super::generation::context(passes, &config, controls).await? {
            Arc::new(DownloadConfig {
                selection_context: Some(Arc::new(context)),
                ..config.as_ref().clone()
            })
        } else {
            config
        };

    // Only the legacy retry owner resets rendition-wide scheduling. Selected
    // destinations retain their own attempts and due times across cycles.
    if controls.run_mode.downloads_files()
        && config.selection_context.is_none()
        && let Some(db) = &config.state_db
    {
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

    if !primary_layout_active && config.selection_context.is_none() {
        super::queue_projection::admit_retained_work(passes, &config, controls, &shutdown_token)
            .await?;
    }

    let result = match &config.sync_mode {
        SyncMode::Incremental { zone_sync_token } if config.inventory_reconciliation => {
            reconcile_inventory(
                download_client,
                passes,
                &config,
                zone_sync_token,
                controls,
                shutdown_token.clone(),
            )
            .await
        }
        SyncMode::Full if config.selection_context.is_some() => {
            Box::pin(super::generation::inventory(
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            ))
            .await
        }
        SyncMode::Incremental { zone_sync_token } if config.selection_context.is_some() => {
            let attempt = download_photos_incremental(
                download_client,
                passes,
                &config,
                zone_sync_token,
                controls,
                shutdown_token.clone(),
            )
            .await;
            finish_incremental_attempt(
                attempt,
                download_client,
                passes,
                &config,
                controls,
                shutdown_token.clone(),
            )
            .await
        }
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
        // Revision repair already owns stale catalogue rows. Re-enumerating the
        // library cannot resolve ambiguous identity and defeats its batch bound.
        SyncMode::Incremental { .. }
            if metadata_capture_repair.stats.metadata_capture_remaining == 0
                && has_metadata_backfill_work(&config).await =>
        {
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
                    finish_incremental_attempt(
                        incremental_result,
                        download_client,
                        passes,
                        &config,
                        controls,
                        shutdown_token.clone(),
                    )
                    .await
                }
            }
        }
    }
    .or_else(|error| {
        if !is_provider_session_error(&error) {
            return Err(error);
        }
        Ok(SyncResult {
            full_enumeration_ran: matches!(config.sync_mode, SyncMode::Full),
            ..SyncResult::from_execution(
                DownloadOutcome::SessionExpired {
                    auth_error_count: 1,
                },
                None,
                SyncStats {
                    enumeration_errors: 1,
                    enumeration_incomplete: true,
                    ..SyncStats::default()
                },
            )
        })
    })?;

    // Current selection already owns admission, replay and completion of every
    // selected destination. Aggregate rendition retry cannot add authority here.
    let mut result = if config.selection_context.is_some() {
        result
    } else {
        append_targeted_recovery_to_sync_result(
            download_client,
            passes,
            &config,
            controls,
            shutdown_token.clone(),
            result,
        )
        .await?
    };

    let repair_outcome = if metadata_capture_repair.failures > 0
        || metadata_capture_repair.stats.metadata_capture_unresolved > 0
    {
        DownloadOutcome::PartialFailure {
            failed_count: metadata_capture_repair.failures.max(1),
        }
    } else {
        DownloadOutcome::Success
    };
    let repair = SyncResult {
        deferred_album_snapshots: Vec::new(),
        retry_exclusions: Default::default(),
        outcome: repair_outcome,
        sync_token: None,
        stats: metadata_capture_repair.stats,
        checkpoint: metadata_capture_repair.checkpoint,
        full_enumeration_ran: false,
        inventory: None,
    };
    if matches!(repair.outcome, DownloadOutcome::PartialFailure { .. }) {
        result.outcome = merge_download_outcomes(&result.outcome, &repair.outcome);
    }
    result.accumulate(&repair);
    if repair.checkpoint.sync_token_blocked {
        result.sync_token = None;
    }

    // Legacy managed publication can complete a newly captured source this cycle.
    // Its bounded admission runs after capture; current selection already owns
    // admission and completion for destination-managed runs.
    if primary_layout_active
        && config.selection_context.is_none()
        && matches!(result.outcome, DownloadOutcome::Success)
        && !shutdown_token.is_cancelled()
    {
        super::queue_projection::admit_retained_work(passes, &config, controls, &shutdown_token)
            .await?;
    }
    super::generation::hold_retained_debt(passes, &config, controls, &mut result).await?;

    // Legacy rendition retry uses failed rows as its next-cycle schedule.
    // Destination-owned and read-only runs must not rewrite that historical state.
    if controls.run_mode.downloads_files()
        && config.selection_context.is_none()
        && let Some(db) = &config.state_db
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
