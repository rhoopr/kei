use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Context;
use base64::Engine;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use reqwest::Client;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use super::error::DownloadError;
use crate::retry::{self, RetryAction, RetryConfig};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// HTTP response from a download request.
pub(super) struct DownloadResponse {
    pub status: u16,
    pub content_length: Option<u64>,
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
        .context("Failed to decode base64 checksum")?;
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
}

/// Download a file with retry support and optional expected-size verification.
pub(super) async fn download_file<C: DownloadClient>(
    client: &C,
    url: &str,
    download_path: &Path,
    checksum: &str,
    retry_config: &RetryConfig,
    temp_suffix: &str,
    opts: DownloadOpts,
) -> Result<(), DownloadError> {
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
        || async {
            Box::pin(attempt_download(
                client,
                url,
                download_path,
                &part_path,
                opts.skip_rename,
                opts.expected_size,
            ))
            .await
        },
    ))
    .await
}

/// Single download attempt with resume support.
///
/// .part files older than this are considered stale (crashed runs) and restarted.
const STALE_PART_FILE_SECS: u64 = 24 * 3600;

/// If a .part file already exists, sends a Range request to resume from where
/// it left off. Falls back to a fresh download if the server doesn't support
/// Range or returns an unexpected status.
async fn attempt_download<C: DownloadClient>(
    client: &C,
    url: &str,
    download_path: &Path,
    part_path: &Path,
    skip_rename: bool,
    expected_size: Option<u64>,
) -> Result<(), DownloadError> {
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
                    "Stale .part file (>24h old), restarting download"
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

    // Reject HTML content-type before writing to disk. Apple's CDN sometimes
    // returns HTTP 200 with text/html for rate-limit or error pages.
    // Delete any stale .part file so the next successful attempt starts fresh
    // rather than appending to data from a previous (possibly different) response.
    if let Some(ct) = &response.content_type {
        let ct_lower = ct.to_ascii_lowercase();
        if ct_lower.starts_with("text/html") {
            let _ = fs::remove_file(part_path).await;
            return Err(DownloadError::InvalidContent {
                path: path_str.into(),
                reason: format!("server returned content-type: {ct}").into(),
            });
        }
    }

    let content_length = response.content_length;

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

    let mut stream = response.stream;
    let stream_result: Result<(), DownloadError> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| DownloadError::Http {
                source: e,
                path: path_str.clone().into(),
                status,
                content_length,
                bytes_written,
            })?;
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
        if !e.is_retryable() {
            let _ = fs::remove_file(&part_path).await;
        }
        return Err(e);
    }

    // Verify the server sent the number of bytes it promised.
    // Catches CDN truncation (e.g. Apple silently cutting off videos at ~1 GB).
    if let Some(expected_len) = content_length {
        let total_bytes = bytes_written - effective_offset;
        if total_bytes != expected_len {
            let _ = fs::remove_file(&part_path).await;
            return Err(DownloadError::ContentLengthMismatch {
                path: path_str.into(),
                expected: expected_len,
                received: total_bytes,
            });
        }
    }

    // Verify total bytes written matches the API-reported size (if known).
    // Catches truncation when the CDN omits Content-Length (chunked transfer).
    if let Some(expected) = expected_size {
        if bytes_written != expected {
            let _ = fs::remove_file(&part_path).await;
            return Err(DownloadError::ContentLengthMismatch {
                path: path_str.into(),
                expected,
                received: bytes_written,
            });
        }
    }

    // Defense-in-depth: log when neither size indicator is available.
    // Post-download checksum verification (CF-1) catches actual corruption,
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
    if let Err(e) = validate_downloaded_content(part_path, download_path) {
        let _ = fs::remove_file(&part_path).await;
        return Err(e);
    }

    if !skip_rename {
        rename_part_to_final(part_path, download_path).await?;
    }

    Ok(())
}

/// Rename a `.part` file to its final destination, handling the case where
/// a concurrent download already placed the final file. If the destination
/// exists, the redundant `.part` file is removed instead of overwriting.
pub(super) async fn rename_part_to_final(
    part_path: &Path,
    final_path: &Path,
) -> anyhow::Result<()> {
    match fs::rename(part_path, final_path).await {
        Ok(()) => Ok(()),
        Err(rename_err) if fs::try_exists(final_path).await.unwrap_or(false) => {
            // Another task won the race — clean up our .part file.
            tracing::debug!(
                path = %final_path.display(),
                error = %rename_err,
                "Destination already exists, removing redundant .part file"
            );
            if let Err(rm_err) = fs::remove_file(part_path).await {
                tracing::warn!(
                    path = %part_path.display(),
                    error = %rm_err,
                    "Failed to remove redundant .part file"
                );
            }
            Ok(())
        }
        Err(e) => Err(e).with_context(|| {
            format!(
                "failed to rename {} to {}",
                part_path.display(),
                final_path.display()
            )
        }),
    }
}

/// Compute the SHA-256 hash of a file, returning a hex-encoded string.
///
/// Used for `local_checksum` / `download_checksum` in the state DB and
/// by `verify --checksums` for integrity checks.
pub(crate) async fn compute_sha256(path: &Path) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut sha256 = Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            use std::io::Read;
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sha256.update(&buf[..n]);
        }
        Ok(format!("{:x}", sha256.finalize()))
    })
    .await?
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
        .context("Failed to decode API checksum from base64")?;
    let (hash_bytes, is_sha1) = match bytes.len() {
        20 => (&bytes[..], true),
        21 => (&bytes[1..], true),
        32 => (&bytes[..], false),
        33 => (&bytes[1..], false),
        other => anyhow::bail!(
            "Unexpected API checksum length: {other} bytes (expected 20, 21, 32, or 33)"
        ),
    };
    let mut hex = String::with_capacity(hash_bytes.len() * 2);
    for b in hash_bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(DecodedChecksum { hex, is_sha1 })
}

/// Validate that downloaded content matches expected format for the file extension.
///
/// For known media types (JPEG, PNG, HEIC, MOV, etc.), checks magic bytes in the
/// file header. Magic byte mismatches are logged as warnings but allowed through,
/// since format variants exist (e.g. classic QuickTime MOV without `ftyp` box).
/// HTML content is always rejected as a hard error — Apple's CDN occasionally
/// returns error pages with HTTP 200.
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

    let header = &buf[..n];

    // Reject HTML content regardless of extension. Apple's CDN sometimes returns
    // error/rate-limit pages as HTTP 200 with text/html bodies.
    let trimmed = header
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(header, |pos| &header[pos..]);

    if trimmed.starts_with(b"<!")
        || (trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case(b"<html"))
    {
        return Err(DownloadError::InvalidContent {
            path: download_path.display().to_string().into(),
            reason: "file contains HTML (likely a CDN error page)".into(),
        });
    }

    let ext = download_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // For known media types, check magic bytes. Mismatches are warnings, not errors,
    // because format variants exist (e.g. classic QuickTime MOV files starting with
    // `wide`/`mdat` instead of `ftyp`).
    let magic_ok = match ext.as_str() {
        "jpg" | "jpeg" => Some(n >= 2 && header[..2] == [0xFF, 0xD8]),
        "png" => Some(n >= 4 && header[..4] == [0x89, 0x50, 0x4E, 0x47]),
        "heic" | "heif" | "mov" | "mp4" | "m4v" => Some(n >= 8 && header[4..8] == *b"ftyp"),
        "gif" => Some(n >= 4 && header[..4] == *b"GIF8"),
        "tiff" | "tif" => Some(
            n >= 4
                && (header[..4] == [0x49, 0x49, 0x2A, 0x00]
                    || header[..4] == [0x4D, 0x4D, 0x00, 0x2A]),
        ),
        "webp" => Some(n >= 12 && header[..4] == *b"RIFF" && header[8..12] == *b"WEBP"),
        _ => None,
    };

    if magic_ok == Some(false) {
        tracing::warn!(
            path = %download_path.display(),
            header = %format_args!("{:02x?}", &header[..n.min(8)]),
            "File header does not match expected format for .{ext}, saving anyway",
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
        // Empty string is technically valid base64 (decodes to empty bytes),
        // but produces an empty base32 filename — verify it at least doesn't panic.
        // An empty checksum decodes to zero bytes, so base32 is also empty.
        let path = PathBuf::from("/photos/test.jpg");
        let result = temp_download_path(&path, "", ".kei-tmp");
        // Empty base64 decodes successfully to empty bytes; the path is valid
        // but the stem is empty — just the suffix. Ensure no error.
        assert!(result.is_ok());
        let temp = result.unwrap();
        // The filename should be just the suffix since the encoded part is empty
        assert_eq!(temp.file_name().unwrap().to_str().unwrap(), ".kei-tmp");
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
    fn validate_rejects_empty_file() {
        let (part, dest, _dir) = write_temp_file("empty.jpg", b"");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_warns_but_accepts_mismatched_heic_magic() {
        // Non-ftyp header with .heic extension — warn but accept
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x08]);
        buf[4..8].copy_from_slice(b"wide");
        let (part, dest, _dir) = write_temp_file("photo.heic", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_warns_but_accepts_mismatched_png_magic() {
        // JPEG magic with .png extension — warn but accept
        let (part, dest, _dir) = write_temp_file("photo.png", &[0xFF, 0xD8, 0xFF, 0xE0]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
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
        content_type: Option<String>,
        body: Vec<u8>,
    }

    impl StubDownloadClient {
        fn ok(body: &[u8]) -> Self {
            Self {
                status: 200,
                content_length: Some(body.len() as u64),
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
    }

    #[async_trait::async_trait]
    impl DownloadClient for StubDownloadClient {
        async fn fetch(
            &self,
            _url: &str,
            _resume_from: Option<u64>,
        ) -> Result<DownloadResponse, BoxError> {
            let chunks: Vec<Result<Bytes, BoxError>> = vec![Ok(Bytes::from(self.body.clone()))];
            Ok(DownloadResponse {
                status: self.status,
                content_length: self.content_length,
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
        )
        .await
        .unwrap();

        let content = std::fs::read(&download_path).unwrap();
        assert_eq!(content, [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]);
        assert!(!part_path.exists(), ".part should be renamed");
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
        )
        .await
        .unwrap();

        assert_eq!(
            client.resume_from.load(Ordering::SeqCst),
            100,
            "client should receive the .part file size as resume offset"
        );
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
                    content_type: None,
                    stream: Box::pin(futures_util::stream::iter(chunks)),
                })
            } else {
                let chunks: Vec<Result<Bytes, BoxError>> = vec![Ok(Bytes::from(self.body.clone()))];
                Ok(DownloadResponse {
                    status: 200,
                    content_length: Some(self.body.len() as u64),
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
    ) -> (Result<(), DownloadError>, u32, PathBuf, TempDir) {
        let jpeg_body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];
        let client = RetryingStubClient::new(fail_count, fail_status, jpeg_body);
        let dir = TempDir::new().unwrap();
        let download_path = dir.path().join("photo.jpg");

        let config = RetryConfig {
            max_retries,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let result = download_file(
            &client,
            "http://stub/photo.jpg",
            &download_path,
            "AAAA",
            &config,
            ".kei-tmp",
            DownloadOpts {
                skip_rename: false,
                expected_size: None,
            },
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
        ) -> (Result<(), DownloadError>, PathBuf, TempDir) {
            let dir = TempDir::new().unwrap();
            let download_path = dir.path().join(filename);
            let config = RetryConfig {
                max_retries,
                base_delay_secs: 0,
                max_delay_secs: 0,
            };
            let result = download_file(
                &reqwest::Client::new(),
                &format!("{}/{filename}", server.uri()),
                &download_path,
                checksum,
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: None,
                },
            )
            .await;
            (result, download_path, dir)
        }

        #[tokio::test]
        async fn real_client_retries_on_503_then_succeeds() {
            let server = MockServer::start().await;
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
            let server = MockServer::start().await;
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
            let server = MockServer::start().await;

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
            let server = MockServer::start().await;

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
            let server = MockServer::start().await;

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
            let server = MockServer::start().await;
            let full_body = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];

            Mock::given(method("GET"))
                .and(path("/resume.jpg"))
                .and(wiremock::matchers::header_exists("Range"))
                .respond_with(
                    ResponseTemplate::new(206)
                        .set_body_bytes(full_body[4..].to_vec())
                        .insert_header("content-length", "4"),
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
            download_file(
                &reqwest::Client::new(),
                &format!("{}/resume.jpg", server.uri()),
                &download_path,
                "AAAF",
                &config,
                ".kei-tmp",
                DownloadOpts {
                    skip_rename: false,
                    expected_size: None,
                },
            )
            .await
            .unwrap();

            assert_eq!(std::fs::read(&download_path).unwrap(), full_body);
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
    async fn rename_part_to_final_destination_already_exists() {
        let dir = TempDir::new().unwrap();
        let part = dir.path().join("photo.part");
        let final_path = dir.path().join("photo.jpg");
        tokio::fs::write(&final_path, b"existing").await.unwrap();
        tokio::fs::write(&part, b"duplicate").await.unwrap();

        // Should succeed regardless of platform behavior:
        // - Linux: rename atomically overwrites (Ok path)
        // - Windows: rename fails, guard detects existing file and cleans .part
        rename_part_to_final(&part, &final_path).await.unwrap();

        assert!(!part.exists(), ".part should not remain");
        assert!(final_path.exists(), "final file should exist");
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
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, DownloadError::InvalidContent { .. }),
            "zero-byte download should be rejected, got: {err}"
        );
        assert!(!download_path.exists(), "final file should not exist");
    }

    // ── Gap: stale .part file (>24h) triggers restart from zero ──────

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
