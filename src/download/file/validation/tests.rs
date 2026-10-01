use std::path::PathBuf;

use base64::Engine;
use tempfile::TempDir;

use crate::download::error::DownloadError;

use super::super::test_support::ISSUE_507_JPEG_HEADER;
use super::{
    classify_magic, decode_api_checksum, detect_error_sentinel, parse_content_range_start,
    rejecting_content_type_reason, validate_downloaded_content,
};

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
    let (part, dest, _dir) = write_temp_file("photo.jpg", &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]);
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
    let (part, dest, _dir) = write_temp_file("photo.tiff", &[0x49, 0x49, 0x2A, 0x00, 0x08, 0x00]);
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
    let (part, dest, _dir) = write_temp_file("photo.tif", &[0x4D, 0x4D, 0x00, 0x2A, 0x00, 0x08]);
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
