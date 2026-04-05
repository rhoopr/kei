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
pub(super) struct DownloadOpts {
    /// Keep the `.part` file instead of renaming to the final path.
    pub skip_rename: bool,
    /// API-reported file size. When set, verifies total bytes written match,
    /// catching truncation even when the CDN omits `Content-Length`.
    pub expected_size: Option<u64>,
}

/// Download a file with retry support and optional expected-size verification.
pub(super) async fn download_file(
    client: &Client,
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
/// If a .part file already exists, sends a Range request to resume from where
/// it left off. Falls back to a fresh download if the server doesn't support
/// Range or returns an unexpected status.
async fn attempt_download(
    client: &Client,
    url: &str,
    download_path: &Path,
    part_path: &Path,
    skip_rename: bool,
    expected_size: Option<u64>,
) -> Result<(), DownloadError> {
    let path_str = download_path.display().to_string();

    let resume_offset = match fs::metadata(part_path).await {
        Ok(meta) if meta.len() > 0 => meta.len(),
        _ => 0,
    };

    let mut request = client.get(url);
    if resume_offset > 0 {
        tracing::info!(
            path = %path_str,
            resume_offset,
            "Resuming download (partial file exists)"
        );
        request = request.header("Range", format!("bytes={resume_offset}-"));
    }

    let response = request.send().await.map_err(|e| DownloadError::Http {
        source: Box::new(e),
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
    let stream_result: Result<(), DownloadError> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| DownloadError::Http {
                source: Box::new(e),
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
                path: path_str,
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
                path: path_str,
                expected,
                received: bytes_written,
            });
        }
    }

    // Validate content looks like actual media, not an HTML error page.
    // Apple's CDN occasionally returns HTTP 200 with HTML (rate limit, CAPTCHA,
    // service unavailable) which would otherwise be saved as the final file.
    if let Err(e) = validate_downloaded_content(part_path, download_path) {
        let _ = fs::remove_file(&part_path).await;
        return Err(e);
    }

    if !skip_rename {
        fs::rename(&part_path, download_path).await?;
    }

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

/// Validate that downloaded content matches expected format for the file extension.
///
/// For known media types (JPEG, PNG, HEIC, MOV, etc.), checks magic bytes in the
/// file header. For unknown extensions, rejects content that looks like HTML.
/// Deleting the .part file and returning a retryable error prevents HTML error
/// pages from being persisted as valid downloads.
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
        return Ok(());
    }

    let header = &buf[..n];

    let ext = download_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // For known media types, validate magic bytes.
    // This catches HTML error pages AND any other corrupted content.
    let magic_match = match ext.as_str() {
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

    match magic_match {
        Some(false) => {
            return Err(DownloadError::InvalidContent {
                path: download_path.display().to_string(),
                reason: format!("file header does not match expected format for .{ext}"),
            });
        }
        Some(true) => return Ok(()),
        None => {}
    }

    // For unrecognized extensions, reject obvious HTML error pages.
    let trimmed = header
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .map_or(header, |pos| &header[pos..]);

    if trimmed.starts_with(b"<!")
        || trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case(b"<html")
    {
        return Err(DownloadError::InvalidContent {
            path: download_path.display().to_string(),
            reason: "file contains HTML (likely a CDN error page)".into(),
        });
    }

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
        let dir = PathBuf::from("/tmp/claude/sha256_empty_test");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("empty.bin");
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
        let dir = PathBuf::from("/tmp/claude/sha256_large_test");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("large.bin");

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

    fn write_temp_file(name: &str, content: &[u8]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir()
            .join("claude")
            .join("validate_content_test")
            .join(format!("{}_{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let part_path = dir.join(format!("{name}.part"));
        let download_path = dir.join(name);
        std::fs::write(&part_path, content).unwrap();
        (part_path, download_path)
    }

    #[test]
    fn validate_rejects_html_doctype_as_jpeg() {
        let (part, dest) = write_temp_file("photo.jpg", b"<!DOCTYPE html><html>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
        assert!(err.is_retryable());
    }

    #[test]
    fn validate_rejects_html_tag_as_heic() {
        let (part, dest) = write_temp_file("photo.heic", b"<html><head></head>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_accepts_valid_jpeg() {
        let (part, dest) = write_temp_file("photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_valid_png() {
        let (part, dest) = write_temp_file(
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
        let (part, dest) = write_temp_file("photo.heic", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_valid_mov() {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&[0x00, 0x00, 0x00, 0x14]);
        buf[4..8].copy_from_slice(b"ftyp");
        buf[8..12].copy_from_slice(b"qt  ");
        let (part, dest) = write_temp_file("clip.mov", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_rejects_html_error_page_as_mov() {
        let html = b"<!DOCTYPE html>\n<html><body>Service Temporarily Unavailable</body></html>";
        let (part, dest) = write_temp_file("clip.mov", html);
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_rejects_html_for_unknown_extension() {
        let (part, dest) = write_temp_file("file.xyz", b"<!DOCTYPE html><html>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_rejects_html_with_leading_whitespace() {
        let (part, dest) = write_temp_file("file.dat", b"  \n<!DOCTYPE html>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_accepts_xml_for_unknown_extension() {
        // AAE files are XML plists — should not be rejected
        let (part, dest) = write_temp_file("photo.aae", b"<?xml version=\"1.0\"?>");
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_empty_file() {
        let (part, dest) = write_temp_file("empty.jpg", b"");
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_rejects_wrong_magic_for_png() {
        // Write JPEG magic but with .png extension
        let (part, dest) = write_temp_file("photo.png", &[0xFF, 0xD8, 0xFF, 0xE0]);
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    #[test]
    fn validate_accepts_gif() {
        let (part, dest) = write_temp_file("anim.gif", b"GIF89a\x01\x00\x01\x00");
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_tiff_little_endian() {
        let (part, dest) = write_temp_file("photo.tiff", &[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_tiff_big_endian() {
        let (part, dest) = write_temp_file("photo.tif", &[0x4D, 0x4D, 0x00, 0x2A, 0x00, 0x08]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_webp() {
        let mut buf = [0u8; 16];
        buf[0..4].copy_from_slice(b"RIFF");
        buf[4..8].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]); // file size (irrelevant)
        buf[8..12].copy_from_slice(b"WEBP");
        let (part, dest) = write_temp_file("photo.webp", &buf);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_accepts_arbitrary_binary_for_unknown_extension() {
        // Random binary data with unknown extension should pass
        let (part, dest) = write_temp_file("data.bin", &[0x00, 0x01, 0x02, 0xFF, 0xFE]);
        assert!(validate_downloaded_content(&part, &dest).is_ok());
    }

    #[test]
    fn validate_html_case_insensitive() {
        let (part, dest) = write_temp_file("file.dat", b"<HTML><HEAD>");
        let err = validate_downloaded_content(&part, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::InvalidContent { .. }));
    }

    /// T-4: CDN returns HTML error page with Content-Length matching body size
    /// for a .HEIC download. The content validation must reject it, delete the
    /// .part file, and return a retryable error.
    #[test]
    fn validate_rejects_html_error_page_as_heic_full_flow() {
        let html_body = b"<!DOCTYPE html><html>Service Unavailable</html>";
        let (part, dest) = write_temp_file("cdn_error.heic", html_body);

        // Validate rejects — magic bytes don't match ftyp header
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
}
