//! Streaming entry points, run modes, and producer/consumer composition.

use std::sync::Arc;

use anyhow::Result;
use futures_util::StreamExt;
use indicatif::ProgressBar;
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::download::filter::{
    DownloadTask, FilterReason, extract_skip_candidates, is_asset_filtered,
};
use crate::download::metadata_rewrite::MetadataFlags;
use crate::download::planner::TaskPlanner;
use crate::download::{
    ClaimedLegacyMasterStates, DownloadConfig, DownloadContext, DownloadControls,
    preload_download_context,
};
use crate::icloud::photos::session::is_session_error as is_provider_session_error;

use super::StreamPipelineShared;
use super::adoption::effective_asset_library;
use super::consumer::{StreamConsumerSettings, consume_stream_download_tasks};
use super::outcome::{StreamingResult, finalize_streaming_download};
use super::producer::spawn_stream_download_producer;

/// Return the subset of `paths` that do not exist on disk.
/// Streaming download pipeline that consumes a pre-built combined stream.
///
/// This is the core producer/consumer download logic from `stream_and_download`,
/// factored out so that `download_photos_full_with_token` can supply a
/// token-aware combined stream while reusing the same download machinery.
pub(in crate::download) async fn stream_and_download_from_stream<S>(
    download_client: &Client,
    combined: S,
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    total: u64,
    shutdown_token: CancellationToken,
    runtime: StreamRuntime,
) -> Result<StreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<crate::icloud::photos::PhotoAsset>>
        + Send
        + 'static,
{
    stream_and_download_from_stream_with_context(
        download_client,
        combined,
        config,
        controls,
        total,
        shutdown_token,
        runtime,
    )
    .await
}

pub(in crate::download) struct StreamRuntime {
    pub(super) shared_pb: Option<ProgressBar>,
    pub(super) shared_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    pub(super) preloaded_download_ctx: Option<Arc<DownloadContext>>,
    /// Set when the caller runs several of these pipelines concurrently and
    /// drains the shared rewrite queue itself once they have all finished.
    /// Concurrent drains can otherwise write the same file, and the later
    /// write wins regardless of which holds the newer metadata.
    pub(super) defer_metadata_drain: bool,
}

impl StreamRuntime {
    pub(in crate::download) fn new(
        shared_pb: Option<ProgressBar>,
        shared_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    ) -> Self {
        Self {
            shared_pb,
            shared_bytes,
            preloaded_download_ctx: None,
            defer_metadata_drain: false,
        }
    }

    pub(in crate::download) fn with_context(
        shared_pb: Option<ProgressBar>,
        shared_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
        preloaded_download_ctx: Option<Arc<DownloadContext>>,
    ) -> Self {
        Self {
            shared_pb,
            shared_bytes,
            preloaded_download_ctx,
            defer_metadata_drain: false,
        }
    }

    /// Leaves the rewrite queue for the caller to drain once every concurrent
    /// pipeline has finished.
    pub(in crate::download) fn deferring_metadata_drain(mut self) -> Self {
        self.defer_metadata_drain = true;
        self
    }
}

pub(in crate::download) async fn stream_and_download_from_stream_with_context<S>(
    download_client: &Client,
    combined: S,
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    total: u64,
    shutdown_token: CancellationToken,
    runtime: StreamRuntime,
) -> Result<StreamingResult>
where
    S: futures_util::Stream<Item = anyhow::Result<crate::icloud::photos::PhotoAsset>>
        + Send
        + 'static,
{
    let reporting = controls.reporting;

    // When the caller passes a `shared_pb`, they own its lifecycle (we only
    // advance position and update the message). Otherwise we create our own
    // bar and finish_and_clear it before returning. The shared-bar path is
    // used by the per-album-pass loop in `download::mod.rs` to avoid the
    // visible "reset" when one pass finishes and the next starts: a small
    // album finishing fast then a large unfiled pass starting fresh reads as
    // a glitch.
    //
    // The byte counter follows the same pairing rule: caller-supplied with
    // `shared_pb`, or freshly created for an internal bar. The friendly
    // sparkline / rate display reads from this atomic on each redraw.
    let owns_pb = runtime.shared_pb.is_none();
    let bytes_counter = runtime
        .shared_bytes
        .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));
    let pb = runtime.shared_pb.unwrap_or_else(|| {
        crate::personality::progress::single(
            reporting.no_progress_bar,
            controls.run_mode.only_print_filenames(),
            total,
            reporting.personality_mode,
            Some(std::sync::Arc::clone(&bytes_counter)),
        )
    });

    // Seed the wide_msg line with the pass label so the user can see which
    // album/pass is active even before any task completes. Otherwise an
    // album that's entirely already-on-disk would advance the bar via the
    // producer's skip path (which doesn't set_message) and leave the
    // wide_msg blank for the whole pass.
    //
    pb.set_message(format!("{} \u{00b7} scanning...", config.pass_label()));

    // Select the durable child identity before any run mode performs state
    // checks or path planning. Print-only and dry-run return before the real
    // download producer starts, so normalizing only in that producer makes
    // their paths disagree with a normal sync.
    let download_ctx = match runtime.preloaded_download_ctx {
        Some(ctx) => ctx,
        None => preload_download_context(config).await,
    };
    let default_library = Arc::clone(&config.library);
    let identity_download_ctx = Arc::clone(&download_ctx);
    let identity_config = Arc::clone(config);
    let mut claimed_legacy_master_states = ClaimedLegacyMasterStates::default();
    let combined = combined.map(move |result| {
        result.map(|asset| {
            let library = asset.source_zone().unwrap_or(default_library.as_ref());
            let state_record_name = if !matches!(
                is_asset_filtered(&asset, identity_config.as_ref()),
                Some(FilterReason::ExcludedAlbum)
            ) {
                identity_download_ctx.select_asset_state_record_name(
                    library,
                    &asset,
                    &mut claimed_legacy_master_states,
                )
            } else {
                identity_download_ctx.select_existing_asset_state_record_name(library, &asset)
            };
            asset.with_state_record_name(state_record_name)
        })
    });

    if controls.run_mode.only_print_filenames() {
        tokio::pin!(combined);
        let mut enum_errors = 0usize;
        let mut provider_auth_errors = 0usize;
        let mut task_planner = TaskPlanner::for_download(config.state_db.as_deref()).await?;
        let mut shutdown_break = false;
        #[cfg(test)]
        let mut printed_filenames = Vec::new();
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                shutdown_break = true;
                break;
            }
            match result {
                Ok(asset) => {
                    if is_asset_filtered(&asset, config.as_ref()).is_some() {
                        continue;
                    }
                    // Fast-skip is path-blind; in `{album}` mode the same
                    // asset legitimately lives at multiple paths, so we'd
                    // under-report the listing if we trusted the DB here.
                    // `album_name.is_some()` is the right signal because by
                    // the time this runs, `with_album_name` has expanded
                    // `{album}` out of `folder_structure` entirely.
                    if config.album_name.is_none() {
                        let candidates = extract_skip_candidates(&asset, config.as_ref());
                        let library = effective_asset_library(&asset, config);
                        if !candidates.is_empty()
                            && candidates.iter().all(|&(vs, cs)| {
                                matches!(
                                    download_ctx.should_download_fast(
                                        library,
                                        asset.state_id(),
                                        vs,
                                        cs,
                                        true
                                    ),
                                    Some(false)
                                )
                            })
                        {
                            continue;
                        }
                    }

                    let plan = task_planner.plan_download_asset(&asset, config).await?;
                    if let Some(resource) = &plan.malformed_resource {
                        enum_errors += 1;
                        tracing::error!(target: "kei::download::pipeline",
                            asset_id = %asset.id(),
                            field = %resource.field,
                            reason = %resource.reason,
                            "Malformed CloudKit resource prevented filename planning"
                        );
                        continue;
                    }
                    #[allow(
                        clippy::print_stdout,
                        reason = "--only-print-filenames writes target paths to stdout so callers can pipe to xargs/etc"
                    )]
                    for task in &plan.tasks {
                        #[cfg(test)]
                        printed_filenames.push(task.download_path.clone());
                        println!("{}", task.download_path.display());
                    }
                }
                Err(e) => {
                    enum_errors += 1;
                    if is_provider_session_error(&e) {
                        provider_auth_errors += 1;
                        break;
                    }
                    tracing::error!(target: "kei::download::pipeline", error = %e, "Error fetching asset");
                }
            }
        }
        return Ok(StreamingResult {
            enumeration_errors: enum_errors,
            provider_auth_errors,
            // Same gate as dry-run — `--only-print-filenames` drains
            // the API stream and can clear the marker on a clean exit.
            enumeration_complete: !shutdown_break && provider_auth_errors == 0,
            #[cfg(test)]
            printed_filenames,
            ..StreamingResult::default()
        });
    }

    if controls.run_mode.is_dry_run() {
        tokio::pin!(combined);
        let mut count = 0usize;
        let mut enum_errors = 0usize;
        let mut provider_auth_errors = 0usize;
        let mut task_planner = TaskPlanner::for_download(config.state_db.as_deref()).await?;
        let mut shutdown_break = false;
        while let Some(result) = combined.next().await {
            if shutdown_token.is_cancelled() {
                tracing::info!(target: "kei::download::pipeline", "Shutdown requested, stopping dry run");
                shutdown_break = true;
                break;
            }
            match result {
                Ok(asset) => {
                    let plan = task_planner.plan_download_asset(&asset, config).await?;
                    if plan.filter_reason.is_some() {
                        continue;
                    }
                    if let Some(resource) = &plan.malformed_resource {
                        enum_errors += 1;
                        tracing::error!(target: "kei::download::pipeline",
                            asset_id = %asset.id(),
                            field = %resource.field,
                            reason = %resource.reason,
                            "Malformed CloudKit resource prevented dry-run planning"
                        );
                        continue;
                    }
                    for task in &plan.tasks {
                        tracing::info!(target: "kei::download::pipeline", path = %task.download_path.display(), "[DRY RUN] Would download");
                    }
                    count += plan.tasks.len();
                }
                Err(e) => {
                    enum_errors += 1;
                    if is_provider_session_error(&e) {
                        provider_auth_errors += 1;
                        break;
                    }
                    tracing::error!(target: "kei::download::pipeline", error = %e, "Error fetching asset");
                }
            }
        }
        return Ok(StreamingResult {
            downloaded: count,
            enumeration_errors: enum_errors,
            provider_auth_errors,
            // Dry-run still drains the API stream; mirror the
            // non-dry-run gate so the enum_in_progress marker can be
            // cleared on a clean dry-run.
            enumeration_complete: !shutdown_break && provider_auth_errors == 0,
            ..StreamingResult::default()
        });
    }

    let download_client = download_client.clone();
    let retry_config = config.retry;
    let metadata_flags = MetadataFlags::from(config.as_ref());
    let concurrency = config.concurrent_downloads;
    let state_db = config.state_db.clone();
    let mode = reporting.personality_mode;

    let defer_metadata_drain = runtime.defer_metadata_drain;

    // Start sync run tracking
    let sync_run_id = if let Some(db) = &state_db {
        match db.start_sync_run().await {
            Ok(id) => {
                tracing::debug!(target: "kei::download::pipeline", run_id = id, "Started sync run");
                Some(id)
            }
            Err(e) => {
                tracing::warn!(target: "kei::download::pipeline", error = %e, "Failed to start sync run tracking");
                None
            }
        }
    } else {
        None
    };

    // Log a one-time backfill notice when pre-v5 assets still have NULL
    // metadata_hash. `download_ctx` already loaded downloaded ids and
    // non-null metadata hashes, so avoid a redundant SQLite EXISTS scan per
    // album pass.
    if download_ctx.has_downloaded_without_metadata_hash() {
        tracing::info!(target: "kei::download::pipeline", "Backfilling metadata for existing assets (one-time after upgrade)");
    }

    let (task_tx, task_rx) = mpsc::channel::<DownloadTask>(concurrency * 2);

    // Batch-size forecast: snapshot free space at enumeration start and
    // track bytes queued to consumers. Emit a one-time warn at 90% and
    // cancel the sync at 100%. This catches the "batch much larger than
    // free space" case early, before downloads run the disk dry mid-stream.
    //
    // A multi-hour sync can have its FS filled by an unrelated process
    // mid-run. Refresh `initial_free` every FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES
    // queued so the bail decision rides on fresher data. Per-task rechecks
    // against a shrinking denominator would fire noisy false-positives as
    // downloads naturally consume the disk; the 10 GiB cadence is a
    // compromise between "stale" and "noisy".
    let initial_free_at_start = crate::available_disk_space(&config.directory);

    // Refuse to start if free disk space is critically low (< 100 MiB).
    // The per-asset batch_forecast_decision catches the rest mid-stream.
    const MIN_FREE_BYTES_HARD: u64 = 100 * 1024 * 1024; // 100 MiB
    if let Some(free) = initial_free_at_start
        && free < MIN_FREE_BYTES_HARD
    {
        return Err(anyhow::anyhow!(
            "Not enough free disk space: only {} bytes are available on {}, but kei needs at least {MIN_FREE_BYTES_HARD} bytes. Free up space or choose a different [download].directory.",
            free,
            config.directory.display(),
        ));
    }

    let pipeline_shutdown = shutdown_token.child_token();
    let shared = StreamPipelineShared {
        config: Arc::clone(config),
        state_db: state_db.clone(),
        pb: pb.clone(),
        pipeline_shutdown: pipeline_shutdown.clone(),
    };
    let producer = spawn_stream_download_producer(
        combined,
        Arc::clone(&download_ctx),
        task_tx,
        initial_free_at_start,
        shared.clone(),
    );

    let consumer_result = consume_stream_download_tasks(
        task_rx,
        download_client,
        shared.clone(),
        StreamConsumerSettings {
            retry_config,
            metadata_flags,
            concurrency,
            mode,
            bytes_counter: Arc::clone(&bytes_counter),
        },
    )
    .await;

    finalize_streaming_download(
        producer,
        consumer_result,
        sync_run_id,
        owns_pb,
        shared,
        defer_metadata_drain,
    )
    .await
}

#[cfg(test)]
mod tests;
