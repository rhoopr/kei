//! Streaming and collecting execution of incremental changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::download::filter::DownloadTask;
use crate::download::metadata_rewrite::CaptureTimestampRepair;
use crate::download::pipeline::{
    AUTH_ERROR_THRESHOLD, MetadataFlags, PassConfig, StreamRuntime, build_download_outcome,
    format_duration, log_sync_summary, run_download_pass, state_confirmed_current_path_exists,
    stream_and_download_from_stream,
};
use crate::download::{filter, metadata_rewrite, planner};
use crate::icloud::photos::PhotoAsset;
use crate::icloud::photos::session::is_session_error as is_provider_session_error;
use crate::types::ChangeReason;

use super::config::DownloadConfig;
use super::context::{
    ClaimedLegacyMasterStates, DownloadContext, LegacyOwnerClaimMode,
    legacy_owner_claim_mode_for_configs, preload_download_context,
};
use super::delta::{
    IncrementalAssetHydrationContext, IncrementalDeltaRouting, IncrementalDeltaState,
    IncrementalDeltaSummary, hydrate_missing_selected_relation_assets,
    hydrate_unpaired_created_asset_deltas,
};
use super::models::{
    ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON, DownloadControls, DownloadOutcome, DownloadRunMode,
    PROVIDER_METADATA_STATE_WRITE_FAILED_REASON, SkipBreakdown, SyncResult, SyncStats,
    block_sync_token_for_incremental_delta, merge_download_outcomes,
};
use super::selection::{
    IncrementalPassRouting, build_pass_configs, route_incremental_asset_to_passes,
};
use super::url_refresh::{
    INCREMENTAL_PREFLIGHT_URL_REFRESH_AFTER, RetryTaskKey, UrlRetrySource,
    build_incremental_expired_url_retry_tasks, merge_expired_url_retry_result,
    refresh_stale_incremental_tasks_before_download,
};

fn single_unfiled_streaming_pass<'a>(
    passes: &'a [crate::commands::AlbumPass],
    config: &DownloadConfig,
    routing: &IncrementalPassRouting,
) -> Option<&'a crate::commands::AlbumPass> {
    // Keep relation-sensitive cases on the collecting path: selected albums
    // need all relation deltas applied before routing created assets, and
    // recent selection uses a separate current inventory. The unfiled-only
    // path can stream created assets immediately because album relation deltas
    // update state for future cycles but do not change this pass's routing.
    if config.recent.is_some()
        || routing.has_selected_albums()
        || routing.unfiled_passes.len() != 1
        || passes.len() != 1
    {
        return None;
    }

    let index = *routing.unfiled_passes.first()?;
    passes
        .get(index)
        .filter(|pass| pass.kind == crate::commands::PassKind::Unfiled)
}

fn stream_incremental_assets_for_single_unfiled_pass(
    pass: crate::commands::AlbumPass,
    config: Arc<DownloadConfig>,
    zone_sync_token: String,
    run_mode: DownloadRunMode,
    shutdown_token: CancellationToken,
) -> (
    ReceiverStream<Result<PhotoAsset>>,
    tokio::task::JoinHandle<Result<IncrementalDeltaSummary>>,
) {
    let capacity = config.concurrent_downloads.saturating_mul(2).max(1);
    let (asset_tx, asset_rx) = mpsc::channel::<Result<PhotoAsset>>(capacity);
    let (mut change_stream, token_rx) = pass.album.changes_stream(&zone_sync_token);
    let handle = tokio::spawn(async move {
        let mut delta = IncrementalDeltaState::new(std::slice::from_ref(&pass));
        let routing = IncrementalPassRouting::from_passes(std::slice::from_ref(&pass));
        let mut album_events = Vec::new();
        let mut relation_events = Vec::new();
        let mut unpaired_asset_events = Vec::new();

        while let Some(result) = change_stream.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            let event = match result {
                Err(error) if is_provider_session_error(&error) => {
                    delta.summary.auth_errors += 1;
                    let _ = asset_tx.send(Err(error)).await;
                    return Ok(delta.summary);
                }
                other => other?,
            };
            delta.observe_event(&event);
            delta.persist_asset_mapping(&event, &config).await;
            match delta.apply_event(&event, &config).await {
                IncrementalDeltaRouting::Album => album_events.push(event),
                IncrementalDeltaRouting::Relation => relation_events.push(event),
                IncrementalDeltaRouting::Created => {
                    if let Some(asset) = event.asset {
                        if asset_tx.send(Ok(asset)).await.is_err() {
                            return Ok(delta.summary);
                        }
                    } else if matches!(event.record_type.as_deref(), Some("CPLAsset")) {
                        unpaired_asset_events.push(event);
                    }
                }
                IncrementalDeltaRouting::None => {
                    if event.reason == ChangeReason::HardDeleted
                        || (event.reason == ChangeReason::SoftDeleted
                            && event.record_type.as_deref() == Some("CPLAsset"))
                    {
                        unpaired_asset_events.push(event);
                    }
                }
            }
        }

        delta.record_completion(token_rx.await.ok());
        hydrate_unpaired_created_asset_deltas(
            &mut unpaired_asset_events,
            Some(&pass),
            &config,
            &mut delta.summary,
            run_mode,
            &shutdown_token,
        )
        .await;
        let download_ctx = if run_mode.downloads_files()
            && unpaired_asset_events
                .iter()
                .any(|event| event.reason == ChangeReason::Created)
        {
            Some(preload_download_context(&config).await)
        } else {
            None
        };
        let mut claimed_legacy_master_states = ClaimedLegacyMasterStates::default();
        for event in unpaired_asset_events
            .into_iter()
            .filter(|event| event.reason == ChangeReason::Created)
        {
            if let Some(mut asset) = event.asset {
                if let Some(download_ctx) = download_ctx.as_deref() {
                    let claim_mode = legacy_owner_claim_mode_for_configs(
                        LegacyOwnerClaimMode::Persist,
                        &asset,
                        std::iter::once(config.as_ref()),
                    );
                    let Some(selected_asset) = delta
                        .summary
                        .select_asset_state_identity(
                            asset,
                            &config,
                            download_ctx,
                            &mut claimed_legacy_master_states,
                            claim_mode,
                        )
                        .await
                    else {
                        continue;
                    };
                    asset = selected_asset;
                    apply_changed_provider_metadata(
                        &config,
                        &asset,
                        download_ctx,
                        &mut delta.summary,
                    )
                    .await;
                }
                if asset_tx.send(Ok(asset)).await.is_err() {
                    return Ok(delta.summary);
                }
            }
        }

        for event in &album_events {
            delta.apply_album_event(event, &config).await;
        }
        for event in &relation_events {
            delta.apply_relation_event(event, &config, &routing).await;
        }

        // Send hydration failures before EOF so the pipeline persists an
        // interrupted run instead of recording a successful enumeration.
        if let Some(error) = delta.summary.first_auth_error.take() {
            let _ = asset_tx.send(Err(error)).await;
            return Ok(delta.summary);
        }
        Ok(delta.summary)
    });

    (ReceiverStream::new(asset_rx), handle)
}

async fn download_photos_incremental_streaming(
    download_client: &Client,
    pass: &crate::commands::AlbumPass,
    pass_config: Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    started: Instant,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let (asset_stream, delta_handle) = stream_incremental_assets_for_single_unfiled_pass(
        pass.clone(),
        Arc::clone(&pass_config),
        zone_sync_token.to_string(),
        controls.run_mode,
        shutdown_token.clone(),
    );
    let mut streaming_result = match stream_and_download_from_stream(
        download_client,
        asset_stream,
        &pass_config,
        controls,
        0,
        shutdown_token.clone(),
        StreamRuntime::new(None, None),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            delta_handle.abort();
            return Err(e);
        }
    };
    let delta_summary = delta_handle
        .await
        .context("incremental changes producer task panicked")??;

    delta_summary.log_debug();
    // The pipeline already counted the representative error sent by the
    // producer. Preserve the full lookup failure count without counting twice.
    streaming_result.provider_auth_errors = streaming_result
        .provider_auth_errors
        .max(delta_summary.auth_errors);
    if delta_summary.auth_errors > 0 {
        streaming_result.enumeration_complete = false;
    }

    let (mut outcome, mut stats) = build_download_outcome(
        download_client,
        std::slice::from_ref(pass),
        &pass_config,
        controls,
        streaming_result,
        started,
        shutdown_token.clone(),
    )
    .await?;

    stats.state_write_failures += delta_summary.state_transition_failures;
    stats.interrupted = stats.interrupted || shutdown_token.is_cancelled();
    stats.identity_incomplete = delta_summary.identity_incomplete;
    if let Some(reason) = delta_summary.token_unsafe_reason {
        block_sync_token_for_incremental_delta(&mut stats, reason);
    }
    if delta_summary.state_transition_failures > 0 {
        outcome = merge_download_outcomes(
            &outcome,
            &DownloadOutcome::PartialFailure {
                failed_count: delta_summary.state_transition_failures,
            },
        );
    }

    let sync_token = if controls.run_mode.only_print_filenames() || controls.run_mode.is_dry_run() {
        None
    } else {
        (!stats.sync_token_blocked && delta_summary.auth_errors == 0)
            .then_some(delta_summary.sync_token)
            .flatten()
    };

    Ok(SyncResult::from_incremental_execution(
        outcome,
        sync_token,
        stats,
        delta_summary.sparse_identity_proofs,
    ))
}

/// Incremental delta sync via `changes_stream`.
///
/// Fetches `ChangeEvent`s since the given sync token, filters to
/// downloadable assets, and feeds them through the download pipeline.
pub(super) async fn download_photos_incremental(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let routing = IncrementalPassRouting::from_passes(passes);
    if !super::recent::active(config)
        .await
        .map_err(super::recent::RecentSelectionError)?
        && let Some(pass) = single_unfiled_streaming_pass(passes, config, &routing)
    {
        let pass_config = Arc::new(config.with_pass(pass));
        return download_photos_incremental_streaming(
            download_client,
            pass,
            pass_config,
            zone_sync_token,
            controls,
            Instant::now(),
            shutdown_token,
        )
        .await;
    }

    download_photos_incremental_collecting(
        download_client,
        passes,
        config,
        zone_sync_token,
        controls,
        shutdown_token,
    )
    .await
}

pub(super) async fn download_photos_incremental_collecting(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    download_photos_incremental_collecting_inner(
        download_client,
        passes,
        config,
        zone_sync_token,
        controls,
        shutdown_token,
        INCREMENTAL_PREFLIGHT_URL_REFRESH_AFTER,
    )
    .await
}

async fn apply_changed_provider_metadata(
    config: &DownloadConfig,
    asset: &PhotoAsset,
    download_ctx: &DownloadContext,
    summary: &mut IncrementalDeltaSummary,
) {
    let Some(db) = &config.state_db else {
        return;
    };
    let library = asset.source_zone().unwrap_or(&config.library);
    let capture = filter::metadata_capture(asset);
    if !download_ctx.has_provider_metadata_drift(library, asset.state_id(), &capture) {
        return;
    }
    let mark_for_rewrite = MetadataFlags::from(config).has_any_write();
    let refresh = db
        .refresh_downloaded_asset_metadata(
            library,
            asset.state_id(),
            (&capture, asset.created(), Some(asset.added_date())),
            mark_for_rewrite,
            false,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await;
    match refresh {
        Ok(updated) if updated > 0 => {}
        Ok(_) => {
            summary.state_transition_failures += 1;
            summary
                .token_unsafe_reason
                .get_or_insert(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON);
            tracing::warn!(
                asset_id = %asset.id(),
                library,
                "Changed provider metadata matched no downloaded state row"
            );
        }
        Err(e) => {
            summary.state_transition_failures += 1;
            summary
                .token_unsafe_reason
                .get_or_insert(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON);
            tracing::warn!(
                asset_id = %asset.id(),
                library,
                error = %e,
                "Failed to persist changed provider metadata"
            );
        }
    }
}

async fn run_collecting_metadata_rewrite_batch(
    config: &DownloadConfig,
    run_mode: DownloadRunMode,
    shutdown_token: &CancellationToken,
) -> usize {
    if !run_mode.downloads_files() {
        return 0;
    }
    let Some(db) = &config.state_db else {
        return 0;
    };
    let metadata = MetadataFlags::from(config);
    if !metadata.has_any_write() {
        return 0;
    }
    metadata_rewrite::run_pending(
        db.as_ref(),
        metadata,
        Arc::clone(&config.temp_suffix),
        shutdown_token,
    )
    .await
    .failed
}

pub(super) async fn download_photos_incremental_collecting_inner(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
    preflight_url_refresh_after: Duration,
) -> Result<SyncResult> {
    let started = Instant::now();
    let legacy_owner_claim_mode = if controls.run_mode.downloads_files() {
        LegacyOwnerClaimMode::Persist
    } else {
        LegacyOwnerClaimMode::ReadOnly
    };
    let pass_configs = build_pass_configs(passes, config);

    // Each asset is paired with its source pass index so both `{album}`
    // expansion and per-pass exclusion (notably, the unfiled pass's set
    // that prevents assets already in some user album from downloading
    // twice) can be applied downstream.
    let mut downloadable_assets: Vec<(PhotoAsset, usize)> = Vec::new();
    let mut change_events = Vec::new();
    let mut first_download_url_obtained_at: Option<Instant> = None;
    let mut delta = IncrementalDeltaState::new(passes);
    let routing = IncrementalPassRouting::from_passes(passes);
    let selected_container_ids = routing.selected_container_refs();

    let recent_active = super::recent::active(config)
        .await
        .map_err(super::recent::RecentSelectionError)?;
    let delta_pass = if recent_active {
        passes
            .iter()
            .find(|pass| pass.kind == crate::commands::PassKind::Unfiled)
            .or_else(|| passes.first())
    } else {
        passes.first()
    };

    // `changes_stream` is zone-scoped, not album-scoped. Query it once and
    // fan created assets out through the selected passes locally; querying
    // once per pass repeats the same `/changes/zone` pages on every watch
    // cycle with work.
    if let Some(pass) = delta_pass {
        let phase_started = Instant::now();
        let (change_stream, token_rx) = pass.album.changes_stream(zone_sync_token);
        tokio::pin!(change_stream);

        while let Some(result) = change_stream.next().await {
            if shutdown_token.is_cancelled() {
                break;
            }
            let event = result?;
            if first_download_url_obtained_at.is_none() && event.asset.is_some() {
                first_download_url_obtained_at = Some(Instant::now());
            }
            delta.observe_event(&event);
            change_events.push(event);
        }

        delta.record_completion(token_rx.await.ok());
        tracing::debug!(
            phase_elapsed = %format_duration(phase_started.elapsed()),
            elapsed = %format_duration(started.elapsed()),
            events = change_events.len(),
            "Incremental changes read complete"
        );
    }

    let phase_started = Instant::now();
    let mut download_ctx = if change_events
        .iter()
        .any(|event| event.reason == ChangeReason::Created)
    {
        Some(preload_download_context(config).await)
    } else {
        None
    };
    for event in &change_events {
        delta.persist_asset_mapping(event, config).await;
    }
    hydrate_unpaired_created_asset_deltas(
        &mut change_events,
        delta_pass,
        config,
        &mut delta.summary,
        controls.run_mode,
        &shutdown_token,
    )
    .await;
    if download_ctx.is_none()
        && change_events
            .iter()
            .any(|event| event.reason == ChangeReason::Created)
    {
        download_ctx = Some(preload_download_context(config).await);
    }
    let mut claimed_legacy_master_states = ClaimedLegacyMasterStates::default();
    if let Some(download_ctx) = download_ctx.as_deref() {
        for event in &mut change_events {
            if event.reason != ChangeReason::Created {
                continue;
            }
            let Some(asset) = event.asset.take() else {
                continue;
            };
            let claim_mode = legacy_owner_claim_mode_for_configs(
                legacy_owner_claim_mode,
                &asset,
                pass_configs.iter().map(AsRef::as_ref),
            );
            event.asset = delta
                .summary
                .select_asset_state_identity(
                    asset,
                    config,
                    download_ctx,
                    &mut claimed_legacy_master_states,
                    claim_mode,
                )
                .await;
        }
    }
    let mut complete_delta_assets: Vec<PhotoAsset> = change_events
        .iter()
        .filter_map(IncrementalDeltaState::created_asset)
        .cloned()
        .collect();
    for event in &change_events {
        delta.remember_asset_mapping(event);
    }
    for event in &change_events {
        delta.apply_album_event(event, config).await;
    }
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        "Incremental album state phase complete"
    );

    let phase_started = Instant::now();
    let downloadable_before_relation_hydration = downloadable_assets.len();
    {
        let mut hydration_context = IncrementalAssetHydrationContext {
            asset_to_master: &mut delta.asset_to_master,
            complete_delta_assets: &mut complete_delta_assets,
            downloadable_assets: &mut downloadable_assets,
            download_ctx: download_ctx.as_deref(),
            claimed_legacy_master_states: &mut claimed_legacy_master_states,
            claim_mode: legacy_owner_claim_mode,
            pass_configs: &pass_configs,
        };
        hydrate_missing_selected_relation_assets(
            &change_events,
            passes,
            config,
            &routing,
            &mut hydration_context,
            &mut delta.summary,
        )
        .await;
    }
    if first_download_url_obtained_at.is_none()
        && downloadable_assets.len() > downloadable_before_relation_hydration
    {
        first_download_url_obtained_at = Some(Instant::now());
    }
    let hydrated_assets = downloadable_assets
        .len()
        .saturating_sub(downloadable_before_relation_hydration);
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        hydrated_assets,
        "Incremental relation hydration phase complete"
    );

    if controls.run_mode.downloads_files() && !complete_delta_assets.is_empty() {
        let context = match download_ctx.take() {
            Some(context) => context,
            None => preload_download_context(config).await,
        };
        let mut refreshed_assets = FxHashSet::default();
        for asset in &complete_delta_assets {
            if !refreshed_assets.insert(asset.state_id_arc()) {
                continue;
            }
            apply_changed_provider_metadata(config, asset, &context, &mut delta.summary).await;
        }
        download_ctx = Some(context);
    }

    let phase_started = Instant::now();
    for event in &change_events {
        delta.apply_relation_event(event, config, &routing).await;
    }

    for event in &change_events {
        if delta.apply_event(event, config).await != IncrementalDeltaRouting::Created {
            continue;
        }
        let Some(asset) = &event.asset else {
            continue;
        };
        match route_incremental_asset_to_passes(asset, &routing, &selected_container_ids, config)
            .await
        {
            Ok(pass_indices) => {
                for pass_index in pass_indices {
                    downloadable_assets.push((asset.clone(), pass_index));
                }
            }
            Err(e) => {
                tracing::warn!(
                    asset_record_name = %asset.asset_record_name(),
                    asset_id = %asset.id(),
                    error = %e,
                    "Failed to route incremental asset through album membership state"
                );
                delta
                    .summary
                    .token_unsafe_reason
                    .get_or_insert(ALBUM_RELATION_HYDRATION_INCOMPLETE_REASON);
            }
        }
    }
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        downloadable_assets = downloadable_assets.len(),
        "Incremental routing phase complete"
    );

    let delta_summary = delta.summary;
    delta_summary.log_debug();

    if delta_summary.auth_errors > 0 {
        let mut stats = SyncStats {
            state_write_failures: delta_summary.state_transition_failures,
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..SyncStats::default()
        };
        stats.identity_incomplete = delta_summary.identity_incomplete;
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut stats, reason);
        }
        return Ok(SyncResult::from_incremental_execution(
            DownloadOutcome::SessionExpired {
                auth_error_count: delta_summary.auth_errors,
            },
            None,
            stats,
            delta_summary.sparse_identity_proofs,
        ));
    }

    if recent_active
        && super::recent::needs_inventory(
            passes,
            config,
            zone_sync_token,
            delta_summary.total_events > 0,
        )
        .await
        .map_err(super::recent::RecentSelectionError)?
    {
        if controls.run_mode.downloads_files() {
            super::recent::record(passes, config, zone_sync_token, false)
                .await
                .map_err(super::recent::RecentSelectionError)?;
        }
        // Completed delta evidence can hold source progress while independent
        // current selection still materializes healthy media. Preserve every
        // delta veto below; the reused queue/publication owner guards each job.
        // Do not infer failure kind from the first-wins diagnostic reason.
        if delta_summary.sync_token.is_none() || shutdown_token.is_cancelled() {
            let mut stats = SyncStats {
                state_write_failures: delta_summary.state_transition_failures,
                identity_incomplete: delta_summary.identity_incomplete,
                interrupted: shutdown_token.is_cancelled(),
                ..SyncStats::default()
            };
            if let Some(reason) = delta_summary.token_unsafe_reason {
                block_sync_token_for_incremental_delta(&mut stats, reason);
            }
            return Ok(SyncResult::from_incremental_execution(
                if delta_summary.state_transition_failures > 0 {
                    DownloadOutcome::PartialFailure {
                        failed_count: delta_summary.state_transition_failures,
                    }
                } else {
                    DownloadOutcome::Success
                },
                None,
                stats,
                delta_summary.sparse_identity_proofs,
            ));
        }
        // Delta arrival is not an ordered library/pass inventory. Reuse the
        // recent selection owner for all currently eligible assets, including
        // unchanged assets that entered the window after a removal.
        let mut selected = Box::pin(super::full::download_photos_full_with_token(
            download_client,
            passes,
            config,
            controls,
            shutdown_token.clone(),
        ))
        .await
        .map_err(|error| {
            // Keep typed session failures visible to the dispatcher so its
            // existing bounded reauthentication path can retain the receipt.
            if crate::icloud::photos::session::is_session_error(&error) {
                error
            } else {
                super::recent::RecentSelectionError(error).into()
            }
        })?;
        let mut delta_stats = SyncStats {
            state_write_failures: delta_summary.state_transition_failures,
            identity_incomplete: delta_summary.identity_incomplete,
            interrupted: shutdown_token.is_cancelled(),
            ..SyncStats::default()
        };
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut delta_stats, reason);
        }
        let delta_result = SyncResult::from_incremental_execution(
            if delta_summary.state_transition_failures > 0 {
                DownloadOutcome::PartialFailure {
                    failed_count: delta_summary.state_transition_failures,
                }
            } else {
                DownloadOutcome::Success
            },
            None,
            delta_stats,
            delta_summary.sparse_identity_proofs,
        );
        selected.outcome = merge_download_outcomes(&selected.outcome, &delta_result.outcome);
        selected.accumulate(&delta_result);
        if let Some(reason) = delta_summary.token_unsafe_reason {
            selected.block_incremental_token(reason);
        }
        // The validated source stream completed even when unresolved identity
        // still holds its successor. Keep the cycle owner from replaying an
        // already completed delta as though this were a pure rank inventory.
        selected.checkpoint.completed_delta_replay = true;
        // The rank query's token is only an EOF proof for selection. It never
        // replaces the actual completed changes/zone successor. Preserve every
        // full-selection and delta veto, including a bounded recent inventory.
        let selection_complete = selected.sync_token.take().is_some()
            && !selected.checkpoint.sync_token_blocked
            && !selected.checkpoint.identity_incomplete
            && !selected.checkpoint.interrupted
            && !selected.checkpoint.enumeration_incomplete
            && selected.checkpoint.enumeration_errors == 0
            && selected.checkpoint.state_write_failures == 0
            && controls.run_mode.downloads_files();
        if selection_complete && let Some(successor) = delta_summary.sync_token {
            super::recent::record(
                passes,
                config,
                &successor,
                matches!(selected.outcome, DownloadOutcome::Success),
            )
            .await
            .map_err(super::recent::RecentSelectionError)?;
            selected.sync_token = Some(successor);
        }
        return Ok(selected);
    }

    if downloadable_assets.is_empty() {
        let rewrite_failures =
            run_collecting_metadata_rewrite_batch(config, controls.run_mode, &shutdown_token).await;
        let mut stats = SyncStats {
            elapsed_secs: started.elapsed().as_secs_f64(),
            state_write_failures: delta_summary.state_transition_failures,
            exif_failures: rewrite_failures,
            interrupted: shutdown_token.is_cancelled(),
            ..SyncStats::default()
        };
        stats.identity_incomplete = delta_summary.identity_incomplete;
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut stats, reason);
        }
        tracing::info!("No new photos to download from incremental sync");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        let sync_token = if controls.run_mode.only_print_filenames() {
            None
        } else {
            (!stats.sync_token_blocked)
                .then_some(delta_summary.sync_token)
                .flatten()
        };
        if recent_active
            && controls.run_mode.downloads_files()
            && let Some(successor) = sync_token.as_deref()
        {
            super::recent::record(
                passes,
                config,
                successor,
                delta_summary.state_transition_failures == 0
                    && !stats.interrupted
                    && !stats.identity_incomplete,
            )
            .await
            .map_err(super::recent::RecentSelectionError)?;
        }
        return Ok(SyncResult::from_incremental_execution(
            if delta_summary.state_transition_failures > 0 || rewrite_failures > 0 {
                DownloadOutcome::PartialFailure {
                    failed_count: delta_summary
                        .state_transition_failures
                        .saturating_add(rewrite_failures),
                }
            } else {
                DownloadOutcome::Success
            },
            sync_token,
            stats,
            delta_summary.sparse_identity_proofs,
        ));
    }

    let download_ctx = match download_ctx {
        Some(download_ctx) => download_ctx,
        None => preload_download_context(config).await,
    };
    let mut planning_state_write_failures = 0usize;

    tracing::debug!(
        count = downloadable_assets.len(),
        "Assets to download from incremental sync"
    );

    // Convert assets to download tasks via path-aware on-disk verification.
    // Each pass (concrete album or unfiled) gets its own derived config so
    // that both album-specific path expansion and per-pass exclude sets are
    // applied. Configs are cached per pass index to avoid redundant
    // allocations when many assets flow through the same pass.
    let mut tasks: Vec<DownloadTask> = Vec::new();
    let mut task_planner = planner::TaskPlanner::for_download(config.state_db.as_deref()).await?;
    let mut skip_breakdown = SkipBreakdown::default();
    let mut enumeration_errors = 0usize;
    // Incremental routing already decides whether a changed asset belongs to
    // a selected album pass or the unfiled pass from trusted membership
    // state. Re-enumerating every album here to rebuild unfiled excludes
    // delays consumption of the fresh signed download URLs from
    // `changes/zone`, which can make those URLs expire before downloads
    // start on large album sets.
    let phase_started = Instant::now();
    let mut retry_sources: FxHashMap<RetryTaskKey, UrlRetrySource> = FxHashMap::default();

    for (asset, pass_index) in &downloadable_assets {
        #[allow(
            clippy::indexing_slicing,
            reason = "pass_index was assigned by the producer from the same `passes` slice \
                      that pass_configs was built from; indices are valid"
        )]
        let effective_config = &pass_configs[*pass_index];

        let mut plan = task_planner
            .plan_download_asset(asset, effective_config)
            .await?;
        if let Some(reason) = plan.filter_reason {
            skip_breakdown.record_filter_reason(reason);
            continue;
        }
        if let Some(resource) = &plan.malformed_resource {
            enumeration_errors += 1;
            tracing::error!(
                asset_id = %asset.id(),
                field = %resource.field,
                reason = %resource.reason,
                "Malformed CloudKit resource prevented incremental download planning"
            );
            continue;
        }

        let mut state_skipped = 0usize;
        let planned_tasks = std::mem::take(&mut plan.tasks);
        plan.tasks.reserve(planned_tasks.len());
        for task in planned_tasks {
            let keep = match download_ctx.should_download_fast(
                &task.library,
                &task.asset_id,
                task.version_size,
                &task.checksum,
                false,
            ) {
                Some(false) => {
                    state_skipped = state_skipped.saturating_add(1);
                    false
                }
                None => {
                    let exists = state_confirmed_current_path_exists(
                        &download_ctx,
                        effective_config,
                        asset,
                        &task,
                        &mut task_planner,
                    )
                    .await
                    .is_some();
                    if exists {
                        state_skipped = state_skipped.saturating_add(1);
                    }
                    !exists
                }
                Some(true) => true,
            };
            if keep {
                plan.tasks.push(task);
            }
        }
        skip_breakdown.by_state = skip_breakdown.by_state.saturating_add(state_skipped);
        if plan.tasks.is_empty() && state_skipped == 0 {
            skip_breakdown.on_disk += 1;
        }

        // Upsert state records so mark_downloaded/mark_failed can find them.
        // Without this, the UPDATE in mark_downloaded matches 0 rows and the
        // file ends up on disk but untracked in the state DB.
        if let Some(db) = &config.state_db {
            let planned_tasks = std::mem::take(&mut plan.tasks);
            for task in planned_tasks {
                if let Err(e) =
                    planner::upsert_seen_for_task(db.as_ref(), effective_config, asset, &task).await
                {
                    planning_state_write_failures = planning_state_write_failures.saturating_add(1);
                    tracing::warn!(
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to record asset in state DB"
                    );
                    continue;
                }
                plan.tasks.push(task);
            }
            // Record this asset's membership in the current album so
            // consumers (EXIF keywords, XMP sidecars, Immich albums) can
            // reconstruct the logical album graph from the state DB.
            if let Err(e) =
                planner::record_album_membership_if_named(db.as_ref(), effective_config, asset)
                    .await
                && let Some(album_name) = effective_config.album_name.as_deref()
            {
                planning_state_write_failures = planning_state_write_failures.saturating_add(1);
                tracing::warn!(
                    asset_id = %asset.id(),
                    album = %album_name,
                    library = %effective_config.library,
                    error = %e,
                    "Failed to record album membership after retries"
                );
            }
        }

        if controls.run_mode.downloads_files()
            && let Some(db) = &config.state_db
        {
            task_planner
                .persist_download_reservations(db.as_ref(), &plan.tasks)
                .await?;
        }

        for task in &plan.tasks {
            retry_sources.insert(
                RetryTaskKey::from(task),
                UrlRetrySource {
                    asset_record_name: asset.asset_record_name_arc(),
                    master_record_name: Arc::from(asset.id()),
                    provider_version: filter::provider_version_for_selected(
                        asset,
                        effective_config.as_ref(),
                        task.version_size,
                    ),
                    pass_index: *pass_index,
                },
            );
        }
        tasks.extend(plan.tasks);
    }
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        planned_tasks = tasks.len(),
        "Incremental task planning phase complete"
    );

    if skip_breakdown.by_state > 0 {
        tracing::debug!(
            skipped = skip_breakdown.by_state,
            "Skipped already-downloaded assets (state DB)"
        );
    }

    if tasks.is_empty() {
        let rewrite_failures =
            run_collecting_metadata_rewrite_batch(config, controls.run_mode, &shutdown_token).await;
        let mut stats = SyncStats {
            skipped: skip_breakdown,
            enumeration_errors,
            state_write_failures: delta_summary
                .state_transition_failures
                .saturating_add(planning_state_write_failures),
            exif_failures: rewrite_failures,
            elapsed_secs: started.elapsed().as_secs_f64(),
            interrupted: shutdown_token.is_cancelled(),
            ..SyncStats::default()
        };
        stats.identity_incomplete = delta_summary.identity_incomplete;
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut stats, reason);
        }
        tracing::info!("All incremental assets already downloaded or filtered");
        tracing::info!(elapsed = %format_duration(started.elapsed()), "  completed");
        let failed_count = enumeration_errors
            .saturating_add(delta_summary.state_transition_failures)
            .saturating_add(planning_state_write_failures)
            .saturating_add(rewrite_failures);
        let outcome = if failed_count > 0 {
            DownloadOutcome::PartialFailure { failed_count }
        } else {
            DownloadOutcome::Success
        };
        let sync_token = if controls.run_mode.only_print_filenames() {
            None
        } else {
            (enumeration_errors == 0 && !stats.sync_token_blocked)
                .then_some(delta_summary.sync_token)
                .flatten()
        };
        return Ok(SyncResult::from_incremental_execution(
            outcome,
            sync_token,
            stats,
            delta_summary.sparse_identity_proofs,
        ));
    }

    if controls.run_mode.only_print_filenames() {
        #[allow(
            clippy::print_stdout,
            reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
        )]
        for task in &tasks {
            println!("{}", task.download_path.display());
        }
        let mut stats = SyncStats {
            skipped: skip_breakdown,
            enumeration_errors,
            state_write_failures: delta_summary
                .state_transition_failures
                .saturating_add(planning_state_write_failures),
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..SyncStats::default()
        };
        stats.identity_incomplete = delta_summary.identity_incomplete;
        if let Some(reason) = delta_summary.token_unsafe_reason {
            block_sync_token_for_incremental_delta(&mut stats, reason);
        }
        // Don't advance the sync token — this is a read-only operation.
        return Ok(SyncResult::from_incremental_execution(
            if enumeration_errors > 0 || delta_summary.state_transition_failures > 0 {
                DownloadOutcome::PartialFailure {
                    failed_count: enumeration_errors + delta_summary.state_transition_failures,
                }
            } else {
                DownloadOutcome::Success
            },
            None,
            stats,
            delta_summary.sparse_identity_proofs,
        ));
    }

    let dispatch_shutdown = shutdown_token.child_token();
    let mut provider_auth_errors = 0usize;
    let mut refresh_rate_limits = 0usize;
    let mut url_obtained_at = Default::default();
    let mut unrefreshed = Vec::new();
    if controls.run_mode.downloads_files() {
        let refresh = refresh_stale_incremental_tasks_before_download(
            passes,
            &retry_sources,
            tasks,
            first_download_url_obtained_at,
            preflight_url_refresh_after,
            shutdown_token.clone(),
        )
        .await;
        tasks = refresh.tasks;
        unrefreshed = refresh.unrefreshed;
        provider_auth_errors += refresh.provider_auth_errors;
        refresh_rate_limits += refresh.rate_limit_observations;
        url_obtained_at = refresh.url_obtained_at;
        // Refused authoritative refreshes are retry debt, never a reason to
        // download a stale or changed resource or infer provider deletion.
        if let Some(db) = &config.state_db {
            for task in &unrefreshed {
                if crate::download::finalize::finalize_failed(
                    db.as_ref(),
                    &task.library,
                    task,
                    "Exact download URL refresh unresolved",
                )
                .await
                .is_err()
                {
                    planning_state_write_failures += 1;
                    dispatch_shutdown.cancel();
                }
            }
        }
    }

    let task_count = tasks.len();
    tracing::info!(
        count = task_count,
        first_enumeration_url_observed_age_secs = ?first_download_url_obtained_at.map(|instant| instant.elapsed().as_secs_f64()),
        "Downloading files from incremental sync"
    );

    // Run the download pass on the collected tasks
    let pass_config = PassConfig {
        prior_auth_errors: 0,
        url_obtained_at,
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        mark_capture_repair_after_download: matches!(
            config.capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ),
        concurrency: config.concurrent_downloads,
        reporting: controls.reporting,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token: dispatch_shutdown.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let mut pass_result = run_download_pass(pass_config, tasks).await;
    pass_result.failed.extend(unrefreshed);
    pass_result.rate_limit_observations += refresh_rate_limits;

    if pass_result.url_expired
        && pass_result.auth_errors < AUTH_ERROR_THRESHOLD
        && provider_auth_errors == 0
        && pass_result.state_write_failures == 0
        && planning_state_write_failures == 0
        && !shutdown_token.is_cancelled()
    {
        let expired_retry_candidates = pass_result.expired_tasks.clone();
        let retry_plan = build_incremental_expired_url_retry_tasks(
            passes,
            &retry_sources,
            &expired_retry_candidates,
            shutdown_token.clone(),
        )
        .await;
        provider_auth_errors += retry_plan.provider_auth_errors;
        pass_result.rate_limit_observations += retry_plan.rate_limit_observations;
        let retry_tasks = retry_plan.tasks;
        if !retry_tasks.is_empty() {
            let retry_task_count = retry_tasks.len();
            tracing::info!(
                count = retry_task_count,
                "Retrying tasks after targeted incremental URL refresh"
            );
            let retry_pass_config = PassConfig {
                prior_auth_errors: pass_result.auth_errors,
                url_obtained_at: retry_plan.url_obtained_at,
                client: download_client,
                retry_config: &config.retry,
                metadata: MetadataFlags::from(config.as_ref()),
                mark_capture_repair_after_download: matches!(
                    config.capture_timestamp_repair,
                    CaptureTimestampRepair::ReplaceWithCaptureLocal
                ),
                concurrency: config.concurrent_downloads,
                reporting: controls.reporting,
                temp_suffix: Arc::clone(&config.temp_suffix),
                shutdown_token: shutdown_token.clone(),
                state_db: config.state_db.clone(),
                rate_limit_counter: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                bandwidth_limiter: config.bandwidth_limiter.clone(),
                library: Arc::clone(&config.library),
            };
            let retry_result = run_download_pass(retry_pass_config, retry_tasks).await;
            merge_expired_url_retry_result(
                &mut pass_result,
                expired_retry_candidates,
                retry_result,
            );
        }
    }

    pass_result.exif_failures = pass_result.exif_failures.saturating_add(
        run_collecting_metadata_rewrite_batch(config, controls.run_mode, &shutdown_token).await,
    );

    let failed = pass_result.failed.len();
    let succeeded = pass_result.downloaded;

    // Log failed downloads before the summary
    if failed > 0 {
        for task in &pass_result.failed {
            tracing::error!(asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
    }

    let mut stats = SyncStats {
        assets_seen: 0, // incremental doesn't have total library count
        downloaded: succeeded,
        failed,
        skipped: skip_breakdown,
        bytes_downloaded: pass_result.bytes_downloaded,
        disk_bytes_written: pass_result.disk_bytes_written,
        exif_failures: pass_result.exif_failures,
        state_write_failures: pass_result
            .state_write_failures
            .saturating_add(delta_summary.state_transition_failures)
            .saturating_add(planning_state_write_failures),
        enumeration_errors,
        pagination_shortfall_warnings: 0,
        pagination_shortfall_assets: 0,
        sync_token_blocked: false,
        sync_token_blocked_reason: None,
        elapsed_secs: started.elapsed().as_secs_f64(),
        interrupted: dispatch_shutdown.is_cancelled()
            || pass_result.auth_errors >= AUTH_ERROR_THRESHOLD
            || provider_auth_errors > 0,
        rate_limited: pass_result.rate_limit_observations,
        photos_downloaded: pass_result.photos_downloaded,
        videos_downloaded: pass_result.videos_downloaded,
        recap: pass_result.recap.clone(),
        ..SyncStats::default()
    };
    stats.identity_incomplete = delta_summary.identity_incomplete;
    if let Some(reason) = delta_summary.token_unsafe_reason {
        block_sync_token_for_incremental_delta(&mut stats, reason);
    }
    log_sync_summary(
        "\u{2500}\u{2500} Incremental Sync Summary \u{2500}\u{2500}",
        &stats,
    );

    if pass_result.auth_errors >= AUTH_ERROR_THRESHOLD || provider_auth_errors > 0 {
        return Ok(SyncResult::from_incremental_execution(
            DownloadOutcome::SessionExpired {
                auth_error_count: pass_result.auth_errors + provider_auth_errors,
            },
            (!stats.sync_token_blocked)
                .then_some(delta_summary.sync_token)
                .flatten(),
            stats,
            delta_summary.sparse_identity_proofs,
        ));
    }

    let outcome = if failed > 0
        || pass_result.exif_failures > 0
        || pass_result.state_write_failures > 0
        || delta_summary.state_transition_failures > 0
        || planning_state_write_failures > 0
        || enumeration_errors > 0
    {
        DownloadOutcome::PartialFailure {
            failed_count: failed
                + pass_result.exif_failures
                + pass_result.state_write_failures
                + delta_summary.state_transition_failures
                + planning_state_write_failures
                + enumeration_errors,
        }
    } else {
        DownloadOutcome::Success
    };

    Ok(SyncResult::from_incremental_execution(
        outcome,
        (enumeration_errors == 0 && !stats.sync_token_blocked)
            .then_some(delta_summary.sync_token)
            .flatten(),
        stats,
        delta_summary.sparse_identity_proofs,
    ))
}

#[cfg(test)]
mod tests;
