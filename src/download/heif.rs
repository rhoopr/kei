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
mod preservation;
mod relationships;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod test_writer;
mod xmp_read;
mod xmp_write;

use mp4_atom::FourCC;

pub(crate) use boxes::{is_heif_content, is_heif_path};
pub(crate) use exif::{
    extract_exif_capture_time, extract_exif_tiff_bytes, rewrite_exif_capture_time,
};
pub(crate) use file::locate_exif_tiff;
#[cfg(feature = "__fuzz_internals")]
pub(crate) use fuzz::fuzz_rewrite_xmp_preserves;
pub(crate) use preservation::validate_rewrite_preserves_non_xmp_items;
pub(crate) use relationships::validate_capture_repair_item_ownership;
#[cfg(test)]
pub(crate) use test_support::{
    apple_multi_exif_heic, apple_multi_xmp_heic, apple_tmap_conflicting_xmp_heic,
    apple_tmap_insertion_heic,
};
#[cfg(test)]
pub(crate) use test_writer::insert_xmp;
pub(crate) use xmp_read::{extract_xmp_bytes, extract_xmp_strict};
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

#[derive(Debug)]
pub(crate) enum HeifExifError {
    Io(std::io::Error),
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
}
