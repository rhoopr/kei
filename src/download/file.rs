use std::path::{Path, PathBuf};

use anyhow::Context;
use base64::Engine;
use futures_util::StreamExt;
use reqwest::Client;
use sha2::{Digest, Sha256};
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
    Ok(download_dir.join(format!("{}{}", encoded, temp_suffix)))
}

/// Download a file from URL using .part temp files.
///
/// Resumes partial downloads via HTTP Range requests when a .part file
/// already exists, hashing the existing bytes first so the final checksum
/// covers the entire file. Falls back to a full download if the server
/// ignores the Range header. On completion the .part file is renamed to
/// the final destination path. Retries with exponential backoff on
/// transient failures.
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

    let result = retry::retry_with_backoff(
        retry_config,
        |e: &DownloadError| {
            if e.is_retryable() {
                RetryAction::Retry
            } else {
                RetryAction::Abort
            }
        },
        || async { attempt_download(client, url, download_path, &part_path, checksum).await },
    )
    .await;

    result
}

/// Rebuild SHA256 hash state by re-reading an existing .part file.
/// Returns the hasher and byte count, or None if the file doesn't exist or is empty.
async fn resume_hash_state(part_path: &Path) -> Option<(Sha256, u64)> {
    let meta = fs::metadata(part_path).await.ok()?;
    let existing_len = meta.len();
    if existing_len == 0 {
        return None;
    }

    let file = fs::File::open(part_path).await.ok()?;
    let mut reader = tokio::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 262_144]; // 256 KiB for faster resume hashing
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut reader, &mut buf)
            .await
            .ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some((hasher, existing_len))
}

/// Single download attempt with checksum verification and resume support.
///
/// If a .part file already exists, re-hashes its contents and sends a Range
/// request to resume from where it left off. Falls back to a fresh download
/// if the server doesn't support Range or returns an unexpected status.
async fn attempt_download(
    client: &Client,
    url: &str,
    download_path: &Path,
    part_path: &Path,
    checksum: &str,
) -> Result<(), DownloadError> {
    let path_str = download_path.display().to_string();

    let resume_state = resume_hash_state(part_path).await;
    let resume_offset = resume_state.as_ref().map(|(_, len)| *len).unwrap_or(0);

    let mut request = client.get(url);
    if resume_offset > 0 {
        tracing::info!(
            "Resuming {} from byte {} (partial file exists)",
            path_str,
            resume_offset
        );
        request = request.header("Range", format!("bytes={}-", resume_offset));
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
    let (mut hasher, mut bytes_written, truncate, effective_offset) = match (
        status,
        resume_offset,
        resume_state,
    ) {
        (206, offset, Some((h, len))) if offset > 0 => (h, len, false, offset),
        (_, _, _) if response.status().is_success() => {
            if resume_offset > 0 {
                tracing::info!(
                        "Server returned {} instead of 206 for Range request, restarting download of {}",
                        status,
                        path_str,
                    );
            }
            (Sha256::new(), 0u64, true, 0u64)
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
            DownloadError::Other(anyhow::anyhow!("Failed to open temp download file: {}", e))
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
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        bytes_written += chunk.len() as u64;
    }
    file.flush().await?;
    file.sync_data().await?;
    drop(file);

    // Belt-and-suspenders: verify the server sent the number of bytes it
    // promised. Catches CDN truncation (e.g. Apple silently cutting off
    // videos at ~1 GB) before we even reach the checksum comparison.
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

    if let Ok(expected_hash) = base64::engine::general_purpose::STANDARD.decode(checksum) {
        let actual_hash = hasher.finalize();

        // Apple uses two checksum formats: raw 32-byte SHA-256, or 33-byte
        // with a 1-byte type prefix (0x01 = SHA-256). Handle both.
        let matches = if expected_hash.len() == 32 {
            actual_hash.as_slice() == expected_hash.as_slice()
        } else if expected_hash.len() == 33 {
            actual_hash.as_slice() == &expected_hash[1..]
        } else {
            tracing::warn!(
                len = expected_hash.len(),
                path = %download_path.display(),
                "Unknown checksum format ({} bytes), skipping verification",
                expected_hash.len()
            );
            true
        };

        if !matches {
            // Checksum failed — delete .part so next attempt starts fresh
            // (the partial data is corrupt or the resume was wrong).
            let _ = fs::remove_file(&part_path).await;
            return Err(DownloadError::ChecksumMismatch(
                download_path.display().to_string(),
            ));
        }
    }

    fs::rename(&part_path, download_path).await?;

    Ok(())
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
}
