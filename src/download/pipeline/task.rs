//! Single-file transfer, metadata completion, and worker error classification.

use std::fs::FileTimes;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use indicatif::ProgressBar;
use tokio_util::sync::CancellationToken;

use crate::download::error::DownloadError;
use crate::download::filter::DownloadTask;
use crate::download::metadata_rewrite::MetadataFlags;
use crate::download::{DownloadConfig, DownloadStore, metadata_rewrite};
use crate::retry::RetryConfig;

/// Threshold of auth errors before aborting the download pass for re-authentication.
/// Counted cumulatively across both phases (streaming + cleanup).
pub(in crate::download) const AUTH_ERROR_THRESHOLD: usize = 3;

pub(super) fn capture_repair_requested(config: &DownloadConfig) -> bool {
    config.refresh_metadata
        && matches!(
            config.capture_timestamp_repair,
            crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal
        )
}

/// Download a single task, handling mtime and EXIF stamping on success.
///
/// Returns `Ok(true)` on full success, `Ok(false)` if the download succeeded
/// but EXIF stamping failed (the file is usable but lacks EXIF metadata).
#[derive(Clone, Copy)]
pub(in crate::download) struct DownloadSingleContext<'a> {
    pub(in crate::download) temp_suffix: &'a str,
    pub(in crate::download) state_db: Option<&'a dyn DownloadStore>,
    pub(in crate::download) rate_limit_counter: Option<&'a std::sync::atomic::AtomicUsize>,
    pub(in crate::download) bandwidth_limiter: Option<&'a crate::download::BandwidthLimiter>,
    pub(in crate::download) shutdown_token: &'a CancellationToken,
    pub(in crate::download) mode: crate::personality::Mode,
    pub(in crate::download) selection_wait: Option<(
        &'a Arc<crate::download::orchestration::generation::SelectionRun>,
        &'a str,
    )>,
}

/// The owned guard remains in the spawned operation even if its waiter is
/// aborted while an async witness lookup or blocking SQLite call is running.
pub(in crate::download) async fn hold_guard_through<G, F, T>(
    guard: G,
    operation: F,
) -> Result<(G, T), tokio::task::JoinError>
where
    G: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    tokio::spawn(async move {
        let result = operation.await;
        (guard, result)
    })
    .await
}

/// An unexpected owner-task drop/panic must not make an unrecorded operation
/// look checkpoint-safe in the still-running process.
pub(in crate::download) struct HoldSelectionOnDrop {
    run: Arc<crate::download::orchestration::generation::SelectionRun>,
    armed: bool,
}
impl HoldSelectionOnDrop {
    pub(in crate::download) fn new(
        run: Arc<crate::download::orchestration::generation::SelectionRun>,
    ) -> Self {
        Self { run, armed: true }
    }
    pub(in crate::download) fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for HoldSelectionOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.run
                .held
                .store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(target:"kei::download::pipeline", "Selection guard owner did not finish safely; checkpoint held");
        }
    }
}
fn item_only_hold(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<crate::state::error::StateError>()
        .is_some_and(|e| {
            matches!(
                e,
                crate::state::error::StateError::ProviderSelectionAmbiguous
                    | crate::state::error::StateError::ProviderSelectionFull
            )
        })
}

#[derive(Debug, thiserror::Error)]
#[error("Selected transfer is waiting for its authenticated predecessor")]
struct SelectionTransferWaiting;

#[derive(Debug, thiserror::Error)]
#[error("Could not validate selected transfer wait")]
struct SelectionTransferWaitInvalid(#[source] anyhow::Error);

#[derive(Debug, thiserror::Error)]
#[error("Selected refresh authentication failed")]
struct SelectedRefreshAuthentication;

pub(super) fn selected_refresh_authentication_error() -> anyhow::Error {
    SelectedRefreshAuthentication.into()
}

pub(super) fn is_selected_refresh_authentication(error: &anyhow::Error) -> bool {
    error.is::<SelectedRefreshAuthentication>()
}

/// The state owner has durably recorded this exact failed transfer and
/// incremented its asset failure once. Preserve the original diagnostic.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct AccountedRefreshFailure(String);

pub(super) fn is_accounted_refresh_failure(error: &anyhow::Error) -> bool {
    error.is::<AccountedRefreshFailure>()
}

#[derive(Debug, thiserror::Error)]
#[error("Selected refresh is unresolved and remains pending")]
struct SelectedRefreshPending;

pub(super) fn selected_refresh_pending() -> anyhow::Error {
    SelectedRefreshPending.into()
}

pub(super) fn missing_retry_owner_error() -> anyhow::Error {
    SelectionTransferWaitInvalid(anyhow::anyhow!(
        "Selected retry owner is unavailable or ambiguous"
    ))
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadTaskErrorClass {
    SelectedRefreshPending,
    SharedWait,
    SharedWaitInvalid,
    Interrupted,
    SessionExpired,
    ExpiredUrl,
    Other,
}

/// Classify per-task download errors at the worker orchestration boundary.
///
/// Session expiry contributes to the reauth threshold; interruption and other
/// failures retain task debt. Streaming expiry can stop stale enumeration,
/// while explicit-task passes isolate expiry so healthy peers finish. The
/// original error reaches state and logging unchanged.
pub(super) fn classify_download_task_error(error: &anyhow::Error) -> DownloadTaskErrorClass {
    if error.is::<SelectedRefreshPending>() {
        return DownloadTaskErrorClass::SelectedRefreshPending;
    }
    if error.is::<SelectionTransferWaiting>() {
        return DownloadTaskErrorClass::SharedWait;
    }
    if error.is::<SelectionTransferWaitInvalid>() {
        return DownloadTaskErrorClass::SharedWaitInvalid;
    }
    let Some(download_err) = error.downcast_ref::<DownloadError>() else {
        return DownloadTaskErrorClass::Other;
    };
    if download_err.is_interrupted() {
        DownloadTaskErrorClass::Interrupted
    } else if download_err.is_session_expired() {
        DownloadTaskErrorClass::SessionExpired
    } else if download_err.is_expired_url() {
        DownloadTaskErrorClass::ExpiredUrl
    } else {
        DownloadTaskErrorClass::Other
    }
}

pub(super) fn log_interrupted_download(
    pb: &ProgressBar,
    task: &DownloadTask,
    error: &anyhow::Error,
) {
    pb.suspend(|| {
        tracing::info!(target: "kei::download::pipeline",
            asset_id = %task.asset_id,
            path = %task.download_path.display(),
            error = %error,
            "Download interrupted before final publish"
        );
    });
}

pub(super) struct RefreshedCompletion {
    pub(super) guard: crate::download::file::StagingGuard,
    pub(super) ticket:
        crate::state::db::provider_generations::refreshed_transfer::RefreshedDispatchTicket,
    pub(super) run: Arc<crate::download::orchestration::generation::SelectionRun>,
    pub(super) hold: HoldSelectionOnDrop,
}

pub(super) type DownloadSingleResult = (
    bool,
    String,
    Option<String>,
    u64,
    u64,
    Option<Arc<crate::download::file::RetainedPendingFile>>,
);

pub(in crate::download) async fn download_single_task<C: crate::download::file::DownloadClient>(
    client: &C,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    metadata_flags: MetadataFlags,
    context: DownloadSingleContext<'_>,
) -> Result<DownloadSingleResult> {
    download_single_task_inner(client, task, retry_config, metadata_flags, context, None)
        .await
        .map(|(result, _)| result)
}

/// Resume selected cleanup with the SAME destination guard acquired before
/// the authenticated provider lookup. No lock is reacquired on this path.
pub(super) async fn download_selected_refresh_with_guard<
    C: crate::download::file::DownloadClient,
>(
    client: &C,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    metadata_flags: MetadataFlags,
    context: DownloadSingleContext<'_>,
    guard: crate::download::file::StagingGuard,
    witness: crate::icloud::photos::AuthenticatedRefresh,
) -> Result<(DownloadSingleResult, Option<RefreshedCompletion>)> {
    download_single_task_inner(
        client,
        task,
        retry_config,
        metadata_flags,
        context,
        Some((guard, witness)),
    )
    .await
}

async fn download_single_task_inner<C: crate::download::file::DownloadClient>(
    client: &C,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    metadata_flags: MetadataFlags,
    context: DownloadSingleContext<'_>,
    refresh: Option<(
        crate::download::file::StagingGuard,
        crate::icloud::photos::AuthenticatedRefresh,
    )>,
) -> Result<(DownloadSingleResult, Option<RefreshedCompletion>)> {
    anyhow::ensure!(
        !(task.require_created_entry && task.replacement_fingerprint.is_some()),
        "Existing-file replacement remains pending because historical ownership is incomplete"
    );
    if let Some(db) = context.state_db {
        db.guard_primary_writer(&task.download_path).await?;
    }
    // A producer can commit retry state immediately before cancellation. Do not
    // start another HTTP request for its queued task while draining that batch.
    if context.shutdown_token.is_cancelled() {
        return Err(DownloadError::Interrupted {
            path: task
                .download_path
                .to_string_lossy()
                .into_owned()
                .into_boxed_str(),
            bytes_written: 0,
        }
        .into());
    }
    if let Some(parent) = task.download_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Could not create directory {}", parent.display()))?;
    }

    tracing::debug!(target: "kei::download::pipeline",
        size_bytes = task.size,
        path = %task.download_path.display(),
        "downloading",
    );

    // Embed writes happen on the .part file before the atomic rename. The
    // extension gate is based on the intended final path before download,
    // then the writer sniffs the downloaded part bytes so the temp suffix
    // does not hide the media type.
    let needs_embed = metadata_flags.any_embed()
        && crate::download::metadata::is_embed_writable_path(&task.download_path);
    let owned_part_path = crate::download::file::temp_download_path(
        &task.download_path,
        &task.checksum,
        context.temp_suffix,
    )
    .context("Could not compute temporary download path")?;
    let (staging_guard, refresh_witness) = match refresh {
        Some((guard, witness)) => (guard, Some(witness)),
        None => (
            crate::download::file::lock_download_destination(
                &task.download_path,
                context.shutdown_token,
            )
            .await?,
            None,
        ),
    };
    if context.shutdown_token.is_cancelled() {
        return Err(DownloadError::Interrupted {
            path: task.download_path.display().to_string().into(),
            bytes_written: 0,
        }
        .into());
    }
    let refresh_url = refresh_witness
        .as_ref()
        .map(|witness| witness.url().to_owned());
    let mut refresh_ticket = None;
    // The owner task retains both the existing guard and witness through the
    // state transaction, even if its caller is cancelled.
    let staging_guard = if let Some(witness) = refresh_witness {
        let (run, pass) = context
            .selection_wait
            .ok_or_else(selected_refresh_pending)?;
        let mut waiter_hold = HoldSelectionOnDrop::new(Arc::clone(run));
        let owner = Arc::clone(run);
        let pass = pass.to_owned();
        let original = task.clone();
        let (guard, admitted) = hold_guard_through(staging_guard, async move {
            let mut hold = HoldSelectionOnDrop::new(Arc::clone(&owner));
            let result = owner
                .begin_refreshed_transfer(&pass, &original, witness)
                .await;
            if result.is_ok() {
                hold.disarm();
            }
            result
        })
        .await
        .map_err(|error| SelectionTransferWaitInvalid(error.into()))?;
        waiter_hold.disarm();
        match admitted {
            Ok(Some(ticket)) => {
                #[cfg(test)]
                real_410_abort_hook::wait_refresh_ticket(&task.download_path).await;
                refresh_ticket = Some(ticket);
                guard
            }
            Ok(None) => return Err(SelectionTransferWaiting.into()),
            Err(error) if item_only_hold(&error) => return Err(selected_refresh_pending()),
            Err(error) => return Err(SelectionTransferWaitInvalid(error).into()),
        }
    } else if let Some((run, pass)) = context.selection_wait {
        let mut waiter_hold = HoldSelectionOnDrop {
            run: Arc::clone(run),
            armed: true,
        };
        let run = Arc::clone(run);
        let pass = pass.to_owned();
        let task = task.clone();
        let (guard, result) = hold_guard_through(staging_guard, async move {
            let mut hold = HoldSelectionOnDrop {
                run: Arc::clone(&run),
                armed: true,
            };
            let result = run.begin_transfer(&pass, &task).await;
            if result.as_ref().map_or_else(item_only_hold, |_| true) {
                hold.armed = false
            }
            result
        })
        .await
        .map_err(|error| SelectionTransferWaitInvalid(error.into()))?;
        waiter_hold.armed = false;
        match result {
            Ok(true) => guard,
            Ok(false) => return Err(SelectionTransferWaiting.into()),
            Err(error) if item_only_hold(&error) => return Err(SelectionTransferWaiting.into()),
            Err(error) => return Err(SelectionTransferWaitInvalid(error).into()),
        }
    } else {
        staging_guard
    };
    // A shutdown arriving during the owned state check must not start a GET.
    if context.shutdown_token.is_cancelled() {
        return Err(DownloadError::Interrupted {
            path: task.download_path.display().to_string().into(),
            bytes_written: 0,
        }
        .into());
    }
    if let Some(db) = context.state_db {
        db.guard_primary_writer(&task.download_path).await?;
        db.claim_temp_file(&owned_part_path)
            .await
            .context("Could not record temporary-file ownership")?;
    }

    let result: Result<DownloadSingleResult> = async {
        let mut downloaded = Box::pin(crate::download::file::download_file_with_mode(
            client,
            refresh_url.as_deref().unwrap_or(&task.url),
            &task.download_path,
            &task.checksum,
            retry_config,
            context.temp_suffix,
            crate::download::file::DownloadOpts {
                skip_rename: true,
                expected_size: if task.size > 0 { Some(task.size) } else { None },
                publication: task.publication(),
            },
            crate::download::file::DownloadLimits {
                rate_limit_counter: context.rate_limit_counter,
                bandwidth_limiter: context.bandwidth_limiter,
                shutdown_token: Some(context.shutdown_token),
            },
            context.mode,
        ))
        .await?;

        // When embed writes are needed, modifications happen on the .part file before
        // the atomic rename, preventing silent corruption on power loss / SIGKILL.
        let part_path = needs_embed.then_some(owned_part_path.as_path());

        // Compute SHA-256 of the downloaded content before EXIF modification
        // so we store a hash that reflects the original download bytes.
        let downloaded_fingerprint = Some(downloaded.fingerprint().await?);
        let download_checksum = downloaded_fingerprint.map(|fingerprint| data_encoding::HEXLOWER.encode(&fingerprint.sha256));

        let mut exif_ok = true;
        if let Some(part) = &part_path {
            let outcome = metadata_rewrite::write_download_metadata(
                metadata_rewrite::MetadataWriteRequest {
                sidecar_namespace: None,
                require_created_entry: task.require_created_entry,
                    final_path: &task.download_path,
                    embed_path: Some(part),
                    expected_embed_fingerprint: downloaded_fingerprint,
                    source_checksum: download_checksum.as_deref(),
                    sidecar_path: None,
                    payload: Arc::clone(&task.metadata),
                    created_local: task.created_local,
                    flags: metadata_flags,
                    capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
                    temp_suffix: context.temp_suffix,
                },
            )
            .await;
            anyhow::ensure!(!outcome.embed_input_changed, "Temporary download changed before metadata write");
            if let Some(expected) = outcome.embed_output_fingerprint {
                downloaded.accept_metadata_output(expected).await?;
            }
            exif_ok = !outcome.any_failed();
        }

        // Set mtime on .part (before rename) or final path directly.
        // rename() preserves mtime so this works in both cases.
        if let Err(e) = downloaded.set_times(download_file_times(&task.download_path, task.created_local.timestamp())).await {
            tracing::warn!(target: "kei::download::pipeline",
                path = %task.download_path.display(), error = %e, "Could not set mtime");
        }

        // The existing temporary owner retains provenance before publication.
        // A crash after rename can then finish the receipt without another GET
        // or any inference from filename/size.
        if let Some(db) = context.state_db {
            let fingerprint = downloaded.fingerprint().await?;
            let proof = crate::state::db::PreparedPublication {
                binding: task.publication_binding.as_deref().cloned(),
                library: task.library.to_string(),
                id: task.asset_id.to_string(),
                version: task.version_size.as_str().to_owned(),
                provider_checksum: task.checksum.to_string(),
                provider_size: task.size,
                temporary: crate::state::db::provider_selection::SelectionPath::from_path(&owned_part_path),
                destination: crate::state::db::provider_selection::SelectionPath::from_path(&task.download_path),
                local_checksum: data_encoding::HEXLOWER.encode(&fingerprint.sha256),
                source_checksum: download_checksum.clone().ok_or_else(|| anyhow::anyhow!("Missing verified source fingerprint"))?,
            };
            db.prepare_temp_publication(&owned_part_path, &proof).await?;
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("publication-prepared");
        }
        downloaded.publish(&task.download_path, task.publication()).await?;

        // Embed work already captured the original checksum. Otherwise take
        // a baseline for the unchanged download before sidecar planning.
        let unmodified_checksum = if download_checksum.is_none()
            && metadata_flags.contains(metadata_rewrite::MetadataFlags::XMP_SIDECAR)
        {
            Some(data_encoding::HEXLOWER.encode(&downloaded.fingerprint().await?.sha256))
        } else {
            None
        };

        let outcome =
            metadata_rewrite::write_download_metadata(metadata_rewrite::MetadataWriteRequest {
                sidecar_namespace: Some(downloaded.metadata_namespace()?),
                require_created_entry: task.require_created_entry,
                final_path: &task.download_path,
                embed_path: None,
                expected_embed_fingerprint: None,
                source_checksum: download_checksum.as_deref().or(unmodified_checksum.as_deref()),
                sidecar_path: Some(&task.download_path),
                payload: Arc::clone(&task.metadata),
                created_local: task.created_local,
                flags: metadata_flags,
                capture_timestamp_repair: crate::download::CaptureTimestampRepair::Preserve,
                temp_suffix: context.temp_suffix,
            })
            .await;
        exif_ok &= !outcome.any_failed();

        let disk_bytes = match tokio::fs::metadata(&task.download_path).await {
            Ok(meta) => meta.len(),
            Err(e) => {
                tracing::warn!(target: "kei::download::pipeline", path = %task.download_path.display(), error = %e, "Could not stat downloaded file for size tracking");
                0
            }
        };

        tracing::debug!(target: "kei::download::pipeline", path = %task.download_path.display(), "Downloaded");

        // Recheck after sidecar work before finalization, preserving the
        // existing final-file checksum boundary.
        let local_checksum = data_encoding::HEXLOWER.encode(&downloaded.fingerprint().await?.sha256);

        // Note: Apple's `fileChecksum` is an MMCS (MobileMe Chunked Storage)
        // compound signature, not a SHA-1/SHA-256 content hash. It cannot be
        // compared against a hash of the downloaded bytes.  Content integrity
        // is verified by size matching (Content-Length + API size field) and
        // magic-byte validation during download instead.

        // Retain original-byte evidence even when no embedding was requested.
        // Future sidecar-only retries must not infer provenance from a local
        // checksum that may have been refreshed after an embedded rewrite.
        let download_checksum = Some(download_checksum.or(unmodified_checksum).unwrap_or_else(|| local_checksum.clone()));
        let retained = match &task.pending_cross_parent_root {
            Some(root) => Some(downloaded.retain_pending_publication(root.as_path(), &task.download_path).await?),
            None => None,
        };
        if let Some(proof) = &retained {
            anyhow::ensure!(data_encoding::HEXLOWER.encode(&proof.fingerprint.sha256) == local_checksum,
                "Reserved pending publication changed after checksum capture");
        }
        Ok((
            exif_ok,
            local_checksum,
            download_checksum,
            downloaded.bytes_written,
            disk_bytes,
            retained,
        ))
    }
    .await;

    let (staging_guard, result) = if refresh_ticket.is_some()
        && result.as_ref().is_err_and(|error| {
            classify_download_task_error(error) == DownloadTaskErrorClass::ExpiredUrl
        }) {
        let ticket = refresh_ticket.take().ok_or_else(selected_refresh_pending)?;
        let (run, _) = context
            .selection_wait
            .ok_or_else(selected_refresh_pending)?;
        let mut waiter_hold = HoldSelectionOnDrop::new(Arc::clone(run));
        let owner = Arc::clone(run);
        let (guard, recorded) = hold_guard_through(staging_guard, async move {
            let mut hold = HoldSelectionOnDrop::new(Arc::clone(&owner));
            let recorded = owner.record_refreshed_410(ticket).await;
            if recorded.is_ok() {
                hold.disarm();
            }
            recorded
        })
        .await
        .map_err(|error| SelectionTransferWaitInvalid(error.into()))?;
        waiter_hold.disarm();
        (
            guard,
            match recorded {
                Ok(()) => result,
                Err(error) => Err(SelectionTransferWaitInvalid(error).into()),
            },
        )
    } else if refresh_ticket.is_some()
        && result.as_ref().is_err_and(|error| {
            matches!(
                error.downcast_ref::<DownloadError>(),
                Some(DownloadError::HttpStatus {
                    status: 429 | 500..=599,
                    ..
                })
            )
        })
    {
        let ticket = refresh_ticket.take().ok_or_else(selected_refresh_pending)?;
        let (run, _) = context
            .selection_wait
            .ok_or_else(selected_refresh_pending)?;
        let Err(error) = &result else {
            return Err(selected_refresh_pending());
        };
        let Some(DownloadError::HttpStatus { status, .. }) = error.downcast_ref::<DownloadError>()
        else {
            return Err(selected_refresh_pending());
        };
        let status = *status;
        let saved_diagnostic = error.to_string();
        let replace_truncated = matches!(
            task.publication(),
            crate::download::file::FinalPublication::ReplaceTruncated(_)
        );
        let diagnostic = saved_diagnostic.clone();
        let mut waiter_hold = HoldSelectionOnDrop::new(Arc::clone(run));
        let owner = Arc::clone(run);
        let (guard, recorded) = hold_guard_through(staging_guard, async move {
            let mut hold = HoldSelectionOnDrop::new(Arc::clone(&owner));
            let recorded = owner
                .record_refreshed_http_failure(ticket, status, diagnostic, replace_truncated)
                .await;
            if recorded.is_ok() {
                hold.disarm();
            }
            recorded
        })
        .await
        .map_err(|error| SelectionTransferWaitInvalid(error.into()))?;
        waiter_hold.disarm();
        (
            guard,
            match recorded {
                Ok(()) => Err(AccountedRefreshFailure(saved_diagnostic).into()),
                Err(error) => Err(SelectionTransferWaitInvalid(error).into()),
            },
        )
    } else if let (Some((run, pass)), Err(error)) = (context.selection_wait, &result)
        && refresh_url.is_none()
        && classify_download_task_error(error) == DownloadTaskErrorClass::ExpiredUrl
    {
        let mut waiter_hold = HoldSelectionOnDrop {
            run: Arc::clone(run),
            armed: true,
        };
        let run = Arc::clone(run);
        let pass = pass.to_owned();
        let task = task.clone();
        let (guard, recorded) = hold_guard_through(staging_guard, async move {
            let mut hold = HoldSelectionOnDrop {
                run: Arc::clone(&run),
                armed: true,
            };
            #[cfg(test)]
            real_410_abort_hook::wait(&task.download_path).await;
            let recorded = run.record_expired_transfer(&pass, &task).await;
            if recorded.is_ok() {
                hold.armed = false
            }
            recorded
        })
        .await
        .map_err(|error| SelectionTransferWaitInvalid(error.into()))?;
        waiter_hold.armed = false;
        let result = match recorded {
            Ok(()) => result,
            Err(error) => Err(SelectionTransferWaitInvalid(error).into()),
        };
        (guard, result)
    } else {
        (staging_guard, result)
    };

    if let Some(db) = context.state_db
        && let Err(error) = db
            .retire_temp_files(std::slice::from_ref(&owned_part_path))
            .await
    {
        if result.is_ok() {
            return Err(error).context("Could not retire temporary-file ownership");
        }
        tracing::warn!(target: "kei::download::pipeline",
            path = %owned_part_path.display(),
            error = %error,
            "Could not retire temporary-file ownership after download stopped"
        );
    }

    // A selected success must keep this same guard until the exact receipt is
    // committed and revalidated. Other outcomes leave their durable dispatch
    // reservation unresolved unless an actual 410 was recorded above.
    match (result, refresh_ticket, context.selection_wait) {
        (Ok(result), Some(ticket), Some((run, _))) => Ok((
            result,
            Some(RefreshedCompletion {
                guard: staging_guard,
                ticket,
                run: Arc::clone(run),
                hold: HoldSelectionOnDrop::new(Arc::clone(run)),
            }),
        )),
        (Ok(result), None, _) => {
            drop(staging_guard);
            Ok((result, None))
        }
        (Ok(_), Some(_), None) => Err(selected_refresh_pending()),
        (Err(error), _, _) => Err(error),
    }
}

/// Set the modification and access times of a file to the given Unix
/// timestamp. Uses `std::fs::File::set_times` (stable since Rust 1.75).
///
/// Handles negative timestamps (dates before 1970) gracefully by clamping
/// to the Unix epoch.
#[cfg(test)]
fn set_file_mtime(path: &Path, timestamp: i64) -> std::io::Result<()> {
    let file = std::fs::File::options().write(true).open(path)?;
    file.set_times(download_file_times(path, timestamp))
}

fn download_file_times(path: &Path, timestamp: i64) -> FileTimes {
    let time = if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp.unsigned_abs())
    } else {
        tracing::warn!(target: "kei::download::pipeline",
            path = %path.display(),
            timestamp,
            "Negative timestamp (pre-1970 date), clamping mtime to epoch"
        );
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(timestamp.unsigned_abs()))
            .unwrap_or(SystemTime::UNIX_EPOCH)
    };
    FileTimes::new().set_modified(time).set_accessed(time)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod staging_tests;

/// Synthetic path-scoped pause after real 410, before witness lookup. The
/// hook cannot affect production builds or a different test destination.
#[cfg(test)]
pub(crate) mod real_410_abort_hook {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Mutex, OnceLock},
    };
    use tokio::sync::oneshot;
    struct Hook {
        entered: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }
    fn hooks() -> &'static Mutex<HashMap<PathBuf, Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Hook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    pub(crate) fn install(path: PathBuf) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, entered_rx) = oneshot::channel();
        let (release_tx, release) = oneshot::channel();
        assert!(
            hooks()
                .lock()
                .unwrap()
                .insert(path, Hook { entered, release })
                .is_none()
        );
        (entered_rx, release_tx)
    }
    pub(super) async fn wait(path: &Path) {
        let hook = hooks().lock().unwrap().remove(path);
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            let _ = hook.release.await;
        }
    }
    fn refresh_ticket_hooks() -> &'static Mutex<HashMap<PathBuf, Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Hook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    pub(crate) fn install_refresh_ticket(
        path: PathBuf,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, entered_rx) = oneshot::channel();
        let (release_tx, release) = oneshot::channel();
        assert!(
            refresh_ticket_hooks()
                .lock()
                .unwrap()
                .insert(path, Hook { entered, release })
                .is_none()
        );
        (entered_rx, release_tx)
    }
    pub(super) async fn wait_refresh_ticket(path: &Path) {
        let hook = refresh_ticket_hooks().lock().unwrap().remove(path);
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            let _ = hook.release.await;
        }
    }
    struct FinalizedHook {
        entered: oneshot::Sender<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        release: oneshot::Receiver<()>,
        abandon: bool,
    }
    fn finalized_hooks() -> &'static Mutex<HashMap<PathBuf, FinalizedHook>> {
        static HOOKS: OnceLock<Mutex<HashMap<PathBuf, FinalizedHook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }
    pub(crate) fn install_refreshed_finalized(
        path: PathBuf,
    ) -> (
        oneshot::Receiver<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        oneshot::Sender<()>,
    ) {
        install_finalized(path, false)
    }
    pub(crate) fn install_refreshed_abandon(
        path: PathBuf,
    ) -> (
        oneshot::Receiver<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        oneshot::Sender<()>,
    ) {
        install_finalized(path, true)
    }
    fn install_finalized(
        path: PathBuf,
        abandon: bool,
    ) -> (
        oneshot::Receiver<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        oneshot::Sender<()>,
    ) {
        let (entered, entered_rx) = oneshot::channel();
        let (release_tx, release) = oneshot::channel();
        assert!(
            finalized_hooks()
                .lock()
                .unwrap()
                .insert(
                    path,
                    FinalizedHook {
                        entered,
                        release,
                        abandon
                    }
                )
                .is_none()
        );
        (entered_rx, release_tx)
    }
    pub(crate) async fn wait_refreshed_finalized(
        path: &Path,
        held: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> bool {
        let hook = finalized_hooks().lock().unwrap().remove(path);
        if let Some(hook) = hook {
            let _ = hook.entered.send(held);
            let _ = hook.release.await;
            return hook.abandon;
        }
        false
    }
    pub(crate) async fn can_lock(path: &Path, timeout: std::time::Duration) -> bool {
        let token = tokio_util::sync::CancellationToken::new();
        matches!(
            tokio::time::timeout(
                timeout,
                crate::download::file::lock_download_destination(path, &token)
            )
            .await,
            Ok(Ok(_))
        )
    }
}
