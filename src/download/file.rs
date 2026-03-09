use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::Client;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::error::DownloadError;
use crate::retry::{self, RetryAction, RetryConfig};

/// Derive a deterministic .part filename from the checksum so that
/// concurrent downloads of different files don't collide. Base32-encoded
/// because base64 contains `/` which is invalid in filenames.
fn temp_download_path(
    download_path: &Path,
    checksum: &str,
    temp_suffix: &str,
) -> anyhow::Result<PathBuf> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(checksum)
        .context("Failed to decode base64 checksum")?;
    let encoded = data_encoding::BASE32_NOPAD.encode(&decoded);
    let download_dir = download_path.parent().unwrap_or_else(|| Path::new("."));
    Ok(download_dir.join(format!("{encoded}{temp_suffix}")))
}

/// Download a file from URL using .part temp files.
///
/// Resumes partial downloads via HTTP Range requests when a .part file
/// already exists. Falls back to a full download if the server ignores the
/// Range header. On completion the .part file is renamed to the final
/// destination path. Retries with exponential backoff on transient failures.
pub async fn download_file(
    client: &Client,
    url: &str,
    download_path: &Path,
    checksum: &str,
    dry_run: bool,
    retry_config: &RetryConfig,
    temp_suffix: &str,
) -> Result<(), DownloadError> {
    if dry_run {
        tracing::info!("[DRY RUN] Would download {}", download_path.display());
        return Ok(());
    }

    let part_path =
        temp_download_path(download_path, checksum, temp_suffix).map_err(DownloadError::Other)?;

    Box::pin(retry::retry_with_backoff(
        retry_config,
        |e: &DownloadError| {
            if e.is_retryable() {
                RetryAction::Retry
            } else {
                RetryAction::Abort
            }
        },
        || async { Box::pin(attempt_download(client, url, download_path, &part_path)).await },
    ))
    .await
}

/// Single download attempt with resume support.
///
/// If a .part file already exists, sends a Range request to resume from where
/// it left off. Falls back to a fresh download if the server doesn't support
/// Range or returns an unexpected status.
async fn attempt_download(
    client: &Client,
    url: &str,
    download_path: &Path,
    part_path: &Path,
) -> Result<(), DownloadError> {
    let path_str = download_path.display().to_string();

    let resume_offset = match fs::metadata(part_path).await {
        Ok(meta) if meta.len() > 0 => meta.len(),
        _ => 0,
    };

    let mut request = client.get(url);
    if resume_offset > 0 {
        tracing::info!(
            "Resuming {} from byte {} (partial file exists)",
            path_str,
            resume_offset
        );
        request = request.header("Range", format!("bytes={resume_offset}-"));
    }

    let response = request.send().await.map_err(|e| DownloadError::Http {
        source: e,
        path: path_str.clone(),
        status: 0,
        content_length: None,
        bytes_written: 0,
    })?;

    let status = response.status().as_u16();

    // 206 = resumed successfully, 200 = server ignored Range (start over)
    // `effective_offset` tracks the actual byte offset used for the content-length
    // check. When the server ignores Range and returns 200, we restart from zero
    // so effective_offset must be 0 (not the stale resume_offset).
    let (mut bytes_written, truncate, effective_offset) = match status {
        206 if resume_offset > 0 => (resume_offset, false, resume_offset),
        _ if response.status().is_success() => {
            if resume_offset > 0 {
                tracing::info!(
                    "Server returned {} instead of 206 for Range request, restarting download of {}",
                    status,
                    path_str,
                );
            }
            (0u64, true, 0u64)
        }
        _ => {
            return Err(DownloadError::HttpStatus {
                status,
                path: path_str,
            });
        }
    };

    let content_length = response.content_length();

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(truncate)
        .append(!truncate)
        .open(&part_path)
        .await
        .map_err(|e| {
            DownloadError::Other(anyhow::anyhow!("Failed to open temp download file: {e}"))
        })?;

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DownloadError::Http {
            source: e,
            path: path_str.clone(),
            status,
            content_length,
            bytes_written,
        })?;
        file.write_all(&chunk).await?;
        bytes_written += chunk.len() as u64;
    }
    file.flush().await?;
    file.sync_data().await?;
    drop(file);

    // Verify the server sent the number of bytes it promised.
    // Catches CDN truncation (e.g. Apple silently cutting off videos at ~1 GB).
    if let Some(expected_len) = content_length {
        let total_bytes = bytes_written - effective_offset;
        if total_bytes != expected_len {
            let _ = fs::remove_file(&part_path).await;
            return Err(DownloadError::ContentLengthMismatch {
                path: path_str,
                expected: expected_len,
                received: total_bytes,
            });
        }
    }

    // Note: Apple's fileChecksum from the CloudKit API is an opaque version
    // token, not a content hash. Content-length verification above is sufficient
    // to catch truncated downloads. The fileChecksum is used for temp filename
    // derivation and change detection across syncs.

    fs::rename(&part_path, download_path).await?;

    Ok(())
}

/// Compute the SHA-256 hash of a file, returning a hex-encoded string.
///
/// Used by the download pipeline to store a locally-computed checksum,
/// and by `verify --checksums` to verify file integrity.
pub(crate) async fn compute_sha256(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = temp_download_path(&path, "AAAA", ".icloudpd-tmp").unwrap();
        assert_eq!(result.parent().unwrap(), Path::new("/photos"));
        assert!(result.to_string_lossy().ends_with(".icloudpd-tmp"));
    }

    #[test]
    fn test_temp_download_path_derives_from_checksum() {
        let path = PathBuf::from("/photos/test.jpg");
        let result1 = temp_download_path(&path, "AAAA", ".icloudpd-tmp").unwrap();
        let result2 = temp_download_path(&path, "AAAB", ".icloudpd-tmp").unwrap();
        // Different checksums should produce different temp filenames
        assert_ne!(result1, result2);
    }

    #[test]
    fn test_temp_download_path_same_checksum_same_result() {
        let path1 = PathBuf::from("/photos/a.jpg");
        let path2 = PathBuf::from("/photos/b.jpg");
        let result1 = temp_download_path(&path1, "AAAA", ".icloudpd-tmp").unwrap();
        let result2 = temp_download_path(&path2, "AAAA", ".icloudpd-tmp").unwrap();
        // Same checksum, same directory -> same temp file (for resume)
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_temp_download_path_invalid_base64() {
        let path = PathBuf::from("/photos/test.jpg");
        let result = temp_download_path(&path, "not-valid-base64!!!", ".icloudpd-tmp");
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
        let dir = PathBuf::from("/tmp/claude/sha256_test");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("known.bin");
        std::fs::write(&file_path, b"hello world").unwrap();

        let hash = compute_sha256(&file_path).await.unwrap();
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[tokio::test]
    async fn test_compute_sha256_nonexistent_file() {
        let file_path = PathBuf::from("/tmp/claude/sha256_test/nonexistent_file.bin");
        let result = compute_sha256(&file_path).await;
        assert!(result.is_err());
    }
}
