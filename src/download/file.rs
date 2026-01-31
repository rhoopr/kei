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

/// Base32 encode bytes using RFC 4648 alphabet (A-Z, 2-7), no padding.
fn base32_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut result = String::with_capacity((data.len() * 8).div_ceil(5));
    let mut buffer: u64 = 0;
    let mut bits_left: u32 = 0;

    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            let index = ((buffer >> bits_left) & 0x1F) as usize;
            result.push(ALPHABET[index] as char);
        }
    }
    if bits_left > 0 {
        let index = ((buffer << (5 - bits_left)) & 0x1F) as usize;
        result.push(ALPHABET[index] as char);
    }
    result
}

/// Derive a deterministic .part filename from the checksum so that
/// concurrent downloads of different files don't collide. Base32-encoded
/// because base64 contains `/` which is invalid in filenames.
fn temp_download_path(download_path: &Path, checksum: &str) -> anyhow::Result<PathBuf> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(checksum)
        .context("Failed to decode base64 checksum")?;
    let encoded = base32_encode(&decoded);
    let download_dir = download_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    Ok(download_dir.join(format!("{}.part", encoded)))
}

/// Download a file from URL using .part temp files.
///
/// Each attempt deletes any existing .part file and downloads from scratch
/// to ensure reliable checksum verification. On completion the .part file
/// is renamed to the final destination path. Retries with exponential
/// backoff on transient failures.
pub async fn download_file(
    client: &Client,
    url: &str,
    download_path: &Path,
    checksum: &str,
    dry_run: bool,
    retry_config: &RetryConfig,
) -> Result<(), DownloadError> {
    if dry_run {
        tracing::info!("[DRY RUN] Would download {}", download_path.display());
        return Ok(());
    }

    let part_path = temp_download_path(download_path, checksum)
        .map_err(DownloadError::Other)?;

    let result = retry::retry_with_backoff(
        retry_config,
        |e: &DownloadError| {
            if e.is_retryable() {
                RetryAction::Retry
            } else {
                RetryAction::Abort
            }
        },
        || async {
            // Delete any partial file so we always start fresh with checksum verification.
            let _ = fs::remove_file(&part_path).await;
            attempt_download(client, url, download_path, &part_path, checksum).await
        },
    )
    .await;

    result.map_err(|e| DownloadError::RetriesExhausted {
        retries: retry_config.max_retries,
        path: download_path.display().to_string(),
        last_error: e.to_string(),
    })
}

/// Single download attempt with checksum verification.
async fn attempt_download(
    client: &Client,
    url: &str,
    download_path: &Path,
    part_path: &Path,
    checksum: &str,
) -> Result<(), DownloadError> {
    let path_str = download_path.display().to_string();
    let response = client.get(url).send().await.map_err(|e| DownloadError::Http {
        source: e,
        path: path_str.clone(),
    })?;

    if !response.status().is_success() {
        return Err(DownloadError::HttpStatus {
            status: response.status().as_u16(),
            path: path_str,
        });
    }

    let status = response.status().as_u16();
    let content_length = response.content_length();

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&part_path)
        .await
        .map_err(|e| DownloadError::Other(anyhow::anyhow!("Failed to open .part file: {}", e)))?;

    // Incremental SHA256 — avoids buffering entire files in memory for large MOVs.
    let mut hasher = Sha256::new();
    let mut bytes_written: u64 = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            tracing::warn!(
                "Body decode error for {} (status={}, content_length={:?}, bytes_so_far={}): {}",
                path_str, status, content_length, bytes_written, e
            );
            DownloadError::Http {
                source: e,
                path: path_str.clone(),
            }
        })?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        bytes_written += chunk.len() as u64;
    }
    file.flush().await?;
    drop(file);

    if let Ok(expected_hash) = base64::engine::general_purpose::STANDARD.decode(checksum) {
        let actual_hash = hasher.finalize();

        // Apple uses two checksum formats: raw 32-byte SHA-256, or 33-byte
        // with a 1-byte type prefix (0x01 = SHA-256). Handle both.
        let matches = if expected_hash.len() == 32 {
            actual_hash.as_slice() == expected_hash.as_slice()
        } else if expected_hash.len() == 33 {
            actual_hash.as_slice() == &expected_hash[1..]
        } else {
            true
        };

        if !matches {
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
        // "Hello" -> "JBSWY3DP" (standard base32, no padding)
        assert_eq!(base32_encode(b"Hello"), "JBSWY3DP");
        assert_eq!(base32_encode(b""), "");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_encode(b"fo"), "MZXQ");
        assert_eq!(base32_encode(b"foo"), "MZXW6");
    }
}
