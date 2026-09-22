//! Pass finalization, retry cleanup, and sync outcome aggregation.

#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::download::filter::DownloadTask;
use crate::download::finalize::{
    StateWriteFlush, flush_pending_state_writes_retaining_failures, state_db_unwritable_error,
    state_write_circuit_breaker_tripped,
};
use crate::download::metadata_rewrite::MetadataFlags;
use crate::download::{
    CheckpointEvidence, DownloadConfig, DownloadControls, DownloadOutcome, SyncResult,
    metadata_rewrite,
};
use crate::state::SyncRunStats;

use super::StreamPipelineShared;
use super::consumer::StreamConsumerResult;
use super::pass::{PassConfig, run_download_pass};
use super::producer::{ProducerSkipSummary, StreamProducer};
use super::progress::{log_sync_summary, maybe_warn_rate_limit_pressure};
use super::task::AUTH_ERROR_THRESHOLD;

/// Result of the streaming download phase.
#[derive(Debug, Default)]
pub(in crate::download) struct StreamingResult {
    pub(in crate::download) downloaded: usize,
    pub(in crate::download) exif_failures: usize,
    pub(in crate::download) failed: Vec<DownloadTask>,
    pub(in crate::download) auth_errors: usize,
    /// CloudKit session failures require recovery even without failed CDN transfers.
    pub(in crate::download) provider_auth_errors: usize,
    pub(in crate::download) state_write_failures: usize,
    pub(in crate::download) enumeration_errors: usize,
    pub(in crate::download) assets_seen: u64,
    pub(in crate::download) skip_summary: ProducerSkipSummary,
    pub(in crate::download) bytes_downloaded: u64,
    pub(in crate::download) disk_bytes_written: u64,
    /// Count of 429/503 observations during Phase 1 downloads (per retry
    /// attempt, not per unique task). Feeds SyncStats.rate_limited.
    pub(in crate::download) rate_limit_observations: usize,
    /// True when any worker observed HTTP 410 for a signed CDN URL. Once this
    /// happens, the rest of the current URL batch is presumed stale too, so
    /// the pass aborts instead of hammering thousands of expired URLs.
    pub(in crate::download) url_expired_abort: bool,
    /// `true` when the producer reached the natural end of the API
    /// stream (so the `enum_in_progress:<zone>` marker can be cleared even
    /// when downstream downloads partially failed). `false` when the
    /// producer aborted via shutdown, channel-close, or panic.
    pub(in crate::download) enumeration_complete: bool,
    /// Photos downloaded in this pass (`MediaType::Photo` /
    /// `LivePhotoImage`). Lifted into `SyncStats.photos_downloaded`.
    pub(in crate::download) photos_downloaded: usize,
    /// Videos downloaded in this pass (`MediaType::Video` /
    /// `LivePhotoVideo`).
    pub(in crate::download) videos_downloaded: usize,
    /// Per-pass recap fold; merged with the cleanup pass's recap before
    /// the friendly card renders.
    pub(in crate::download) recap: crate::download::recap::RunRecap,
    #[cfg(test)]
    pub(in crate::download) printed_filenames: Vec<PathBuf>,
}

pub(super) async fn finalize_streaming_download(
    producer: StreamProducer,
    mut consumer: StreamConsumerResult,
    sync_run_id: Option<i64>,
    owns_pb: bool,
    shared: StreamPipelineShared,
    defer_metadata_drain: bool,
) -> Result<StreamingResult> {
    let StreamProducer { handle, metrics } = producer;
    let config = shared.config;
    let state_db = shared.state_db;
    let pb = shared.pb;
    let pipeline_shutdown = shared.pipeline_shutdown;

    let (producer_panicked, producer_skips) = match handle.await {
        Ok(skips) => (false, skips),
        Err(e) if e.is_panic() => {
            tracing::error!(target: "kei::download::pipeline", error = ?e, "Asset producer task panicked");
            (true, ProducerSkipSummary::default())
        }
        Err(e) => {
            tracing::warn!(target: "kei::download::pipeline", error = ?e, "Asset producer task failed (skip counts lost)");
            (false, ProducerSkipSummary::default())
        }
    };

    let assets_seen_count = metrics
        .assets_seen
        .load(std::sync::atomic::Ordering::Relaxed);

    // Only finish the bar when we created it ourselves; if the caller passed
    // a shared bar (per-pass loop), they'll finish it after the last pass.
    if owns_pb {
        pb.finish_and_clear();
    }

    let mut complete_sync_failed = false;
    if let (Some(db), Some(run_id)) = (&state_db, sync_run_id) {
        let stats = SyncRunStats {
            assets_seen: assets_seen_count,
            assets_downloaded: consumer.downloaded as u64,
            assets_failed: consumer.failed.len() as u64,
            enumeration_errors: u64::try_from(
                metrics
                    .enum_errors
                    .load(std::sync::atomic::Ordering::Relaxed),
            )
            .unwrap_or(u64::MAX),
            interrupted: pipeline_shutdown.is_cancelled()
                || consumer.auth_errors >= AUTH_ERROR_THRESHOLD
                || metrics
                    .provider_auth_errors
                    .load(std::sync::atomic::Ordering::Relaxed)
                    > 0
                || producer_panicked
                || consumer.url_expired_abort,
            ..Default::default()
        };
        match db.complete_sync_run(run_id, &stats).await {
            Err(e) => {
                tracing::warn!(target: "kei::download::pipeline", error = %e, "Failed to complete sync run tracking");
                complete_sync_failed = true;
            }
            Ok(()) => {
                tracing::debug!(target: "kei::download::pipeline",
                    run_id,
                    assets_seen = assets_seen_count,
                    downloaded = consumer.downloaded,
                    failed = consumer.failed.len(),
                    "Completed sync run"
                );
            }
        }
    }

    // Retry any state writes that failed during the streaming loop. This
    // must run before the producer-panic bail so rows that successfully
    // landed on disk before the panic are recorded in state; otherwise the
    // next sync re-downloads them and the pending-retry safety net becomes
    // a no-op on panic paths.
    let final_state_flush = if let Some(db) = &state_db {
        if consumer.state_write_circuit_error.is_some() {
            StateWriteFlush {
                attempted: consumer.pending_state_writes.len(),
                failures: consumer.pending_state_writes.len(),
            }
        } else {
            flush_pending_state_writes_retaining_failures(
                db.as_ref(),
                &mut consumer.pending_state_writes,
            )
            .await
        }
    } else {
        StateWriteFlush::default()
    };
    let producer_state_write_failures = metrics
        .state_write_failures
        .load(std::sync::atomic::Ordering::Relaxed);
    let state_write_failures = final_state_flush.failures
        + producer_state_write_failures
        + usize::from(complete_sync_failed);
    if consumer.state_write_circuit_error.is_none()
        && state_write_circuit_breaker_tripped(&final_state_flush)
    {
        consumer.state_write_circuit_error =
            Some(state_db_unwritable_error(final_state_flush.attempted));
    }

    // Drain metadata-rewrite markers set earlier in this cycle (or left over
    // from a previous one). This re-applies EXIF/XMP on the existing files
    // without re-downloading bytes; the alternative was to leave markers
    // accumulating in the DB forever.
    if consumer.state_write_circuit_error.is_none()
        && !defer_metadata_drain
        && !config.refresh_metadata
        && let Some(db) = &state_db
    {
        let metadata_flags = MetadataFlags::from(config.as_ref());
        if metadata_flags.has_any_write() {
            consumer.exif_failures += metadata_rewrite::run_pending(
                db.as_ref(),
                metadata_flags,
                Arc::clone(&config.temp_suffix),
                &pipeline_shutdown,
            )
            .await
            .failed;
        }
    }

    if let Some(err) = consumer.state_write_circuit_error {
        return Err(err);
    }

    if producer_panicked {
        return Err(anyhow::anyhow!(
            "The asset producer task crashed, so sync may be incomplete ({} pending state writes were flushed).",
            state_write_failures,
        ));
    }

    // A panicked producer never reached the post-loop "enumeration
    // complete" assignment, so the flag stays `false` even if the bail
    // path above was suppressed. `producer_panicked` is checked above,
    // but if a future change ever returns Ok despite a panic, the flag
    // here protects the `enum_in_progress` marker.
    let enumeration_complete_flag = !producer_panicked
        && metrics
            .enumeration_complete
            .load(std::sync::atomic::Ordering::Relaxed);

    Ok(StreamingResult {
        downloaded: consumer.downloaded,
        exif_failures: consumer.exif_failures,
        failed: consumer.failed,
        auth_errors: consumer.auth_errors,
        provider_auth_errors: metrics
            .provider_auth_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        state_write_failures,
        enumeration_errors: metrics
            .enum_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        assets_seen: assets_seen_count,
        skip_summary: producer_skips,
        bytes_downloaded: consumer.bytes_downloaded_total,
        disk_bytes_written: consumer.disk_bytes_total,
        rate_limit_observations: consumer.rate_limit_observations,
        enumeration_complete: enumeration_complete_flag,
        photos_downloaded: consumer.photos_downloaded,
        videos_downloaded: consumer.videos_downloaded,
        recap: consumer.recap,
        url_expired_abort: consumer.url_expired_abort,
        #[cfg(test)]
        printed_filenames: Vec::new(),
    })
}

fn producer_enumeration_incomplete(
    result: &StreamingResult,
    shutdown_token: &CancellationToken,
) -> bool {
    !result.enumeration_complete
        && result.assets_seen > 0
        && !shutdown_token.is_cancelled()
        && !result.url_expired_abort
}

fn mark_producer_enumeration_incomplete(
    stats: &mut crate::download::SyncStats,
    checkpoint: &mut CheckpointEvidence,
    incomplete: bool,
) {
    checkpoint.enumeration_incomplete = incomplete;
    checkpoint.sync_token_blocked |= incomplete;
    checkpoint.project(stats);
    if !incomplete {
        return;
    }
    if stats.sync_token_blocked_reason.is_none() {
        stats.sync_token_blocked_reason =
            Some(crate::download::PRODUCER_ENUMERATION_INCOMPLETE_REASON);
        stats.sync_token_blocked_source = Some(crate::download::sync_token_blocked_source(
            crate::download::PRODUCER_ENUMERATION_INCOMPLETE_REASON,
        ));
        stats.sync_token_blocked_explanation =
            Some(crate::download::sync_token_blocked_explanation(
                crate::download::PRODUCER_ENUMERATION_INCOMPLETE_REASON,
            ));
    }
    tracing::warn!(target: "kei::download::pipeline",
        reason = crate::download::PRODUCER_ENUMERATION_INCOMPLETE_REASON,
        "Asset producer stopped before iCloud enumeration completed; treating sync as partial failure"
    );
}

/// Build a `DownloadOutcome` from a `StreamingResult`, running a cleanup
/// pass if there were failures. Shared between `download_photos` and
/// `download_photos_full_with_token`.
pub(in crate::download) async fn build_download_outcome(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    streaming_result: StreamingResult,
    started: Instant,
    shutdown_token: CancellationToken,
) -> Result<(DownloadOutcome, crate::download::SyncStats)> {
    let result = build_download_result(
        download_client,
        passes,
        config,
        controls,
        streaming_result,
        started,
        shutdown_token,
    )
    .await?;
    Ok((result.outcome, result.stats))
}

/// Finalize execution evidence before deriving its reporting snapshot.
pub(in crate::download) async fn build_download_result(
    download_client: &Client,
    passes: &[crate::commands::AlbumPass],
    config: &Arc<DownloadConfig>,
    controls: DownloadControls,
    streaming_result: StreamingResult,
    started: Instant,
    shutdown_token: CancellationToken,
) -> Result<SyncResult> {
    let run_mode = controls.run_mode;
    let downloaded = streaming_result.downloaded;
    let mut exif_failures = streaming_result.exif_failures;
    let auth_errors = streaming_result.auth_errors;
    let provider_auth_errors = streaming_result.provider_auth_errors;
    let mut state_write_failures = streaming_result.state_write_failures;
    let enumeration_errors = streaming_result.enumeration_errors;
    let enumeration_incomplete =
        producer_enumeration_incomplete(&streaming_result, &shutdown_token);
    let mut checkpoint = CheckpointEvidence {
        state_write_failures,
        enumeration_errors,
        interrupted: shutdown_token.is_cancelled(),
        ..CheckpointEvidence::default()
    };
    let failed_tasks = streaming_result.failed;
    let skip_breakdown: crate::download::SkipBreakdown = streaming_result.skip_summary.into();

    if auth_errors >= AUTH_ERROR_THRESHOLD || provider_auth_errors > 0 {
        checkpoint.interrupted = true;
        checkpoint.enumeration_incomplete = enumeration_incomplete;
        let stats = crate::download::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: failed_tasks.len(),
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written,
            exif_failures,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            elapsed_secs: started.elapsed().as_secs_f64(),
            rate_limited: streaming_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded,
            recap: streaming_result.recap.clone(),
            ..crate::download::SyncStats::default()
        };
        return Ok(finish_download_result(
            DownloadOutcome::SessionExpired {
                auth_error_count: auth_errors + provider_auth_errors,
            },
            stats,
            checkpoint,
        ));
    }

    if downloaded == 0 && failed_tasks.is_empty() {
        let retry_exhausted = skip_breakdown.retry_exhausted;
        let mut stats = crate::download::SyncStats {
            assets_seen: streaming_result.assets_seen,
            skipped: skip_breakdown,
            exif_failures,
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..crate::download::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, &mut checkpoint, enumeration_incomplete);
        if run_mode.is_dry_run() {
            tracing::info!(target: "kei::download::pipeline", "── Dry Run Summary ──");
            tracing::info!(target: "kei::download::pipeline", "  0 files would be downloaded");
            tracing::info!(target: "kei::download::pipeline", destination = %config.directory.display(), "  destination");
        } else if streaming_result.url_expired_abort {
            tracing::warn!(target: "kei::download::pipeline", "Download batch aborted because signed iCloud URLs expired");
        } else {
            tracing::info!(target: "kei::download::pipeline", "No new photos to download");
        }
        // A metadata-only edit cycle downloads nothing, so rewrite failures
        // must still surface here. They stay out of `state_write_failures`:
        // the catalogue and marker are durable, so the checkpoint may advance.
        let failed_count = state_write_failures
            + exif_failures
            + retry_exhausted
            + enumeration_errors
            + usize::from(streaming_result.url_expired_abort)
            + usize::from(enumeration_incomplete);
        if failed_count > 0 {
            return Ok(finish_download_result(
                DownloadOutcome::PartialFailure { failed_count },
                stats,
                checkpoint,
            ));
        }
        return Ok(finish_download_result(
            DownloadOutcome::Success,
            stats,
            checkpoint,
        ));
    }

    if run_mode.is_dry_run() {
        checkpoint.state_write_failures = 0;
        let mut stats = crate::download::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            skipped: skip_breakdown,
            elapsed_secs: started.elapsed().as_secs_f64(),
            ..crate::download::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, &mut checkpoint, enumeration_incomplete);
        tracing::info!(target: "kei::download::pipeline", "── Dry Run Summary ──");
        if shutdown_token.is_cancelled() {
            tracing::info!(target: "kei::download::pipeline", scanned = downloaded, "  Interrupted before shutdown");
        } else {
            tracing::info!(target: "kei::download::pipeline", count = downloaded, "  files would be downloaded");
        }
        tracing::info!(target: "kei::download::pipeline", destination = %config.directory.display(), "  destination");
        tracing::info!(target: "kei::download::pipeline", concurrency = config.concurrent_downloads, "  concurrency");
        let failed_count = enumeration_errors + usize::from(enumeration_incomplete);
        if failed_count > 0 {
            return Ok(finish_download_result(
                DownloadOutcome::PartialFailure { failed_count },
                stats,
                checkpoint,
            ));
        }
        return Ok(finish_download_result(
            DownloadOutcome::Success,
            stats,
            checkpoint,
        ));
    }

    if streaming_result.url_expired_abort {
        let retry_exhausted = skip_breakdown.retry_exhausted;
        let mut stats = crate::download::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: failed_tasks.len(),
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written,
            exif_failures,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            elapsed_secs: started.elapsed().as_secs_f64(),
            rate_limited: streaming_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded,
            recap: streaming_result.recap.clone(),
            ..crate::download::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, &mut checkpoint, enumeration_incomplete);
        log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);
        return Ok(finish_download_result(
            DownloadOutcome::PartialFailure {
                failed_count: failed_tasks.len()
                    + state_write_failures
                    + enumeration_errors
                    + exif_failures
                    + retry_exhausted
                    + 1,
            },
            stats,
            checkpoint,
        ));
    }

    if failed_tasks.is_empty() {
        let retry_exhausted = skip_breakdown.retry_exhausted;
        let mut stats = crate::download::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: 0,
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written,
            exif_failures,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            elapsed_secs: started.elapsed().as_secs_f64(),
            rate_limited: streaming_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded,
            recap: streaming_result.recap.clone(),
            ..crate::download::SyncStats::default()
        };
        mark_producer_enumeration_incomplete(&mut stats, &mut checkpoint, enumeration_incomplete);
        log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);
        if state_write_failures > 0
            || enumeration_errors > 0
            || exif_failures > 0
            || retry_exhausted > 0
            || enumeration_incomplete
        {
            return Ok(finish_download_result(
                DownloadOutcome::PartialFailure {
                    failed_count: state_write_failures
                        + enumeration_errors
                        + exif_failures
                        + retry_exhausted
                        + usize::from(enumeration_incomplete),
                },
                stats,
                checkpoint,
            ));
        }
        return Ok(finish_download_result(
            DownloadOutcome::Success,
            stats,
            checkpoint,
        ));
    }

    // Phase 2: cleanup pass with fresh CDN URLs
    let cleanup_concurrency = 5;
    let failure_count = failed_tasks.len();
    tracing::info!(target: "kei::download::pipeline",
        failure_count,
        concurrency = cleanup_concurrency,
        "── Cleanup pass: re-fetching URLs and retrying failed downloads ──"
    );

    let fresh_tasks = crate::download::build_retry_download_tasks(
        passes,
        config,
        &failed_tasks,
        shutdown_token.clone(),
    )
    .await?;
    tracing::debug!(target: "kei::download::pipeline",
        count = fresh_tasks.len(),
        "  Re-fetched failed tasks with fresh URLs"
    );

    let phase2_rate_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pass_config = PassConfig {
        client: download_client,
        retry_config: &config.retry,
        metadata: MetadataFlags::from(config.as_ref()),
        mark_capture_repair_after_download: matches!(
            config.capture_timestamp_repair,
            crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal
        ),
        concurrency: cleanup_concurrency,
        reporting: controls.reporting,
        temp_suffix: Arc::clone(&config.temp_suffix),
        shutdown_token: shutdown_token.clone(),
        state_db: config.state_db.clone(),
        rate_limit_counter: Arc::clone(&phase2_rate_counter),
        bandwidth_limiter: config.bandwidth_limiter.clone(),
        library: Arc::clone(&config.library),
    };
    let pass_result = run_download_pass(pass_config, fresh_tasks).await;

    let phase2_downloaded = pass_result.downloaded;
    let remaining_failed = pass_result.failed;
    let phase2_auth_errors = pass_result.auth_errors;
    exif_failures += pass_result.exif_failures;
    state_write_failures += pass_result.state_write_failures;
    checkpoint.state_write_failures = state_write_failures;
    checkpoint.interrupted = shutdown_token.is_cancelled();
    let total_auth_errors = auth_errors + phase2_auth_errors;

    if total_auth_errors >= AUTH_ERROR_THRESHOLD {
        checkpoint.interrupted = true;
        let mut merged_recap = streaming_result.recap.clone();
        merged_recap.merge(pass_result.recap.clone());
        let stats = crate::download::SyncStats {
            assets_seen: streaming_result.assets_seen,
            downloaded,
            failed: remaining_failed.len(),
            skipped: skip_breakdown,
            bytes_downloaded: streaming_result.bytes_downloaded + pass_result.bytes_downloaded,
            disk_bytes_written: streaming_result.disk_bytes_written
                + pass_result.disk_bytes_written,
            exif_failures,
            pagination_shortfall_warnings: 0,
            pagination_shortfall_assets: 0,
            elapsed_secs: started.elapsed().as_secs_f64(),
            rate_limited: streaming_result.rate_limit_observations
                + pass_result.rate_limit_observations,
            photos_downloaded: streaming_result.photos_downloaded + pass_result.photos_downloaded,
            videos_downloaded: streaming_result.videos_downloaded + pass_result.videos_downloaded,
            recap: merged_recap,
            ..crate::download::SyncStats::default()
        };
        return Ok(finish_download_result(
            DownloadOutcome::SessionExpired {
                auth_error_count: total_auth_errors,
            },
            stats,
            checkpoint,
        ));
    }

    let failed = remaining_failed.len();
    let succeeded = downloaded + phase2_downloaded;

    // Log failed downloads before the summary. `retry_exhausted` is asset
    // rows the producer skipped because they already exceeded max attempts
    // across prior syncs — they belong in the failure total so Docker /
    // systemd / k8s exit-code signalling can notice a chronic backlog.
    let retry_exhausted = skip_breakdown.retry_exhausted;
    let total_failures = failed
        + state_write_failures
        + exif_failures
        + enumeration_errors
        + retry_exhausted
        + usize::from(enumeration_incomplete);
    if total_failures > 0 {
        for task in &remaining_failed {
            tracing::error!(target: "kei::download::pipeline", asset_id = %task.asset_id, path = %task.download_path.display(), "Download failed");
        }
    }

    let mut merged_recap = streaming_result.recap.clone();
    merged_recap.merge(pass_result.recap.clone());
    let mut stats = crate::download::SyncStats {
        assets_seen: streaming_result.assets_seen,
        downloaded: succeeded,
        failed,
        skipped: skip_breakdown,
        bytes_downloaded: streaming_result.bytes_downloaded + pass_result.bytes_downloaded,
        disk_bytes_written: streaming_result.disk_bytes_written + pass_result.disk_bytes_written,
        exif_failures,
        pagination_shortfall_warnings: 0,
        pagination_shortfall_assets: 0,
        elapsed_secs: started.elapsed().as_secs_f64(),
        rate_limited: streaming_result.rate_limit_observations
            + pass_result.rate_limit_observations,
        photos_downloaded: streaming_result.photos_downloaded + pass_result.photos_downloaded,
        videos_downloaded: streaming_result.videos_downloaded + pass_result.videos_downloaded,
        recap: merged_recap,
        ..crate::download::SyncStats::default()
    };
    mark_producer_enumeration_incomplete(&mut stats, &mut checkpoint, enumeration_incomplete);
    maybe_warn_rate_limit_pressure(&stats);
    log_sync_summary("\u{2500}\u{2500} Summary \u{2500}\u{2500}", &stats);

    if total_failures > 0 {
        return Ok(finish_download_result(
            DownloadOutcome::PartialFailure {
                failed_count: total_failures,
            },
            stats,
            checkpoint,
        ));
    }

    Ok(finish_download_result(
        DownloadOutcome::Success,
        stats,
        checkpoint,
    ))
}

#[must_use]
fn finish_download_result(
    outcome: DownloadOutcome,
    mut stats: crate::download::SyncStats,
    checkpoint: CheckpointEvidence,
) -> SyncResult {
    checkpoint.project(&mut stats);
    SyncResult {
        outcome,
        sync_token: None,
        stats,
        checkpoint,
        full_enumeration_ran: false,
    }
}

#[cfg(test)]
mod tests;
