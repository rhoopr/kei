use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine;
use futures_util::StreamExt;
use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

const MAX_RETRIES: u32 = 5;
const WAIT_SECONDS: u64 = 5;

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

/// Compute the temporary .part file path from the checksum.
///
/// The checksum is base64-encoded; we decode it then base32-encode for
/// a filesystem-safe temp filename.
fn temp_download_path(download_path: &Path, checksum: &str) -> Result<PathBuf> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(checksum)
        .context("Failed to decode base64 checksum")?;
    let encoded = base32_encode(&decoded);
    let download_dir = download_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    Ok(download_dir.join(format!("{}.part", encoded)))
}

/// Download a file from URL with resume support using .part temp files.
///
/// Uses HTTP Range header to resume partial downloads. On completion the
/// .part file is renamed to the final destination path. Retries with
/// exponential backoff on failure.
///
/// Returns `Ok(true)` on success, `Ok(false)` after exhausting retries.
pub async fn download_file(
    client: &Client,
    url: &str,
    download_path: &Path,
    checksum: &str,
    dry_run: bool,
) -> Result<bool> {
    if dry_run {
        tracing::info!("[DRY RUN] Would download {}", download_path.display());
        return Ok(true);
    }

    let part_path = temp_download_path(download_path, checksum)?;

    for retry in 0..MAX_RETRIES {
        match attempt_download(client, url, download_path, &part_path, checksum).await {
            Ok(()) => return Ok(true),
            Err(e) => {
                if retry + 1 >= MAX_RETRIES {
                    tracing::error!(
                        "Could not download {} after {} retries: {}",
                        download_path.display(),
                        MAX_RETRIES,
                        e
                    );
                    return Ok(false);
                }
                let wait_time = (retry as u64 + 1) * WAIT_SECONDS;
                tracing::warn!(
                    "Error downloading {}, retrying after {} seconds: {}",
                    download_path.display(),
                    wait_time,
                    e
                );
                tokio::time::sleep(std::time::Duration::from_secs(wait_time)).await;
            }
        }
    }

    Ok(false)
}

/// Single download attempt with resume support and checksum verification.
async fn attempt_download(
    client: &Client,
    url: &str,
    download_path: &Path,
    part_path: &Path,
    checksum: &str,
) -> Result<()> {
    let current_size = if part_path.exists() {
        let meta = fs::metadata(&part_path).await?;
        let size = meta.len();
        tracing::debug!(
            "Resuming download of {} from byte {}",
            download_path.display(),
            size
        );
        size
    } else {
        0
    };

    let mut request = client.get(url);
    if current_size > 0 {
        request = request.header("Range", format!("bytes={}-", current_size));
    }

    let response = request.send().await.context("HTTP request failed")?;

    if !response.status().is_success() && response.status().as_u16() != 206 {
        anyhow::bail!(
            "HTTP {} for {}",
            response.status(),
            download_path.display()
        );
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(current_size > 0)
        .write(true)
        .truncate(current_size == 0)
        .open(&part_path)
        .await
        .context("Failed to open .part file")?;

    // Stream the response body in chunks, computing SHA256 incrementally
    let mut hasher = if current_size == 0 {
        Some(Sha256::new())
    } else {
        None
    };

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read response chunk")?;
        if let Some(ref mut h) = hasher {
            h.update(&chunk);
        }
        file.write_all(&chunk).await.context("Failed to write to .part file")?;
    }
    file.flush().await?;
    drop(file);

    // Verify checksum if this was a fresh (non-resumed) download.
    if let Some(hasher) = hasher {
        if let Ok(expected_hash) = base64::engine::general_purpose::STANDARD.decode(checksum) {
            let actual_hash = hasher.finalize();

            let matches = if expected_hash.len() == 32 {
                actual_hash.as_slice() == expected_hash.as_slice()
            } else if expected_hash.len() == 33 {
                // Skip 1-byte prefix
                actual_hash.as_slice() == &expected_hash[1..]
            } else {
                // Unknown hash format, skip verification
                true
            };

            if !matches {
                let _ = fs::remove_file(&part_path).await;
                anyhow::bail!(
                    "Checksum mismatch for {}",
                    download_path.display()
                );
            }
        }
    }

    fs::rename(&part_path, download_path)
        .await
        .context("Failed to rename .part file to final path")?;

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
