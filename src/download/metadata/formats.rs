//! Content and extension detection for supported metadata formats.

#[cfg(feature = "xmp")]
use crate::download::heif;
#[cfg(not(feature = "xmp"))]
use little_exif::filetype::FileExtension;
use std::path::Path;

/// Read the first 12 bytes of `path` and dispatch to [`heif::is_heif_content`].
/// On read error, fall back to extension-based detection — preserves the
/// pre-content-sniff behavior for any caller that hands us an unreadable
/// path, rather than misclassifying every such call as non-HEIF.
#[cfg(feature = "xmp")]
pub(super) fn is_heif_file(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 12];
    match std::fs::File::open(path).and_then(|mut f| f.read(&mut head)) {
        Ok(n) => heif::is_heif_content(head.get(..n).unwrap_or(&[])),
        Err(_) => heif::is_heif_path(path),
    }
}

/// Camera raw formats built on TIFF. Their readers expect the vendor layout,
/// so kei never rewrites them in place.
const TIFF_RAW_EXTENSIONS: &[&str] = &[
    "3fr", "arw", "cr2", "dcr", "dng", "erf", "iiq", "kdc", "mef", "mos", "nef", "nrw", "orf",
    "pef", "raw", "rw2", "rwl", "sr2", "srf", "srw",
];

fn has_raw_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            TIFF_RAW_EXTENSIONS
                .iter()
                .any(|raw| extension.eq_ignore_ascii_case(raw))
        })
}

/// Whether the in-place embedded-metadata writer can patch this file.
///
/// The first 12 bytes take precedence over the extension so downloaded part
/// files and misleading extensions route safely. HEIC / HEIF / AVIF use the
/// byte-preserving item-map writer. JPEG / PNG / TIFF / MP4 / MOV continue
/// through XMP Toolkit.
#[must_use]
pub(crate) fn is_embed_writable_path(path: &Path) -> bool {
    // Raw originals such as DNG are TIFF inside, so content alone would send
    // them to the TIFF writer. They get sidecars only.
    if has_raw_extension(path) {
        return false;
    }
    let mut head = [0u8; 12];
    if let Ok(n) = std::fs::File::open(path).and_then(|mut f| {
        use std::io::Read;
        f.read(&mut head)
    }) {
        let head = head.get(..n).unwrap_or(&[]);
        if head.starts_with(&[0xff, 0xd8, 0xff])
            || head.starts_with(b"II*\0")
            || head.starts_with(b"MM\0*")
        {
            return true;
        }
        #[cfg(feature = "xmp")]
        if heif::is_heif_content(head) {
            return true;
        }
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    if ext
        .as_deref()
        .is_some_and(|e| matches!(e, "jpg" | "jpeg" | "tif" | "tiff"))
    {
        return true;
    }
    #[cfg(feature = "xmp")]
    if heif::is_heif_path(path) {
        return true;
    }
    #[cfg(feature = "xmp")]
    {
        ext.as_deref()
            .is_some_and(|e| matches!(e, "png" | "mp4" | "mov"))
    }
    #[cfg(not(feature = "xmp"))]
    {
        false
    }
}

#[cfg(not(feature = "xmp"))]
pub(super) fn native_file_type(bytes: &[u8], path: &Path) -> Option<FileExtension> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(FileExtension::JPEG);
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some(FileExtension::TIFF);
    }
    path.extension()
        .and_then(|e| e.to_str())
        .and_then(|e| match e.to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Some(FileExtension::JPEG),
            "tif" | "tiff" => Some(FileExtension::TIFF),
            _ => None,
        })
}

#[cfg(all(test, feature = "xmp"))]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    #[cfg(feature = "xmp")]
    use super::super::test_support::{heif_ftyp_without_meta, test_tmp_dir};
    use super::is_embed_writable_path;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[test]
    fn is_embed_writable_path_recognises_supported_formats() {
        for ext in [
            "jpg", "jpeg", "JPG", "png", "PNG", "tif", "tiff", "mp4", "MOV",
        ] {
            let p = PathBuf::from(format!("/a/b.{ext}"));
            assert!(is_embed_writable_path(&p), "{ext} should be writable");
        }
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn is_embed_writable_path_recognises_heif_formats() {
        for ext in ["heic", "HEIF", "hif", "avif"] {
            let p = PathBuf::from(format!("/a/b.{ext}"));
            assert!(is_embed_writable_path(&p), "{ext} should be writable");
        }

        let dir = test_tmp_dir("embed_writable_heif");
        fs::create_dir_all(&dir).unwrap();
        let part_path = dir.join("image.kei-tmp");
        fs::write(&part_path, heif_ftyp_without_meta()).unwrap();
        assert!(is_embed_writable_path(&part_path));
    }

    #[test]
    fn is_embed_writable_path_rejects_unsupported_formats() {
        for ext in ["dng", "raf", "aae", "gif", "webp", ""] {
            let p = PathBuf::from(format!("/a/b.{ext}"));
            assert!(!is_embed_writable_path(&p), "{ext} should NOT be writable");
        }
        assert!(!is_embed_writable_path(Path::new("/a/b")));
    }

    #[test]
    fn is_embed_writable_path_keeps_tiff_based_raw_files_sidecar_only() {
        let dir = tempfile::tempdir().unwrap();
        let dng = include_bytes!("../../../tests/data/media/pattern.dng");
        assert!(dng.starts_with(b"II*\0"), "the fixture is TIFF inside");
        for name in ["pattern.dng", "PATTERN.DNG", "camera.NEF", "camera.cr2"] {
            let path = dir.path().join(name);
            fs::write(&path, dng).unwrap();
            assert!(!is_embed_writable_path(&path), "{name}");
        }
        let tiff = dir.path().join("pattern.tif");
        fs::write(&tiff, dng).unwrap();
        assert!(
            is_embed_writable_path(&tiff),
            "a real TIFF name still embeds"
        );
    }
}
