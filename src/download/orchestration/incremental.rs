//! Apply a zone delta, then let current selection publish eligible destinations.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use futures_util::StreamExt;
use reqwest::Client;
use rustc_hash::FxHashSet;
use tokio_util::sync::CancellationToken;

use crate::download::filter;
use crate::download::pipeline::{MetadataFlags, format_duration};
use crate::icloud::photos::PhotoAsset;
use crate::icloud::photos::asset::ChangeEvent;
use crate::icloud::photos::error::AcceptedReplay;
use crate::types::ChangeReason;

use super::config::DownloadConfig;
use super::context::{
    ClaimedLegacyMasterStates, DownloadContext, LegacyOwnerClaimMode,
    legacy_owner_claim_mode_for_configs, preload_download_context,
};
use super::delta::{
    IncrementalAssetHydrationContext, IncrementalDeltaState, IncrementalDeltaSummary,
    hydrate_missing_selected_relation_assets, hydrate_unpaired_created_asset_deltas,
};
use super::models::{
    DownloadControls, DownloadOutcome, PROVIDER_METADATA_STATE_WRITE_FAILED_REASON, SyncResult,
    merge_download_outcomes, replay_refusal_reason,
};
use super::selection::{IncrementalPassRouting, build_pass_configs};
async fn apply_changed_provider_metadata(
    config: &DownloadConfig,
    asset: &PhotoAsset,
    download_ctx: &DownloadContext,
    summary: &mut IncrementalDeltaSummary,
) {
    let Some(db) = &config.state_db else {
        return;
    };
    match super::primary_layout::owns_metadata(asset, config).await {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            summary.state_transition_failures += 1;
            summary
                .token_unsafe_reason
                .get_or_insert(PROVIDER_METADATA_STATE_WRITE_FAILED_REASON);
            tracing::warn!(%error, "Could not resolve managed metadata ownership for delta");
            return;
        }
    }
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

pub(super) async fn download_photos_incremental(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let started = Instant::now();
    // The baseline must match before the new pages change its source basis.
    let prior = if let Some(context) = &config.selection_context {
        context.library_delta_base(config).await?
    } else {
        None
    };
    let mut delta_summary = match read_zone_delta(
        download_client,
        passes,
        config,
        zone_sync_token,
        controls,
        &shutdown_token,
    )
    .await?
    {
        std::ops::ControlFlow::Break(refused) => return Ok(*refused),
        std::ops::ControlFlow::Continue(delta) => delta,
    };
    delta_summary.log_debug();
    if delta_summary.auth_errors > 0 {
        delta_summary.sparse_target_stats.elapsed_secs += started.elapsed().as_secs_f64();
    }

    Box::pin(super::generation::after_delta(
        download_client,
        passes,
        config,
        controls,
        shutdown_token,
        delta_summary,
        prior,
    ))
    .await
}

/// Read and apply provider facts before current selection decides what may publish.
/// Refusal can publish an already accepted prefix but cannot certify the delta.
pub(super) async fn read_zone_delta(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    zone_sync_token: &str,
    controls: DownloadControls,
    shutdown_token: &CancellationToken,
) -> Result<std::ops::ControlFlow<Box<SyncResult>, IncrementalDeltaSummary>> {
    let started = Instant::now();
    let legacy_owner_claim_mode = if controls.run_mode.downloads_files() {
        LegacyOwnerClaimMode::Persist
    } else {
        LegacyOwnerClaimMode::ReadOnly
    };
    let pass_configs = build_pass_configs(passes, config);

    let mut change_events: Vec<ChangeEvent> = Vec::new();
    let mut delta = IncrementalDeltaState::new(passes);
    let routing = IncrementalPassRouting::from_passes(passes);

    let delta_pass = passes.first();

    // `changes_stream` is zone-scoped, not album-scoped. Query it once and
    // fan created assets out through the selected passes locally; querying
    // once per pass repeats the same `/changes/zone` pages on every watch
    // cycle with work.
    if let Some(pass) = delta_pass {
        let phase_started = Instant::now();
        let (mut change_stream, token_rx, completion_rx) =
            pass.album.changes_stream_with_completion(zone_sync_token);

        loop {
            let result = tokio::select! {
                biased;
                () = shutdown_token.cancelled() => break,
                result = change_stream.next() => result,
            };
            let Some(result) = result else { break };
            let event = match result {
                Ok(event) => event,
                Err(error) => {
                    let (Some(reason), Some(prefix)) = (
                        replay_refusal_reason(&error),
                        AcceptedReplay::from_shadow_error(&error),
                    ) else {
                        return Err(error);
                    };
                    if !super::generation::supports_accepted_replay(config, controls) {
                        return Err(error);
                    }
                    drop(change_stream);
                    let Some(mut partial) = Box::pin(super::generation::publish_accepted_replay(
                        download_client,
                        passes,
                        config,
                        controls,
                        shutdown_token.clone(),
                        &prefix.pages,
                        &change_events,
                    ))
                    .await?
                    else {
                        return Err(error);
                    };
                    // Publication is useful progress, never a complete delta.
                    partial.outcome = merge_download_outcomes(
                        &partial.outcome,
                        &DownloadOutcome::PartialFailure { failed_count: 1 },
                    );
                    partial.block_incremental_token(reason);
                    return Ok(std::ops::ControlFlow::Break(Box::new(partial)));
                }
            };
            delta.observe_event(&event);
            change_events.push(event);
        }

        // Drop the owned receiver, not just a pinned borrow of it.
        drop(change_stream);
        let token = tokio::select! {
            biased;
            () = shutdown_token.cancelled() => None,
            token = token_rx => token.ok(),
        };
        if let Ok(completion) = completion_rx.await {
            delta.summary.raw_records_observed = Some(completion.records != 0);
            // Pairing can omit unknown records or leave orphans. Only direct
            // pairs which account for the entire raw stream replace a listing.
            let mut masters = FxHashSet::default();
            if completion.records == change_events.len().saturating_mul(2)
                && change_events.iter().all(|event| {
                    IncrementalDeltaState::created_asset(event).is_some_and(|asset| {
                        !asset.metadata().is_hidden
                            && !asset.metadata().is_deleted
                            && masters.insert(asset.id())
                    })
                })
            {
                delta.summary.paired_pages = completion.pages;
            }
        }
        delta.record_completion(token.filter(|_| delta.summary.raw_records_observed.is_some()));
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
        shutdown_token,
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
    let before_hydration = complete_delta_assets.len();
    {
        let mut hydration_context = IncrementalAssetHydrationContext {
            asset_to_master: &mut delta.asset_to_master,
            complete_delta_assets: &mut complete_delta_assets,
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
            shutdown_token,
        )
        .await;
    }
    let hydrated_assets = complete_delta_assets.len() - before_hydration;
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
    }

    let phase_started = Instant::now();
    for event in &change_events {
        delta.apply_relation_event(event, config, &routing).await;
    }

    for event in &change_events {
        delta.apply_event(event, config).await;
    }
    tracing::debug!(
        phase_elapsed = %format_duration(phase_started.elapsed()),
        elapsed = %format_duration(started.elapsed()),
        "Incremental routing phase complete"
    );

    if !delta.summary.paired_pages.is_empty() {
        delta.summary.paired_assets = complete_delta_assets;
    }
    let delta_summary = delta.summary;
    Ok(std::ops::ControlFlow::Continue(delta_summary))
}

#[cfg(test)]
mod tests;
