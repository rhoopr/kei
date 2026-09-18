//! Bounded streaming workers and transfer-result accumulation.

use std::sync::Arc;

use futures_util::StreamExt;
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::download::filter::DownloadTask;
use crate::download::finalize::{
    DownloadedFinalization, PendingStateWrite, check_state_write_circuit_breaker,
    finalize_downloaded, finalize_failed,
};
use crate::download::metadata_rewrite::MetadataFlags;
use crate::retry::RetryConfig;

use super::StreamPipelineShared;
use super::task::{
    AUTH_ERROR_THRESHOLD, DownloadSingleContext, DownloadTaskErrorClass, capture_repair_requested,
    classify_download_task_error, download_single_task, log_interrupted_download,
};

pub(super) struct StreamConsumerSettings {
    pub(super) retry_config: RetryConfig,
    pub(super) metadata_flags: MetadataFlags,
    pub(super) concurrency: usize,
    pub(super) mode: crate::personality::Mode,
    pub(super) bytes_counter: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Default)]
pub(super) struct StreamConsumerResult {
    pub(super) downloaded: usize,
    pub(super) exif_failures: usize,
    pub(super) failed: Vec<DownloadTask>,
    pub(super) auth_errors: usize,
    pub(super) pending_state_writes: Vec<PendingStateWrite>,
    pub(super) bytes_downloaded_total: u64,
    pub(super) disk_bytes_total: u64,
    pub(super) url_expired_abort: bool,
    pub(super) rate_limit_observations: usize,
    pub(super) photos_downloaded: usize,
    pub(super) videos_downloaded: usize,
    pub(super) recap: crate::download::recap::RunRecap,
    pub(super) state_write_circuit_error: Option<anyhow::Error>,
}

pub(super) async fn consume_stream_download_tasks(
    task_rx: mpsc::Receiver<DownloadTask>,
    download_client: Client,
    shared: StreamPipelineShared,
    settings: StreamConsumerSettings,
) -> StreamConsumerResult {
    let StreamConsumerSettings {
        retry_config,
        metadata_flags,
        concurrency,
        mode,
        bytes_counter,
    } = settings;
    let config = &shared.config;
    let mark_capture_repair_after_download = capture_repair_requested(config);
    let pb = &shared.pb;
    let pipeline_shutdown = shared.pipeline_shutdown;
    let state_db = shared.state_db;
    let temp_suffix: Arc<str> = Arc::clone(&config.temp_suffix);
    let bandwidth_limiter = config.bandwidth_limiter.clone();
    let rate_limit_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let download_stream = ReceiverStream::new(task_rx)
        .map(|task| {
            let client = download_client.clone();
            let task_state_db = state_db.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            let rate_limit_counter = Arc::clone(&rate_limit_counter);
            let bandwidth_limiter = bandwidth_limiter.clone();
            let shutdown_token = pipeline_shutdown.clone();
            async move {
                let result = Box::pin(download_single_task(
                    &client,
                    &task,
                    &retry_config,
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

    tokio::pin!(download_stream);

    // On cancellation we keep consuming results so in-flight downloads
    // still get their state rows written; new downloads are gated off by
    // the producer's own cancellation (which closes task_tx, naturally
    // ending this stream). The 30s watchdog in shutdown.rs is the backstop
    // if a hung download blocks the drain.
    let mut downloaded = 0usize;
    let mut exif_failures = 0usize;
    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut auth_errors = 0usize;
    let mut pending_state_writes: Vec<PendingStateWrite> = Vec::new();
    let mut bytes_downloaded_total: u64 = 0;
    let mut disk_bytes_total: u64 = 0;
    let mut url_expired_abort = false;
    let mut photos_downloaded = 0usize;
    let mut videos_downloaded = 0usize;
    let mut recap = crate::download::recap::RunRecap::default();
    let mut drain_logged = false;
    let mut state_write_circuit_error: Option<anyhow::Error> = None;
    while let Some((task, result)) = download_stream.next().await {
        if pipeline_shutdown.is_cancelled() && !drain_logged {
            pb.suspend(|| tracing::info!(target: "kei::download::pipeline", "Shutdown requested, draining in-flight downloads..."));
            drain_logged = true;
        }
        let filename = task
            .download_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        // Prefix the active filename with the pass's album label so the user
        // can tell which album's items are downloading.
        pb.set_message(format!("{} \u{00b7} {filename}", config.pass_label()));
        match result {
            Ok((exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes)) => {
                downloaded += 1;
                bytes_downloaded_total += bytes_dl;
                // Photos / videos fire in both modes (SyncStats serialises
                // them into the JSON report); the recap fold is
                // friendly-only so off-mode skips the per-success String
                // allocation `to_recap_asset` does for the filename.
                if task.media_type.is_photo_like() {
                    photos_downloaded += 1;
                } else if task.media_type.is_video_like() {
                    videos_downloaded += 1;
                }
                if mode.is_friendly() {
                    recap.observe(config.pass_label(), task.to_recap_asset());
                }
                // Feed the friendly bar's bandwidth sparkline / rate display.
                // Atomic+Relaxed is fine: the bar reads it on each redraw,
                // doesn't depend on it for correctness, and a missed update
                // smooths out within an EMA tick.
                bytes_counter.fetch_add(bytes_dl, std::sync::atomic::Ordering::Relaxed);
                disk_bytes_total += disk_bytes;
                if !exif_ok {
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
                        local_checksum,
                        download_checksum,
                        exif_ok,
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
                            if state_write_circuit_error.is_none()
                                && let Some(err) = check_state_write_circuit_breaker(
                                    db.as_ref(),
                                    &mut pending_state_writes,
                                )
                                .await
                            {
                                pb.suspend(|| {
                                    tracing::error!(target: "kei::download::pipeline",
                                        error = %err,
                                        "State write circuit breaker opened; halting downloads"
                                    );
                                });
                                state_write_circuit_error = Some(err);
                                pipeline_shutdown.cancel();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                match classify_download_task_error(&e) {
                    DownloadTaskErrorClass::Interrupted => {
                        log_interrupted_download(pb, &task, &e);
                        continue;
                    }
                    DownloadTaskErrorClass::SessionExpired => {
                        auth_errors += 1;
                        pb.suspend(|| {
                            tracing::warn!(target: "kei::download::pipeline",
                                auth_errors,
                                threshold = AUTH_ERROR_THRESHOLD,
                                path = %task.download_path.display(),
                                error = %e,
                                "Auth error"
                            );
                        });
                        if auth_errors >= AUTH_ERROR_THRESHOLD {
                            pb.suspend(|| {
                                tracing::warn!(target: "kei::download::pipeline",
                                    "Auth error threshold reached, aborting for re-authentication"
                                );
                            });
                            break;
                        }
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
                        pipeline_shutdown.cancel();
                        continue;
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
    }

    StreamConsumerResult {
        downloaded,
        exif_failures,
        failed,
        auth_errors,
        pending_state_writes,
        bytes_downloaded_total,
        disk_bytes_total,
        url_expired_abort,
        rate_limit_observations: rate_limit_counter.load(std::sync::atomic::Ordering::Relaxed),
        photos_downloaded,
        videos_downloaded,
        recap,
        state_write_circuit_error,
    }
}

#[cfg(test)]
mod tests;
