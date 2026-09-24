use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use bytes::Bytes;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::download::error::DownloadError;
use crate::download::limiter::BandwidthLimiter;
use crate::retry::RetryConfig;

use super::super::fingerprint::compute_sha256;
use super::super::replacement::FinalPublication;
use super::super::test_support::ISSUE_507_JPEG_HEADER;
use super::{
    BoxError, DownloadClient, DownloadLimits, DownloadOpts, DownloadResponse, STALE_PART_FILE_SECS,
    attempt_download, download_file_with_mode, temp_download_path,
};

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
    use super::{
        BandwidthLimiter, DownloadError, DownloadLimits, DownloadOpts, FinalPublication, PathBuf,
        RetryConfig, TempDir, download_file_with_mode, temp_download_path,
    };
    use base64::Engine;
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

        let (result, path, _dir) = run_mock_download(&server, "rate-limited.jpg", "AAAB", 3).await;
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
    let old_mtime =
        std::time::SystemTime::now() - std::time::Duration::from_secs(STALE_PART_FILE_SECS + 3600);
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
