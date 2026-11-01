//! Bounded execution of an explicit download-task pass.

use std::sync::Arc;

use futures_util::{StreamExt, stream};
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::download::filter::DownloadTask;
use crate::download::finalize::{
    DownloadedFinalization, PendingStateWrite, check_state_write_circuit_breaker,
    finalize_downloaded_with_proof, finalize_failed, flush_pending_state_writes,
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
    pub(in crate::download) selection_run:
        Option<Arc<crate::download::orchestration::generation::SelectionRun>>,
    pub(in crate::download) retry_owners: Option<Arc<super::retry_owners::RetryOwners>>,
    pub(in crate::download) refresh_requests: Option<
        rustc_hash::FxHashMap<crate::download::RetryTaskKey, crate::icloud::photos::RefreshRequest>,
    >,
    pub(in crate::download) prior_auth_errors: usize,
    pub(in crate::download) url_obtained_at:
        rustc_hash::FxHashMap<crate::download::RetryTaskKey, std::time::Instant>,
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
    /// Admitted tasks held without a physical transfer attempt.
    pub(in crate::download) shared_waiting: Vec<DownloadTask>,
    pub(in crate::download) auth_errors: usize,
    pub(in crate::download) state_write_failures: usize,
    pub(in crate::download) bytes_downloaded: u64,
    pub(in crate::download) disk_bytes_written: u64,
    pub(in crate::download) rate_limit_observations: usize,
    pub(in crate::download) url_expired: bool,
    pub(in crate::download) expired_tasks: Vec<DownloadTask>,
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
    let url_obtained_at = &config.url_obtained_at;
    let planned_tasks = tasks.clone();
    let selection_run = config.selection_run.clone();
    let retry_owners = config.retry_owners.clone();
    let selected_cleanup = config.refresh_requests.is_some();
    let mut refresh_requests = config.refresh_requests.unwrap_or_default();
    let refresh_capacity = Arc::new(tokio::sync::Semaphore::new(
        crate::icloud::photos::AUTH_REFRESH_MAX_IN_FLIGHT,
    ));

    let mut download_stream = stream::iter(tasks)
        .take_while(|_| std::future::ready(!pass_shutdown.is_cancelled()))
        .map(|task| {
            let request = refresh_requests.remove(&crate::download::RetryTaskKey::from(&task));
            let refresh_capacity = Arc::clone(&refresh_capacity);
            let client = client.clone();
            let task_state_db = state_db.clone();
            let temp_suffix = Arc::clone(&temp_suffix);
            let rate_limit_counter = Arc::clone(&rate_limit_counter);
            let bandwidth_limiter = bandwidth_limiter.clone();
            let shutdown_token = pass_shutdown.clone();
            let selection_run = selection_run.clone();
            let retry_owners = retry_owners.clone();
            async move {
                let selected_owner = retry_owners.as_ref().is_some_and(|owners| matches!(
                    owners.get(&task), Some(super::retry_owners::ResolvedOwner::Selected(_, _))
                ));
                let owner = match retry_owners.as_ref() {
                    Some(owners) => match owners.get(&task) {
                        Some(super::retry_owners::ResolvedOwner::Selected(run, pass)) => {
                            Some((run, pass))
                        }
                        Some(super::retry_owners::ResolvedOwner::Ordinary) => {
                            selection_run.as_ref().zip(
                                task.publication_binding
                                    .as_deref()
                                    .map(|binding| binding.origin_pass.as_str()),
                            )
                        }
                        None => return (task, Err(super::task::missing_retry_owner_error())),
                    },
                    None => selection_run.as_ref().zip(
                        task.publication_binding
                            .as_deref()
                            .map(|binding| binding.origin_pass.as_str()),
                    ),
                };
                // This prerequisite is intentionally non-dispatching until a
                // state transaction can bind the new source and preserve a
                // refreshed 410 across restart. The actual lookup is bounded,
                // cancellable, and performed under the destination guard.
                if selected_cleanup {
                    if selected_owner {
                        let Some((run, pass)) = owner else {
                            return (task, Err(super::task::missing_retry_owner_error()));
                        };
                        let mut hold = super::task::HoldSelectionOnDrop::new(Arc::clone(run));
                        let Some(request) = request else {
                            return (task, Err(super::task::selected_refresh_pending()));
                        };
                        if !run.matches_refresh_request(pass, &task, &request) {
                            return (task, Err(super::task::selected_refresh_pending()));
                        }
                        let permit = tokio::select! {
                            () = shutdown_token.cancelled() => return (task, Err(super::task::selected_refresh_pending())),
                            permit = refresh_capacity.acquire_owned() => match permit {
                                Ok(permit) => permit,
                                Err(_) => return (task, Err(super::task::selected_refresh_pending())),
                            },
                        };
                        let guard = match crate::download::file::lock_download_destination(&task.download_path, &shutdown_token).await {
                            Ok(guard) => guard,
                            Err(_) => return (task, Err(super::task::selected_refresh_pending())),
                        };
                        let result = tokio::select! {
                            () = shutdown_token.cancelled() => None,
                            result = request.resolve() => Some(result),
                        };
                        #[cfg(test)]
                        if matches!(&result, Some(Ok(Some(_)))) {
                            eprintln!("ISSUE845_SELECTED_REFRESH_WITNESS_HELD");
                        }
                        let witness = match result {
                            Some(Ok(Some(witness))) => witness,
                            Some(Err(error)) if crate::icloud::photos::session::is_session_error(&error) => {
                                return (task, Err(super::task::selected_refresh_authentication_error()));
                            }
                            _ => return (task, Err(super::task::selected_refresh_pending())),
                        };
                        let result = Box::pin(super::task::download_selected_refresh_with_guard(
                            &client, &task, retry_config, metadata_flags,
                            DownloadSingleContext {
                                temp_suffix: &temp_suffix,
                                state_db: task_state_db.as_deref(),
                                rate_limit_counter: Some(rate_limit_counter.as_ref()),
                                bandwidth_limiter: bandwidth_limiter.as_ref(),
                                shutdown_token: &shutdown_token,
                                mode,
                                selection_wait: owner,
                            }, guard, witness,
                        )).await;
                        drop(permit);
                        if result.is_ok() { hold.disarm(); }
                        return (task, result);
                    } else if request.is_some() {
                        // A descriptor attached to an ordinary/unknown task is
                        // ambiguous; never issue its old URL.
                        return (task, Err(super::task::missing_retry_owner_error()));
                    }
                }
                if let Some(obtained_at) =
                    url_obtained_at.get(&crate::download::RetryTaskKey::from(&task))
                {
                    tracing::debug!(target: "kei::download::pipeline",
                        refreshed_url_observed_age_secs = obtained_at.elapsed().as_secs_f64(),
                        "Dispatching task with a refreshed URL; provider expiry is unknown"
                    );
                }
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
                        selection_wait: owner,
                    },
                ))
                .await;
                (task, result.map(|result| (result, None)))
            }
        })
        .buffer_unordered(concurrency);

    let mut failed: Vec<DownloadTask> = Vec::new();
    let mut shared_waiting: Vec<DownloadTask> = Vec::new();
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
    let mut url_expired = false;
    let mut expired_tasks = Vec::new();
    let mut failure_state_writes = 0usize;
    // Cleanup pass doesn't carry an album label (it's a flat retry list);
    // recap.observe gets the library name so a recovered asset still
    // counts toward the per-album newest tracker rather than vanishing.
    let pass_label = library.as_ref();

    // Stream results as each task completes so state writes and progress-bar
    // updates fire per-item. Collecting first would freeze the progress bar
    // until the last download finished and defer every mark_downloaded to
    // the end of the pass — defeating the point of parallel cleanup.
    while let Some((task, mut result)) = download_stream.next().await {
        match &mut result {
            Ok((
                (exif_ok, local_checksum, download_checksum, bytes_dl, disk_bytes, retained),
                completion,
            )) => {
                if let Some(completion) = completion.take() {
                    let Some(db) = state_db.as_ref().cloned() else {
                        tracing::error!(target: "kei::download::pipeline", "Selected completion has no state owner");
                        failed.push(task.clone());
                        continue;
                    };
                    let super::task::RefreshedCompletion {
                        guard,
                        ticket,
                        run,
                        mut hold,
                    } = completion;
                    let mut waiter_hold = super::task::HoldSelectionOnDrop::new(Arc::clone(&run));
                    let owner = Arc::clone(&run);
                    let original = task.clone();
                    let local = local_checksum.clone();
                    let source = download_checksum.clone();
                    let proof = retained.clone();
                    let exif = *exif_ok;
                    let finalized = super::task::hold_guard_through(guard, async move {
                        let outcome = finalize_downloaded_with_proof(
                            db.as_ref(),
                            &original.library,
                            &original,
                            local.clone(),
                            source.clone(),
                            (exif, metadata_flags.bits()),
                            mark_capture_repair_after_download,
                            proof,
                        )
                        .await;
                        #[cfg(test)]
                        let abandon = if matches!(&outcome, DownloadedFinalization::Persisted) {
                            super::task::real_410_abort_hook::wait_refreshed_finalized(
                                &original.download_path,
                                Arc::clone(&owner.held),
                            )
                            .await
                        } else {
                            false
                        };
                        #[cfg(not(test))]
                        let abandon = false;
                        let terminal = if abandon {
                            Err(anyhow::anyhow!(
                                "synthetic interruption before selected terminal"
                            ))
                        } else if matches!(&outcome, DownloadedFinalization::Persisted) {
                            match source {
                                Some(source) => {
                                    owner
                                        .finish_refreshed_transfer(ticket, &original, local, source)
                                        .await
                                }
                                None => Err(anyhow::anyhow!("Missing selected source checksum")),
                            }
                        } else {
                            Err(anyhow::anyhow!("Selected receipt finalization deferred"))
                        };
                        if terminal.is_ok() {
                            hold.disarm();
                        }
                        (outcome, terminal)
                    })
                    .await;
                    match finalized {
                        Ok((guard, (DownloadedFinalization::Persisted, Ok(())))) => {
                            waiter_hold.disarm();
                            drop(guard);
                        }
                        Ok((guard, (DownloadedFinalization::Deferred { write: _, error }, _))) => {
                            tracing::warn!(target: "kei::download::pipeline", asset_id = %task.asset_id, error = %error, "Selected receipt finalization remains pending");
                            // The selected ticket remains durably unresolved. A generic
                            // receipt retry cannot safely complete its reservation.
                            failed.push(task.clone());
                            drop(guard);
                            continue;
                        }
                        Ok((guard, (_, Err(error)))) => {
                            tracing::warn!(target: "kei::download::pipeline", asset_id = %task.asset_id, error = %error, "Selected transaction remains pending");
                            failed.push(task.clone());
                            drop(guard);
                            continue;
                        }
                        Err(error) => {
                            tracing::error!(target: "kei::download::pipeline", asset_id = %task.asset_id, error = %error, "Selected transaction owner did not finish");
                            failed.push(task.clone());
                            continue;
                        }
                    }
                } else {
                    if let Some(db) = &state_db {
                        match finalize_downloaded_with_proof(
                            db.as_ref(),
                            &task.library,
                            &task,
                            local_checksum.clone(),
                            download_checksum.clone(),
                            (*exif_ok, metadata_flags.bits()),
                            mark_capture_repair_after_download,
                            retained.clone(),
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
                downloaded += 1;
                downloaded_tasks.push(task.clone());
                bytes_downloaded_total += *bytes_dl;
                cleanup_bytes_counter.fetch_add(*bytes_dl, std::sync::atomic::Ordering::Relaxed);
                disk_bytes_total += *disk_bytes;
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
            }
            Err(e) => {
                if super::task::is_selected_refresh_authentication(e) {
                    auth_errors += 1;
                    // No physical transfer was attempted; preserve the
                    // unresolved task without inventing a transfer failure or
                    // an authenticated shared wait.
                    failed.push(task.clone());
                    if auth_errors + config.prior_auth_errors >= super::task::AUTH_ERROR_THRESHOLD {
                        pass_shutdown.cancel();
                    }
                    continue;
                }
                match classify_download_task_error(e) {
                    DownloadTaskErrorClass::SelectedRefreshPending => {
                        // No GET occurred, so retain the task without writing
                        // an attempted download or claiming a shared wait.
                        failed.push(task.clone());
                        #[cfg(test)]
                        eprintln!("ISSUE845_SELECTED_REFRESH_UNRESOLVED_CLASSIFIED");
                        continue;
                    }
                    DownloadTaskErrorClass::SharedWait => {
                        shared_waiting.push(task.clone());
                        continue;
                    }
                    DownloadTaskErrorClass::SharedWaitInvalid => {
                        failure_state_writes += 1;
                        // This exact task remains held; independent tasks continue.
                        shared_waiting.push(task.clone());
                        continue;
                    }
                    DownloadTaskErrorClass::Interrupted => {
                        log_interrupted_download(&pb, &task, e);
                        // Interrupted transfers remain explicit retry debt.
                        // The common failure path persists them before returning.
                    }
                    DownloadTaskErrorClass::SessionExpired => {
                        auth_errors += 1;
                        if auth_errors + config.prior_auth_errors
                            >= super::task::AUTH_ERROR_THRESHOLD
                        {
                            pass_shutdown.cancel();
                        }
                        pb.suspend(|| {
                            tracing::warn!(target: "kei::download::pipeline", path = %task.download_path.display(), error = %e, "Auth error");
                        });
                    }
                    DownloadTaskErrorClass::ExpiredUrl => {
                        url_expired = true;
                        expired_tasks.push(task.clone());
                        pb.suspend(|| {
                            tracing::warn!(target: "kei::download::pipeline",
                                asset_id = %task.asset_id,
                                path = %task.download_path.display(),
                                error = %e,
                                "Download URL expired; retaining task for bounded refresh"
                            );
                        });
                    }
                    DownloadTaskErrorClass::Other => {
                        pb.suspend(|| {
                            tracing::error!(target: "kei::download::pipeline", asset_id = %task.asset_id, path = %task.download_path.display(), error = %e, "Download failed");
                        });
                    }
                }
                if let Some(db) = &state_db
                    && !super::task::is_accounted_refresh_failure(e)
                    && let Err(e) =
                        finalize_failed(db.as_ref(), &task.library, &task, &e.to_string()).await
                {
                    failure_state_writes += 1;
                    pass_shutdown.cancel();
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

    // Cancellation can leave queued tasks unpolled. Keep every unfinished
    // dispatch in the returned debt and persist its failure before checkpointing.
    let completed: rustc_hash::FxHashSet<_> = downloaded_tasks
        .iter()
        .chain(&failed)
        .chain(&shared_waiting)
        .map(crate::download::RetryTaskKey::from)
        .collect();
    for task in planned_tasks {
        if completed.contains(&crate::download::RetryTaskKey::from(&task)) {
            continue;
        }
        if let Some(db) = &state_db
            && finalize_failed(
                db.as_ref(),
                &task.library,
                &task,
                "Download dispatch interrupted",
            )
            .await
            .is_err()
        {
            failure_state_writes += 1;
        }
        failed.push(task);
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
        shared_waiting,
        auth_errors,
        state_write_failures: state_write_failures + failure_state_writes,
        bytes_downloaded: bytes_downloaded_total,
        disk_bytes_written: disk_bytes_total,
        rate_limit_observations: rate_limit_counter.load(std::sync::atomic::Ordering::Relaxed),
        url_expired,
        expired_tasks,
        photos_downloaded,
        videos_downloaded,
        recap,
    }
}

#[cfg(test)]
mod tests;
