//! Embedded metadata (XMP + native EXIF/IPTC reconciliation).
//!
//! With the default `xmp` feature, JPEG / PNG / TIFF / MP4 / MOV run through
//! Adobe's XMPFiles implementation, which reconciles XMP with native EXIF/IPTC
//! blocks so EXIF-only consumers still see values like `Rating`, GPS, and
//! `DateTimeOriginal`. HEIC / HEIF / AVIF route through the ISO-BMFF helper in
//! [`super::heif`].
//!
//! Without the `xmp` feature, kei still writes the native EXIF subset supported
//! by `little_exif` for JPEG/TIFF and quietly skips formats that need XMP
//! serialization.
//!
//! Owners: `probe` reads existing fields; `source_gps` decodes source EXIF;
//! `values` holds write values; `xmp_fields` owns managed XMP properties.
//! `embedded` dispatches to format writers, which prepare output through
//! `prepared`. Only `prepared` publishes embedded replacements. `sidecar`
//! owns sidecar preparation, ownership, and publication. `formats` owns
//! content detection. Child production dependencies are one-way.
//!
//! Tests retain their names and assertions. Former `metadata::tests` paths
//! become `metadata::<owner>::tests`; native tests become
//! `metadata::<owner>::native_tests`. Shared fixtures live in `test_support`.
//! The audited datetime conversion stays here so its `UNSAFE.md` entry remains
//! valid within this issue's facade-plus-child-directory boundary.

mod embedded;
mod formats;
#[cfg(feature = "xmp")]
mod heif_writer;
#[cfg(not(feature = "xmp"))]
mod native_writer;
mod prepared;
mod probe;
#[cfg(feature = "xmp")]
mod sidecar;
#[cfg(feature = "xmp")]
mod source_gps;
#[cfg(test)]
mod test_support;
mod values;
#[cfg(feature = "xmp")]
mod xmp_fields;
#[cfg(feature = "xmp")]
mod xmp_writer;

#[cfg(test)]
pub(crate) use embedded::apply_metadata;
#[cfg(test)]
#[allow(
    unused_imports,
    reason = "preserve the existing test helper path without the xmp feature"
)]
pub(super) use embedded::apply_metadata_with_expected_fingerprint;
pub(super) use embedded::prepare_metadata_with_expected_fingerprint;
pub(crate) use formats::is_embed_writable_path;
pub(super) use prepared::PreparedMetadataFile;
#[allow(
    unused_imports,
    reason = "preserve the existing facade type path in non-test builds"
)]
pub(super) use probe::HeifNativeCaptureTime;
pub(crate) use probe::{ExifProbe, probe_exif};
#[cfg(feature = "xmp")]
pub(crate) use sidecar::write_sidecar;
#[cfg(feature = "xmp")]
pub(super) use sidecar::{ReconciledSidecar, write_reconciled_sidecar};
#[cfg(feature = "__fuzz_internals")]
pub(crate) use source_gps::fuzz_tiff_source_gps;
#[cfg(feature = "xmp")]
pub(crate) use source_gps::read_source_gps;
#[cfg(feature = "xmp")]
pub(super) use source_gps::read_source_gps_from_file;
#[cfg(feature = "xmp")]
pub(crate) use values::SourceGpsMetadata;
#[allow(
    unused_imports,
    reason = "preserve the existing facade type path for both feature configurations"
)]
pub(crate) use values::XmpRational;
pub(crate) use values::{GpsCoords, MetadataWrite};

/// EXIF stores datetimes as `"YYYY:MM:DD HH:MM:SS"`; XMP wants ISO 8601
/// `"YYYY-MM-DDTHH:MM:SS"`. Best-effort conversion — on malformed input we
/// return the original so XMP Toolkit can reject it with a clear error.
#[allow(
    clippy::indexing_slicing,
    reason = "indices 4, 7, 10 are provably in-bounds under the `bytes.len() == 19` guard"
)]
#[cfg(feature = "xmp")]
fn exif_datetime_to_iso(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() == 19 && bytes[4] == b':' && bytes[7] == b':' && bytes[10] == b' ' {
        let mut out = s.to_owned();
        // SAFETY: `out` is a freshly-owned String with no aliases. The length
        // check above proves indices 4, 7, 10 are in-bounds, and the
        // replacement bytes are all valid 7-bit ASCII, so UTF-8
        // well-formedness is preserved.
        unsafe {
            let b = out.as_bytes_mut();
            b[4] = b'-';
            b[7] = b'-';
            b[10] = b'T';
        }
        out
    } else {
        s.to_owned()
    }
}
