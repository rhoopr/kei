//! Bounded execution of an explicit download-task pass.

use std::sync::Arc;

use futures_util::{StreamExt, stream};
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::download::filter::DownloadTask;
use crate::download::finalize::{
    DownloadedFinalization, PendingStateWrite, check_state_write_circuit_breaker,
    finalize_downloaded, finalize_failed, flush_pending_state_writes,
};
use crate::download::metadata_rewrite::MetadataFlags;
use crate::download::{DownloadReporting, DownloadStore};
use crate::retry::RetryConfig;

use super::task::{
    DownloadSingleContext, DownloadTaskErrorClass, classify_download_task_error,
    download_single_task, log_interrupted_download,
};

/// Configuration for a download pass.
pub(in crate::download) struct PassConfig<'a> {
    pub(in crate::download) client: &'a Client,
    pub(in crate::download) retry_config: &'a RetryConfig,
    pub(in crate::download) metadata: MetadataFlags,
    pub(in crate::download) mark_capture_repair_after_download: bool,
    pub(in crate::download) concurrency: usize,
    pub(in crate::download) reporting: DownloadReporting,
    pub(in crate::download) temp_suffix: Arc<str>,
    pub(in crate::download) shutdown_token: CancellationToken,
    pub(in crate::download) state_db: Option<Arc<dyn DownloadStore>>,
    /// Accumulator for 429/503 observations during this pass. Counted per
    /// retry attempt, not per unique task. Aggregated into SyncStats for
    /// the rate-limit pressure warning.
    pub(in crate::download) rate_limit_counter: Arc<std::sync::atomic::AtomicUsize>,
    pub(in crate::download) bandwidth_limiter: Option<crate::download::BandwidthLimiter>,
    /// CloudKit zone name scoping every state-DB key written by this pass.
    /// Sourced from `DownloadConfig::library` at pass dispatch.
    pub(in crate::download) library: Arc<str>,
}

impl std::fmt::Debug for PassConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PassConfig")
            .field("metadata", &self.metadata)
            .field(
                "mark_capture_repair_after_download",
                &self.mark_capture_repair_after_download,
            )
            .field("concurrency", &self.concurrency)
            .field("reporting", &self.reporting)
            .field("temp_suffix", &self.temp_suffix)
            .field("state_db", &self.state_db.as_ref().map(|_| ".."))
            .finish_non_exhaustive()
    }
}

/// Result of a download pass.
#[derive(Debug)]
pub(in crate::download) struct PassResult {
    pub(in crate::download) downloaded: usize,
    pub(in crate::download) downloaded_tasks: Vec<DownloadTask>,
    pub(in crate::download) exif_failures: usize,
    pub(in crate::download) failed: Vec<DownloadTask>,
    pub(in crate::download) auth_errors: usize,
    pub(in crate::download) state_write_failures: usize,
    pub(in crate::download) bytes_downloaded: u64,
    pub(in crate::download) disk_bytes_written: u64,
    pub(in crate::download) rate_limit_observations: usize,
    pub(in crate::download) url_expired_abort: bool,
    /// Photos / videos / recap observed during this pass, mirroring
    /// `StreamingResult`. Folded into the cycle's `SyncStats` at the
    /// caller. Defaults are zero / empty so the existing cleanup-pass
    /// path behaves identically in non-friendly mode.
    pub(in crate::download) photos_downloaded: usize,
    pub(in crate::download) videos_downloaded: usize,
    pub(in crate::download) recap: crate::download::recap::RunRecap,
}

/// Execute a download pass over the given tasks, returning any that failed.
pub(in crate::download) async fn run_download_pass(
    config: PassConfig<'_>,
    tasks: Vec<DownloadTask>,
) -> PassResult {
    // Cleanup-pass bar: same bytes-counter as the main bar would have if
    // wired in, but this pass runs after the main pass closes its bar so we
    // create a fresh counter here. The retry pass downloads less data on
    // average, so the bandwidth display reads as the cleanup-only rate.
    let cleanup_bytes_counter = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let pb = crate::personality::progress::single(
        config.reporting.no_progress_bar,
        false,
        tasks.len() as u64,
        config.reporting.personality_mode,
        Some(std::sync::Arc::clone(&cleanup_bytes_counter)),
    );
    let client = config.client.clone();
    let retry_config = config.retry_config;
    let metadata_flags = config.metadata;
    let mark_capture_repair_after_download = config.mark_capture_repair_after_download;
    let state_db = config.state_db.clone();
    let pass_shutdown = config.shutdown_token.child_token();
    let concurrency = config.concurrency;
    let temp_suffix: Arc<str> = config.temp_suffix;
    let rate_limit_counter = Arc::clone(&config.rate_limit_counter);
    let bandwidth_limiter = config.bandwidth_limiter.clone();
    let library: Arc<str> = Arc::clone(&config.library);
    let mode = config.reporting.personality_mode;

    let mut download_stream = stream::iter(tasks)
        .take_while(|_| std::future::ready(!pass_shutdown.is_cancelled()))
        .map(|task| {
            let client = client.clone();
            let task_state_db = state_db.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            let rate_limit_counter = Arc::clone(&rate_limit_counter);
            let bandwidth_limiter = bandwidth_limiter.clone();
            let shutdown_token = pass_shutdown.clone();
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    retry_config,
                    metadata_flags,
                    DownloadSingleContext {
                        temp_suffix: &temp_suffix,
                        state_db: task_state_db.as_deref(),
                        rate_limit_counter: Some(rate_limit_counter.as_ref()),
                        bandwidth_limiter: bandwidth_limiter.as_ref(),
                        shutdown_token: &shutdown_token,
                        mode,
                    },
                ))
                .await;
                (task, result)
            }
        })
        .buffer_unordered(concurrency);

    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut downloaded = 0usize;
    let mut downloaded_tasks: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;
    let mut exif_failures = 0usize;
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();
    let mut bytes_downloaded_total: u64 = 0;
    let mut disk_bytes_total: u64 = 0;
    let mut photos_downloaded = 0usize;
    let mut videos_downloaded = 0usize;
    let mut recap = crate::download::recap::RunRecap::default();
    let mut state_write_circuit_open = false;
    let mut url_expired_abort = false;
    // Cleanup pass doesn't carry an album label (it's a flat retry list);
    // recap.observe gets the library name so a recovered asset still
    // counts toward the per-album newest tracker rather than vanishing.
    let pass_label = library.as_ref();

    // Stream results as each task completes so state writes and progress-bar
    // updates fire per-item. Collecting first would freeze the progress bar
    // until the last download finished and defer every mark_downloaded to
    // the end of the pass — defeating the point of parallel cleanup.
    while let Some((task, result)) = download_stream.next().await {
        match &result {
            Ok((exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes)) => {
                downloaded += 1;
                downloaded_tasks.push(task.clone());
                bytes_downloaded_total += bytes_dl;
                cleanup_bytes_counter.fetch_add(*bytes_dl, std::sync::atomic::Ordering::Relaxed);
                disk_bytes_total += disk_bytes;
                if task.media_type.is_photo_like() {
                    photos_downloaded += 1;
                } else if task.media_type.is_video_like() {
                    videos_downloaded += 1;
                }
                if mode.is_friendly() {
                    recap.observe(pass_label, task.to_recap_asset());
                }
                if !*exif_ok {
                    exif_failures += 1;
                    pb.suspend(|| {
                        tracing::error!(target: "kei::download::pipeline",
                            asset_id = %task.asset_id,
                            path = %task.download_path.display(),
                            "Metadata write failed after download; marker set for retry on next sync"
                        );
                    });
                }
                if let Some(db) = &state_db {
                    match finalize_downloaded(
                        db.as_ref(),
                        &task.library,
                        &task,
                        local_checksum.clone(),
                        download_checksum.clone(),
                        *exif_ok,
                        mark_capture_repair_after_download,
                    )
                    .await
                    {
                        DownloadedFinalization::Persisted => {}
                        DownloadedFinalization::Deferred { write, error } => {
                            pb.suspend(|| {
                                tracing::warn!(target: "kei::download::pipeline",
                                    asset_id = %task.asset_id,
                                    error = %error,
                                    "State write failed, deferring for retry"
                                );
                            });
                            pending_state_writes.push(write);
                            if !state_write_circuit_open
                                && let Some(err) = check_state_write_circuit_breaker(
                                    db.as_ref(),
                                    &mut pending_state_writes,
                                )
                                .await
                            {
                                pb.suspend(|| {
                                        tracing::error!(target: "kei::download::pipeline",
                                            error = %err,
                                            "State write circuit breaker opened; halting cleanup downloads"
                                        );
                                    });
                                state_write_circuit_open = true;
                                pass_shutdown.cancel();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                match classify_download_task_error(e) {
                    DownloadTaskErrorClass::Interrupted => {
                        log_interrupted_download(&pb, &task, e);
                        pb.inc(1);
                        continue;
                    }
                    DownloadTaskErrorClass::SessionExpired => {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(target: "kei::download::pipeline", path = %task.download_path.display(), error = %e, "Auth error");
                        });
                    }
                    DownloadTaskErrorClass::ExpiredUrl => {
                        url_expired_abort = true;
                        pb.suspend(|| {
                            tracing::warn!(target: "kei::download::pipeline",
                                asset_id = %task.asset_id,
                                path = %task.download_path.display(),
                                error = %e,
                                "Download URL expired; aborting current URL batch"
                            );
                        });
                        pass_shutdown.cancel();
                    }
                    DownloadTaskErrorClass::Other => {
                        pb.suspend(|| {
                            tracing::error!(target: "kei::download::pipeline", asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                        });
                    }
                }
                if let Some(db) = &state_db
                    && let Err(e) =
                        finalize_failed(db.as_ref(), &task.library, &task, &e.to_string()).await
                {
                    tracing::warn!(target: "kei::download::pipeline",
                        asset_id = %task.asset_id,
                        error = %e,
                        "Failed to mark failure"
                    );
                }
                failed.push(task);
            }
        }
        pb.inc(1);
    }

    // Retry any state writes that failed during the pass
    let state_write_failures = if let Some(db) = &state_db {
        if state_write_circuit_open {
            pending_state_writes.len()
        } else {
            flush_pending_state_writes(db.as_ref(), &pending_state_writes).await
        }
    } else {
        0
    };

    pb.finish_and_clear();
    PassResult {
        downloaded,
        downloaded_tasks,
        exif_failures,
        failed,
        auth_errors,
        state_write_failures,
        bytes_downloaded: bytes_downloaded_total,
        disk_bytes_written: disk_bytes_total,
        rate_limit_observations: rate_limit_counter.load(std::sync::atomic::Ordering::Relaxed),
        url_expired_abort,
        photos_downloaded,
        videos_downloaded,
        recap,
    }
}

#[cfg(test)]
mod tests;
