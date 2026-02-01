use std::path::Path;

use anyhow::{Context, Result};

/// Read the `DateTimeOriginal` EXIF tag from an image file.
///
/// Returns `Ok(Some(value))` if the tag is present, `Ok(None)` if the file
/// has no EXIF data or the tag is missing, and `Err` only on I/O failure.
pub fn get_photo_exif(path: &Path) -> Result<Option<String>> {
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
            tracing::debug!("No EXIF data in {}: {}", path.display(), e);
            Ok(None)
        }
    }
}

/// Write the EXIF date tags to a JPEG file.
///
/// Writes `DateTime`, `DateTimeOriginal`, and `DateTimeDigitized` to match
/// the behavior of the Python icloudpd. The `datetime_str` should be in
/// `"YYYY:MM:DD HH:MM:SS"` format.
pub fn set_photo_exif(path: &Path, datetime_str: &str) -> Result<()> {
    use little_exif::exif_tag::ExifTag;
    use little_exif::metadata::Metadata;

    let mut metadata = match Metadata::new_from_path(path) {
        Ok(m) => m,
        Err(_) => Metadata::new(),
    };
    metadata.set_tag(ExifTag::ModifyDate(datetime_str.to_string()));
    metadata.set_tag(ExifTag::DateTimeOriginal(datetime_str.to_string()));
    metadata.set_tag(ExifTag::CreateDate(datetime_str.to_string()));
    metadata
        .write_to_file(path)
        .with_context(|| format!("Writing EXIF metadata to {}", path.display()))?;

    tracing::debug!(
        "Set EXIF DateTime/DateTimeOriginal/DateTimeDigitized={} on {}",
        datetime_str,
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
        let dir = Path::new("/tmp/claude/exif_tests");
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
        let dir = Path::new("/tmp/claude/exif_tests");
        fs::create_dir_all(dir).unwrap();
        let path = dir.join("test_no_exif.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        let result = get_photo_exif(&path).unwrap();
        assert_eq!(result, None);

        fs::remove_file(&path).ok();
    }

    #[test]
    fn test_set_exif_preserves_existing() {
        let dir = Path::new("/tmp/claude/exif_tests");
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
}
