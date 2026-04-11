use std::path::Path;

use anyhow::{Context, Result};

/// Read the `DateTimeOriginal` EXIF tag from an image file.
///
/// Uses `kamadak-exif` (read-only EXIF crate). A separate crate (`little_exif`)
/// handles writing because no single Rust EXIF library supports both reliable
/// reading and writing. See [`set_photo_exif`] for the write side.
///
/// Returns `Ok(Some(value))` if the tag is present, `Ok(None)` if the file
/// has no EXIF data or the tag is missing, and `Err` only on I/O failure.
pub(crate) fn get_photo_exif(path: &Path) -> Result<Option<String>> {
    let file = std::fs::File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    let mut bufreader = std::io::BufReader::new(&file);
    let exif_reader = exif::Reader::new();

    match exif_reader.read_from_container(&mut bufreader) {
        Ok(exif_data) => {
            if let Some(field) = exif_data.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
            {
                Ok(Some(field.display_value().to_string()))
            } else {
                Ok(None)
            }
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "No EXIF data");
            Ok(None)
        }
    }
}

/// Write the EXIF date tags to a JPEG file.
///
/// Uses `little_exif` (write-capable EXIF crate). A separate crate (`kamadak-exif`)
/// handles reading because `little_exif` doesn't support fine-grained tag reads.
/// See [`get_photo_exif`] for the read side.
///
/// Writes `DateTime`, `DateTimeOriginal`, and `DateTimeDigitized` to match
/// the behavior of `icloudpd`. The `datetime_str` should be in
/// `"YYYY:MM:DD HH:MM:SS"` format.
pub(crate) fn set_photo_exif(path: &Path, datetime_str: &str) -> Result<()> {
    use little_exif::exif_tag::ExifTag;
    use little_exif::filetype::FileExtension;
    use little_exif::metadata::Metadata;

    // Read the file into memory and use write_to_vec with an explicit
    // FileExtension::JPEG.  write_to_file derives the type from the file
    // extension, which fails for .kei-tmp / .part temp files.
    let mut buf =
        std::fs::read(path).with_context(|| format!("Reading {} for EXIF", path.display()))?;

    let mut metadata = match Metadata::new_from_vec(&buf, FileExtension::JPEG) {
        Ok(m) => m,
        Err(_) => Metadata::new(),
    };
    metadata.set_tag(ExifTag::ModifyDate(datetime_str.to_string()));
    metadata.set_tag(ExifTag::DateTimeOriginal(datetime_str.to_string()));
    metadata.set_tag(ExifTag::CreateDate(datetime_str.to_string()));
    metadata
        .write_to_vec(&mut buf, FileExtension::JPEG)
        .with_context(|| format!("Writing EXIF metadata for {}", path.display()))?;

    // Write to a sibling temp file and atomically rename to avoid leaving a
    // truncated file if the process is killed mid-write.
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".exif-tmp");
    let tmp_path = path.with_file_name(&tmp_name);
    std::fs::write(&tmp_path, &buf)
        .with_context(|| format!("Writing EXIF temp file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Renaming {} -> {}", tmp_path.display(), path.display()))?;

    tracing::debug!(
        datetime = %datetime_str,
        path = %path.display(),
        "Set EXIF DateTime/DateTimeOriginal/DateTimeDigitized"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Cross-platform temp directory for tests
    fn test_tmp_dir(subdir: &str) -> PathBuf {
        std::env::temp_dir().join("claude").join(subdir)
    }

    /// Minimal valid JPEG with no EXIF data (SOI + APP0 JFIF + EOI).
    fn minimal_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, // SOI
            0xFF, 0xE0, // APP0 marker
            0x00, 0x10, // Length: 16
            0x4A, 0x46, 0x49, 0x46, 0x00, // "JFIF\0"
            0x01, 0x01, // Version 1.1
            0x00, // Aspect ratio units: none
            0x00, 0x01, // X density: 1
            0x00, 0x01, // Y density: 1
            0x00, 0x00, // No thumbnail
            0xFF, 0xD9, // EOI
        ]
    }

    #[test]
    fn test_set_and_get_exif_roundtrip() {
        let dir = &test_tmp_dir("exif_tests");
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("test_roundtrip.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        let datetime = "2023:06:15 14:30:00";
        set_photo_exif(&path, datetime).unwrap();

        let result = get_photo_exif(&path).unwrap();
        // kamadak-exif formats the date with dashes in the date portion
        assert_eq!(result, Some("2023-06-15 14:30:00".to_string()));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_get_exif_no_exif_data() {
        let dir = &test_tmp_dir("exif_tests");
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("test_no_exif.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        let result = get_photo_exif(&path).unwrap();
        assert_eq!(result, None);

        fs::remove_file(&path).ok();
    }

    /// T-1: Simulate the two-phase download flow (download → EXIF on .part → rename).
    /// If EXIF write panics/fails mid-operation, only the .part file exists — the
    /// final path is never created, preventing corrupt files from reaching the user.
    /// On retry, the .part file contains the unmodified download and can be reprocessed.
    #[test]
    fn test_exif_crash_leaves_no_corrupt_file() {
        let dir = &test_tmp_dir("exif_crash_test");
        fs::create_dir_all(dir).unwrap();

        let final_path = dir.join("photo.jpg");
        // The real download pipeline uses a base32-encoded .part filename, but
        // little_exif requires a recognizable image extension. Use .part.jpg so
        // the EXIF library can identify the file type on retry.
        let part_path = dir.join("photo_part.jpg");

        // Clean up from any previous run
        let _ = fs::remove_file(&final_path);
        let _ = fs::remove_file(&part_path);

        // Phase 1: "Download" — write a valid JPEG to the .part file
        let jpeg_bytes = minimal_jpeg();
        fs::write(&part_path, &jpeg_bytes).unwrap();

        // At this point: .part exists, final path does NOT
        assert!(part_path.exists());
        assert!(
            !final_path.exists(),
            "final path must not exist before rename"
        );

        // Phase 2: Simulate EXIF crash — attempt EXIF write on a corrupt/non-JPEG file
        let corrupt_part = dir.join("corrupt_part.jpg");
        fs::write(&corrupt_part, b"not a jpeg at all").unwrap();
        let exif_result = set_photo_exif(&corrupt_part, "2023:06:15 14:30:00");
        assert!(
            exif_result.is_err(),
            "EXIF write on corrupt file should fail"
        );

        // Critical invariant: final path still does not exist because we never renamed
        assert!(
            !final_path.exists(),
            "final path must not exist after EXIF failure"
        );

        // The original .part file is still intact (unmodified download)
        assert!(
            part_path.exists(),
            ".part file should still exist for retry"
        );
        assert_eq!(
            fs::read(&part_path).unwrap(),
            jpeg_bytes,
            ".part file should contain the unmodified download"
        );

        // Phase 3: Retry — EXIF write succeeds on the valid .part, then rename
        set_photo_exif(&part_path, "2023:06:15 14:30:00").unwrap();
        fs::rename(&part_path, &final_path).unwrap();

        // After successful retry: final path exists, .part is gone
        assert!(
            final_path.exists(),
            "final path should exist after successful retry"
        );
        assert!(!part_path.exists(), ".part should be gone after rename");

        // Verify EXIF was written correctly
        let result = get_photo_exif(&final_path).unwrap();
        assert_eq!(result, Some("2023-06-15 14:30:00".to_string()));
    }

    #[test]
    fn test_set_exif_preserves_existing() {
        let dir = &test_tmp_dir("exif_tests");
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("test_preserve.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        let datetime = "2023:01:01 00:00:00";
        set_photo_exif(&path, datetime).unwrap();

        // Write again with a different date
        let datetime2 = "2024:12:25 12:00:00";
        set_photo_exif(&path, datetime2).unwrap();

        let result = get_photo_exif(&path).unwrap();
        assert_eq!(result, Some("2024-12-25 12:00:00".to_string()));

        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_exif_tmp_file_not_left_behind() {
        let dir = test_tmp_dir("exif_atomic");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("atomic_test.jpg");
        let _ = fs::remove_file(&path);

        // Create a minimal valid JPEG
        fs::write(&path, minimal_jpeg()).unwrap();

        set_photo_exif(&path, "2025:01:01 00:00:00").unwrap();

        // The .exif-tmp file must not exist after a successful call.
        let mut tmp_name = path.file_name().unwrap().to_os_string();
        tmp_name.push(".exif-tmp");
        let tmp_path = path.with_file_name(&tmp_name);
        assert!(
            !tmp_path.exists(),
            ".exif-tmp should be cleaned up after successful write"
        );

        // The original file should still exist and have valid EXIF.
        assert!(path.exists());
        let result = get_photo_exif(&path).unwrap();
        assert_eq!(result, Some("2025-01-01 00:00:00".to_string()));

        fs::remove_file(&path).ok();
    }
}
