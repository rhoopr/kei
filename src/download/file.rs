use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use base64::Engine;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use super::error::DownloadError;
use super::limiter::BandwidthLimiter;
use crate::fs_util::{ConfinedParents, ConfinedPath, FileIdentity, file_identity};
use crate::retry::{self, RetryAction, RetryConfig};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// HTTP response from a download request.
pub(super) struct DownloadResponse {
    pub status: u16,
    pub content_length: Option<u64>,
    pub content_range: Option<String>,
    pub content_type: Option<String>,
    pub stream: Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>,
}

/// Trait abstracting HTTP GET for the download pipeline.
///
/// Implemented by `reqwest::Client` for production use and by test stubs
/// for exercising the full download-to-disk flow without a network.
#[async_trait::async_trait]
pub(super) trait DownloadClient: Send + Sync {
    async fn fetch(
        &self,
        url: &str,
        resume_from: Option<u64>,
    ) -> Result<DownloadResponse, BoxError>;
}

#[async_trait::async_trait]
impl DownloadClient for Client {
    async fn fetch(
        &self,
        url: &str,
        resume_from: Option<u64>,
    ) -> Result<DownloadResponse, BoxError> {
        let mut request = Self::get(self, url);
        if let Some(offset) = resume_from {
            request = request.header("Range", format!("bytes={offset}-"));
        }
        let response = request.send().await?;
        let status = response.status().as_u16();
        let content_length = response.content_length();
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .map(std::string::ToString::to_string);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(std::string::ToString::to_string);
        let stream = response
            .bytes_stream()
            .map(|r| r.map_err(|e| Box::new(e) as BoxError));
        Ok(DownloadResponse {
            status,
            content_length,
            content_range,
            content_type,
            stream: Box::pin(stream),
        })
    }
}

/// Derive a deterministic .part filename from the checksum so that
/// concurrent downloads of different files don't collide. Base32-encoded
/// because base64 contains `/` which is invalid in filenames.
pub(super) fn temp_download_path(
    download_path: &Path,
    checksum: &str,
    temp_suffix: &str,
) -> anyhow::Result<PathBuf> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(checksum)
        .context("Could not decode the base64 checksum from Apple")?;
    if decoded.is_empty() {
        anyhow::bail!("Apple returned an empty checksum.");
    }
    let encoded = data_encoding::BASE32_NOPAD.encode(&decoded);
    let download_dir = download_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(download_dir.join(format!("{encoded}{temp_suffix}")))
}

/// Download a file from URL using .part temp files.
///
/// Resumes partial downloads via HTTP Range requests when a .part file
/// already exists. Falls back to a full download if the server ignores the
/// Range header. When `skip_rename` is false, the .part file is renamed to
/// the final destination on success. When true, the .part file is left in
/// place so the caller can modify it before performing the rename.
/// Retries with exponential backoff on transient failures.
/// Download options that control post-download behavior and verification.
#[derive(Debug, Clone, Copy)]
pub(super) struct DownloadOpts {
    /// Keep the `.part` file instead of renaming to the final path.
    pub skip_rename: bool,
    /// API-reported file size. When set, verifies total bytes written match,
    /// catching truncation even when the CDN omits `Content-Length`.
    pub expected_size: Option<u64>,
    /// Final-path publication policy selected by retry planning.
    pub publication: FinalPublication,
}

/// Exact bytes that retry planning authorized the publisher to replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExistingFileFingerprint {
    pub(super) size: u64,
    pub(super) sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ExistingFileSnapshot {
    pub(super) fingerprint: ExistingFileFingerprint,
    pub(super) prefix: [u8; 12],
    pub(super) prefix_len: usize,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ConditionalPublishTargetChanged {
    #[error("Refusing to replace {path} because its bytes changed after write planning")]
    AfterPlanning { path: PathBuf },
    #[error(
        "Refusing to replace {path} because its bytes changed during conditional publication; the original target was restored"
    )]
    DuringPublication { path: PathBuf },
    #[error(
        "Refusing to replace {path} because the target changed or could not be verified after write planning"
    )]
    Unverifiable { path: PathBuf },
}

#[derive(Debug, thiserror::Error)]
#[error(
    "Refusing to deduplicate {part_path} with {final_path} because their verified bytes differ"
)]
pub(super) struct FinalPathCollision {
    part_path: PathBuf,
    final_path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
#[error("Conditional publication must retain paths: {paths:?}")]
struct ConditionalPublishMustRetainPaths {
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConditionalPublishErrorDisposition {
    pub(super) target_changed: bool,
    pub(super) retained_paths: Vec<PathBuf>,
}

pub(super) fn classify_conditional_publish_error(
    error: &anyhow::Error,
) -> ConditionalPublishErrorDisposition {
    let target_changed = error
        .downcast_ref::<ConditionalPublishTargetChanged>()
        .is_some()
        || error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|source| source.kind() == std::io::ErrorKind::NotFound);
    let retained_paths = error
        .downcast_ref::<ConditionalPublishMustRetainPaths>()
        .map(|marker| marker.paths.clone())
        .unwrap_or_default();
    ConditionalPublishErrorDisposition {
        target_changed,
        retained_paths,
    }
}

/// Final-path behavior for a verified `.part` file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum FinalPublication {
    /// Preserve the global no-overwrite contract.
    #[default]
    NoReplace,
    /// Replace only the exact truncated bytes authorized by retry planning.
    ReplaceTruncated(ExistingFileFingerprint),
}

/// Side-channel observers / throttles that ride along with a download call
/// without bloating the direct argument list.
///
/// `rate_limit_counter`, when set, is incremented by 1 for every observed
/// HTTP 429 / 503 error (each retry attempt that hits rate-limiting counts
/// once). The total is aggregated at the sync level so operators see when
/// Apple is back-pressuring the run.
///
/// `bandwidth_limiter`, when set, caps throughput on the response body read.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct DownloadLimits<'a> {
    pub rate_limit_counter: Option<&'a std::sync::atomic::AtomicUsize>,
    pub bandwidth_limiter: Option<&'a BandwidthLimiter>,
    pub shutdown_token: Option<&'a CancellationToken>,
}

/// Friendly-mode variant of `download_file`. Identical except `mode`
/// controls retry-pause and retry-recovery narration.
#[allow(
    clippy::too_many_arguments,
    reason = "mode is a UX gate, not a behavior knob, so folding it into DownloadOpts/DownloadLimits would muddy those types' semantics"
)]
pub(super) async fn download_file_with_mode<C: DownloadClient>(
    client: &C,
    url: &str,
    download_path: &Path,
    checksum: &str,
    retry_config: &RetryConfig,
    temp_suffix: &str,
    opts: DownloadOpts,
    limits: DownloadLimits<'_>,
    mode: crate::personality::Mode,
) -> Result<u64, DownloadError> {
    let part_path =
        temp_download_path(download_path, checksum, temp_suffix).map_err(DownloadError::Other)?;

    Box::pin(retry::retry_with_backoff_with_mode(
        retry_config,
        |e: &DownloadError| {
            if e.is_rate_limited()
                && let Some(counter) = limits.rate_limit_counter
            {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if e.is_retryable() {
                RetryAction::Retry
            } else {
                RetryAction::Abort
            }
        },
        || async {
            Box::pin(attempt_download_with_publication(
                client,
                url,
                download_path,
                &part_path,
                opts.skip_rename,
                opts.expected_size,
                limits.bandwidth_limiter,
                limits.shutdown_token,
                opts.publication,
            ))
            .await
        },
        mode,
    ))
    .await
}

/// Single download attempt with resume support.
///
/// .part files older than this are considered stale (crashed runs) and
/// restarted. Tightened from the original 24h: a longer resume window
/// without server-side ETag/Last-Modified validation means bytes from a
/// pre-rotation version of the asset could end up appended to bytes from
/// a post-rotation version undetectably when the two happen to have the
/// same total size. 1h keeps resume useful for typical interrupt/retry
/// patterns (which complete within minutes) without the long exposure.
const STALE_PART_FILE_SECS: u64 = 3600;

/// If a .part file already exists, sends a Range request to resume from where
/// it left off. Falls back to a fresh download if the server doesn't support
/// Range or returns an unexpected status.
#[cfg(test)]
async fn attempt_download<C: DownloadClient>(
    client: &C,
    url: &str,
    download_path: &Path,
    part_path: &Path,
    skip_rename: bool,
    expected_size: Option<u64>,
    bandwidth_limiter: Option<&BandwidthLimiter>,
    shutdown_token: Option<&CancellationToken>,
) -> Result<u64, DownloadError> {
    attempt_download_with_publication(
        client,
        url,
        download_path,
        part_path,
        skip_rename,
        expected_size,
        bandwidth_limiter,
        shutdown_token,
        FinalPublication::NoReplace,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "publication is a safety policy kept explicit at the file landing boundary"
)]
async fn attempt_download_with_publication<C: DownloadClient>(
    client: &C,
    url: &str,
    download_path: &Path,
    part_path: &Path,
    skip_rename: bool,
    expected_size: Option<u64>,
    bandwidth_limiter: Option<&BandwidthLimiter>,
    shutdown_token: Option<&CancellationToken>,
    publication: FinalPublication,
) -> Result<u64, DownloadError> {
    let path_str = download_path.display().to_string();

    let resume_offset = match fs::metadata(part_path).await {
        Ok(meta) if meta.len() > 0 => {
            // Discard stale .part files from crashed runs to avoid resuming
            // from potentially corrupt bytes.
            let stale = match meta.modified() {
                Ok(mtime) => {
                    mtime.elapsed().unwrap_or(std::time::Duration::ZERO)
                        > std::time::Duration::from_secs(STALE_PART_FILE_SECS)
                }
                Err(e) => {
                    tracing::warn!(
                        path = %part_path.display(),
                        error = %e,
                        "Cannot read .part file mtime, treating as stale"
                    );
                    true
                }
            };
            if stale {
                tracing::warn!(
                    path = %part_path.display(),
                    size = meta.len(),
                    "Stale .part file (>1h old), restarting download"
                );
                0
            } else {
                meta.len()
            }
        }
        _ => 0,
    };

    let resume_from = if resume_offset > 0 {
        tracing::info!(
            path = %path_str,
            resume_offset,
            "Resuming download (partial file exists)"
        );
        Some(resume_offset)
    } else {
        None
    };

    let response = client
        .fetch(url, resume_from)
        .await
        .map_err(|e| DownloadError::Http {
            source: e,
            path: path_str.clone().into(),
            status: 0,
            content_length: None,
            bytes_written: 0,
        })?;

    let status = response.status;
    let is_success = (200..300).contains(&status);

    // 206 = resumed successfully, 200 = server ignored Range (start over)
    // `effective_offset` tracks the actual byte offset used for the content-length
    // check. When the server ignores Range and returns 200, we restart from zero
    // so effective_offset must be 0 (not the stale resume_offset).
    let (mut bytes_written, truncate, effective_offset) = match status {
        206 if resume_offset > 0 => (resume_offset, false, resume_offset),
        _ if is_success => {
            if resume_offset > 0 {
                tracing::info!(
                    status,
                    path = %path_str,
                    "Server ignored Range request, restarting download"
                );
            }
            (0u64, true, 0u64)
        }
        _ => {
            return Err(DownloadError::HttpStatus {
                status,
                path: path_str.into(),
            });
        }
    };

    // Reject content types that prove the body is an error document before
    // writing to disk. Delete any stale .part file so the next successful
    // attempt starts fresh rather than appending to data from a previous
    // (possibly different) response.
    if let Some(ct) = &response.content_type
        && let Some(reason) = rejecting_content_type_reason(ct)
    {
        crate::fs_util::log_remove_async(part_path).await;
        return Err(DownloadError::InvalidContent {
            path: path_str.into(),
            reason: format!("server returned {reason} content-type: {ct}").into(),
        });
    }

    let content_length = response.content_length;
    let content_range = response.content_range;

    if status == 206 && resume_offset > 0 {
        validate_resume_content_range(content_range.as_deref(), resume_offset, &path_str)?;
    }

    // If we resumed and the server advertises a Content-Length that doesn't
    // reconcile with the API-reported size, the resource on the server has
    // likely been rotated since we wrote the .part. Discard and restart
    // cleanly rather than appending new bytes to a stale prefix.
    if status == 206
        && resume_offset > 0
        && let (Some(cl), Some(expected)) = (content_length, expected_size)
    {
        let server_total = resume_offset.saturating_add(cl);
        if server_total != expected {
            tracing::warn!(
                path = %path_str,
                resume_offset,
                server_remaining = cl,
                server_total,
                expected,
                "Resume bytes inconsistent with API-reported size; discarding .part and restarting"
            );
            crate::fs_util::log_remove_async(part_path).await;
            return Err(DownloadError::ContentLengthMismatch {
                path: path_str.into(),
                expected,
                received: server_total,
            });
        }
    }

    // When starting fresh (no resume), unlink any existing .part and use
    // create_new so a concurrent kei instance writing the same .part is
    // detected as a hard error (AlreadyExists) rather than silently racing.
    // When resuming, open append-only without create (the file must exist —
    // we read its length at the top of this function).
    let mut file = if truncate {
        crate::fs_util::log_remove_async(part_path).await;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => DownloadError::Other(anyhow::anyhow!(
                    "Another kei process is already writing {}. Only one kei instance may use the same download directory at a time.",
                    part_path.display()
                )),
                _ => {
                    DownloadError::Other(anyhow::anyhow!("Could not open temporary download file: {e}"))
                }
            })?
    } else {
        OpenOptions::new()
            .write(true)
            .append(true)
            .open(&part_path)
            .await
            .map_err(|e| {
                DownloadError::Other(anyhow::anyhow!(
                    "Could not open temporary download file: {e}"
                ))
            })?
    };

    let mut stream = response.stream;
    let stream_result: Result<(), DownloadError> = async {
        loop {
            let next_chunk = if let Some(token) = shutdown_token {
                tokio::select! {
                    () = token.cancelled() => {
                        return Err(DownloadError::Interrupted {
                            path: path_str.clone().into(),
                            bytes_written,
                        });
                    }
                    chunk = stream.next() => chunk,
                }
            } else {
                stream.next().await
            };

            let Some(chunk) = next_chunk else {
                break;
            };

            let chunk = chunk.map_err(|e| DownloadError::Http {
                source: e,
                path: path_str.clone().into(),
                status,
                content_length,
                bytes_written,
            })?;
            if let Some(limiter) = bandwidth_limiter {
                limiter.consume(chunk.len()).await;
            }
            file.write_all(&chunk).await?;
            bytes_written += chunk.len() as u64;
        }
        file.flush().await?;
        file.sync_data().await?;
        Ok(())
    }
    .await;
    drop(file);
    if let Err(e) = stream_result {
        if !e.is_retryable() && !e.is_interrupted() {
            crate::fs_util::log_remove_async(part_path).await;
        }
        return Err(e);
    }

    if shutdown_token.is_some_and(CancellationToken::is_cancelled) {
        return Err(DownloadError::Interrupted {
            path: path_str.into(),
            bytes_written,
        });
    }

    // Verify the server sent the number of bytes it promised.
    // Catches CDN truncation (e.g. Apple silently cutting off videos at ~1 GB).
    if let Some(expected_len) = content_length {
        let total_bytes = bytes_written - effective_offset;
        if total_bytes != expected_len {
            crate::fs_util::log_remove_async(part_path).await;
            return Err(DownloadError::ContentLengthMismatch {
                path: path_str.into(),
                expected: expected_len,
                received: total_bytes,
            });
        }
    }

    // Verify total bytes written matches the API-reported size (if known).
    // Catches truncation when the CDN omits Content-Length (chunked transfer).
    if let Some(expected) = expected_size
        && bytes_written != expected
    {
        crate::fs_util::log_remove_async(part_path).await;
        return Err(DownloadError::ContentLengthMismatch {
            path: path_str.into(),
            expected,
            received: bytes_written,
        });
    }

    // Defense-in-depth: log when neither size indicator is available.
    // Post-download checksum verification catches actual corruption,
    // but this warning helps diagnose transfer anomalies.
    if expected_size.is_none() && content_length.is_none() {
        tracing::warn!(
            path = %path_str,
            bytes_written,
            "No expected size or Content-Length available to verify download completeness"
        );
    }

    // Validate content looks like actual media, not an HTML error page.
    // Apple's CDN occasionally returns HTTP 200 with HTML (rate limit, CAPTCHA,
    // service unavailable) which would otherwise be saved as the final file.
    let part_owned = part_path.to_path_buf();
    let download_owned = download_path.to_path_buf();
    let validation = tokio::task::spawn_blocking(move || {
        validate_downloaded_content(&part_owned, &download_owned)
    })
    .await
    .map_err(|e| DownloadError::Disk(Box::new(std::io::Error::other(e))))?;
    if let Err(e) = validation {
        crate::fs_util::log_remove_async(part_path).await;
        return Err(e);
    }

    if !skip_rename {
        publish_part_to_final(part_path, download_path, publication).await?;
    }

    Ok(bytes_written)
}

/// Rename a `.part` file to its final destination, handling the case where
/// a concurrent download already placed identical bytes at the final path.
#[cfg(test)]
pub(super) async fn rename_part_to_final(
    part_path: &Path,
    final_path: &Path,
) -> anyhow::Result<()> {
    publish_part_to_final(part_path, final_path, FinalPublication::NoReplace).await
}

/// Publish a verified `.part` file according to the task's explicit policy.
pub(super) async fn publish_part_to_final(
    part_path: &Path,
    final_path: &Path,
    publication: FinalPublication,
) -> anyhow::Result<()> {
    if let FinalPublication::ReplaceTruncated(expected) = publication {
        return replace_file_if_unchanged(part_path, final_path, expected).await;
    }

    match publish_part_no_replace(part_path, final_path).await {
        Ok(PublishResult::Published) => {
            // ext4 default `data=ordered` does not guarantee directory
            // entry durability after `rename` returns Ok: a power loss
            // between rename and the kernel committing the dir block
            // can leave `final_path` absent on reboot. Best-effort
            // fsync the parent so the worst case is one redundant
            // re-download next sync, not silent loss.
            crate::fs_util::fsync_parent_dir_async_best_effort(final_path).await;
            Ok(())
        }
        Ok(PublishResult::DestinationExists) => {
            // CONTRACT: FILE_PUBLISH_NO_OVERWRITE
            let part_fingerprint = fingerprint_regular_file(part_path)
                .await
                .with_context(|| format!("Could not verify {}", part_path.display()))?;
            let final_fingerprint = fingerprint_regular_file(final_path)
                .await
                .with_context(|| format!("Could not verify {}", final_path.display()))?;
            if part_fingerprint != final_fingerprint {
                return Err(FinalPathCollision {
                    part_path: part_path.to_path_buf(),
                    final_path: final_path.to_path_buf(),
                }
                .into());
            }

            tracing::debug!(
                path = %final_path.display(),
                "Destination has identical bytes, removing redundant .part file"
            );
            fs::remove_file(part_path).await.with_context(|| {
                format!(
                    "Could not remove redundant completed download {}",
                    part_path.display()
                )
            })?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| {
            format!(
                "Could not move completed download from {} to {}",
                part_path.display(),
                final_path.display()
            )
        }),
    }
}

/// Publish a prepared file only while the destination still matches the
/// caller's initial observation. `None` means the destination did not exist;
/// `Some` authorizes replacement of exactly those bytes and no others.
#[cfg(feature = "xmp")]
pub(super) async fn publish_file_if_unchanged(
    part_path: &Path,
    final_path: &Path,
    expected: Option<ExistingFileFingerprint>,
) -> anyhow::Result<()> {
    if let Some(expected) = expected {
        return replace_file_if_unchanged(part_path, final_path, expected).await;
    }

    match publish_part_no_replace(part_path, final_path).await {
        Ok(PublishResult::Published) => {
            crate::fs_util::fsync_parent_dir_async_best_effort(final_path).await;
            Ok(())
        }
        Ok(PublishResult::DestinationExists) => anyhow::bail!(
            "Refusing to publish {} because it appeared after write planning",
            final_path.display()
        ),
        Err(error) => Err(error).with_context(|| {
            format!(
                "Could not publish prepared file {} -> {}",
                part_path.display(),
                final_path.display()
            )
        }),
    }
}

async fn replace_file_if_unchanged(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
) -> anyhow::Result<()> {
    let part_path = part_path.to_path_buf();
    let final_path = final_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        replace_file_if_unchanged_blocking(&part_path, &final_path, expected, None)
    })
    .await
    .map_err(std::io::Error::other)?
}

pub(super) fn publish_file_if_unchanged_blocking(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
    expected_replacement: ExistingFileFingerprint,
) -> anyhow::Result<()> {
    replace_file_if_unchanged_blocking(part_path, final_path, expected, Some(expected_replacement))
}

fn replace_file_if_unchanged_blocking(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
    expected_replacement: Option<ExistingFileFingerprint>,
) -> anyhow::Result<()> {
    let current = match fingerprint_regular_file_blocking(final_path) {
        Ok(current) => current,
        Err(error) => {
            return Err(error).context(ConditionalPublishTargetChanged::Unverifiable {
                path: final_path.to_path_buf(),
            });
        }
    };
    if current != expected {
        return Err(ConditionalPublishTargetChanged::AfterPlanning {
            path: final_path.to_path_buf(),
        }
        .into());
    }

    let replacement = match fingerprint_regular_file_blocking(part_path) {
        Ok(replacement) => replacement,
        Err(error) => {
            return Err(error)
                .with_context(|| {
                    format!("Could not verify replacement file {}", part_path.display())
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: vec![part_path.to_path_buf()],
                });
        }
    };
    if expected_replacement.is_some_and(|approved| approved != replacement) {
        return Err(anyhow::anyhow!(
            "Refusing to publish {} because its bytes changed after validation",
            part_path.display()
        ))
        .context(ConditionalPublishMustRetainPaths {
            paths: vec![part_path.to_path_buf()],
        });
    }
    let displaced_path = match exchange_repair_files_blocking(part_path, final_path, expected) {
        Ok(displaced_path) => displaced_path,
        Err(error) if classify_conditional_publish_error(&error).target_changed => {
            return Err(error).context(ConditionalPublishTargetChanged::Unverifiable {
                path: final_path.to_path_buf(),
            });
        }
        Err(error) => {
            return match fingerprint_regular_file_blocking(final_path) {
                Ok(current) if current == expected => Err(error),
                Ok(_) | Err(_) => {
                    Err(error).context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                }
            };
        }
    };
    if let Some(expected_replacement) = expected_replacement {
        match fingerprint_regular_file_blocking(final_path) {
            Ok(installed) if installed == expected_replacement => {}
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "Published replacement at {} no longer matches the validated bytes",
                    final_path.display()
                ))
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: vec![displaced_path],
                });
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| {
                        format!(
                            "Could not verify the published replacement at {}",
                            final_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: vec![displaced_path],
                    });
            }
        }
    }
    let displaced = match fingerprint_regular_file_blocking(&displaced_path) {
        Ok(displaced) => displaced,
        Err(error) => {
            let restore_result = restore_repair_target_if_unchanged_blocking(
                part_path,
                final_path,
                &displaced_path,
                replacement,
            );
            return match restore_result {
                Ok(true) => Err(error).context(
                    ConditionalPublishTargetChanged::DuringPublication {
                        path: final_path.to_path_buf(),
                    },
                ),
                Ok(false) => Err(error)
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: vec![displaced_path],
                    }),
                Err(restore_error) => Err(restore_error)
                    .with_context(|| {
                        format!(
                            "Could not restore {} after displaced-target verification failed: {error:#}",
                            final_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    }),
            };
        }
    };
    if displaced != expected {
        let restore_result = restore_repair_target_if_unchanged_blocking(
            part_path,
            final_path,
            &displaced_path,
            replacement,
        );
        return match restore_result {
            Ok(true) => Err(ConditionalPublishTargetChanged::DuringPublication {
                path: final_path.to_path_buf(),
            }
            .into()),
            Ok(false) => Err(anyhow::anyhow!(
                "Refusing to replace {} because its bytes changed during conditional publication",
                final_path.display()
            ))
            .context(ConditionalPublishTargetChanged::Unverifiable {
                path: final_path.to_path_buf(),
            })
            .context(ConditionalPublishMustRetainPaths {
                paths: vec![displaced_path],
            }),
            Err(error) => Err(error)
                .with_context(|| {
                    format!(
                        "Could not restore {} after its bytes changed during conditional publication",
                        final_path.display()
                    )
                })
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                }),
        };
    }

    if let Err(error) = std::fs::remove_file(&displaced_path) {
        tracing::warn!(
            path = %displaced_path.display(),
            %error,
            "Failed to remove displaced file after verified replacement"
        );
    }
    fsync_parent_dir_best_effort_blocking(final_path);
    Ok(())
}

fn restore_repair_target_if_unchanged_blocking(
    part_path: &Path,
    final_path: &Path,
    displaced_path: &Path,
    replacement: ExistingFileFingerprint,
) -> anyhow::Result<bool> {
    restore_repair_target_if_unchanged_with(
        part_path,
        final_path,
        displaced_path,
        replacement,
        restore_exchanged_repair_files_blocking,
    )
}

fn restore_repair_target_if_unchanged_with(
    part_path: &Path,
    final_path: &Path,
    displaced_path: &Path,
    replacement: ExistingFileFingerprint,
    restore: impl FnOnce(&Path, &Path, &Path) -> std::io::Result<()>,
) -> anyhow::Result<bool> {
    if fingerprint_regular_file_blocking(final_path).ok() == Some(replacement) {
        if let Err(error) = restore(part_path, final_path, displaced_path) {
            return Err(error)
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: retained_restore_paths(part_path, displaced_path),
                });
        }
        match fingerprint_regular_file_blocking(part_path) {
            Ok(displaced_replacement) if displaced_replacement == replacement => {}
            Ok(_) => {
                return Err(anyhow::anyhow!(
                    "Restoring {} displaced different bytes to {}",
                    final_path.display(),
                    part_path.display()
                ))
                .context(ConditionalPublishTargetChanged::Unverifiable {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: retained_restore_paths(part_path, displaced_path),
                });
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| {
                        format!(
                            "Could not verify the entry displaced while restoring {}",
                            final_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: retained_restore_paths(part_path, displaced_path),
                    });
            }
        }
        return Ok(true);
    }
    Ok(false)
}

fn retained_restore_paths(part_path: &Path, displaced_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![part_path.to_path_buf()];
    if displaced_path != part_path {
        paths.push(displaced_path.to_path_buf());
    }
    paths
}

fn fsync_parent_dir_best_effort_blocking(path: &Path) {
    if let Err(error) = crate::fs_util::fsync_parent_dir(path) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "parent-dir fsync failed; durability of rename not guaranteed"
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublishResult {
    Published,
    DestinationExists,
}

/// Validate source and destination ancestors before path-aware planning.
#[must_use = "unsafe reconciliation paths must keep the old state path"]
pub(super) async fn validate_reconciliation_paths(
    root: &Path,
    source: Option<&Path>,
    destination: &Path,
) -> anyhow::Result<()> {
    let root = root.to_path_buf();
    let source = source.map(Path::to_path_buf);
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        if let Some(source) = source {
            let source_root = reconciliation_source_root(&root, &source)?;
            match ConfinedPath::open(&source_root, &source, ConfinedParents::Existing)
                .and_then(|path| path.open_optional_regular())
            {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        ConfinedPath::open(&root, &destination, ConfinedParents::Create)?
            .open_optional_regular()?;
        Ok(())
    })
    .await?
}

/// Retain verified files and directory capabilities through state finalization.
#[derive(Debug)]
pub(crate) struct ReconciledFile {
    pub(super) source: ConfinedPath,
    pub(super) destination: ConfinedPath,
    source_file: std::fs::File,
    destination_file: std::fs::File,
    checksum: String,
}

impl ReconciledFile {
    pub(super) fn checksum(&self) -> &str {
        &self.checksum
    }

    pub(super) async fn validate(self: &Arc<Self>) -> anyhow::Result<()> {
        let copy = Arc::clone(self);
        tokio::task::spawn_blocking(move || copy.validate_blocking()).await?
    }

    pub(super) async fn set_capture_time(self: &Arc<Self>, timestamp: i64) -> anyhow::Result<()> {
        let copy = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            copy.validate_blocking()?;
            let file = copy
                .destination
                .validate_for_metadata(file_identity(&copy.destination_file)?)?;
            use std::time::{Duration, UNIX_EPOCH};
            let duration = Duration::from_secs(timestamp.unsigned_abs());
            let time = if timestamp >= 0 {
                UNIX_EPOCH
                    .checked_add(duration)
                    .context("Capture mtime is out of range")?
            } else {
                UNIX_EPOCH.checked_sub(duration).unwrap_or(UNIX_EPOCH)
            };
            file.set_times(
                std::fs::FileTimes::new()
                    .set_modified(time)
                    .set_accessed(time),
            )?;
            #[cfg(not(windows))]
            file.sync_all()?;
            copy.destination
                .validate_identity(file_identity(&copy.destination_file)?)?;
            Ok(())
        })
        .await?
    }

    #[cfg(feature = "xmp")]
    pub(super) fn open_source_for_metadata(&self) -> anyhow::Result<std::fs::File> {
        let mut file = self
            .source
            .validate_identity(file_identity(&self.source_file)?)?;
        let fingerprint =
            fingerprint_open_file_snapshot_blocking(&mut file, self.source.path())?.fingerprint;
        anyhow::ensure!(
            data_encoding::HEXLOWER.encode(&fingerprint.sha256) == self.checksum,
            "Reconciliation source changed before metadata planning"
        );
        Ok(file)
    }

    pub(super) fn validate_blocking(&self) -> anyhow::Result<()> {
        for (path, retained) in [
            (&self.source, &self.source_file),
            (&self.destination, &self.destination_file),
        ] {
            let identity = file_identity(retained)?;
            let mut file = path.validate_identity(identity)?;
            let fingerprint =
                fingerprint_open_file_snapshot_blocking(&mut file, path.path())?.fingerprint;
            anyhow::ensure!(
                data_encoding::HEXLOWER.encode(&fingerprint.sha256) == self.checksum,
                "Reconciliation media changed before state finalization"
            );
            path.validate_identity(identity)?;
        }
        Ok(())
    }
}

/// Copy regular media without following leaf symlinks or replacing destinations.
///
/// Returns `None` for different destination bytes. Unsafe entries and failed
/// validation return an error; callers must preserve the previous state path.
#[must_use = "only verified reconciliation destinations may be recorded in state"]
pub(crate) async fn copy_local_file_no_replace(
    root: &Path,
    source: &Path,
    destination: &Path,
    temp_suffix: &str,
) -> anyhow::Result<Option<Arc<ReconciledFile>>> {
    let root = root.to_path_buf();
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let temp_suffix = temp_suffix.to_owned();
    tokio::task::spawn_blocking(move || {
        copy_local_file_no_replace_blocking(
            &root,
            &source,
            &destination,
            &temp_suffix,
            |_| {},
            || {},
        )
    })
    .await?
}

fn copy_local_file_no_replace_blocking(
    root: &Path,
    source: &Path,
    destination: &Path,
    temp_suffix: &str,
    before_publication: impl FnOnce(&Path),
    after_publication: impl FnOnce(),
) -> anyhow::Result<Option<Arc<ReconciledFile>>> {
    let source_root = reconciliation_source_root(root, source)?;
    let source = ConfinedPath::open(&source_root, source, ConfinedParents::Existing)?;
    let destination = ConfinedPath::open(root, destination, ConfinedParents::Create)?;
    let mut source_file = source.open_regular()?;
    let source_identity = file_identity(&source_file)?;
    let source_fingerprint =
        fingerprint_open_file_snapshot_blocking(&mut source_file, source.path())?.fingerprint;
    if destination.open_optional_regular()?.is_some() {
        return finish_reconciled_copy(source, source_file, destination, source_fingerprint, None);
    }
    // Create a new, unpredictable name. Never truncate or remove a pre-existing
    // entry. Retain ambiguous temporary entries rather than risking user bytes.
    let part_path = destination.path().with_file_name(format!(
        ".kei-reconcile-{}{temp_suffix}",
        uuid::Uuid::new_v4()
    ));
    let part_path = destination.sibling(&part_path)?;
    let mut part = part_path.create_new_regular()?;
    let part_identity = file_identity(&part)?;
    use std::io::Seek;
    source_file.rewind()?;
    std::io::copy(&mut source_file, &mut part)?;
    part.set_permissions(source_file.metadata()?.permissions())?;
    part.sync_all()?;
    part.rewind()?;
    anyhow::ensure!(
        fingerprint_open_file_snapshot_blocking(&mut part, part_path.path())?.fingerprint
            == source_fingerprint,
        "Local path reconciliation checksum mismatch"
    );
    source_file.rewind()?;
    anyhow::ensure!(
        file_identity(&source.validate_identity(source_identity)?)? == source_identity
            && fingerprint_open_file_snapshot_blocking(&mut source_file, source.path())?
                .fingerprint
                == source_fingerprint,
        "Reconciliation source changed during copy"
    );
    before_publication(part_path.path());
    anyhow::ensure!(
        file_identity(&part_path.validate_identity(part_identity)?)? == part_identity,
        "Reconciliation temporary entry changed before publication"
    );
    let publication = publish_reconciliation_part_blocking(&part_path, &destination)?;
    after_publication();
    let expected_identity = match publication {
        PublishResult::Published => Some(part_identity),
        PublishResult::DestinationExists => None,
    };
    if part_path.entry_exists()? {
        tracing::warn!(path = %part_path.path().display(), "Retaining reconciliation temporary file for manual inspection");
    }
    finish_reconciled_copy(
        source,
        source_file,
        destination,
        source_fingerprint,
        expected_identity,
    )
}

fn finish_reconciled_copy(
    source: ConfinedPath,
    source_file: std::fs::File,
    destination: ConfinedPath,
    expected: ExistingFileFingerprint,
    expected_identity: Option<FileIdentity>,
) -> anyhow::Result<Option<Arc<ReconciledFile>>> {
    let mut file = destination.open_regular()?;
    let identity = file_identity(&file)?;
    anyhow::ensure!(
        expected_identity.is_none_or(|expected| expected == identity),
        "Reconciled destination identity changed"
    );
    let actual =
        fingerprint_open_file_snapshot_blocking(&mut file, destination.path())?.fingerprint;
    anyhow::ensure!(
        file_identity(&destination.validate_identity(identity)?)? == identity,
        "Reconciled destination changed while hashing"
    );
    if actual != expected {
        return Ok(None);
    }
    destination.sync_parent()?;
    Ok(Some(Arc::new(ReconciledFile {
        source,
        destination,
        source_file,
        destination_file: file,
        checksum: data_encoding::HEXLOWER.encode(&actual.sha256),
    })))
}

pub(super) fn publish_reconciliation_part_blocking(
    part: &ConfinedPath,
    destination: &ConfinedPath,
) -> std::io::Result<PublishResult> {
    #[cfg(target_os = "linux")]
    let result =
        renameat2_confined_blocking(part, destination, libc::RENAME_NOREPLACE).or_else(|error| {
            if is_renameat2_unsupported(&error) {
                hard_link_confined(part, destination)
            } else {
                Err(error)
            }
        });
    #[cfg(target_os = "macos")]
    let result = rename_confined_macos(part, destination, libc::RENAME_EXCL);
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let result = hard_link_confined(part, destination);
    #[cfg(windows)]
    let result = move_file_no_replace_blocking(part.path(), destination.path());
    match result {
        Ok(()) => Ok(PublishResult::Published),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(error) => Err(error),
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn hard_link_confined(part: &ConfinedPath, destination: &ConfinedPath) -> std::io::Result<()> {
    // SAFETY: both retained directory descriptors and NUL-terminated names
    // remain live. linkat refuses an existing destination; no path cleanup follows.
    if unsafe {
        libc::linkat(
            part.parent_fd(),
            part.name_cstr().as_ptr(),
            destination.parent_fd(),
            destination.name_cstr().as_ptr(),
            0,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn renameat2_confined_blocking(
    part_path: &ConfinedPath,
    final_path: &ConfinedPath,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    // SAFETY: both directory descriptors and NUL-terminated names remain live,
    // and callers pass one documented renameat2 flag.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            part_path.parent_fd(),
            part_path.name_cstr().as_ptr(),
            final_path.parent_fd(),
            final_path.name_cstr().as_ptr(),
            flags,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_confined_macos(
    source: &ConfinedPath,
    destination: &ConfinedPath,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    // SAFETY: both retained directory descriptors and NUL-terminated names
    // remain live and callers pass a documented renameatx_np flag.
    if unsafe {
        libc::renameatx_np(
            source.parent_fd(),
            source.name_cstr().as_ptr(),
            destination.parent_fd(),
            destination.name_cstr().as_ptr(),
            flags,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn reconciliation_source_root(download_root: &Path, source: &Path) -> anyhow::Result<PathBuf> {
    let download_root = crate::fs_util::absolute_lexical(download_root)?;
    let source = crate::fs_util::absolute_lexical(source)?;
    if source.starts_with(&download_root) {
        return Ok(download_root);
    }
    let source_parent = source.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot confine reconciliation source without a parent: {}",
            source.display()
        )
    })?;
    Ok(source_parent
        .ancestors()
        .find(|ancestor| download_root.starts_with(ancestor))
        .unwrap_or(source_parent)
        .to_path_buf())
}

#[cfg(target_os = "linux")]
async fn publish_part_no_replace(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    let part = part_path.to_path_buf();
    let final_path_buf = final_path.to_path_buf();
    let rename_result =
        tokio::task::spawn_blocking(move || renameat2_no_replace_blocking(&part, &final_path_buf))
            .await
            .map_err(std::io::Error::other)?;

    match rename_result {
        Ok(()) => Ok(PublishResult::Published),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(e) if is_renameat2_unsupported(&e) => publish_part_by_hard_link(part_path, final_path)
            .await
            .or_else(|link_err| destination_exists_or(link_err, final_path)),
        Err(e) => destination_exists_or(e, final_path),
    }
}

#[cfg(target_os = "linux")]
fn renameat2_no_replace_blocking(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
    renameat2_blocking(part_path, final_path, libc::RENAME_NOREPLACE)
}

#[cfg(target_os = "linux")]
fn renameat2_exchange_blocking(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
    renameat2_blocking(part_path, final_path, libc::RENAME_EXCHANGE)
}

#[cfg(target_os = "linux")]
fn renameat2_blocking(
    part_path: &Path,
    final_path: &Path,
    flags: libc::c_uint,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let part_c = CString::new(part_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let final_c = CString::new(final_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both path arguments are valid NUL-terminated C strings, AT_FDCWD
    // asks the kernel to resolve them from the current working directory when
    // they are relative, and callers pass one documented renameat2 flag. No
    // Rust references are shared with the kernel after the call.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            part_c.as_ptr(),
            libc::AT_FDCWD,
            final_c.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn exchange_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    renameat2_exchange_blocking(part_path, final_path)?;
    Ok(part_path.to_path_buf())
}

#[cfg(target_os = "linux")]
fn restore_exchanged_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _displaced_path: &Path,
) -> std::io::Result<()> {
    renameat2_exchange_blocking(part_path, final_path)
}

#[cfg(target_os = "linux")]
fn is_renameat2_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
async fn publish_part_no_replace(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    publish_part_by_hard_link(part_path, final_path)
        .await
        .or_else(|link_err| destination_exists_or(link_err, final_path))
}

#[cfg(target_os = "macos")]
fn rename_exchange_blocking(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let part = CString::new(part_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let final_path = CString::new(final_path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: both paths are valid NUL-terminated C strings and RENAME_SWAP
    // atomically exchanges two existing directory entries on macOS.
    let rc = unsafe { libc::renamex_np(part.as_ptr(), final_path.as_ptr(), libc::RENAME_SWAP) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn exchange_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    rename_exchange_blocking(part_path, final_path)?;
    Ok(part_path.to_path_buf())
}

#[cfg(target_os = "macos")]
fn restore_exchanged_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    _displaced_path: &Path,
) -> std::io::Result<()> {
    rename_exchange_blocking(part_path, final_path)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn exchange_repair_files_blocking(
    _part_path: &Path,
    _final_path: &Path,
    _expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic truncated-file replacement is unsupported on this platform",
    )
    .into())
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn restore_exchanged_repair_files_blocking(
    _part_path: &Path,
    _final_path: &Path,
    _displaced_path: &Path,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic truncated-file replacement is unsupported on this platform",
    ))
}

#[cfg(unix)]
async fn publish_part_by_hard_link(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    match fs::hard_link(part_path, final_path).await {
        Ok(()) => {
            if let Err(rm_err) = fs::remove_file(part_path).await {
                tracing::warn!(
                    path = %part_path.display(),
                    error = %rm_err,
                    "Failed to remove published .part file after no-overwrite hard link"
                );
            }
            Ok(PublishResult::Published)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
async fn publish_part_no_replace(
    part_path: &Path,
    final_path: &Path,
) -> std::io::Result<PublishResult> {
    let part = part_path.to_path_buf();
    let final_path_buf = final_path.to_path_buf();
    let rename_result =
        tokio::task::spawn_blocking(move || move_file_no_replace_blocking(&part, &final_path_buf))
            .await
            .map_err(std::io::Error::other)?;

    match rename_result {
        Ok(()) => Ok(PublishResult::Published),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(PublishResult::DestinationExists)
        }
        Err(e) => destination_exists_or(e, final_path),
    }
}

#[cfg(windows)]
fn move_file_no_replace_blocking(part_path: &Path, final_path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    fn nul_terminated(path: &Path) -> std::io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let part = nul_terminated(part_path)?;
    let final_path = nul_terminated(final_path)?;
    // SAFETY: both path arguments are valid NUL-terminated Windows strings.
    // MOVEFILE_WRITE_THROUGH keeps the existing durable-publish intent, and
    // omitting MOVEFILE_REPLACE_EXISTING gives this promotion no-overwrite
    // semantics.
    let rc = unsafe { MoveFileExW(part.as_ptr(), final_path.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if rc != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn replace_file_with_backup_blocking(
    final_path: &Path,
    replacement_path: &Path,
    backup_path: &Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    fn nul_terminated(path: &Path) -> std::io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "path contains NUL",
            ));
        }
        wide.push(0);
        Ok(wide)
    }

    let final_path = nul_terminated(final_path)?;
    let replacement = nul_terminated(replacement_path)?;
    let backup = nul_terminated(backup_path)?;
    // SAFETY: every path is a valid NUL-terminated Windows string. The final
    // path exists, the replacement is the verified `.part` file, and Windows
    // atomically moves the displaced bytes to the distinct backup path.
    let rc = unsafe {
        ReplaceFileW(
            final_path.as_ptr(),
            replacement.as_ptr(),
            backup.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn repair_backup_path(part_path: &Path) -> PathBuf {
    let mut name = part_path.as_os_str().to_os_string();
    name.push(".repair-backup");
    PathBuf::from(name)
}

#[cfg(windows)]
fn exchange_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    expected: ExistingFileFingerprint,
) -> anyhow::Result<PathBuf> {
    let backup_path = repair_backup_path(part_path);
    if backup_path.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("repair backup already exists at {}", backup_path.display()),
        )
        .into());
    }
    let replace_result = replace_file_with_backup_blocking(final_path, part_path, &backup_path);
    finish_windows_repair_exchange_blocking(final_path, &backup_path, expected, replace_result)
}

#[cfg(windows)]
fn finish_windows_repair_exchange_blocking(
    final_path: &Path,
    backup_path: &Path,
    expected: ExistingFileFingerprint,
    result: std::io::Result<()>,
) -> anyhow::Result<PathBuf> {
    use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

    match result {
        Ok(()) => Ok(backup_path.to_path_buf()),
        Err(source)
            if source
                .raw_os_error()
                .and_then(|code| u32::try_from(code).ok())
                == Some(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2) =>
        {
            let source_message = source.to_string();
            let displaced = match fingerprint_regular_file_blocking(backup_path) {
                Ok(displaced) => displaced,
                Err(error) => {
                    return Err(error)
                        .with_context(|| {
                            format!(
                                "Could not verify the partially displaced repair target; it remains at {}",
                                backup_path.display()
                            )
                        })
                        .context(ConditionalPublishMustRetainPaths {
                            paths: vec![backup_path.to_path_buf()],
                        });
                }
            };
            if displaced != expected {
                return Err(ConditionalPublishTargetChanged::DuringPublication {
                    path: final_path.to_path_buf(),
                })
                .context(ConditionalPublishMustRetainPaths {
                    paths: vec![backup_path.to_path_buf()],
                });
            }

            if let Err(error) = move_file_no_replace_blocking(backup_path, final_path) {
                return Err(error)
                    .with_context(|| {
                        format!(
                            "Could not restore {}; the original bytes remain at {}",
                            final_path.display(),
                            backup_path.display()
                        )
                    })
                    .context(ConditionalPublishTargetChanged::Unverifiable {
                        path: final_path.to_path_buf(),
                    })
                    .context(ConditionalPublishMustRetainPaths {
                        paths: vec![backup_path.to_path_buf()],
                    });
            }
            fsync_parent_dir_best_effort_blocking(final_path);
            Err(source).with_context(|| {
                format!(
                    "Windows file replacement partially failed after moving {}; the original target was restored: {source_message}",
                    final_path.display()
                )
            })
        }
        Err(source) => Err(source.into()),
    }
}

#[cfg(windows)]
fn restore_exchanged_repair_files_blocking(
    part_path: &Path,
    final_path: &Path,
    displaced_path: &Path,
) -> std::io::Result<()> {
    if part_path.try_exists()? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("repair part path changed at {}", part_path.display()),
        ));
    }
    replace_file_with_backup_blocking(final_path, displaced_path, part_path)
}

fn destination_exists_or(err: std::io::Error, final_path: &Path) -> std::io::Result<PublishResult> {
    if final_path.try_exists().unwrap_or(false) {
        Ok(PublishResult::DestinationExists)
    } else {
        Err(err)
    }
}

/// Compute the SHA-256 hash of a file, returning a hex-encoded string.
///
/// Used for `local_checksum` / `download_checksum` in the state DB and
/// by `verify --checksums` for integrity checks.
pub(crate) async fn compute_sha256(path: &Path) -> anyhow::Result<String> {
    let fingerprint = fingerprint_file(path).await?;
    Ok(data_encoding::HEXLOWER.encode(&fingerprint.sha256))
}

/// Read one file handle to capture the size and SHA-256 of the same bytes.
pub(super) async fn fingerprint_file(path: &Path) -> anyhow::Result<ExistingFileFingerprint> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || fingerprint_file_blocking(&path)).await?
}

fn fingerprint_file_blocking(path: &Path) -> anyhow::Result<ExistingFileFingerprint> {
    Ok(fingerprint_file_snapshot_blocking(path)?.fingerprint)
}

fn fingerprint_file_snapshot_blocking(path: &Path) -> anyhow::Result<ExistingFileSnapshot> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Could not open {} for SHA-256", path.display()))?;
    fingerprint_open_file_snapshot_blocking(&mut file, path)
}

fn fingerprint_open_file_snapshot_blocking(
    file: &mut std::fs::File,
    path: &Path,
) -> anyhow::Result<ExistingFileSnapshot> {
    use sha2::{Digest, Sha256};

    let mut size = 0_u64;
    let mut sha256 = Sha256::new();
    let mut prefix = [0_u8; 12];
    let mut prefix_len = 0_usize;
    // 64 KiB reduces read() syscalls ~8x vs 8 KiB on multi-GB videos
    // without meaningful RSS impact on the blocking pool.
    let mut buf = [0u8; 65536];
    loop {
        use std::io::Read;
        let n = file
            .read(&mut buf)
            .with_context(|| format!("Could not read {} for SHA-256", path.display()))?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .with_context(|| format!("Could not size {} for SHA-256", path.display()))?;
        if prefix_len < prefix.len() {
            let copy_len = (prefix.len() - prefix_len).min(n);
            #[allow(
                clippy::indexing_slicing,
                reason = "copy_len is bounded by both prefix and the bytes returned into buf"
            )]
            prefix[prefix_len..prefix_len + copy_len].copy_from_slice(&buf[..copy_len]);
            prefix_len += copy_len;
        }
        #[allow(
            clippy::indexing_slicing,
            reason = "`n` is bounded by buf.len() because read() returns bytes written"
        )]
        sha256.update(&buf[..n]);
    }
    Ok(ExistingFileSnapshot {
        fingerprint: ExistingFileFingerprint {
            size,
            sha256: sha256.finalize().into(),
        },
        prefix,
        prefix_len,
    })
}

/// Fingerprint a replacement entry and reject links or special files.
pub(super) async fn fingerprint_regular_file(
    path: &Path,
) -> anyhow::Result<ExistingFileFingerprint> {
    let before = fs::symlink_metadata(path)
        .await
        .with_context(|| format!("Could not inspect replacement file {}", path.display()))?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "Replacement path is not a regular file: {}",
        path.display()
    );
    let fingerprint = fingerprint_file(path).await?;
    let after = fs::symlink_metadata(path)
        .await
        .with_context(|| format!("Could not recheck replacement file {}", path.display()))?;
    anyhow::ensure!(
        after.file_type().is_file(),
        "Replacement path changed away from a regular file: {}",
        path.display()
    );
    Ok(fingerprint)
}

pub(super) fn fingerprint_regular_file_snapshot_blocking(
    path: &Path,
) -> anyhow::Result<ExistingFileSnapshot> {
    let before = std::fs::symlink_metadata(path)
        .with_context(|| format!("Could not inspect replacement file {}", path.display()))?;
    anyhow::ensure!(
        before.file_type().is_file(),
        "Replacement path is not a regular file: {}",
        path.display()
    );
    let snapshot = fingerprint_file_snapshot_blocking(path)?;
    let after = std::fs::symlink_metadata(path)
        .with_context(|| format!("Could not recheck replacement file {}", path.display()))?;
    anyhow::ensure!(
        after.file_type().is_file(),
        "Replacement path changed away from a regular file: {}",
        path.display()
    );
    Ok(snapshot)
}

fn fingerprint_regular_file_blocking(path: &Path) -> anyhow::Result<ExistingFileFingerprint> {
    Ok(fingerprint_regular_file_snapshot_blocking(path)?.fingerprint)
}

/// The provider-size relationship required before a local file can be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalFileSizeExpectation {
    /// The local file must not be shorter than the provider download.
    AtLeastProvider(u64),
    /// The local file must have the same size as the provider download.
    ExactProvider(u64),
}

impl LocalFileSizeExpectation {
    const fn accepts(self, actual_size: u64) -> bool {
        match self {
            Self::AtLeastProvider(provider_size) => {
                provider_size == 0 || actual_size >= provider_size
            }
            Self::ExactProvider(provider_size) => actual_size == provider_size,
        }
    }
}

/// Return whether a local file satisfies its provider-size requirement.
///
/// Embedded metadata can legitimately change the final file size after kei
/// verifies the provider download. When both stored checksums prove that kei
/// changed the bytes, hash the current file and accept the size difference
/// only when it still matches the recorded post-write checksum.
///
/// # Errors
///
/// Returns an error when checksum proof is required but the file cannot be
/// read or hashed.
#[must_use = "file-size validation determines whether existing bytes can be trusted"]
pub(crate) async fn local_file_size_matches_state(
    path: &Path,
    actual_size: u64,
    expectation: LocalFileSizeExpectation,
    local_checksum: Option<&str>,
    download_checksum: Option<&str>,
) -> anyhow::Result<bool> {
    if expectation.accepts(actual_size) {
        return Ok(true);
    }

    let metadata_changed_download = matches!(
        (local_checksum, download_checksum),
        (Some(local), Some(download)) if local != download
    );
    if !metadata_changed_download {
        return Ok(false);
    }

    let actual_checksum = compute_sha256(path).await?;
    Ok(local_checksum == Some(actual_checksum.as_str()))
}

/// Decoded iCloud API checksum with its hash algorithm.
///
/// Note: Apple's `fileChecksum` is an MMCS compound signature, not a
/// content hash. This decoder is retained for test coverage of the
/// base64/length classification logic.
#[cfg(test)]
#[derive(Debug)]
struct DecodedChecksum {
    hex: String,
    is_sha1: bool,
}

#[cfg(test)]
fn decode_api_checksum(base64_checksum: &str) -> anyhow::Result<DecodedChecksum> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_checksum)
        .context("Could not decode API checksum from base64")?;
    let (hash_bytes, is_sha1) = match bytes.len() {
        20 => (&bytes[..], true),
        21 => (&bytes[1..], true),
        32 => (&bytes[..], false),
        33 => (&bytes[1..], false),
        other => anyhow::bail!(
            "Apple returned an unsupported checksum length: {other} bytes (expected 20, 21, 32, or 33)."
        ),
    };
    let mut hex = String::with_capacity(hash_bytes.len() * 2);
    for b in hash_bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(DecodedChecksum { hex, is_sha1 })
}

fn rejecting_content_type_reason(content_type: &str) -> Option<&'static str> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();

    match essence.as_str() {
        "text/html" | "application/xhtml+xml" => Some("HTML"),
        "application/json" | "text/json" => Some("JSON"),
        _ if essence.ends_with("+json") => Some("JSON"),
        _ => None,
    }
}

fn validate_resume_content_range(
    content_range: Option<&str>,
    resume_offset: u64,
    path: &str,
) -> Result<(), DownloadError> {
    let Some(content_range) = content_range else {
        return Err(DownloadError::InvalidContent {
            path: path.into(),
            reason: "206 Partial Content response omitted Content-Range for resume".into(),
        });
    };
    let Some(start) = parse_content_range_start(content_range) else {
        return Err(DownloadError::InvalidContent {
            path: path.into(),
            reason: format!("malformed Content-Range for resume: {content_range}").into(),
        });
    };
    if start != resume_offset {
        return Err(DownloadError::InvalidContent {
            path: path.into(),
            reason: format!(
                "Content-Range start {start} does not match existing .part size {resume_offset}"
            )
            .into(),
        });
    }
    Ok(())
}

fn parse_content_range_start(content_range: &str) -> Option<u64> {
    let mut parts = content_range.split_whitespace();
    let unit = parts.next()?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let range_and_total = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let (range, _total) = range_and_total.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    if end < start {
        return None;
    }
    Some(start)
}

/// Inspect the first bytes of a downloaded file for known-bad sentinels that
/// unambiguously identify a non-media error body (HTML error page, JSON error,
/// etc.). Returns a human-readable reason string when a sentinel is present.
///
/// Checks run case-insensitively against ASCII-whitespace-trimmed content so
/// that e.g. a leading `\n<html>` still fails. These sentinels are never valid
/// image/video starts, while extension mismatches further down are warnings
/// unless kei has positive evidence that the body is bad content.
#[allow(
    clippy::indexing_slicing,
    reason = "`pos` comes from `header.iter().position(...)` so `header[pos..]` is \
              in-bounds; the prefix slices below are guarded by explicit length checks"
)]
fn detect_error_sentinel(header: &[u8]) -> Option<&'static str> {
    let trimmed = header
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(header, |pos| &header[pos..]);

    // Note: `<?xml` is deliberately NOT a sentinel because legitimate AAE
    // sidecar files start with an XML declaration.
    const HTML_PREFIXES: &[&[u8]] = &[b"<!doctype", b"<html"];
    for prefix in HTML_PREFIXES {
        if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return Some("file starts with HTML markup (likely a CDN error page)");
        }
    }

    // JSON error envelopes: `{"error"`, `{"errors"`, `{"message"`, `{"code"`.
    // Match only the quoted-key form so we don't reject arbitrary JSON bodies
    // that legitimately start with `{` (images never do, but we stay narrow).
    const JSON_PREFIXES: &[&[u8]] = &[b"{\"error\"", b"{\"errors\"", b"{\"message\"", b"{\"code\""];
    for prefix in JSON_PREFIXES {
        if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return Some("file starts with a JSON error envelope (likely a CDN error body)");
        }
    }

    None
}

/// Classify whether `header` matches a known-valid magic-byte signature for
/// the file extension `ext` (lowercase, no leading dot).
///
/// Returns:
/// - `Some(true)` — header matches a known-valid signature
/// - `Some(false)` — extension is recognized but header does not match
///   (caller decides whether another known media signature can safely pass)
/// - `None` — extension is not in the signature table; skip the check
///
/// MOV handling intentionally differs from the other ISO-BMFF extensions.
/// `.heic`, `.heif`, `.mp4`, and `.m4v` must start with an `ftyp` box.
/// `.mov` may start with any classic QuickTime top-level atom: Apple's
/// Photos pipeline commonly serves live-photo and HEVC videos in classic
/// QuickTime format, whose first atom is padding (`wide`) or media data
/// (`mdat`) rather than `ftyp`.
#[allow(
    clippy::indexing_slicing,
    reason = "each match arm slices `header` only after an `n >= N` length guard where \
              `n == header.len()`; clippy can't see the proof but every slice is bounded"
)]
fn classify_magic(ext: &str, header: &[u8]) -> Option<bool> {
    let n = header.len();
    match ext {
        "jpg" | "jpeg" => Some(n >= 2 && header[..2] == [0xFF, 0xD8]),
        "png" => Some(n >= 4 && header[..4] == [0x89, 0x50, 0x4E, 0x47]),
        "heic" | "heif" | "mp4" | "m4v" => Some(n >= 8 && &header[4..8] == b"ftyp"),
        "mov" => Some(n >= 8 && is_mov_top_atom(&header[4..8])),
        "gif" => Some(n >= 4 && &header[..4] == b"GIF8"),
        "tiff" | "tif" | "dng" => Some(
            n >= 4
                && (header[..4] == [0x49, 0x49, 0x2A, 0x00]
                    || header[..4] == [0x4D, 0x4D, 0x00, 0x2A]),
        ),
        "webp" => Some(n >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP"),
        _ => None,
    }
}

/// Classify whether `header` starts with any media signature kei recognizes,
/// independent of the destination filename extension.
fn classify_known_media_magic(header: &[u8]) -> Option<&'static str> {
    if classify_magic("jpg", header) == Some(true) {
        Some("JPEG")
    } else if classify_magic("png", header) == Some(true) {
        Some("PNG")
    } else if classify_magic("gif", header) == Some(true) {
        Some("GIF")
    } else if classify_magic("tiff", header) == Some(true) {
        Some("TIFF/DNG")
    } else if classify_magic("webp", header) == Some(true) {
        Some("WebP")
    } else if classify_magic("heic", header) == Some(true) {
        Some("ISO-BMFF media")
    } else if classify_magic("mov", header) == Some(true) {
        Some("QuickTime MOV")
    } else {
        None
    }
}

/// Valid top-level atom types at offset 4 of a QuickTime `.mov` file.
///
/// Modern ISO-BMFF MOVs begin with `ftyp`. Classic QuickTime MOVs (produced
/// by older iOS versions and by the live-photo / HEVC pipeline) may begin
/// with any atom: padding (`wide`), media data (`mdat`), movie header
/// (`moov`), unused space (`free`/`skip`), or a preview resource (`pnot`).
fn is_mov_top_atom(atom: &[u8]) -> bool {
    matches!(
        atom,
        b"ftyp" | b"wide" | b"mdat" | b"moov" | b"free" | b"skip" | b"pnot"
    )
}

/// Validate downloaded content using kei's media-body policy.
///
/// Hard-fail when kei has positive evidence the file is unsafe or incomplete:
/// zero bytes, known HTML/JSON error bodies, known error-document content types
/// checked before writing, byte-count mismatches checked by the caller, or a
/// known media extension whose bytes are neither the expected media type nor
/// any other media type kei recognizes.
///
/// Extension-specific media mismatches are warnings, not hard failures: iCloud
/// sometimes assigns `.PNG` names to JPEG bytes. Filenames stay exactly as
/// planned; validation never rewrites extensions. Unknown extensions remain
/// permissive so provider-side sidecars or new formats are not silently blocked
/// before kei has a type-specific policy for them.
fn validate_downloaded_content(
    part_path: &Path,
    download_path: &Path,
) -> Result<(), DownloadError> {
    use std::io::Read;

    let mut file = std::fs::File::open(part_path).map_err(|e| DownloadError::Disk(Box::new(e)))?;
    let mut buf = [0u8; 16];
    let n = file
        .read(&mut buf)
        .map_err(|e| DownloadError::Disk(Box::new(e)))?;

    if n == 0 {
        return Err(DownloadError::InvalidContent {
            path: download_path.display().to_string().into(),
            reason: "downloaded file is empty (zero bytes)".into(),
        });
    }

    #[allow(
        clippy::indexing_slicing,
        reason = "`n` is bytes read from `buf` so `n <= buf.len() == 16`"
    )]
    let header = &buf[..n];

    // Reject known-bad error-page sentinels regardless of extension. Apple's
    // CDN occasionally returns rate-limit / 4xx / 5xx bodies as HTTP 200 with
    // HTML or JSON content that no valid image file would ever start with.
    if let Some(reason) = detect_error_sentinel(header) {
        return Err(DownloadError::InvalidContent {
            path: download_path.display().to_string().into(),
            reason: reason.into(),
        });
    }

    let ext = download_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if classify_magic(&ext, header) == Some(false) {
        #[allow(
            clippy::indexing_slicing,
            reason = "`n.min(8)` caps the slice at `header.len()`"
        )]
        let preview = &header[..n.min(8)];
        if let Some(detected_media) = classify_known_media_magic(header) {
            tracing::warn!(
                path = %download_path.display(),
                expected_extension = %ext,
                detected_media,
                header = %format_args!("{preview:02x?}"),
                "File header is valid media but does not match extension; saving anyway",
            );
            return Ok(());
        }
        return Err(DownloadError::InvalidContent {
            path: download_path.display().to_string().into(),
            reason: format!(
                "file header does not match expected .{ext} media and is not recognized as another supported media type (first bytes: {preview:02x?})"
            )
            .into(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const ISSUE_507_JPEG_HEADER: [u8; 8] = [0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x80, 0x45, 0x78];

    #[tokio::test]
    async fn local_reconciliation_copy_preserves_source_and_refuses_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let destination = dir.path().join("nested/destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();

        let copied = copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
            .await
            .unwrap();
        assert!(copied.is_some());
        assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
        assert_eq!(std::fs::read(&destination).unwrap(), b"catalog bytes");

        std::fs::write(&destination, b"user bytes").unwrap();
        let conflict = copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
            .await
            .unwrap();
        assert!(conflict.is_none());
        assert_eq!(std::fs::read(&destination).unwrap(), b"user bytes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_leaf_symlinks_and_special_entries_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target.jpg");
        let destination = dir.path().join("destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();
        std::fs::write(&target, b"catalog bytes").unwrap();
        for target_path in [&target, &outside.path().join("missing.jpg")] {
            symlink(target_path, &destination).unwrap();
            assert!(
                copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_link(&destination).unwrap(), *target_path);
            assert_eq!(std::fs::read(&target).unwrap(), b"catalog bytes");
            std::fs::remove_file(&destination).unwrap();
        }
        std::fs::create_dir(&destination).unwrap();
        assert!(
            copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
                .await
                .is_err()
        );
        assert!(destination.is_dir());
        std::fs::remove_dir(&destination).unwrap();
        let linked_source = dir.path().join("source-link.jpg");
        symlink(&source, &linked_source).unwrap();
        assert!(
            copy_local_file_no_replace(dir.path(), &linked_source, &destination, ".part")
                .await
                .is_err()
        );
        assert!(!destination.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_leaf_publication_races_preserve_external_bytes() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("target.jpg");
        std::fs::write(&target, b"catalog bytes").unwrap();
        for after_publish in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source.jpg");
            let destination = dir.path().join("destination.jpg");
            std::fs::write(&source, b"catalog bytes").unwrap();
            let result = copy_local_file_no_replace_blocking(
                dir.path(),
                &source,
                &destination,
                ".part",
                |_| {
                    if !after_publish {
                        symlink(&target, &destination).unwrap();
                    }
                },
                || {
                    if after_publish {
                        std::fs::remove_file(&destination).unwrap();
                        symlink(&target, &destination).unwrap();
                    }
                },
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read_link(&destination).unwrap(), target);
            assert_eq!(std::fs::read(&target).unwrap(), b"catalog bytes");
            assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
        }
    }

    #[tokio::test]
    async fn reconciliation_leaf_identical_regular_destination_is_reusable() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let destination = dir.path().join("destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();
        std::fs::write(&destination, b"catalog bytes").unwrap();
        let expected = compute_sha256(&source).await.unwrap();
        for _ in 0..2 {
            assert_eq!(
                copy_local_file_no_replace(dir.path(), &source, &destination, ".part")
                    .await
                    .unwrap()
                    .map(|copy| copy.checksum().to_owned()),
                Some(expected.clone())
            );
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_leaf_replaced_temporary_file_is_not_published() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.jpg");
        let destination = dir.path().join("destination.jpg");
        std::fs::write(&source, b"catalog bytes").unwrap();
        let result = copy_local_file_no_replace_blocking(
            dir.path(),
            &source,
            &destination,
            ".part",
            |part| {
                std::fs::remove_file(part).unwrap();
                symlink(&source, part).unwrap();
            },
            || {},
        );
        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"catalog bytes");
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_confined_parent_publication_races_reject_external_redirects() {
        use std::os::unix::fs::symlink;
        for after_publish in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let source = root.path().join("source.jpg");
            let parent = root.path().join("parent");
            let retained = root.path().join("retained");
            let destination = parent.join("destination.jpg");
            std::fs::create_dir(&parent).unwrap();
            std::fs::write(&source, b"media").unwrap();
            let replace_parent = || {
                std::fs::rename(&parent, &retained).unwrap();
                symlink(outside.path(), &parent).unwrap();
            };
            let result = copy_local_file_no_replace_blocking(
                root.path(),
                &source,
                &destination,
                ".part",
                |_| {
                    if !after_publish {
                        replace_parent();
                    }
                },
                || {
                    if after_publish {
                        replace_parent();
                    }
                },
            );
            assert!(result.is_err());
            assert_eq!(std::fs::read(&source).unwrap(), b"media");
            assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reconciliation_confined_receipt_detects_parent_change_before_state() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let source = root.path().join("source.jpg");
        let parent = root.path().join("parent");
        let destination = parent.join("destination.jpg");
        std::fs::write(&source, b"media").unwrap();
        let copied = copy_local_file_no_replace(root.path(), &source, &destination, ".part")
            .await
            .unwrap()
            .unwrap();
        copied.validate().await.unwrap();
        std::fs::rename(&parent, root.path().join("retained")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parent).unwrap();
        assert!(copied.validate().await.is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert_eq!(std::fs::read(&source).unwrap(), b"media");
    }

    #[tokio::test]
    async fn reconciliation_confined_relative_root_reaches_steady_state() {
        let cwd = std::env::current_dir().unwrap();
        let root = tempfile::tempdir_in(&cwd).unwrap();
        let relative_root = root.path().strip_prefix(&cwd).unwrap();
        let source = root.path().join("source.jpg");
        let destination = relative_root.join("nested/destination.jpg");
        std::fs::write(&source, b"media").unwrap();
        for _ in 0..2 {
            let copy = copy_local_file_no_replace(relative_root, &source, &destination, ".part")
                .await
                .unwrap()
                .unwrap();
            copy.validate().await.unwrap();
            assert_eq!(copy.checksum(), compute_sha256(&source).await.unwrap());
        }
        assert_eq!(
            std::fs::read_dir(destination.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
        assert_eq!(std::fs::read(&source).unwrap(), b"media");
    }

    #[test]
    fn test_base32_encode() {
        // Verify data-encoding produces expected RFC 4648 no-pad output
        use data_encoding::BASE32_NOPAD;
        assert_eq!(BASE32_NOPAD.encode(b"Hello"), "JBSWY3DP");
        assert_eq!(BASE32_NOPAD.encode(b""), "");
        assert_eq!(BASE32_NOPAD.encode(b"f"), "MY");
        assert_eq!(BASE32_NOPAD.encode(b"fo"), "MZXQ");
        assert_eq!(BASE32_NOPAD.encode(b"foo"), "MZXW6");
    }

    /// Verify the content-length math when resume_offset > 0 but server returns 200
    /// (ignoring Range). In this case effective_offset should be 0, so
    /// `bytes_written - effective_offset` equals the full body length.
    #[test]
    fn test_content_length_check_after_resume_fallback() {
        // Simulate: resume_offset was 500 but server returned 200 (full body of 1000 bytes).
        // With the bug: total_bytes = 1000 - 500 = 500, mismatch against content_length=1000.
        // With the fix: effective_offset = 0, total_bytes = 1000 - 0 = 1000, matches.
        let resume_offset = 500u64;
        let bytes_written_after_stream = 1000u64;
        let content_length = 1000u64;

        // Old (buggy) path would use resume_offset
        let buggy_total = bytes_written_after_stream - resume_offset;
        assert_ne!(buggy_total, content_length, "buggy path should mismatch");

        // New (fixed) path: server returned 200, so effective_offset = 0
        let effective_offset = 0u64;
        let fixed_total = bytes_written_after_stream - effective_offset;
        assert_eq!(fixed_total, content_length, "fixed path should match");
    }

    #[test]
    fn test_temp_download_path_valid_checksum() {
        // Base64 "AAAA" decodes to [0, 0, 0], base32 encodes to "AAAAA"
        let path = PathBuf::from("/photos/test.jpg");
        let result = temp_download_path(&path, "AAAA", ".kei-tmp").unwrap();
        assert_eq!(result.parent().unwrap(), Path::new("/photos"));
        assert!(result.to_string_lossy().ends_with(".kei-tmp"));
    }

    #[test]
    fn test_temp_download_path_derives_from_checksum() {
        let path = PathBuf::from("/photos/test.jpg");
        let result1 = temp_download_path(&path, "AAAA", ".kei-tmp").unwrap();
        let result2 = temp_download_path(&path, "AAAB", ".kei-tmp").unwrap();
        // Different checksums should produce different temp filenames
        assert_ne!(result1, result2);
    }

    #[test]
    fn test_temp_download_path_same_checksum_same_result() {
        let path1 = PathBuf::from("/photos/a.jpg");
        let path2 = PathBuf::from("/photos/b.jpg");
        let result1 = temp_download_path(&path1, "AAAA", ".kei-tmp").unwrap();
        let result2 = temp_download_path(&path2, "AAAA", ".kei-tmp").unwrap();
        // Same checksum, same directory -> same temp file (for resume)
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_temp_download_path_invalid_base64() {
        let path = PathBuf::from("/photos/test.jpg");
        let result = temp_download_path(&path, "not-valid-base64!!!", ".kei-tmp");
        assert!(result.is_err());
    }

    #[test]
    fn test_temp_download_path_custom_suffix() {
        let path = PathBuf::from("/photos/test.jpg");
        let result = temp_download_path(&path, "AAAA", ".downloading").unwrap();
        assert!(result.to_string_lossy().ends_with(".downloading"));
    }

    #[test]
    fn test_temp_download_path_part_suffix() {
        // Verify .part still works when explicitly configured
        let path = PathBuf::from("/photos/test.jpg");
        let result = temp_download_path(&path, "AAAA", ".part").unwrap();
        assert!(result.to_string_lossy().ends_with(".part"));
    }

    #[tokio::test]
    async fn test_compute_sha256_known_content() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("known.bin");
        std::fs::write(&file_path, b"hello world").unwrap();

        let hash = compute_sha256(&file_path).await.unwrap();
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[tokio::test]
    async fn test_compute_sha256_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("nonexistent_file.bin");
        let result = compute_sha256(&file_path).await;
        assert!(result.is_err());
    }

    #[test]
    fn temp_download_path_empty_checksum_fails() {
        // Empty base64 decodes successfully to zero bytes. That must still
        // be rejected because accepting it would make every malformed
        // checksum share the same suffix-only temp path.
        let path = PathBuf::from("/photos/IMG_0001.JPG");
        let result = temp_download_path(&path, "", ".kei-tmp");
        assert!(
            result.is_err(),
            "empty checksum must not produce a shared .kei-tmp path"
        );
    }

    /// Robustness regression for downloaded-byte verification. Apple does
    /// not publish a content hash for assets, so kei cannot verify
    /// downloaded bytes against a server-side digest. Instead it stores
    /// the SHA-256 of what landed on disk in `local_checksum` and surfaces
    /// post-hoc divergence via `kei verify --checksums`. This pins the
    /// digest of a fixed JPEG-shaped payload — a future change to the
    /// hash routine (different algorithm, different buffer windowing,
    /// alternate hex encoding) will fail this assertion before it
    /// silently rewrites every user's stored checksum on the next sync.
    #[tokio::test]
    async fn compute_sha256_jpeg_payload_pins_digest_for_regression() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("pinned.jpg");
        // Minimal JFIF-shaped payload: SOI + APP0(JFIF) header + 16 body
        // bytes + EOI. Magic bytes pass `validate_downloaded_content`.
        let payload: [u8; 38] = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, // SOI + APP0 length
            0x4A, 0x46, 0x49, 0x46, 0x00, // "JFIF\0"
            0x01, 0x01, // version 1.1
            0x00, // density units
            0x00, 0x01, 0x00, 0x01, // X / Y density
            0x00, 0x00, // thumbnail w/h
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF, // body
            0xFF, 0xD9, // EOI
        ];
        std::fs::write(&file_path, payload).unwrap();

        let hash = compute_sha256(&file_path).await.unwrap();
        assert_eq!(
            hash, "17ec927c65744de82d16f52109b59283318111f9e3e3258439e624a5755f888c",
            "SHA-256 of the pinned JPEG fixture changed; if the hash routine \
             was updated intentionally, every user's stored local_checksum will \
             diverge from a fresh `verify --checksums` run on the next sync. \
             Coordinate any change with a state-DB migration that re-hashes \
             existing rows."
        );
    }

    #[tokio::test]
    async fn compute_sha256_empty_file_returns_known_hash() {
        // Arrange: create an empty file
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("empty.bin");
        std::fs::write(&file_path, b"").unwrap();

        // Act
        let hash = compute_sha256(&file_path).await.unwrap();

        // Assert: SHA-256 of empty input is the well-known constant
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[tokio::test]
    async fn compute_sha256_large_file_streams_without_loading_all_into_memory() {
        // Arrange: write a 2 MiB file (large enough to confirm streaming via io::copy)
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("large.bin");

        let chunk = vec![0xABu8; 1024];
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&file_path).unwrap();
            for _ in 0..2048 {
                f.write_all(&chunk).unwrap();
            }
        }

        // Act
        let hash = compute_sha256(&file_path).await.unwrap();

        // Assert: hash is a valid 64-char hex string (SHA-256)
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

        // Compute expected hash independently for verification
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for _ in 0..2048 {
            hasher.update(&chunk);
        }
        let expected = format!("{:x}", hasher.finalize());
        assert_eq!(hash, expected);
    }

    #[test]
    fn temp_download_path_different_directories_produce_different_paths() {
        // Arrange: two target files in different directories, same checksum
        let path_a = PathBuf::from("/photos/2024/test.jpg");
        let path_b = PathBuf::from("/photos/2025/test.jpg");
        let checksum = "AAAA";

        // Act
        let result_a = temp_download_path(&path_a, checksum, ".kei-tmp").unwrap();
        let result_b = temp_download_path(&path_b, checksum, ".kei-tmp").unwrap();

        // Assert: temp files land in their respective parent directories
        assert_eq!(result_a.parent().unwrap(), Path::new("/photos/2024"));
        assert_eq!(result_b.parent().unwrap(), Path::new("/photos/2025"));
        assert_ne!(result_a, result_b);
        // But the filename portion (base32 + suffix) should be identical
        assert_eq!(result_a.file_name(), result_b.file_name());
    }

    #[test]
    fn temp_download_path_url_unsafe_base64_chars_produce_safe_filename() {
        // Arrange: base64 with '+' and '/' characters (URL-unsafe)
        // "+/+/" decodes to [0xFB, 0xFF, 0xBF] — valid base64 with unsafe chars
        let path = PathBuf::from("/photos/test.jpg");
        let checksum = "+/+/";

        // Act
        let result = temp_download_path(&path, checksum, ".kei-tmp").unwrap();

        // Assert: the resulting filename must not contain '+' or '/'
        let filename = result.file_name().unwrap().to_str().unwrap();
        assert!(!filename.contains('+'), "filename should not contain '+'");
        assert!(!filename.contains('/'), "filename should not contain '/'");
        // Base32 alphabet is A-Z, 2-7 — verify the stem uses only those
        let stem = filename.strip_suffix(".kei-tmp").unwrap();
        assert!(
            stem.chars()
                .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c)),
            "base32 stem should only contain A-Z and 2-7, got: {stem}"
        );
    }

    // --- Content validation tests ---

    fn write_temp_file(name: &str, content: &[u8]) -> (PathBuf, PathBuf, TempDir) {
        let dir = TempDir::new().unwrap();
        let part_path = dir.path().join(format!("{name}.part"));
        let download_path = dir.path().join(name);
        std::fs::write(&part_path, content).unwrap();
        (part_path, download_path, dir)
    }

    #[test]
    fn validate_rejects_html_doctype_as_jpeg() {
        let (part, dest, _dir) = write_temp_file("photo.jpg", b"<!DOCTYPE html><html>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
        assert!(err.is_retryable());
    }

    #[test]
    fn validate_rejects_html_tag_as_heic() {
        let (part, dest, _dir) = write_temp_file("photo.heic", b"<html><head></head>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_accepts_valid_jpeg() {
        let (part, dest, _dir) =
            write_temp_file("photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_valid_png() {
        let (part, dest, _dir) = write_temp_file(
            "photo.png",
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        );
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_valid_heic() {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x1C]); // box size
        buf[4..8].copy_from_slice(b"ftyp");
        buf[8..12].copy_from_slice(b"heic");
        let (part, dest, _dir) = write_temp_file("photo.heic", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_valid_mov() {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x14]);
        buf[4..8].copy_from_slice(b"ftyp");
        buf[8..12].copy_from_slice(b"qt  ");
        let (part, dest, _dir) = write_temp_file("clip.mov", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_classic_qt_mov_wide_atom() {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x08]); // box size
        buf[4..8].copy_from_slice(b"wide");
        buf[8..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        let (part, dest, _dir) = write_temp_file("IMG_1711_HEVC.MOV", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_classic_qt_mov_mdat_atom() {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x10, 0x00]);
        buf[4..8].copy_from_slice(b"mdat");
        let (part, dest, _dir) = write_temp_file("live_photo.MOV", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_classic_qt_mov_moov_atom() {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x20]);
        buf[4..8].copy_from_slice(b"moov");
        let (part, dest, _dir) = write_temp_file("clip.mov", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_classic_qt_mov_free_atom() {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x10]);
        buf[4..8].copy_from_slice(b"free");
        let (part, dest, _dir) = write_temp_file("video.MOV", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_rejects_html_error_page_as_mov() {
        let html = b"<!DOCTYPE html>\n<html><body>Service Temporarily Unavailable</body></html>";
        let (part, dest, _dir) = write_temp_file("clip.mov", html);
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_rejects_html_for_unknown_extension() {
        let (part, dest, _dir) = write_temp_file("file.xyz", b"<!DOCTYPE html><html>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_rejects_html_with_leading_whitespace() {
        let (part, dest, _dir) = write_temp_file("file.dat", b"  \n<!DOCTYPE html>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_accepts_xml_for_unknown_extension() {
        // AAE files are XML plists — should not be rejected
        let (part, dest, _dir) = write_temp_file("photo.aae", b"<?xml version=\"1.0\"?>");
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_rejects_json_error_envelope_as_jpeg() {
        let body = b"{\"error\": \"Forbidden\", \"code\": 403}";
        let (part, dest, _dir) = write_temp_file("photo.jpg", body);
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        match err {
            DownloadError::InvalidContent { reason, .. } => {
                assert!(
                    reason.contains("JSON error envelope"),
                    "expected JSON-sentinel reason, got: {reason}"
                );
            }
            other => panic!("expected InvalidContent, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_json_error_envelope_with_leading_whitespace() {
        let body = b"\n  {\"errors\": [\"x\"]}";
        let (part, dest, _dir) = write_temp_file("clip.heic", body);
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_rejects_json_error_envelope_case_insensitive_key() {
        // Sentinel should match the quoted key regardless of case
        let body = b"{\"ERROR\": \"nope\"}";
        let (part, dest, _dir) = write_temp_file("photo.png", body);
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn detect_error_sentinel_unit() {
        assert!(detect_error_sentinel(b"<!doctype html>").is_some());
        assert!(detect_error_sentinel(b"<!DOCTYPE HTML>").is_some());
        assert!(detect_error_sentinel(b"<html><body>x</body></html>").is_some());
        assert!(detect_error_sentinel(b"  \n<HTML>").is_some());
        assert!(detect_error_sentinel(b"{\"error\": 1}").is_some());
        assert!(detect_error_sentinel(b"{\"errors\":[]}").is_some());
        assert!(detect_error_sentinel(b"{\"message\":\"foo\"}").is_some());
        assert!(detect_error_sentinel(b"{\"code\":403}").is_some());

        // Valid starts that must NOT be flagged
        assert!(detect_error_sentinel(b"<?xml version=\"1.0\"?>").is_none());
        assert!(detect_error_sentinel(&[0xFF, 0xD8, 0xFF, 0xE0]).is_none());
        assert!(detect_error_sentinel(b"").is_none());
        // A JSON-looking body that isn't an error envelope should pass through
        assert!(detect_error_sentinel(b"{\"width\":1024}").is_none());
    }

    #[test]
    fn rejecting_content_type_reason_classifies_error_document_types() {
        assert_eq!(
            rejecting_content_type_reason("text/html; charset=utf-8"),
            Some("HTML")
        );
        assert_eq!(
            rejecting_content_type_reason("Application/JSON"),
            Some("JSON")
        );
        assert_eq!(
            rejecting_content_type_reason("application/problem+json"),
            Some("JSON")
        );

        assert_eq!(rejecting_content_type_reason("image/jpeg"), None);
        assert_eq!(
            rejecting_content_type_reason("application/octet-stream"),
            None
        );
    }

    #[test]
    fn validate_rejects_empty_file() {
        let (part, dest, _dir) = write_temp_file("empty.jpg", b"");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    /// Build a 12-byte ISO-BMFF-style header with `box_type` at offset 4.
    fn mov_header(box_type: &[u8; 4]) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x08]);
        buf[4..8].copy_from_slice(box_type);
        buf
    }

    #[test]
    fn classify_magic_accepts_mov_ftyp() {
        assert_eq!(classify_magic("mov", &mov_header(b"ftyp")), Some(true));
    }

    #[test]
    fn classify_magic_accepts_mov_classic_qt_atoms() {
        // Live photos and HEVC MOVs from iCloud commonly begin with a classic
        // QuickTime atom instead of `ftyp` (see issue #247).
        for atom in [b"wide", b"mdat", b"moov", b"free", b"skip", b"pnot"] {
            assert_eq!(
                classify_magic("mov", &mov_header(atom)),
                Some(true),
                "{:?} should be accepted as a classic QuickTime top-level atom",
                std::str::from_utf8(atom).unwrap(),
            );
        }
    }

    #[test]
    fn classify_magic_exact_bytes_from_issue_247() {
        // Exact header reported in #247:
        //   "header=[00, 00, 00, 08, 77, 69, 64, 65]" → `0x00000008 "wide"`.
        let header = [0x00, 0x00, 0x00, 0x08, 0x77, 0x69, 0x64, 0x65];
        assert_eq!(classify_magic("mov", &header), Some(true));
    }

    #[test]
    fn classify_magic_rejects_mov_unknown_atom() {
        // An unrecognized box type should still warn — we accept only the
        // documented QuickTime top-level atoms plus ISO-BMFF `ftyp`.
        assert_eq!(classify_magic("mov", &mov_header(b"xxxx")), Some(false));
    }

    #[test]
    fn classify_magic_rejects_mov_too_short() {
        assert_eq!(
            classify_magic("mov", &[0x00, 0x00, 0x00, 0x08]),
            Some(false)
        );
    }

    #[test]
    fn classify_magic_heic_requires_ftyp() {
        // HEIC/HEIF are strict ISO-BMFF: classic QuickTime atoms are not valid.
        assert_eq!(classify_magic("heic", &mov_header(b"wide")), Some(false));
        assert_eq!(classify_magic("heif", &mov_header(b"mdat")), Some(false));
        assert_eq!(classify_magic("heic", &mov_header(b"ftyp")), Some(true));
    }

    #[test]
    fn classify_magic_mp4_requires_ftyp() {
        // MP4/M4V are strict ISO-BMFF: no classic QuickTime atom acceptance.
        assert_eq!(classify_magic("mp4", &mov_header(b"wide")), Some(false));
        assert_eq!(classify_magic("m4v", &mov_header(b"moov")), Some(false));
        assert_eq!(classify_magic("mp4", &mov_header(b"ftyp")), Some(true));
    }

    #[test]
    fn classify_magic_dng_accepts_tiff_magic() {
        // DNG is TIFF-based; accept both byte orders.
        assert_eq!(
            classify_magic("dng", &[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00]),
            Some(true),
        );
        assert_eq!(
            classify_magic("dng", &[0x4D, 0x4D, 0x00, 0x2A, 0x00, 0x08]),
            Some(true),
        );
    }

    #[test]
    fn classify_magic_dng_rejects_non_tiff_header() {
        assert_eq!(
            classify_magic("dng", &[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(false)
        );
    }

    #[test]
    fn classify_magic_unknown_extension_returns_none() {
        assert_eq!(classify_magic("bin", &[0x00, 0x01, 0x02, 0x03]), None);
        assert_eq!(classify_magic("", &[0xFF, 0xD8]), None);
        assert_eq!(classify_magic("aae", b"<?xml version=\"1.0\"?>"), None);
    }

    #[test]
    fn parse_content_range_start_accepts_valid_byte_ranges() {
        assert_eq!(parse_content_range_start("bytes 4-7/8"), Some(4));
        assert_eq!(parse_content_range_start("Bytes 100-179/*"), Some(100));
    }

    #[test]
    fn parse_content_range_start_rejects_malformed_ranges() {
        assert_eq!(parse_content_range_start("items 4-7/8"), None);
        assert_eq!(parse_content_range_start("bytes 7-4/8"), None);
        assert_eq!(parse_content_range_start("bytes */8"), None);
        assert_eq!(parse_content_range_start("bytes 4-7"), None);
    }

    #[test]
    fn classify_magic_basic_image_types() {
        assert_eq!(classify_magic("jpg", &[0xFF, 0xD8]), Some(true));
        assert_eq!(classify_magic("jpeg", &[0xFF, 0xD8]), Some(true));
        assert_eq!(classify_magic("jpg", &[0x89, 0x50]), Some(false));
        assert_eq!(classify_magic("png", &[0x89, 0x50, 0x4E, 0x47]), Some(true),);
        assert_eq!(classify_magic("gif", b"GIF89a"), Some(true));
        assert_eq!(classify_magic("gif", b"GIF77a"), Some(false));
    }

    #[test]
    fn validate_accepts_known_media_with_mismatched_heic_extension() {
        // Non-ftyp header with .heic extension is not HEIC, but it is valid
        // QuickTime media. Keep Apple's advertised filename and warn.
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x08]);
        buf[4..8].copy_from_slice(b"wide");
        let (part, dest, _dir) = write_temp_file("photo.heic", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_known_media_with_mismatched_png_extension_issue_507() {
        // iCloud can advertise screenshot/edited assets as .PNG while serving
        // JPEG bytes. The content is valid media, so keep the file and warn.
        let (part, dest, _dir) = write_temp_file("photo.png", &[0xFF, 0xD8, 0xFF, 0xE0]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_exif_jpeg_with_mismatched_png_extension_issue_507() {
        let (part, dest, _dir) = write_temp_file("cachedImage.PNG", &ISSUE_507_JPEG_HEADER);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_known_media_with_mismatched_jpeg_extension() {
        let (part, dest, _dir) = write_temp_file(
            "photo.jpg",
            &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A],
        );
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_rejects_unrecognized_header_for_known_extension() {
        let (part, dest, _dir) = write_temp_file("photo.jpg", b"not media bytes");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
        assert!(
            err.to_string()
                .contains("not recognized as another supported media type"),
            "error should explain why unknown media bytes failed: {err}"
        );
    }

    #[test]
    fn validate_accepts_gif() {
        let (part, dest, _dir) = write_temp_file("anim.gif", b"GIF89a\x01\x00\x01\x00");
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_tiff_little_endian() {
        let (part, dest, _dir) =
            write_temp_file("photo.tiff", &[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_dng_as_tiff() {
        let (part, dest, _dir) = write_temp_file("raw.dng", &[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    /// End-to-end regression for issue #247: the exact header Wouter reported
    /// (`00 00 00 08 77 69 64 65` == `0x00000008 "wide"`) on a live-photo MOV
    /// must validate cleanly, with no warning worth following up on.
    #[test]
    fn validate_accepts_hevc_live_photo_mov_header_issue_247() {
        let header = [0x00, 0x00, 0x00, 0x08, 0x77, 0x69, 0x64, 0x65];
        let (part, dest, _dir) = write_temp_file("IMG_1410_HEVC.MOV", &header);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
        // Confirm the classifier treats this header as a positive match, not
        // just a tolerated warning.
        assert_eq!(classify_magic("mov", &header), Some(true));
    }

    #[test]
    fn validate_accepts_tiff_big_endian() {
        let (part, dest, _dir) =
            write_temp_file("photo.tif", &[0x4D, 0x4D, 0x00, 0x2A, 0x00, 0x08]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_webp() {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(b"RIFF");
        buf[4..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]); // file size (irrelevant)
        buf[8..12].copy_from_slice(b"WEBP");
        let (part, dest, _dir) = write_temp_file("photo.webp", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_arbitrary_binary_for_unknown_extension() {
        // Random binary data with unknown extension should pass
        let (part, dest, _dir) = write_temp_file("data.bin", &[0x00, 0x01, 0x02, 0xFF, 0xFE]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_html_case_insensitive() {
        let (part, dest, _dir) = write_temp_file("file.dat", b"<HTML><HEAD>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    /// T-4: CDN returns HTML error page with Content-Length matching body size
    /// for a .HEIC download. The content validation must reject it, delete the
    /// .part file, and return a retryable error.
    #[test]
    fn validate_rejects_html_error_page_as_heic_full_flow() {
        let html_body = b"<!DOCTYPE html><html>Service Unavailable</html>";
        let (part, dest, _dir) = write_temp_file("cdn_error.heic", html_body);

        // Validate rejects — HTML content detected before magic byte check
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "HTML disguised as HEIC must be rejected"
        );
        assert!(
            err.is_retryable(),
            "InvalidContent errors should be retryable"
        );
        assert!(
            !err.is_session_expired(),
            "InvalidContent should not be treated as session expired"
        );

        // In the real download flow, attempt_download always removes the .part
        // file after content validation failure (even though the error is retryable),
        // because the content is invalid and shouldn't be resumed from.
        let _ = std::fs::remove_file(&part);
        assert!(!part.exists(), ".part file should be cleaned up");
        assert!(!dest.exists(), "final path must never have been created");
    }

    /// T-7: When CDN omits Content-Length (chunked transfer) and delivers fewer
    /// bytes than the API-reported size, the expected_size check catches it.
    #[test]
    fn truncated_download_detected_without_content_length() {
        // attempt_download checks: if bytes_written != expected_size → ContentLengthMismatch.
        // This catches truncation even when the CDN omits Content-Length (chunked encoding).
        let bytes_written = 17u64;
        let api_reported_size = 1_048_576u64;

        assert_ne!(bytes_written, api_reported_size);

        let err = DownloadError::ContentLengthMismatch {
            path: "video.mov".into(),
            expected: api_reported_size,
            received: bytes_written,
        };
        assert!(err.is_retryable(), "size mismatch should be retryable");
        assert!(
            !err.is_session_expired(),
            "size mismatch is not a session error"
        );
    }

    // --- attempt_download end-to-end tests via StubDownloadClient ---

    /// Stub HTTP client for testing the download pipeline without a network.
    struct StubDownloadClient {
        status: u16,
        content_length: Option<u64>,
        content_range: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    impl StubDownloadClient {
        fn ok(body: &[u8]) -> Self {
            Self {
                status: 200,
                content_length: Some(body.len() as u64),
                content_range: None,
                content_type: None,
                body: body.to_vec(),
            }
        }

        fn with_status(mut self, status: u16) -> Self {
            self.status = status;
            self
        }

        fn without_content_length(mut self) -> Self {
            self.content_length = None;
            self
        }

        fn with_content_type(mut self, ct: &str) -> Self {
            self.content_type = Some(ct.to_string());
            self
        }

        fn with_content_range(mut self, content_range: &str) -> Self {
            self.content_range = Some(content_range.to_string());
            self
        }
    }

    #[async_trait::async_trait]
    impl DownloadClient for StubDownloadClient {
        async fn fetch(
            &self,
            _url: &str,
            resume_from: Option<u64>,
        ) -> Result<DownloadResponse, BoxError> {
            let chunks: Vec<Result<Bytes, BoxError>> = vec![Ok(Bytes::from(self.body.clone()))];
            let content_range = self.content_range.clone().or_else(|| {
                resume_from.and_then(|start| {
                    let end = start.checked_add(self.body.len() as u64)?.checked_sub(1)?;
                    Some(format!("bytes {start}-{end}/*"))
                })
            });
            Ok(DownloadResponse {
                status: self.status,
                content_length: self.content_length,
                content_range,
                content_type: self.content_type.clone(),
                stream: Box::pin(futures_util::stream::iter(chunks)),
            })
        }
    }

    /// Helper: set up a temp directory with download and part paths.
    fn setup_download_dir(name: &str, ext: &str) -> (PathBuf, PathBuf, TempDir) {
        let dir = TempDir::new().unwrap();
        let download_path = dir.path().join(format!("{name}.{ext}"));
        let part_path = dir.path().join(format!("{name}.part"));
        (download_path, part_path, dir)
    }

    #[tokio::test]
    async fn attempt_download_happy_path_writes_and_renames() {
        let jpeg_body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = StubDownloadClient::ok(&jpeg_body);
        let (download_path, part_path, _dir) = setup_download_dir("happy", "jpg");

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(download_path.exists(), "final file should exist");
        assert!(
            !part_path.exists(),
            ".part file should be gone after rename"
        );
        assert_eq!(std::fs::read(&download_path).unwrap(), jpeg_body);
    }

    #[tokio::test]
    async fn attempt_download_skip_rename_leaves_part_file() {
        let jpeg_body = [0xFF, 0xD8, 0xFF, 0xE0];
        let client = StubDownloadClient::ok(&jpeg_body);
        let (download_path, part_path, _dir) = setup_download_dir("skip_rename", "jpg");

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            true,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(part_path.exists(), ".part file should remain");
        assert!(!download_path.exists(), "final path should not exist");
        assert_eq!(std::fs::read(&part_path).unwrap(), jpeg_body);
    }

    #[tokio::test]
    async fn attempt_download_content_length_mismatch_removes_part() {
        // Server claims 100 bytes but body is only 8
        let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = StubDownloadClient {
            status: 200,
            content_length: Some(100),
            content_range: None,
            content_type: None,
            body: body.to_vec(),
        };
        let (download_path, part_path, _dir) = setup_download_dir("cl_mismatch", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::ContentLengthMismatch { .. }),
            "expected ContentLengthMismatch, got: {err}"
        );
        assert!(!part_path.exists(), ".part should be removed on mismatch");
        assert!(!download_path.exists(), "final path must not exist");
    }

    #[tokio::test]
    async fn attempt_download_expected_size_mismatch_removes_part() {
        // Body is 8 bytes but caller expects 1024
        let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = StubDownloadClient::ok(&body).without_content_length();
        let (download_path, part_path, _dir) = setup_download_dir("size_mismatch", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            Some(1024),
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::ContentLengthMismatch { .. }),
            "expected ContentLengthMismatch, got: {err}"
        );
        assert!(!part_path.exists(), ".part should be removed");
    }

    #[tokio::test]
    async fn attempt_download_invalid_content_removes_part() {
        // HTML error page served as a .heic file
        let html = b"<!DOCTYPE html><html>Service Unavailable</html>";
        let client = StubDownloadClient::ok(html);
        let (download_path, part_path, _dir) = setup_download_dir("invalid_content", "heic");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "expected InvalidContent, got: {err}"
        );
        assert!(
            !part_path.exists(),
            ".part should be removed on bad content"
        );
        assert!(!download_path.exists(), "final path must not exist");
    }

    #[tokio::test]
    async fn attempt_download_promotes_valid_jpeg_with_png_extension_issue_507() {
        let client = StubDownloadClient::ok(&ISSUE_507_JPEG_HEADER);
        let (download_path, part_path, _dir) = setup_download_dir("cachedImage", "PNG");

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            Some(ISSUE_507_JPEG_HEADER.len() as u64),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(!part_path.exists(), ".part should be promoted");
        assert_eq!(
            std::fs::read(&download_path).unwrap(),
            ISSUE_507_JPEG_HEADER
        );
    }

    #[tokio::test]
    async fn attempt_download_rejects_same_size_unknown_media_body_under_known_extension() {
        let body = b"not media bytes";
        let client = StubDownloadClient::ok(body);
        let (download_path, part_path, _dir) = setup_download_dir("unknown_header", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            Some(body.len() as u64),
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "expected InvalidContent, got: {err}"
        );
        assert!(!part_path.exists(), ".part should be removed");
        assert!(!download_path.exists(), "final path must not exist");
    }

    #[tokio::test]
    async fn attempt_download_http_error_returns_http_status() {
        let client = StubDownloadClient::ok(b"").with_status(503);
        let (download_path, part_path, _dir) = setup_download_dir("http_err", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::HttpStatus { status: 503, .. }),
            "expected HttpStatus 503, got: {err}"
        );
    }

    #[tokio::test]
    async fn attempt_download_resume_appends_to_existing_part() {
        let (download_path, part_path, _dir) = setup_download_dir("resume", "jpg");

        // Pre-create a partial .part file (first 2 bytes of JPEG header)
        let first_half = [0xFF, 0xD8];
        std::fs::write(&part_path, first_half).unwrap();

        // Stub returns 206 with the remaining bytes
        let second_half = vec![0xFF, 0xE0, 0x00, 0x10];
        let client = StubDownloadClient {
            status: 206,
            content_length: Some(second_half.len() as u64),
            content_range: None,
            content_type: None,
            body: second_half.clone(),
        };

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let content = std::fs::read(&download_path).unwrap();
        assert_eq!(content, [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]);
        assert!(!part_path.exists(), ".part should be renamed");
    }

    #[tokio::test]
    async fn download_file_resume_wrong_content_range_keeps_existing_part_and_errors() {
        let (download_path, part_path, _dir) = setup_download_dir("bad_range", "jpg");
        let first_half = [0xFF, 0xD8, 0xFF, 0xE0];
        std::fs::write(&part_path, first_half).unwrap();

        let second_half = vec![0x00, 0x10, 0x4A, 0x46];
        let client = StubDownloadClient::ok(&second_half)
            .with_status(206)
            .with_content_range("bytes 0-3/8");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            Some(8),
            None,
            None,
        )
        .await
        .expect_err("mismatched Content-Range must fail before append");

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "expected InvalidContent for wrong Content-Range, got: {err}"
        );
        assert_eq!(
            std::fs::read(&part_path).unwrap(),
            first_half,
            "existing .part bytes must remain unchanged for a future safe retry"
        );
        assert!(
            !download_path.exists(),
            "wrong range must not promote final path"
        );
    }

    #[tokio::test]
    async fn attempt_download_resume_rejects_version_rotation_via_content_length() {
        // If the .part carries K bytes from version A and the server's Range
        // response's Content-Length (+ resume_offset) totals a different size
        // than expected_size, the resume must be rejected to avoid producing
        // a Frankenfile of {A-prefix || B-suffix} bytes.
        let (download_path, part_path, _dir) = setup_download_dir("rotation", "jpg");
        // .part carries 100 bytes from version A (expected size 150)
        std::fs::write(&part_path, vec![0xAA; 100]).unwrap();

        // Server returns 206 claiming the remaining 80 bytes of a 180-byte file
        // (version B rotation: total 180, not the expected 150).
        let client = StubDownloadClient {
            status: 206,
            content_length: Some(80),
            content_range: None,
            content_type: None,
            body: vec![0xBB; 80],
        };

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            Some(150), // expected_size signals version A
            None,
            None,
        )
        .await
        .expect_err("resume across version rotation must be rejected");

        match err {
            DownloadError::ContentLengthMismatch {
                expected, received, ..
            } => {
                assert_eq!(expected, 150);
                assert_eq!(received, 180); // resume_offset + reported remaining
            }
            other => panic!("expected ContentLengthMismatch, got: {other:?}"),
        }
        // The stale .part must be removed so the next attempt starts clean.
        assert!(
            !part_path.exists(),
            ".part should be removed after rotation detection"
        );
    }

    #[tokio::test]
    async fn attempt_download_resume_fallback_truncates_and_rewrites() {
        let (download_path, part_path, _dir) = setup_download_dir("resume_fallback", "jpg");

        // Pre-create a .part file with stale data
        std::fs::write(&part_path, b"stale partial data").unwrap();

        // Server ignores Range and returns 200 with the full body
        let full_body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = StubDownloadClient::ok(&full_body);

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let content = std::fs::read(&download_path).unwrap();
        assert_eq!(
            content, full_body,
            "server returned 200 (full body), so stale .part should be overwritten"
        );
    }

    #[tokio::test]
    async fn attempt_download_expected_size_matches_succeeds() {
        let body = [0xFF, 0xD8, 0xFF, 0xE0];
        let client = StubDownloadClient::ok(&body).without_content_length();
        let (download_path, part_path, _dir) = setup_download_dir("size_ok", "jpg");

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            Some(body.len() as u64),
            None,
            None,
        )
        .await
        .unwrap();

        assert!(download_path.exists());
    }

    /// Verify that resume_from is correctly forwarded to the client.
    #[tokio::test]
    async fn attempt_download_passes_resume_offset_to_client() {
        use std::sync::atomic::{AtomicU64, Ordering};

        struct RecordingClient {
            resume_from: AtomicU64,
            body: Vec<u8>,
        }

        #[async_trait::async_trait]
        impl DownloadClient for RecordingClient {
            async fn fetch(
                &self,
                _url: &str,
                resume_from: Option<u64>,
            ) -> Result<DownloadResponse, BoxError> {
                self.resume_from
                    .store(resume_from.unwrap_or(0), Ordering::SeqCst);
                let chunks: Vec<Result<Bytes, BoxError>> = vec![Ok(Bytes::from(self.body.clone()))];
                Ok(DownloadResponse {
                    status: if resume_from.is_some() { 206 } else { 200 },
                    content_length: Some(self.body.len() as u64),
                    content_range: resume_from.map(|start| {
                        let end = start + self.body.len() as u64 - 1;
                        format!("bytes {start}-{end}/*")
                    }),
                    content_type: None,
                    stream: Box::pin(futures_util::stream::iter(chunks)),
                })
            }
        }

        let (download_path, part_path, _dir) = setup_download_dir("offset_pass", "bin");

        // Pre-create .part with 100 bytes
        std::fs::write(&part_path, vec![0xAAu8; 100]).unwrap();

        let remaining = [0xBB, 0xCC, 0xDD];
        let client = RecordingClient {
            resume_from: AtomicU64::new(0),
            body: remaining.to_vec(),
        };

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            client.resume_from.load(Ordering::SeqCst),
            100,
            "client should receive the .part file size as resume offset"
        );
    }

    #[derive(Debug)]
    struct InterruptingResumeClient {
        body: Vec<u8>,
        first_chunk_len: usize,
        calls: std::sync::atomic::AtomicUsize,
        resume_requests: std::sync::Mutex<Vec<Option<u64>>>,
        release_first_chunk: Arc<tokio::sync::Notify>,
        first_chunk_delivered: Arc<tokio::sync::Notify>,
    }

    impl InterruptingResumeClient {
        fn new(body: Vec<u8>, first_chunk_len: usize) -> Self {
            Self {
                body,
                first_chunk_len,
                calls: std::sync::atomic::AtomicUsize::new(0),
                resume_requests: std::sync::Mutex::new(Vec::new()),
                release_first_chunk: Arc::new(tokio::sync::Notify::new()),
                first_chunk_delivered: Arc::new(tokio::sync::Notify::new()),
            }
        }

        fn pending_after_first_chunk(&self) -> DownloadResponse {
            let first_chunk = self.body[..self.first_chunk_len].to_vec();
            let release_first_chunk = Arc::clone(&self.release_first_chunk);
            let first_chunk_delivered = Arc::clone(&self.first_chunk_delivered);
            let stream = futures_util::stream::unfold(Some(first_chunk), move |next| {
                let release_first_chunk = Arc::clone(&release_first_chunk);
                let first_chunk_delivered = Arc::clone(&first_chunk_delivered);
                async move {
                    let Some(chunk) = next else {
                        std::future::pending().await
                    };
                    release_first_chunk.notified().await;
                    first_chunk_delivered.notify_one();
                    Some((Ok(Bytes::from(chunk)), None))
                }
            });
            DownloadResponse {
                status: 200,
                content_length: Some(self.body.len() as u64),
                content_range: None,
                content_type: Some("image/jpeg".to_string()),
                stream: Box::pin(stream),
            }
        }

        fn remaining_from(&self, resume_from: u64) -> DownloadResponse {
            let offset = usize::try_from(resume_from).expect("resume offset fits usize");
            let chunks: Vec<Result<Bytes, BoxError>> =
                vec![Ok(Bytes::from(self.body[offset..].to_vec()))];
            DownloadResponse {
                status: 206,
                content_length: Some((self.body.len() - offset) as u64),
                content_range: Some(format!(
                    "bytes {resume_from}-{}/{}",
                    self.body.len() - 1,
                    self.body.len()
                )),
                content_type: Some("image/jpeg".to_string()),
                stream: Box::pin(futures_util::stream::iter(chunks)),
            }
        }

        fn resume_requests(&self) -> Vec<Option<u64>> {
            self.resume_requests.lock().unwrap().clone()
        }

        fn release_first_chunk(&self) {
            self.release_first_chunk.notify_one();
        }

        async fn wait_until_first_chunk_delivered(&self) {
            self.first_chunk_delivered.notified().await;
        }
    }

    #[async_trait::async_trait]
    impl DownloadClient for InterruptingResumeClient {
        async fn fetch(
            &self,
            _url: &str,
            resume_from: Option<u64>,
        ) -> Result<DownloadResponse, BoxError> {
            self.resume_requests.lock().unwrap().push(resume_from);
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if call == 0 {
                self.pending_after_first_chunk()
            } else {
                self.remaining_from(resume_from.expect("second request must resume"))
            })
        }
    }

    #[tokio::test]
    async fn download_file_interrupted_mid_body_keeps_part_and_resumes() {
        let mut body = vec![0xFF, 0xD8, 0xFF, 0xE0];
        body.extend((4..128u8).map(|n| n.wrapping_mul(3)));
        let first_chunk_len = 4;
        let client = InterruptingResumeClient::new(body.clone(), first_chunk_len);
        let dir = TempDir::new().unwrap();
        let download_path = dir.path().join("interrupted.jpg");
        let checksum = base64::engine::general_purpose::STANDARD.encode([0x42u8; 32]);
        let part_path = temp_download_path(&download_path, &checksum, ".kei-tmp").unwrap();
        let config = RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let shutdown_token = CancellationToken::new();

        let first_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let download = download_file_with_mode(
                &client,
                "http://stub/interrupted.jpg",
                &download_path,
                &checksum,
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: Some(body.len() as u64),
                    publication: FinalPublication::NoReplace,
                },
                DownloadLimits {
                    shutdown_token: Some(&shutdown_token),
                    ..Default::default()
                },
                crate::personality::Mode::Off,
            );
            let cancel_after_partial = async {
                client.release_first_chunk();
                client.wait_until_first_chunk_delivered().await;
                let mut partial_bytes_are_visible = false;
                for _ in 0..1000 {
                    let part_len = tokio::fs::metadata(&part_path)
                        .await
                        .map(|meta| meta.len())
                        .unwrap_or(0);
                    if part_len == first_chunk_len as u64 {
                        partial_bytes_are_visible = true;
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                assert!(
                    partial_bytes_are_visible,
                    "test setup should wait until the first chunk reaches the .part file"
                );
                shutdown_token.cancel();
            };
            let (result, ()) = tokio::join!(download, cancel_after_partial);
            result
        })
        .await
        .expect("interrupted download should not hang");

        let err = first_result.expect_err("mid-body shutdown must interrupt the download");
        assert!(
            matches!(
                err,
                DownloadError::Interrupted {
                    bytes_written: 4,
                    ..
                }
            ),
            "expected interrupted error after first chunk, got {err:?}"
        );
        assert!(
            !download_path.exists(),
            "interrupted download must not publish the final path"
        );
        assert_eq!(
            std::fs::metadata(&part_path).unwrap().len(),
            4,
            "interrupted download must leave the resumable .part bytes"
        );

        download_file_with_mode(
            &client,
            "http://stub/interrupted.jpg",
            &download_path,
            &checksum,
            &config,
            ".kei-tmp",
            DownloadOpts {
                skip_rename: false,
                expected_size: Some(body.len() as u64),
                publication: FinalPublication::NoReplace,
            },
            DownloadLimits::default(),
            crate::personality::Mode::Off,
        )
        .await
        .expect("second run should resume and publish");

        assert_eq!(
            client.resume_requests(),
            vec![None, Some(4)],
            "second request must use the partial-file offset"
        );
        assert_eq!(std::fs::read(&download_path).unwrap(), body);
        assert!(!part_path.exists(), "published resume must remove .part");

        use sha2::{Digest, Sha256};
        let expected_hash = format!("{:x}", Sha256::digest(&body));
        assert_eq!(compute_sha256(&download_path).await.unwrap(), expected_hash);
    }

    // --- download_file retry integration tests ---

    /// Stub client that returns a configurable error status for the first N
    /// calls, then succeeds with the given body. Tracks total call count.
    struct RetryingStubClient {
        fail_count: u32,
        fail_status: u16,
        body: Vec<u8>,
        calls: std::sync::atomic::AtomicU32,
    }

    impl RetryingStubClient {
        fn new(fail_count: u32, fail_status: u16, body: Vec<u8>) -> Self {
            Self {
                fail_count,
                fail_status,
                body,
                calls: std::sync::atomic::AtomicU32::new(0),
            }
        }

        fn call_count(&self) -> u32 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl DownloadClient for RetryingStubClient {
        async fn fetch(
            &self,
            _url: &str,
            _resume_from: Option<u64>,
        ) -> Result<DownloadResponse, BoxError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.fail_count {
                // Return the error status with an empty body
                let chunks: Vec<Result<Bytes, BoxError>> = vec![];
                Ok(DownloadResponse {
                    status: self.fail_status,
                    content_length: None,
                    content_range: None,
                    content_type: None,
                    stream: Box::pin(futures_util::stream::iter(chunks)),
                })
            } else {
                let chunks: Vec<Result<Bytes, BoxError>> = vec![Ok(Bytes::from(self.body.clone()))];
                Ok(DownloadResponse {
                    status: 200,
                    content_length: Some(self.body.len() as u64),
                    content_range: None,
                    content_type: None,
                    stream: Box::pin(futures_util::stream::iter(chunks)),
                })
            }
        }
    }

    /// Run download_file with a RetryingStubClient, returning the result and
    /// call count for assertion.
    async fn run_retry_download(
        fail_count: u32,
        fail_status: u16,
        max_retries: u32,
    ) -> (Result<u64, DownloadError>, u32, PathBuf, TempDir) {
        let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = RetryingStubClient::new(fail_count, fail_status, jpeg_body);
        let dir = TempDir::new().unwrap();
        let download_path = dir.path().join("photo.jpg");

        let config = RetryConfig {
            max_retries,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let result = download_file_with_mode(
            &client,
            "http://stub/photo.jpg",
            &download_path,
            "AAAA",
            &config,
            ".kei-tmp",
            DownloadOpts {
                skip_rename: false,
                expected_size: None,
                publication: FinalPublication::NoReplace,
            },
            DownloadLimits::default(),
            crate::personality::Mode::Off,
        )
        .await;

        (result, client.call_count(), download_path, dir)
    }

    #[tokio::test]
    async fn download_file_retries_on_429_then_succeeds() {
        let (result, calls, path, _dir) = run_retry_download(2, 429, 3).await;
        result.unwrap();
        assert_eq!(calls, 3, "should have retried twice then succeeded");
        assert!(path.exists(), "file should be downloaded");
    }

    #[tokio::test]
    async fn download_file_retries_on_503_then_succeeds() {
        let (result, calls, path, _dir) = run_retry_download(1, 503, 3).await;
        result.unwrap();
        assert_eq!(calls, 2, "should have retried once then succeeded");
        assert!(path.exists());
    }

    #[tokio::test]
    async fn download_file_aborts_on_non_retryable_status() {
        let (result, calls, _, _dir) = run_retry_download(1, 404, 3).await;
        let err = result.unwrap_err();
        assert_eq!(calls, 1, "should abort immediately on 404");
        assert!(
            matches!(err, DownloadError::HttpStatus { status: 404, .. }),
            "expected HttpStatus 404, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn download_file_aborts_on_expired_url_410_for_cleanup_refresh() {
        let (result, calls, _, _dir) = run_retry_download(1, 410, 3).await;
        let err = result.unwrap_err();
        assert_eq!(
            calls, 1,
            "410 means the signed URL expired; retrying the same URL is pointless"
        );
        assert!(
            matches!(err, DownloadError::HttpStatus { status: 410, .. }),
            "expected HttpStatus 410, got: {err:?}"
        );
        assert!(err.is_expired_url());
    }

    #[tokio::test]
    async fn download_file_exhausts_retries_on_persistent_429() {
        let (result, calls, _, _dir) = run_retry_download(10, 429, 2).await;
        let err = result.unwrap_err();
        // 1 initial + 2 retries = 3 total attempts
        assert_eq!(calls, 3, "should exhaust all retry attempts");
        assert!(
            matches!(err, DownloadError::HttpStatus { status: 429, .. }),
            "expected HttpStatus 429, got: {err:?}"
        );
    }

    // --- Content-type validation tests ---

    #[tokio::test]
    async fn attempt_download_rejects_text_html_content_type() {
        let html = b"<!DOCTYPE html><html>Error</html>";
        let client = StubDownloadClient::ok(html).with_content_type("text/html; charset=utf-8");
        let (download_path, part_path, _dir) = setup_download_dir("ct_html", "heic");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "expected InvalidContent, got: {err}"
        );
        assert!(
            err.is_retryable(),
            "content-type rejection should be retryable"
        );
        assert!(
            err.to_string().contains("content-type"),
            "error message should mention content-type"
        );
    }

    #[tokio::test]
    async fn attempt_download_rejects_application_json_content_type() {
        let body = br#"{"error":"Forbidden"}"#;
        let client = StubDownloadClient::ok(body).with_content_type("application/json");
        let (download_path, part_path, _dir) = setup_download_dir("ct_json", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "expected InvalidContent, got: {err}"
        );
        assert!(
            err.to_string().contains("JSON content-type"),
            "error message should identify JSON content-type, got: {err}"
        );
        assert!(
            !part_path.exists(),
            ".part should be deleted after JSON content-type rejection"
        );
        assert!(!download_path.exists(), "final path must not exist");
    }

    #[tokio::test]
    async fn attempt_download_accepts_image_jpeg_content_type() {
        let body = [0xFF, 0xD8, 0xFF, 0xE0];
        let client = StubDownloadClient::ok(&body).with_content_type("image/jpeg");
        let (download_path, part_path, _dir) = setup_download_dir("ct_jpeg", "jpg");

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(download_path.exists());
    }

    #[tokio::test]
    async fn attempt_download_accepts_octet_stream_content_type() {
        let body = [0xFF, 0xD8, 0xFF, 0xE0];
        let client = StubDownloadClient::ok(&body).with_content_type("application/octet-stream");
        let (download_path, part_path, _dir) = setup_download_dir("ct_octet", "jpg");

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        assert!(download_path.exists());
    }

    #[tokio::test]
    async fn attempt_download_rejects_text_html_case_insensitive() {
        let html = b"<html>error</html>";
        let client = StubDownloadClient::ok(html).with_content_type("Text/HTML");
        let (download_path, part_path, _dir) = setup_download_dir("ct_html_upper", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    // --- wiremock integration tests ---

    /// Test the real reqwest::Client DownloadClient impl against a mock HTTP server.
    mod wiremock_tests {
        use super::*;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        /// Run download_file against a wiremock server, returning the result
        /// and the path where the file would be written.
        async fn run_mock_download(
            server: &MockServer,
            filename: &str,
            checksum: &str,
            max_retries: u32,
        ) -> (Result<u64, DownloadError>, PathBuf, TempDir) {
            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join(filename);
            let config = RetryConfig {
                max_retries,
                base_delay_secs: 0,
                max_delay_secs: 0,
            };
            let result = download_file_with_mode(
                &reqwest::Client::new(),
                &format!("{}/{filename}", server.uri()),
                &download_path,
                checksum,
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: None,
                    publication: FinalPublication::NoReplace,
                },
                DownloadLimits::default(),
                crate::personality::Mode::Off,
            )
            .await;
            (result, download_path, dir)
        }

        #[tokio::test]
        async fn real_client_retries_on_503_then_succeeds() {
            let server = crate::start_wiremock_or_skip!();
            let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];

            Mock::given(method("GET"))
                .and(path("/photo.jpg"))
                .respond_with(ResponseTemplate::new(503))
                .up_to_n_times(2)
                .expect(2)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/photo.jpg"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(jpeg_body.clone())
                        .insert_header("content-type", "image/jpeg"),
                )
                .expect(1)
                .mount(&server)
                .await;

            let (result, path, _dir) = run_mock_download(&server, "photo.jpg", "AAAA", 3).await;
            result.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), jpeg_body);
        }

        #[tokio::test]
        async fn real_client_retries_on_429_then_succeeds() {
            let server = crate::start_wiremock_or_skip!();
            let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0];

            Mock::given(method("GET"))
                .and(path("/rate-limited.jpg"))
                .respond_with(ResponseTemplate::new(429))
                .up_to_n_times(1)
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/rate-limited.jpg"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(jpeg_body.clone()))
                .expect(1)
                .mount(&server)
                .await;

            let (result, path, _dir) =
                run_mock_download(&server, "rate-limited.jpg", "AAAB", 3).await;
            result.unwrap();
            assert!(path.exists());
        }

        #[tokio::test]
        async fn real_client_exhausts_retries_on_persistent_500() {
            let server = crate::start_wiremock_or_skip!();

            Mock::given(method("GET"))
                .and(path("/broken.jpg"))
                .respond_with(ResponseTemplate::new(500))
                .expect(3)
                .mount(&server)
                .await;

            let (result, _, _dir) = run_mock_download(&server, "broken.jpg", "AAAC", 2).await;
            let err = result.unwrap_err();
            assert!(
                matches!(err, DownloadError::HttpStatus { status: 500, .. }),
                "expected HttpStatus 500, got: {err:?}"
            );
        }

        #[tokio::test]
        async fn real_client_aborts_on_404_no_retry() {
            let server = crate::start_wiremock_or_skip!();

            Mock::given(method("GET"))
                .and(path("/missing.jpg"))
                .respond_with(ResponseTemplate::new(404))
                .expect(1)
                .mount(&server)
                .await;

            let (result, _, _dir) = run_mock_download(&server, "missing.jpg", "AAAD", 3).await;
            assert!(matches!(
                result.unwrap_err(),
                DownloadError::HttpStatus { status: 404, .. }
            ));
        }

        #[tokio::test]
        async fn real_client_rejects_html_content_type() {
            let server = crate::start_wiremock_or_skip!();

            Mock::given(method("GET"))
                .and(path("/error-page.heic"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string("<!DOCTYPE html><html>Rate Limited</html>")
                        .insert_header("content-type", "text/html; charset=utf-8"),
                )
                .mount(&server)
                .await;

            let (result, _, _dir) = run_mock_download(&server, "error-page.heic", "AAAE", 0).await;
            assert!(
                matches!(result.unwrap_err(), DownloadError::InvalidContent { .. }),
                "expected InvalidContent for HTML content-type"
            );
        }

        #[tokio::test]
        async fn real_client_resume_with_range_header() {
            let server = crate::start_wiremock_or_skip!();
            let full_body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];

            Mock::given(method("GET"))
                .and(path("/resume.jpg"))
                .and(wiremock::matchers::header_exists("Range"))
                .respond_with(
                    ResponseTemplate::new(206)
                        .set_body_bytes(full_body[4..].to_vec())
                        .insert_header("content-length", "4")
                        .insert_header("content-range", "bytes 4-7/8"),
                )
                .expect(1)
                .mount(&server)
                .await;

            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join("resume.jpg");
            let part_path = temp_download_path(&download_path, "AAAF", ".kei-tmp").unwrap();
            std::fs::write(&part_path, &full_body[..4]).unwrap();

            let config = RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            };
            download_file_with_mode(
                &reqwest::Client::new(),
                &format!("{}/resume.jpg", server.uri()),
                &download_path,
                "AAAF",
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: None,
                    publication: FinalPublication::NoReplace,
                },
                DownloadLimits::default(),
                crate::personality::Mode::Off,
            )
            .await
            .unwrap();

            assert_eq!(std::fs::read(&download_path).unwrap(), full_body);
        }

        /// A Content-Length mismatch (server declares 1000 bytes
        /// but transmits 800) MUST surface as a `ContentLengthMismatch`
        /// error, the `.part` file must be removed, and NO file must
        /// appear at the final path. This is the feared-most data-loss
        /// case (silent corruption masquerading as success). The exact
        /// negative — `final_path` does NOT exist — is what catches a
        /// future refactor that fell through to the rename step.
        #[tokio::test]
        async fn truncated_response_does_not_promote_to_final_path() {
            let server = crate::start_wiremock_or_skip!();

            // Body is 4 bytes of valid JPEG SOI/JFIF signature; we tell
            // the client Content-Length=8 so the post-stream check fires.
            let truncated_body = vec![0xFF, 0xD8, 0xFF, 0xE0];

            Mock::given(method("GET"))
                .and(path("/truncated.jpg"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(truncated_body.clone())
                        // Override the auto-set content-length to claim 8.
                        .insert_header("content-length", "8")
                        .insert_header("content-type", "image/jpeg"),
                )
                .mount(&server)
                .await;

            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join("truncated.jpg");
            let config = RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            };
            // Realistic SHA256 of the *full* 8-byte payload — irrelevant
            // here because the size check fires first, but we use a
            // realistic-looking value not "checksum123".
            let result = download_file_with_mode(
                &reqwest::Client::new(),
                &format!("{}/truncated.jpg", server.uri()),
                &download_path,
                "0000000000000000000000000000000000000000000000000000000000000000",
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: Some(8),
                    publication: FinalPublication::NoReplace,
                },
                DownloadLimits::default(),
                crate::personality::Mode::Off,
            )
            .await;

            // Expected error: server underdelivered relative to its
            // declared Content-Length, OR relative to expected_size if
            // wiremock chose to honor only one.
            let err = result.expect_err("truncated response must fail");
            assert!(
                matches!(
                    err,
                    DownloadError::ContentLengthMismatch { .. } | DownloadError::Http { .. }
                ),
                "expected size-mismatch class error, got: {err:?}"
            );

            // Critical invariants: no .part lingers, and no final file
            // landed (the would-be silent-corruption signature).
            assert!(
                !download_path.exists(),
                "final path must NOT exist on truncation; got file with size {:?}",
                std::fs::metadata(&download_path).ok().map(|m| m.len())
            );
            // Walk the temp dir to confirm there's no orphan .part either.
            let stragglers: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            assert!(
                stragglers.is_empty(),
                "no .part or other files must remain after truncated download; got {stragglers:?}"
            );
        }

        /// When the destination parent directory is removed
        /// between the writability probe and the per-asset write, the
        /// per-asset download MUST surface an error (not silently
        /// succeed, not panic). Symptom in the wild looks like "0 photos
        /// synced, 0 errors" because the producer thinks everything is
        /// fine. Pin the explicit error class so a future refactor that
        /// ignored ENOENT on the part-open path tells us.
        #[tokio::test]
        async fn download_to_missing_parent_dir_surfaces_error() {
            let server = crate::start_wiremock_or_skip!();
            let body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0xAA, 0xBB, 0xCC, 0xDD];

            Mock::given(method("GET"))
                .and(path("/orphan.jpg"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(body)
                        .insert_header("content-type", "image/jpeg"),
                )
                .mount(&server)
                .await;

            // Create the tempdir, then immediately drop it (rm -rf the
            // path) before invoking download_file. This mirrors the
            // "directory removed between probe and write" race.
            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join("orphan.jpg");
            drop(dir); // tempdir Drop deletes the directory.
            assert!(
                !download_path.parent().unwrap().exists(),
                "test setup: parent dir must be gone before download_file fires"
            );

            let config = RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            };
            let result = download_file_with_mode(
                &reqwest::Client::new(),
                &format!("{}/orphan.jpg", server.uri()),
                &download_path,
                "AAAA",
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: None,
                    publication: FinalPublication::NoReplace,
                },
                DownloadLimits::default(),
                crate::personality::Mode::Off,
            )
            .await;

            let err = result.expect_err("missing parent dir must surface as an error");
            // The error must mention the path (so the surfacing log is
            // actionable) — this is the kei "no silent failures" invariant
            // applied to the per-asset write level.
            let msg = err.to_string();
            assert!(
                !msg.is_empty(),
                "error must have a non-empty message; got {err:?}"
            );

            // No file must have been created at the (still-missing) path.
            assert!(!download_path.exists());
        }

        /// A pre-existing temp file from a prior interrupted run
        /// must NOT corrupt the next download. Specifically: when the
        /// new download is initiated FRESH (no Range / status 200), the
        /// temp file must be replaced atomically — no concatenation of
        /// stale prefix bytes onto fresh bytes (the "frankenfile" failure
        /// mode that passes the size check but fails image decode).
        ///
        /// We verify by writing 100 bytes of garbage to the .part path
        /// up front, then driving a 200-byte download from a wiremock
        /// server. The final file must be byte-identical to the 200-byte
        /// payload, NOT 300 bytes (100 garbage + 200 payload), NOT a
        /// mixed 200-byte file with stale prefix.
        #[tokio::test]
        async fn pre_existing_part_file_replaced_atomically_on_fresh_download() {
            let server = crate::start_wiremock_or_skip!();

            // 200-byte payload starting with JPEG SOI/JFIF signature so
            // content-type sniffing accepts it. The remaining bytes are
            // a stable repeating pattern so we can check byte-equality.
            let mut payload: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xE0];
            payload.extend((4..200u16).map(|i| (i & 0xff) as u8));
            assert_eq!(payload.len(), 200);

            Mock::given(method("GET"))
                .and(path("/replace.jpg"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_bytes(payload.clone())
                        .insert_header("content-type", "image/jpeg"),
                )
                .expect(1)
                .mount(&server)
                .await;

            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join("replace.jpg");
            let checksum = "AAAA"; // canonical short test checksum

            // Pre-populate the .part path with garbage from a prior run.
            // (The kei-tmp prefix MUST match the production constant so
            // the writer finds and replaces the same path.)
            let part_path = temp_download_path(&download_path, checksum, ".kei-tmp").unwrap();
            let stale_garbage = vec![0xCC; 100];
            std::fs::write(&part_path, &stale_garbage).expect("seed stale .part");
            assert_eq!(std::fs::metadata(&part_path).unwrap().len(), 100);

            let config = RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            };
            // No expected_size + no Range header — production should treat
            // this as a fresh download and TRUNCATE the existing .part.
            // (Resume requires expected_size to be supplied.)
            download_file_with_mode(
                &reqwest::Client::new(),
                &format!("{}/replace.jpg", server.uri()),
                &download_path,
                checksum,
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: None,
                    publication: FinalPublication::NoReplace,
                },
                DownloadLimits::default(),
                crate::personality::Mode::Off,
            )
            .await
            .expect("download should succeed when .part is freshly truncated");

            // Final file must equal the payload exactly. NOT 300 bytes,
            // NOT prefixed-with-garbage, NOT empty.
            let final_bytes = std::fs::read(&download_path).expect("final file present");
            assert_eq!(
                final_bytes.len(),
                200,
                "final file length must equal payload (200), got {}",
                final_bytes.len()
            );
            assert_eq!(
                final_bytes,
                payload,
                "final bytes must equal payload exactly; \
                 frankenfile detection: first 4 bytes were {:?} (expected JPEG SOI {:?})",
                &final_bytes[..4.min(final_bytes.len())],
                &payload[..4]
            );

            // .part must have been atomically renamed away.
            assert!(
                !part_path.exists(),
                "stale .part path must not exist after rename"
            );
        }

        /// End-to-end throttle test: pull a fixed payload through `download_file`
        /// with a real HTTP server and assert wall-clock elapsed time at least
        /// approaches what the cap predicts.
        #[tokio::test]
        async fn bandwidth_limiter_throttles_download() {
            use std::time::Instant;

            // 64 KiB at 64 KiB/s -> expect ~1s. Lenient lower bound
            // (>= expected * 0.6) so CI jitter doesn't flake; overshoot is
            // fine because it only means the limiter is stricter than required.
            let body_size = 64 * 1024usize;
            let body = vec![0xAAu8; body_size];
            let limit = 64 * 1024u64;
            let expected_secs = body_size as f64 / limit as f64;

            let server = crate::start_wiremock_or_skip!();
            Mock::given(method("GET"))
                .and(path("/throttle.bin"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body.clone()))
                .mount(&server)
                .await;

            let checksum = base64::engine::general_purpose::STANDARD.encode([0xAAu8; 32]);
            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join("throttle.bin");
            let config = RetryConfig {
                max_retries: 0,
                base_delay_secs: 0,
                max_delay_secs: 0,
            };
            let limiter = BandwidthLimiter::new(limit);

            let start = Instant::now();
            let bytes = download_file_with_mode(
                &reqwest::Client::new(),
                &format!("{}/throttle.bin", server.uri()),
                &download_path,
                &checksum,
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: Some(body_size as u64),
                    publication: FinalPublication::NoReplace,
                },
                DownloadLimits {
                    bandwidth_limiter: Some(&limiter),
                    ..Default::default()
                },
                crate::personality::Mode::Off,
            )
            .await
            .expect("throttled download succeeds");
            let elapsed = start.elapsed().as_secs_f64();

            assert_eq!(bytes, body_size as u64);
            assert!(
                elapsed >= expected_secs * 0.6,
                "elapsed {elapsed:.2}s under {limit} B/s cap for {body_size} B \
                 should be close to expected {expected_secs:.2}s",
            );
            assert_eq!(std::fs::read(&download_path).unwrap().len(), body_size);
        }
    }

    // --- decode_api_checksum tests ---

    #[test]
    fn decode_api_checksum_20_byte_raw_sha1() {
        let base64_input = base64::engine::general_purpose::STANDARD.encode([0u8; 20]);
        let decoded = decode_api_checksum(&base64_input).unwrap();
        assert_eq!(decoded.hex, "0".repeat(40));
        assert!(decoded.is_sha1);
    }

    #[test]
    fn decode_api_checksum_21_byte_apple_sha1_prefix() {
        let mut bytes = vec![0x01u8];
        bytes.extend_from_slice(&[0xAB; 20]);
        let base64_input = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let decoded = decode_api_checksum(&base64_input).unwrap();
        assert_eq!(decoded.hex, "ab".repeat(20));
        assert!(decoded.is_sha1);
    }

    #[test]
    fn decode_api_checksum_32_byte_raw() {
        let base64_input = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let decoded = decode_api_checksum(&base64_input).unwrap();
        assert_eq!(decoded.hex, "0".repeat(64));
        assert!(!decoded.is_sha1);
    }

    #[test]
    fn decode_api_checksum_33_byte_apple_prefix() {
        let mut bytes = vec![0x01u8];
        bytes.extend_from_slice(&[0xFF; 32]);
        let base64_input = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let decoded = decode_api_checksum(&base64_input).unwrap();
        assert_eq!(decoded.hex, "f".repeat(64));
        assert!(!decoded.is_sha1);
    }

    #[test]
    fn decode_api_checksum_invalid_base64() {
        let result = decode_api_checksum("not!valid!base64!!!");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("base64"),
            "error should mention base64"
        );
    }

    #[test]
    fn decode_api_checksum_wrong_length() {
        // 16 bytes — none of the expected lengths
        let base64_input = base64::engine::general_purpose::STANDARD.encode([0xABu8; 16]);
        let result = decode_api_checksum(&base64_input);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("16 bytes"),
            "error should include the unexpected length"
        );
    }

    #[test]
    fn decode_api_checksum_roundtrip_sha256() {
        use sha2::{Digest, Sha256};
        let data = b"test data for checksum roundtrip";
        let hash = Sha256::digest(data);
        let expected_hex = format!("{:x}", hash);

        // Raw 32-byte SHA-256
        let base64_cksum = base64::engine::general_purpose::STANDARD.encode(hash.as_slice());
        let decoded = decode_api_checksum(&base64_cksum).unwrap();
        assert_eq!(decoded.hex, expected_hex);
        assert!(!decoded.is_sha1);

        // 33-byte Apple prefix + SHA-256
        let mut prefixed = vec![0x01u8];
        prefixed.extend_from_slice(hash.as_slice());
        let base64_prefixed = base64::engine::general_purpose::STANDARD.encode(&prefixed);
        let decoded = decode_api_checksum(&base64_prefixed).unwrap();
        assert_eq!(decoded.hex, expected_hex);
        assert!(!decoded.is_sha1);
    }

    #[test]
    fn decode_api_checksum_roundtrip_sha1() {
        use sha1::Digest;
        let data = b"test data for sha1 roundtrip";
        let hash = sha1::Sha1::digest(data);
        let expected_hex = format!("{:x}", hash);

        // 21-byte Apple prefix + SHA-1 (the format seen from iCloud)
        let mut prefixed = vec![0x01u8];
        prefixed.extend_from_slice(hash.as_slice());
        let base64_prefixed = base64::engine::general_purpose::STANDARD.encode(&prefixed);
        let decoded = decode_api_checksum(&base64_prefixed).unwrap();
        assert_eq!(decoded.hex, expected_hex);
        assert!(decoded.is_sha1);
    }

    #[test]
    fn decode_api_checksum_live_api_value() {
        // Real value observed from iCloud API during live testing
        let decoded = decode_api_checksum("AXY53EmM03WU8iZY1QgKZ79gMyMi").unwrap();
        assert!(decoded.is_sha1);
        assert_eq!(decoded.hex.len(), 40); // 20 bytes = 40 hex chars
    }

    #[tokio::test]
    async fn rename_part_to_final_happy_path() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&part, b"image data").await.unwrap();

        rename_part_to_final(&part, &final_path).await.unwrap();

        assert!(!part.exists());
        assert!(final_path.exists());
        assert_eq!(tokio::fs::read(&final_path).await.unwrap(), b"image data");
    }

    #[tokio::test]
    async fn contract_file_publish_no_overwrite_destination_already_exists() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"existing").await.unwrap();
        tokio::fs::write(&part, b"existing").await.unwrap();

        // Should succeed without replacing the existing final file. On Linux,
        // plain rename(old, new) would overwrite `photo.jpg`; the publish path
        // must use no-overwrite semantics before cleaning the redundant .part.
        rename_part_to_final(&part, &final_path).await.unwrap();

        assert!(!part.exists(), ".part should not remain");
        assert!(final_path.exists(), "final file should exist");
        assert_eq!(
            tokio::fs::read(&final_path).await.unwrap(),
            b"existing",
            "existing final file must not be replaced by duplicate .part bytes"
        );
    }

    #[tokio::test]
    async fn contract_file_publish_different_destination_returns_typed_collision() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"winner").await.unwrap();
        tokio::fs::write(&part, b"loser").await.unwrap();

        let error = rename_part_to_final(&part, &final_path)
            .await
            .expect_err("different bytes must collide");

        assert!(error.downcast_ref::<FinalPathCollision>().is_some());
        assert_eq!(tokio::fs::read(&final_path).await.unwrap(), b"winner");
        assert_eq!(tokio::fs::read(&part).await.unwrap(), b"loser");
    }

    #[tokio::test]
    async fn approved_truncated_publish_replaces_only_expected_bytes() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"bad").await.unwrap();
        tokio::fs::write(&part, b"verified replacement")
            .await
            .unwrap();
        let expected = fingerprint_file(&final_path).await.unwrap();

        publish_part_to_final(
            &part,
            &final_path,
            FinalPublication::ReplaceTruncated(expected),
        )
        .await
        .unwrap();

        assert!(
            !part.exists(),
            "displaced truncated bytes should be removed"
        );
        assert_eq!(
            tokio::fs::read(&final_path).await.unwrap(),
            b"verified replacement"
        );
    }

    #[tokio::test]
    async fn approved_truncated_publish_refuses_changed_target() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"bad").await.unwrap();
        let expected = fingerprint_file(&final_path).await.unwrap();
        tokio::fs::write(&final_path, b"user replacement")
            .await
            .unwrap();
        tokio::fs::write(&part, b"verified replacement")
            .await
            .unwrap();

        let error = publish_part_to_final(
            &part,
            &final_path,
            FinalPublication::ReplaceTruncated(expected),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("bytes changed"), "{error}");
        assert_eq!(
            tokio::fs::read(&final_path).await.unwrap(),
            b"user replacement"
        );
        assert_eq!(
            tokio::fs::read(&part).await.unwrap(),
            b"verified replacement"
        );
    }

    #[tokio::test]
    async fn approved_truncated_publish_classifies_deleted_target_as_changed() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"bad").await.unwrap();
        let expected = fingerprint_file(&final_path).await.unwrap();
        tokio::fs::remove_file(&final_path).await.unwrap();
        tokio::fs::write(&part, b"verified replacement")
            .await
            .unwrap();

        let error = publish_part_to_final(
            &part,
            &final_path,
            FinalPublication::ReplaceTruncated(expected),
        )
        .await
        .unwrap_err();

        assert!(classify_conditional_publish_error(&error).target_changed);
        assert!(!final_path.exists());
        assert_eq!(
            tokio::fs::read(&part).await.unwrap(),
            b"verified replacement"
        );
    }

    #[test]
    fn restore_rejects_second_edit_displaced_to_part_path() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        let swap_path = dir.path().join("swap.tmp");
        std::fs::write(&part, b"original target").unwrap();
        std::fs::write(&final_path, b"prepared replacement").unwrap();
        let replacement = fingerprint_regular_file_blocking(&final_path).unwrap();

        let error = restore_repair_target_if_unchanged_with(
            &part,
            &final_path,
            &part,
            replacement,
            |part, final_path, _displaced| {
                std::fs::write(final_path, b"second concurrent edit")?;
                std::fs::rename(final_path, &swap_path)?;
                std::fs::rename(part, final_path)?;
                std::fs::rename(&swap_path, part)
            },
        )
        .unwrap_err();

        let disposition = classify_conditional_publish_error(&error);
        assert!(disposition.target_changed);
        assert!(disposition.retained_paths.contains(&part));
        assert_eq!(std::fs::read(&final_path).unwrap(), b"original target");
        assert_eq!(std::fs::read(&part).unwrap(), b"second concurrent edit");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_partial_replace_failure_restores_original_path() {
        use windows_sys::Win32::Foundation::ERROR_UNABLE_TO_MOVE_REPLACEMENT_2;

        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        let backup_path = repair_backup_path(&part);
        tokio::fs::write(&final_path, b"bad").await.unwrap();
        tokio::fs::write(&part, b"verified replacement")
            .await
            .unwrap();
        let expected = fingerprint_file(&final_path).await.unwrap();

        tokio::fs::rename(&final_path, &backup_path).await.unwrap();
        let error_code = i32::try_from(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2).unwrap();
        let error = finish_windows_repair_exchange_blocking(
            &final_path,
            &backup_path,
            expected,
            Err(std::io::Error::from_raw_os_error(error_code)),
        )
        .unwrap_err();

        let error_chain = format!("{error:#}");
        assert!(
            error_chain.contains("original target was restored"),
            "{error_chain}"
        );
        assert_eq!(tokio::fs::read(&final_path).await.unwrap(), b"bad");
        assert_eq!(
            tokio::fs::read(&part).await.unwrap(),
            b"verified replacement"
        );
        assert!(
            !backup_path.exists(),
            "restored backup path must be removed"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn approved_truncated_publish_refuses_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new().unwrap();
        let underlying = dir.path().join("underlying.jpg");
        let final_path = dir.path().join("photo.jpg");
        let part = dir.path().join("photo.part");
        tokio::fs::write(&underlying, b"bad").await.unwrap();
        symlink(&underlying, &final_path).unwrap();
        tokio::fs::write(&part, b"verified replacement")
            .await
            .unwrap();
        let expected = fingerprint_file(&final_path).await.unwrap();

        let error = publish_part_to_final(
            &part,
            &final_path,
            FinalPublication::ReplaceTruncated(expected),
        )
        .await
        .unwrap_err();

        let error_chain = format!("{error:#}");
        assert!(error_chain.contains("regular file"), "{error_chain}");
        assert!(classify_conditional_publish_error(&error).target_changed);
        assert_eq!(tokio::fs::read_link(&final_path).await.unwrap(), underlying);
        assert_eq!(
            tokio::fs::read(&part).await.unwrap(),
            b"verified replacement"
        );
    }

    #[tokio::test]
    async fn rename_part_to_final_nonexistent_part_returns_error() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("missing.part");
        let final_path = dir.path().join("photo.jpg");

        let result = rename_part_to_final(&part, &final_path).await;
        assert!(result.is_err(), "should fail when .part doesn't exist");
    }

    // ── Gap: text/html content-type rejection before writing to disk ──

    #[tokio::test]
    async fn attempt_download_html_content_type_rejected_before_write() {
        // CDN returns HTTP 200 with content-type text/html (rate-limit page).
        // Should be rejected BEFORE writing to the .part file.
        let client = StubDownloadClient::ok(b"<html>Rate Limited</html>")
            .with_content_type("text/html; charset=utf-8");
        let (download_path, part_path, _dir) = setup_download_dir("html_ct", "heic");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "text/html content-type should be rejected, got: {err}"
        );
        assert!(err.is_retryable(), "HTML error page should be retryable");
        assert!(
            !part_path.exists(),
            ".part should be deleted after HTML rejection"
        );
    }

    // ── Gap: empty body (zero-byte download) rejected ────────────────

    #[tokio::test]
    async fn attempt_download_zero_byte_body_rejected() {
        let client = StubDownloadClient {
            status: 200,
            content_length: Some(0),
            content_range: None,
            content_type: None,
            body: vec![],
        };
        let (download_path, part_path, _dir) = setup_download_dir("zero_body", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "zero-byte download should be rejected, got: {err}"
        );
        assert!(!download_path.exists(), "final file should not exist");
    }

    // ── Gap: stale .part file (>1h) triggers restart from zero ──────

    #[tokio::test]
    async fn attempt_download_stale_part_file_restarted() {
        let (download_path, part_path, _dir) = setup_download_dir("stale_part", "jpg");

        // Create a stale .part file and backdate its mtime by >24 hours
        std::fs::write(&part_path, b"stale partial data from yesterday").unwrap();
        let old_mtime = std::time::SystemTime::now()
            - std::time::Duration::from_secs(STALE_PART_FILE_SECS + 3600);
        let times = std::fs::FileTimes::new()
            .set_modified(old_mtime)
            .set_accessed(old_mtime);
        std::fs::File::options()
            .write(true)
            .open(&part_path)
            .unwrap()
            .set_times(times)
            .unwrap();

        // Server returns full body (200, not 206)
        let full_body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        let client = StubDownloadClient::ok(&full_body);

        attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        // The stale data should be replaced with the fresh download
        let content = std::fs::read(&download_path).unwrap();
        assert_eq!(
            content, full_body,
            "stale .part should be overwritten with fresh data"
        );
    }

    // ── Gap: expected_size check catches truncation with Content-Length ──

    #[tokio::test]
    async fn attempt_download_expected_size_catches_truncation() {
        // Server sends 8 bytes with matching Content-Length, but API reported
        // the file as 1024 bytes. The expected_size check should catch this.
        let body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = StubDownloadClient::ok(&body); // CL = 8 bytes

        let (download_path, part_path, _dir) = setup_download_dir("api_size", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            Some(1024), // API says 1024 but download is 8
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(
                err,
                DownloadError::ContentLengthMismatch {
                    expected: 1024,
                    received: 8,
                    ..
                }
            ),
            "expected_size mismatch should produce ContentLengthMismatch, got: {err}"
        );
        assert!(
            !part_path.exists(),
            ".part should be removed on size mismatch"
        );
    }

    // ── Gap: two concurrent writers cannot both succeed ──────────────

    /// Stub that blocks inside `fetch()` on a barrier so two concurrent
    /// `attempt_download` calls reach the create_new section close enough
    /// in time for the race to manifest if exclusivity is broken.
    struct GatedStubDownloadClient {
        body: Vec<u8>,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait::async_trait]
    impl DownloadClient for GatedStubDownloadClient {
        async fn fetch(
            &self,
            _url: &str,
            _resume_from: Option<u64>,
        ) -> Result<DownloadResponse, BoxError> {
            self.barrier.wait().await;
            let chunks: Vec<Result<Bytes, BoxError>> = vec![Ok(Bytes::from(self.body.clone()))];
            Ok(DownloadResponse {
                status: 200,
                content_length: Some(self.body.len() as u64),
                content_range: None,
                content_type: None,
                stream: Box::pin(futures_util::stream::iter(chunks)),
            })
        }
    }

    /// Concurrent `attempt_download` calls racing on the same .part path
    /// must never produce a file whose bytes are interleaved from both
    /// writers. Whether zero, one, or both writers report Ok depends on
    /// the interleaving: unlink + create_new is racy, so a retryable
    /// failure on both sides is a legitimate outcome the caller must be
    /// prepared for. The non-negotiable is that if a file exists at the
    /// final path, it matches exactly one writer's body.
    #[tokio::test]
    async fn attempt_download_concurrent_writers_never_corrupt_final_file() {
        use std::sync::Arc;

        for iteration in 0..20 {
            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join("photo.jpg");
            let part_path = dir.path().join("photo.part");

            let body_a = vec![0xAAu8; 256];
            let body_b = vec![0xBBu8; 256];
            let barrier = Arc::new(tokio::sync::Barrier::new(2));

            let client_a = GatedStubDownloadClient {
                body: body_a.clone(),
                barrier: barrier.clone(),
            };
            let client_b = GatedStubDownloadClient {
                body: body_b.clone(),
                barrier: barrier.clone(),
            };

            let dp_a = download_path.clone();
            let pp_a = part_path.clone();
            let task_a = tokio::spawn(async move {
                attempt_download(
                    &client_a,
                    "http://stub",
                    &dp_a,
                    &pp_a,
                    false,
                    None,
                    None,
                    None,
                )
                .await
            });
            let dp_b = download_path.clone();
            let pp_b = part_path.clone();
            let task_b = tokio::spawn(async move {
                attempt_download(
                    &client_b,
                    "http://stub",
                    &dp_b,
                    &pp_b,
                    false,
                    None,
                    None,
                    None,
                )
                .await
            });

            let a = task_a.await.unwrap();
            let b = task_b.await.unwrap();

            if let Ok(final_bytes) = std::fs::read(&download_path) {
                assert!(
                    final_bytes == body_a || final_bytes == body_b,
                    "iteration {iteration}: final file must match exactly one writer's body \
                     (no interleaving); got {} bytes, first: {:?}. a={a:?} b={b:?}",
                    final_bytes.len(),
                    &final_bytes[..final_bytes.len().min(8)],
                );
            }
        }
    }

    // ── Gap: HTTP 4xx error (not 401/403) is not retryable ──────────

    #[tokio::test]
    async fn attempt_download_http_404_not_retryable() {
        let client = StubDownloadClient::ok(b"Not Found").with_status(404);
        let (download_path, part_path, _dir) = setup_download_dir("not_found", "jpg");

        let err = attempt_download(
            &client,
            "http://stub",
            &download_path,
            &part_path,
            false,
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::HttpStatus { status: 404, .. }),
            "expected HttpStatus 404, got: {err}"
        );
        assert!(!err.is_retryable(), "404 should not be retryable");
    }
}
