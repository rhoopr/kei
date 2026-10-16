//! ISO-BMFF helpers for reading and safely updating metadata in HEIC / HEIF /
//! AVIF files.
//!
//! Adobe's XMP Toolkit has no HEIF handler, so kei reads HEIF item metadata
//! directly via [`mp4_atom`]. The writer edits only the selected metadata item
//! and raw box headers. All other boxes and payloads are copied byte-for-byte.
//!
//! Ownership stays inside this facade and its child modules. File-backed reads
//! use `file`; in-memory layout and references use `boxes`, `items`, and
//! `relationships`. Native Exif repair, XMP reads, XMP writes, and post-rewrite
//! validation have separate owners. Production children do not depend on test
//! builders or the legacy test writer.
//!
//! Tests moved from `download::heif::tests::<name>` to
//! `download::heif::<owner>::tests::<name>`, except the error-type test here.
//! Names and assertions are unchanged. The `download::heif::` filter still
//! selects the complete HEIF suite.

#![allow(
    clippy::map_err_ignore,
    reason = "Malformed untrusted bytes are reduced to stable typed layout errors at this boundary."
)]
#![allow(
    clippy::type_complexity,
    reason = "The parser returns a fixed group of layout coordinates used together for one rewrite."
)]

mod boxes;
mod exif;
mod file;
#[cfg(feature = "__fuzz_internals")]
mod fuzz;
mod items;
#[cfg(any(test, feature = "__fuzz_internals"))]
mod preservation;
mod relationships;
mod stream;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod test_writer;
#[cfg(any(test, feature = "__fuzz_internals"))]
mod xmp_read;
mod xmp_write;

#[cfg(any(test, feature = "__fuzz_internals"))]
use mp4_atom::FourCC;

pub(crate) use boxes::{is_heif_content, is_heif_path};
#[cfg(test)]
pub(crate) use exif::{extract_exif_capture_time, extract_exif_tiff_bytes};
pub(crate) use file::locate_exif_tiff;
#[cfg(feature = "__fuzz_internals")]
pub(crate) use fuzz::fuzz_rewrite_xmp_preserves;
#[cfg(test)]
pub(crate) use preservation::validate_rewrite_preserves_non_xmp_items;

pub(crate) use stream::FileLayout;
#[cfg(test)]
pub(crate) use test_support::{
    apple_multi_exif_heic, apple_multi_xmp_heic, apple_tmap_conflicting_xmp_heic,
    apple_tmap_insertion_heic, set_test_file_item_extent,
};
#[cfg(test)]
pub(crate) use test_writer::insert_xmp;
#[cfg(any(test, feature = "__fuzz_internals"))]
pub(crate) use xmp_read::extract_xmp_bytes;
#[cfg(test)]
pub(crate) use xmp_read::extract_xmp_strict;
#[cfg(test)]
pub(crate) use xmp_write::rewrite_xmp;

/// Typed failures from the HEIC writer. Each variant names the precise mode
/// so call-site logging and any future fall-back logic can distinguish
/// "file is truncated" from "kei's own re-encoder failed" from "this isn't
/// a HEIC at all" — instead of grepping anyhow strings.
///
/// `Decode`/`Encode` wrap the underlying [`mp4_atom::Error`] so the original
/// failure detail is preserved (`UnderDecode("infe")`, `OutOfBounds`, etc.)
/// while kei adds the byte offset / atom kind context.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HeifError {
    #[error(
        "HEIF metadata operation unsupported within resource allowance: {kind} requires {bytes} bytes; materialized-data allowance is {budget} bytes (not a format limit)"
    )]
    MetadataLimit {
        kind: &'static str,
        bytes: u64,
        budget: usize,
    },
    #[cfg(test)]
    #[error("Could not read HEIC metadata at byte {offset} of {total}: {source}")]
    Decode {
        offset: u64,
        total: u64,
        #[source]
        source: mp4_atom::Error,
    },

    #[cfg(test)]
    #[error(
        "Could not read trailing HEIC bytes at byte {offset} of {total}; the file may be truncated"
    )]
    UnparsableTail { offset: u64, total: u64 },

    #[cfg(any(test, feature = "__fuzz_internals"))]
    #[error("Could not read HEIC metadata box `{kind}`: {source}")]
    MetaSubBoxDecode {
        kind: FourCC,
        #[source]
        source: mp4_atom::Error,
    },

    #[error("Could not safely rewrite HEIC XMP: {reason}")]
    InvalidLayout { reason: &'static str },

    #[error("HEIC XMP rewrite value does not fit in {field}")]
    ValueOverflow { field: &'static str },

    #[cfg(test)]
    #[error("Could not find a top-level HEIC `meta` box after scanning {input_len} bytes")]
    MissingMeta { input_len: usize },

    #[cfg(test)]
    #[error("Could not rewrite HEIC atom `{kind}`: {source}")]
    Encode {
        kind: FourCC,
        #[source]
        source: mp4_atom::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Classify bounded metadata refusals through anyhow context at the owner boundary.
#[must_use]
pub(crate) fn classify_heif_metadata_limit(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HeifError>()
        .is_some_and(|error| matches!(error, HeifError::MetadataLimit { .. }))
}

#[derive(Debug)]
pub(crate) enum HeifExifError {
    Io(std::io::Error),
    ResourceUnsupported(HeifError),
    Malformed,
}

impl From<std::io::Error> for HeifExifError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub(crate) struct ExifCaptureTime {
    pub(crate) datetime_original: Option<String>,
    pub(crate) subseconds_original: Option<String>,
    pub(crate) offset_time_original: Option<String>,
}

fn invalid_layout(reason: &'static str) -> HeifError {
    HeifError::InvalidLayout { reason }
}

#[cfg(test)]
mod tests {
    use super::HeifError;

    #[test]
    fn heif_error_io_variant_carries_underlying_io_error() {
        // Sanity-check the Io variant — the writer used by insert_xmp is
        // any std::io::Write, and io::Error must convert via From.
        let io_err = std::io::Error::other("disk full");
        let err: HeifError = io_err.into();
        assert!(matches!(err, HeifError::Io(_)));
    }
    #[test]
    fn classify_heif_metadata_limit_preserves_context_and_rejects_text_matches() {
        let bounded = anyhow::Error::new(super::HeifError::MetadataLimit {
            kind: "test packet",
            bytes: 9 * 1024 * 1024,
            budget: 8 * 1024 * 1024,
        })
        .context("probe context");
        assert!(super::classify_heif_metadata_limit(&bounded));
        assert!(!super::classify_heif_metadata_limit(&anyhow::anyhow!(
            "HEIF metadata exceeds the bounded rewrite limits"
        )));
        assert!(!super::classify_heif_metadata_limit(&anyhow::Error::new(
            super::invalid_layout("unsupported item map")
        )));
    }
}
