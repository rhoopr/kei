//! HTTP requests, retry, resume, and temporary-file writes.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Context;
use base64::Engine;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::download::error::DownloadError;
use crate::download::limiter::BandwidthLimiter;
use crate::retry::{self, RetryAction, RetryConfig};

use super::publication::publish_part_to_final;
use super::replacement::FinalPublication;
use super::validation::{
    rejecting_content_type_reason, validate_download_lengths, validate_downloaded_content,
    validate_resume_content_range, validate_resume_size,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// HTTP response from a download request.
pub(in crate::download) struct DownloadResponse {
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
pub(in crate::download) trait DownloadClient: Send + Sync {
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
pub(in crate::download) fn temp_download_path(
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
pub(in crate::download) struct DownloadOpts {
    /// Keep the `.part` file instead of renaming to the final path.
    pub skip_rename: bool,
    /// API-reported file size. When set, verifies total bytes written match,
    /// catching truncation even when the CDN omits `Content-Length`.
    pub expected_size: Option<u64>,
    /// Final-path publication policy selected by retry planning.
    pub publication: FinalPublication,
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
pub(in crate::download) struct DownloadLimits<'a> {
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
pub(in crate::download) async fn download_file_with_mode<C: DownloadClient>(
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
                    tracing::warn!(target: "kei::download::file",
                        path = %part_path.display(),
                        error = %e,
                        "Cannot read .part file mtime, treating as stale"
                    );
                    true
                }
            };
            if stale {
                tracing::warn!(target: "kei::download::file",
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
        tracing::info!(target: "kei::download::file",
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
                tracing::info!(target: "kei::download::file",
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

    if status == 206
        && resume_offset > 0
        && let Err(error) =
            validate_resume_size(resume_offset, content_length, expected_size, &path_str)
    {
        crate::fs_util::log_remove_async(part_path).await;
        return Err(error);
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

    if let Err(error) = validate_download_lengths(
        bytes_written,
        effective_offset,
        content_length,
        expected_size,
        &path_str,
    ) {
        crate::fs_util::log_remove_async(part_path).await;
        return Err(error);
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

#[cfg(test)]
mod tests;
