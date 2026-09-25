//! Response, provider-size, checksum-format, and media validation.

use std::path::Path;

#[cfg(test)]
use anyhow::Context;
#[cfg(test)]
use base64::Engine;

use crate::download::error::DownloadError;

use super::fingerprint::compute_sha256;

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

pub(super) fn rejecting_content_type_reason(content_type: &str) -> Option<&'static str> {
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

/// Reject a resumed response whose remaining length no longer fits the provider size.
pub(super) fn validate_resume_size(
    resume_offset: u64,
    content_length: Option<u64>,
    expected_size: Option<u64>,
    path_str: &str,
) -> Result<(), DownloadError> {
    // If we resumed and the server advertises a Content-Length that doesn't
    // reconcile with the API-reported size, the resource on the server has
    // likely been rotated since we wrote the .part. Discard and restart
    // cleanly rather than appending new bytes to a stale prefix.
    if let (Some(cl), Some(expected)) = (content_length, expected_size) {
        let server_total = resume_offset.saturating_add(cl);
        if server_total != expected {
            tracing::warn!(target: "kei::download::file",
                path = %path_str,
                resume_offset,
                server_remaining = cl,
                server_total,
                expected,
                "Resume bytes inconsistent with API-reported size; discarding .part and restarting"
            );
            return Err(DownloadError::ContentLengthMismatch {
                path: path_str.into(),
                expected,
                received: server_total,
            });
        }
    }
    Ok(())
}

/// Validate streamed and total byte counts before content inspection or publication.
pub(super) fn validate_download_lengths(
    bytes_written: u64,
    effective_offset: u64,
    content_length: Option<u64>,
    expected_size: Option<u64>,
    path_str: &str,
) -> Result<(), DownloadError> {
    // Verify the server sent the number of bytes it promised.
    // Catches CDN truncation (e.g. Apple silently cutting off videos at ~1 GB).
    if let Some(expected_len) = content_length {
        let total_bytes = bytes_written - effective_offset;
        if total_bytes != expected_len {
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
        tracing::warn!(target: "kei::download::file",
            path = %path_str,
            bytes_written,
            "No expected size or Content-Length available to verify download completeness"
        );
    }

    Ok(())
}

pub(super) fn validate_resume_content_range(
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
pub(super) fn validate_downloaded_content(
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
            tracing::warn!(target: "kei::download::file",
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
mod tests;
