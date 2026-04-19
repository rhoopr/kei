use std::path::Path;

use anyhow::{Context, Result};

/// EXIF tag 0x4746 (`xmp:Rating` mapping, short in IFD0, 0-5 scale).
const EXIF_TAG_RATING: u16 = 0x4746;

/// Bundle of EXIF fields to apply in a single read-modify-write cycle.
///
/// Any field left `None` is not written. `None` values also mean the caller
/// chose not to enrich the corresponding tag — they are distinct from the tag
/// being explicitly cleared, which kei never does.
#[derive(Debug, Default, Clone)]
pub(crate) struct ExifWrite {
    /// `"YYYY:MM:DD HH:MM:SS"` string applied to DateTime/DateTimeOriginal/
    /// DateTimeDigitized. Only written when the file has no DateTimeOriginal.
    pub(crate) datetime: Option<String>,
    /// 1-5 star rating. Writes EXIF tag 0x4746.
    pub(crate) rating: Option<u8>,
    /// GPS triple (decimal degrees WGS84 for lat/lng; meters for alt). Written
    /// as EXIF GPS IFD tags only when the file has no existing GPS data.
    pub(crate) gps: Option<GpsCoords>,
    /// ImageDescription (EXIF tag 0x010E). Always overwrites.
    pub(crate) description: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct GpsCoords {
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
    pub(crate) altitude: Option<f64>,
}

impl ExifWrite {
    pub(crate) fn is_empty(&self) -> bool {
        self.datetime.is_none()
            && self.rating.is_none()
            && self.gps.is_none()
            && self.description.is_none()
    }
}

/// Snapshot of the EXIF fields that gate enrichment decisions. A single
/// parse answers "do we need to write datetime?" and "do we need to write
/// GPS?" in one pass, avoiding two file opens per asset.
#[derive(Debug, Clone, Default)]
pub(crate) struct ExifProbe {
    pub(crate) datetime_original: Option<String>,
    pub(crate) has_gps: bool,
}

/// Parse the image's existing EXIF once and report which gating fields are
/// present. Returns `Ok(default)` on a file with no EXIF — consumers treat
/// missing as "unknown" and fall back to overwrite.
pub(crate) fn probe_exif(path: &Path) -> Result<ExifProbe> {
    let file = std::fs::File::open(path).with_context(|| format!("Opening {}", path.display()))?;
    let mut bufreader = std::io::BufReader::new(&file);
    let reader = exif::Reader::new();
    match reader.read_from_container(&mut bufreader) {
        Ok(data) => {
            let datetime_original = data
                .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
                .map(|f| f.display_value().to_string());
            let has_gps = data
                .get_field(exif::Tag::GPSLatitude, exif::In::PRIMARY)
                .is_some()
                || data
                    .get_field(exif::Tag::GPSLongitude, exif::In::PRIMARY)
                    .is_some();
            Ok(ExifProbe {
                datetime_original,
                has_gps,
            })
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "No EXIF data");
            Ok(ExifProbe::default())
        }
    }
}

/// Read the `DateTimeOriginal` EXIF tag. Thin test-only wrapper around
/// `probe_exif` for the tests that only care about that single field.
#[cfg(test)]
pub(crate) fn get_photo_exif(path: &Path) -> Result<Option<String>> {
    probe_exif(path).map(|p| p.datetime_original)
}

/// Write the EXIF date tags to a JPEG file — thin test-only wrapper.
///
/// Equivalent to `apply_exif(path, ExifWrite { datetime: Some(..), .. })`.
/// Production call sites go through `apply_exif` directly.
#[cfg(test)]
pub(crate) fn set_photo_exif(path: &Path, datetime_str: &str) -> Result<()> {
    apply_exif(
        path,
        &ExifWrite {
            datetime: Some(datetime_str.to_string()),
            ..ExifWrite::default()
        },
    )
}

/// Apply the requested EXIF fields to a JPEG file in a single read-modify-write.
///
/// Uses `little_exif` (write-capable EXIF crate). A separate crate (`kamadak-exif`)
/// handles reading because `little_exif` doesn't support fine-grained tag reads.
///
/// The write is atomic: metadata is serialized to a sibling `.exif-tmp` file
/// and renamed over the target. A crash mid-write leaves the original intact.
pub(crate) fn apply_exif(path: &Path, write: &ExifWrite) -> Result<()> {
    use little_exif::exif_tag::ExifTag;
    use little_exif::filetype::FileExtension;
    use little_exif::ifd::ExifTagGroup;
    use little_exif::metadata::Metadata;

    if write.is_empty() {
        return Ok(());
    }

    // Read into memory with explicit FileExtension::JPEG — the download path is
    // a .part-style temp file whose extension can't be auto-detected.
    let mut buf =
        std::fs::read(path).with_context(|| format!("Reading {} for EXIF", path.display()))?;

    let mut metadata = match Metadata::new_from_vec(&buf, FileExtension::JPEG) {
        Ok(m) => m,
        Err(_) => Metadata::new(),
    };

    if let Some(dt) = &write.datetime {
        metadata.set_tag(ExifTag::ModifyDate(dt.clone()));
        metadata.set_tag(ExifTag::DateTimeOriginal(dt.clone()));
        metadata.set_tag(ExifTag::CreateDate(dt.clone()));
    }

    if let Some(rating) = write.rating {
        // EXIF Rating (0x4746) is an INT16U in IFD0. little_exif doesn't have a
        // named variant, so we use the UnknownINT16U escape hatch.
        metadata.set_tag(ExifTag::UnknownINT16U(
            vec![u16::from(rating.min(5))],
            EXIF_TAG_RATING,
            ExifTagGroup::GENERIC,
        ));
    }

    if let Some(gps) = write.gps {
        let (lat_ref, lat_triple) = to_gps_ref_and_dms(gps.latitude, "N", "S");
        let (lng_ref, lng_triple) = to_gps_ref_and_dms(gps.longitude, "E", "W");
        metadata.set_tag(ExifTag::GPSLatitudeRef(lat_ref));
        metadata.set_tag(ExifTag::GPSLatitude(lat_triple));
        metadata.set_tag(ExifTag::GPSLongitudeRef(lng_ref));
        metadata.set_tag(ExifTag::GPSLongitude(lng_triple));
        if let Some(alt) = gps.altitude {
            let alt_ref: u8 = if alt < 0.0 { 1 } else { 0 };
            metadata.set_tag(ExifTag::GPSAltitudeRef(vec![alt_ref]));
            metadata.set_tag(ExifTag::GPSAltitude(vec![
                little_exif::rational::uR64::from(alt.abs()),
            ]));
        }
    }

    if let Some(desc) = &write.description {
        metadata.set_tag(ExifTag::ImageDescription(desc.clone()));
    }

    metadata
        .write_to_vec(&mut buf, FileExtension::JPEG)
        .with_context(|| format!("Writing EXIF metadata for {}", path.display()))?;

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".exif-tmp");
    let tmp_path = path.with_file_name(&tmp_name);
    std::fs::write(&tmp_path, &buf)
        .with_context(|| format!("Writing EXIF temp file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Renaming {} -> {}", tmp_path.display(), path.display()))?;

    tracing::debug!(
        path = %path.display(),
        datetime = ?write.datetime.as_deref(),
        rating = ?write.rating,
        has_gps = write.gps.is_some(),
        has_description = write.description.is_some(),
        "Applied EXIF metadata"
    );
    Ok(())
}

/// Decompose a decimal degree value into an EXIF GPS `[deg, min, sec]` rational
/// triple plus its hemisphere reference.
fn to_gps_ref_and_dms(
    deg: f64,
    positive_ref: &str,
    negative_ref: &str,
) -> (String, Vec<little_exif::rational::uR64>) {
    use little_exif::rational::uR64;
    let hemisphere = if deg >= 0.0 {
        positive_ref
    } else {
        negative_ref
    };
    let abs = deg.abs();
    let d = abs.floor();
    let m_frac = (abs - d) * 60.0;
    let m = m_frac.floor();
    let s = (m_frac - m) * 60.0;
    // Store seconds with 4-decimal precision, which is enough to round-trip
    // Apple's f64 lat/lng to ~1 cm accuracy.
    let s_scaled = (s * 10_000.0).round();
    let triple = vec![
        uR64 {
            nominator: d as u32,
            denominator: 1,
        },
        uR64 {
            nominator: m as u32,
            denominator: 1,
        },
        uR64 {
            nominator: s_scaled as u32,
            denominator: 10_000,
        },
    ];
    (hemisphere.to_string(), triple)
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

    // ── Feature 2: rating / GPS / description writers ───────────────────

    fn read_field(path: &Path, tag: exif::Tag) -> Option<String> {
        let file = std::fs::File::open(path).ok()?;
        let mut r = std::io::BufReader::new(&file);
        let data = exif::Reader::new().read_from_container(&mut r).ok()?;
        data.get_field(tag, exif::In::PRIMARY)
            .map(|f| f.display_value().to_string())
    }

    fn read_u32_field(path: &Path, tag: exif::Tag) -> Option<u32> {
        let file = std::fs::File::open(path).ok()?;
        let mut r = std::io::BufReader::new(&file);
        let data = exif::Reader::new().read_from_container(&mut r).ok()?;
        data.get_field(tag, exif::In::PRIMARY)
            .and_then(|f| f.value.get_uint(0))
    }

    #[test]
    fn apply_exif_is_noop_when_empty() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("noop.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();
        let before = fs::read(&path).unwrap();
        apply_exif(&path, &ExifWrite::default()).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "empty write must not touch the file");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_exif_rating_roundtrips() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rating.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        apply_exif(
            &path,
            &ExifWrite {
                rating: Some(4),
                ..ExifWrite::default()
            },
        )
        .unwrap();

        // Tag 0x4746 is Rating (no named constant in kamadak-exif).
        let rating = read_u32_field(&path, exif::Tag(exif::Context::Tiff, 0x4746))
            .expect("Rating tag missing");
        assert_eq!(rating, 4);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_exif_rating_clamps_above_5() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rating_clamp.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        apply_exif(
            &path,
            &ExifWrite {
                rating: Some(99),
                ..ExifWrite::default()
            },
        )
        .unwrap();

        let rating = read_u32_field(&path, exif::Tag(exif::Context::Tiff, 0x4746)).unwrap();
        assert_eq!(rating, 5, "rating must clamp to 5");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_exif_gps_writes_ref_and_dms_triple() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gps.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        apply_exif(
            &path,
            &ExifWrite {
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: Some(17.0),
                }),
                ..ExifWrite::default()
            },
        )
        .unwrap();

        assert_eq!(
            read_field(&path, exif::Tag::GPSLatitudeRef).as_deref(),
            Some("N")
        );
        assert_eq!(
            read_field(&path, exif::Tag::GPSLongitudeRef).as_deref(),
            Some("W")
        );
        // GPSLatitude / GPSLongitude are displayed as "deg/1, min/1, sec/10000"
        // in kamadak-exif. Just verify they're present — decomposition is
        // covered by the unit test on to_gps_ref_and_dms.
        assert!(read_field(&path, exif::Tag::GPSLatitude).is_some());
        assert!(read_field(&path, exif::Tag::GPSLongitude).is_some());
        // Altitude is RATIONAL64U meters; ref=0 means above sea level.
        assert!(read_field(&path, exif::Tag::GPSAltitude).is_some());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_exif_gps_negative_altitude_sets_alt_ref_1() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gps_neg_alt.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        apply_exif(
            &path,
            &ExifWrite {
                gps: Some(GpsCoords {
                    latitude: 0.0,
                    longitude: 0.0,
                    altitude: Some(-50.0),
                }),
                ..ExifWrite::default()
            },
        )
        .unwrap();

        let alt_ref = read_u32_field(&path, exif::Tag::GPSAltitudeRef).unwrap();
        assert_eq!(alt_ref, 1, "below-sea-level altitude must set ref = 1");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_exif_description_roundtrips() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desc.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        apply_exif(
            &path,
            &ExifWrite {
                description: Some("Beach sunset".to_string()),
                ..ExifWrite::default()
            },
        )
        .unwrap();

        let desc = read_field(&path, exif::Tag::ImageDescription).unwrap();
        assert!(
            desc.contains("Beach sunset"),
            "expected description to contain 'Beach sunset', got {desc}"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_exif_all_fields_single_pass() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("all.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();

        apply_exif(
            &path,
            &ExifWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                rating: Some(3),
                gps: Some(GpsCoords {
                    latitude: 1.0,
                    longitude: 2.0,
                    altitude: None,
                }),
                description: Some("caption".to_string()),
            },
        )
        .unwrap();

        assert_eq!(
            read_field(&path, exif::Tag::DateTimeOriginal).as_deref(),
            Some("2024-06-15 10:00:00")
        );
        assert_eq!(
            read_u32_field(&path, exif::Tag(exif::Context::Tiff, 0x4746)),
            Some(3)
        );
        assert_eq!(
            read_field(&path, exif::Tag::GPSLatitudeRef).as_deref(),
            Some("N")
        );
        assert!(read_field(&path, exif::Tag::ImageDescription)
            .unwrap()
            .contains("caption"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_exif_reports_no_gps_on_blank_jpeg() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("no_gps.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(!probe.has_gps);
        assert!(probe.datetime_original.is_none());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_exif_reports_gps_after_write() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("with_gps.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();
        apply_exif(
            &path,
            &ExifWrite {
                gps: Some(GpsCoords {
                    latitude: 10.0,
                    longitude: 20.0,
                    altitude: None,
                }),
                ..ExifWrite::default()
            },
        )
        .unwrap();
        assert!(probe_exif(&path).unwrap().has_gps);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_exif_reports_datetime_when_present() {
        let dir = test_tmp_dir("exif_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("probe_dt.jpg");
        fs::write(&path, minimal_jpeg()).unwrap();
        apply_exif(
            &path,
            &ExifWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..ExifWrite::default()
            },
        )
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_some());
        assert!(!probe.has_gps);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn to_gps_ref_and_dms_positive_latitude() {
        let (refstr, triple) = to_gps_ref_and_dms(37.7749, "N", "S");
        assert_eq!(refstr, "N");
        assert_eq!(triple.len(), 3);
        assert_eq!(triple[0].nominator, 37);
        assert_eq!(triple[1].nominator, 46); // 0.7749 * 60 = 46.494
                                             // seconds with 4-decimal scaling: (0.494 * 60) * 10000 ≈ 296400
        assert!(
            triple[2].nominator > 290_000 && triple[2].nominator < 300_000,
            "seconds nominator was {}",
            triple[2].nominator
        );
    }

    #[test]
    fn to_gps_ref_and_dms_negative_longitude() {
        let (refstr, triple) = to_gps_ref_and_dms(-122.4194, "E", "W");
        assert_eq!(refstr, "W");
        assert_eq!(triple[0].nominator, 122);
    }
}
