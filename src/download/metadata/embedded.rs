//! Embedded writer dispatch with approved-input validation.

#[cfg(all(test, not(feature = "xmp")))]
use super::formats::is_embed_writable_path;
#[cfg(feature = "xmp")]
use super::formats::is_heif_file;
#[cfg(feature = "xmp")]
use super::heif_writer::prepare_metadata_heif;
#[cfg(not(feature = "xmp"))]
use super::native_writer::prepare_metadata_native;
use super::prepared::PreparedMetadataFile;
use super::values::MetadataWrite;
#[cfg(feature = "xmp")]
use super::xmp_writer::prepare_metadata_xmp_toolkit;
#[cfg(feature = "xmp")]
use crate::download::heif;
#[cfg(feature = "xmp")]
use anyhow::Context;
use anyhow::Result;
use std::path::Path;

/// Write the requested metadata into the file, using XMP Toolkit in default
/// builds and native EXIF for JPEG/TIFF in no-`xmp` builds.
///
/// HEIF-family embedded writes use a byte-preserving XMP item update. The
/// writer rejects layouts it cannot update without re-encoding the item graph.
///
/// Atomic: we write to an exclusively created unique sibling, then replace the
/// target only while it still matches the bytes initially read. A crash or
/// concurrent edit leaves the original untouched.
///
/// Dispatch is content-based: the first 12 bytes are inspected for an
/// ISO-BMFF `ftyp` box with a HEIF-family brand. The download pipeline
/// calls this on `.kei-tmp` part files where the path extension has been
/// shadowed by the temp suffix; sniffing bytes makes that safe. Falls
/// back to extension-based dispatch only when the read itself fails, so
/// callers operating on a transient/unreadable file degrade to today's
/// behavior rather than spuriously routing everything to XMP Toolkit.
#[cfg(test)]
pub(crate) fn apply_metadata(path: &Path, write: &MetadataWrite, temp_suffix: &str) -> Result<()> {
    apply_metadata_with_expected_fingerprint(path, write, temp_suffix, None).map(|_| ())
}

#[cfg(test)]
pub(in crate::download) fn apply_metadata_with_expected_fingerprint(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
) -> Result<Option<crate::download::file::ExistingFileFingerprint>> {
    if write.is_empty() {
        return Ok(None);
    }
    #[cfg(not(feature = "xmp"))]
    if !is_embed_writable_path(path) {
        return Ok(None);
    }
    prepare_metadata_with_expected_fingerprint(path, write, temp_suffix, expected_fingerprint)?
        .publish(path)
        .map(Some)
}

pub(in crate::download) fn prepare_metadata_with_expected_fingerprint(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    #[cfg(not(feature = "xmp"))]
    {
        prepare_metadata_native(path, write, temp_suffix, expected_fingerprint)
    }
    #[cfg(feature = "xmp")]
    let is_heif = if let Some(expected) = expected_fingerprint {
        let snapshot = match crate::download::file::fingerprint_regular_file_snapshot_blocking(path)
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(error).context(
                    crate::download::file::ConditionalPublishTargetChanged::Unverifiable {
                        path: path.to_path_buf(),
                    },
                );
            }
        };
        if snapshot.fingerprint != expected {
            return Err(
                crate::download::file::ConditionalPublishTargetChanged::AfterPlanning {
                    path: path.to_path_buf(),
                }
                .into(),
            );
        }
        heif::is_heif_content(
            snapshot
                .prefix
                .get(..snapshot.prefix_len)
                .unwrap_or_default(),
        )
    } else {
        is_heif_file(path)
    };
    #[cfg(feature = "xmp")]
    if is_heif {
        prepare_metadata_heif(path, write, temp_suffix, expected_fingerprint)
    } else {
        prepare_metadata_xmp_toolkit(path, write, temp_suffix, expected_fingerprint)
    }
}

#[cfg(all(test, feature = "xmp"))]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    #[cfg(feature = "xmp")]
    use super::super::test_support::{
        apply_metadata_with_default_suffix, fresh_jpeg, test_tmp_dir,
    };
    use super::super::values::MetadataWrite;
    use std::fs;

    #[test]
    fn apply_metadata_noop_when_empty() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "noop.jpg");
        let before = fs::read(&path).unwrap();
        apply_metadata_with_default_suffix(&path, &MetadataWrite::default()).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "empty write must not touch the file");
        fs::remove_file(&path).ok();
    }
}
