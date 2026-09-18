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
pub(super) struct DownloadSingleContext<'a> {
    pub(super) temp_suffix: &'a str,
    pub(super) state_db: Option<&'a dyn DownloadStore>,
    pub(super) rate_limit_counter: Option<&'a std::sync::atomic::AtomicUsize>,
    pub(super) bandwidth_limiter: Option<&'a crate::download::BandwidthLimiter>,
    pub(super) shutdown_token: &'a CancellationToken,
    pub(super) mode: crate::personality::Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DownloadTaskErrorClass {
    Interrupted,
    SessionExpired,
    ExpiredUrl,
    Other,
}

/// Classify per-task download errors at the worker orchestration boundary.
///
/// The stream and cleanup workers share the same behavioral split: interrupted
/// downloads are drain-only, session expiry contributes to the reauth abort
/// threshold, expired CDN URLs abort the current URL batch, and ordinary
/// failures are recorded on the task. The original error is still propagated
/// to state/logging unchanged.
pub(super) fn classify_download_task_error(error: &anyhow::Error) -> DownloadTaskErrorClass {
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

pub(super) async fn download_single_task<C: crate::download::file::DownloadClient>(
    client: &C,
    task: &DownloadTask,
    retry_config: &RetryConfig,
    metadata_flags: MetadataFlags,
    context: DownloadSingleContext<'_>,
) -> Result<(bool, String, Option<String>, u64, u64)> {
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
    if let Some(db) = context.state_db {
        db.claim_temp_file(&owned_part_path)
            .await
            .context("Could not record temporary-file ownership")?;
    }

    let result: Result<(bool, String, Option<String>, u64, u64)> = async {
        let bytes_downloaded = Box::pin(crate::download::file::download_file_with_mode(
            client,
            &task.url,
            &task.download_path,
            &task.checksum,
            retry_config,
            context.temp_suffix,
            crate::download::file::DownloadOpts {
                skip_rename: needs_embed,
                expected_size: if task.size > 0 { Some(task.size) } else { None },
                publication: task.publication,
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
        let download_checksum = if let Some(path) = &part_path {
            Some(crate::download::file::compute_sha256(path).await?)
        } else {
            None
        };

        let mut exif_ok = true;
        if let Some(part) = &part_path {
            let outcome = metadata_rewrite::write_download_metadata(
                metadata_rewrite::MetadataWriteRequest {
                    final_path: &task.download_path,
                    embed_path: Some(part),
                    expected_embed_fingerprint: None,
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
            exif_ok = !outcome.any_failed();
        }

        // Set mtime on .part (before rename) or final path directly.
        // rename() preserves mtime so this works in both cases.
        let mtime_target = part_path.unwrap_or(&task.download_path).to_path_buf();
        let ts = task.created_local.timestamp();
        if let Err(e) = tokio::task::spawn_blocking(move || set_file_mtime(&mtime_target, ts)).await?
        {
            tracing::warn!(target: "kei::download::pipeline",
                path = %task.download_path.display(),
                error = %e,
                "Could not set mtime"
            );
        }

        // Atomic rename: .part → final (only when EXIF path was used)
        if let Some(part) = &part_path {
            crate::download::file::publish_part_to_final(part, &task.download_path, task.publication).await?;
        }

        // Embed work already captured the original checksum. Otherwise take
        // a baseline for the unchanged download before sidecar planning.
        let unmodified_checksum = if download_checksum.is_none()
            && metadata_flags.contains(metadata_rewrite::MetadataFlags::XMP_SIDECAR)
        {
            Some(crate::download::file::compute_sha256(&task.download_path).await?)
        } else {
            None
        };

        let outcome =
            metadata_rewrite::write_download_metadata(metadata_rewrite::MetadataWriteRequest {
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
        let local_checksum = crate::download::file::compute_sha256(&task.download_path).await?;

        // Note: Apple's `fileChecksum` is an MMCS (MobileMe Chunked Storage)
        // compound signature, not a SHA-1/SHA-256 content hash. It cannot be
        // compared against a hash of the downloaded bytes.  Content integrity
        // is verified by size matching (Content-Length + API size field) and
        // magic-byte validation during download instead.

        // Retain original-byte evidence even when no embedding was requested.
        // Future sidecar-only retries must not infer provenance from a local
        // checksum that may have been refreshed after an embedded rewrite.
        let download_checksum = Some(download_checksum.or(unmodified_checksum).unwrap_or_else(|| local_checksum.clone()));
        Ok((
            exif_ok,
            local_checksum,
            download_checksum,
            bytes_downloaded,
            disk_bytes,
        ))
    }
    .await;

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

    result
}

/// Set the modification and access times of a file to the given Unix
/// timestamp. Uses `std::fs::File::set_times` (stable since Rust 1.75).
///
/// Handles negative timestamps (dates before 1970) gracefully by clamping
/// to the Unix epoch.
fn set_file_mtime(path: &Path, timestamp: i64) -> std::io::Result<()> {
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
    let times = FileTimes::new().set_modified(time).set_accessed(time);
    let file = std::fs::File::options().write(true).open(path)?;
    file.set_times(times)?;
    Ok(())
}

#[cfg(test)]
mod tests;
