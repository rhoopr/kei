//! Embedded metadata (XMP + native EXIF/IPTC reconciliation) via Adobe's
//! XMP Toolkit.
//!
//! The writer runs through [`xmp_toolkit::XmpFile`], which vendors Adobe's
//! reference XMPFiles implementation. One code path covers JPEG, HEIC, PNG,
//! TIFF, MP4, MOV, and more — whatever kei downloads from iCloud ends up with
//! the same metadata embedded in its file bytes.
//!
//! XMP Toolkit also reconciles XMP with native EXIF/IPTC blocks on formats
//! that carry them (notably JPEG), so a consumer reading only EXIF still
//! sees values like `Rating`, GPS, and `DateTimeOriginal`.

use std::path::Path;
use std::sync::Once;

use anyhow::{Context, Result};
use xmp_toolkit::{xmp_ns, OpenFileOptions, XmpFile, XmpMeta, XmpValue};

/// Custom XMP namespace for kei-specific fields that don't fit standard
/// schemas (`hidden`, `archived`, `mediaSubtype`, `burstId`). Consumers that
/// care about these know to look for the `kei` prefix.
const KEI_XMP_NS: &str = "https://github.com/rhoopr/kei/ns/1.0/";
const KEI_XMP_PREFIX: &str = "kei";

static INIT: Once = Once::new();

fn ensure_initialized() {
    INIT.call_once(|| {
        // Registering the same namespace twice is fine; XMP Toolkit returns
        // the existing prefix. Ignore the Result — even a failure here only
        // disables the kei: fields, and standard XMP continues to work.
        let _ = XmpMeta::register_namespace(KEI_XMP_NS, KEI_XMP_PREFIX);
    });
}

/// Snapshot of existing metadata fields that gate write decisions. Populated
/// from whatever XMP Toolkit sees in the file (XMP + reconciled EXIF/IPTC).
#[derive(Debug, Clone, Default)]
pub(crate) struct ExifProbe {
    pub(crate) datetime_original: Option<String>,
    pub(crate) has_gps: bool,
}

pub(crate) fn probe_exif(path: &Path) -> Result<ExifProbe> {
    ensure_initialized();
    let mut file = XmpFile::new().context("creating XmpFile handle")?;
    if file
        .open_file(path, OpenFileOptions::default().for_read().only_xmp())
        .is_err()
    {
        return Ok(ExifProbe::default());
    }
    let meta = match file.xmp() {
        Some(m) => m,
        None => return Ok(ExifProbe::default()),
    };
    let datetime_original = meta
        .property(xmp_ns::EXIF, "DateTimeOriginal")
        .map(|v| v.value);
    let has_gps = meta.contains_property(xmp_ns::EXIF, "GPSLatitude")
        || meta.contains_property(xmp_ns::EXIF, "GPSLongitude");
    Ok(ExifProbe {
        datetime_original,
        has_gps,
    })
}

/// GPS triple passed to [`apply_metadata`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpsCoords {
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
    pub(crate) altitude: Option<f64>,
}

/// Bundle of every field the writer knows how to embed. Empty / default
/// fields are skipped.
#[derive(Debug, Default, Clone)]
pub(crate) struct MetadataWrite {
    /// `"YYYY:MM:DD HH:MM:SS"` EXIF-style datetime string.
    pub(crate) datetime: Option<String>,
    pub(crate) rating: Option<u8>,
    pub(crate) gps: Option<GpsCoords>,
    pub(crate) title: Option<String>,
    pub(crate) description: Option<String>,
    /// `dc:subject` bag — iCloud keyword tags and album names merge here.
    pub(crate) keywords: Vec<String>,
    /// MWG-RS person names for `iptcExt:PersonInImage`.
    pub(crate) people: Vec<String>,
    pub(crate) is_hidden: bool,
    pub(crate) is_archived: bool,
    pub(crate) media_subtype: Option<String>,
    pub(crate) burst_id: Option<String>,
}

impl MetadataWrite {
    pub(crate) fn is_empty(&self) -> bool {
        self.datetime.is_none()
            && self.rating.is_none()
            && self.gps.is_none()
            && self.title.is_none()
            && self.description.is_none()
            && self.keywords.is_empty()
            && self.people.is_empty()
            && !self.is_hidden
            && !self.is_archived
            && self.media_subtype.is_none()
            && self.burst_id.is_none()
    }
}

/// Write the requested metadata into the file's XMP packet, with EXIF/IPTC
/// reconciliation where the container supports it.
///
/// Atomic: we copy the input to a sibling `.meta-tmp`, patch it in place via
/// XmpFile, then rename over the target. A crash mid-write leaves the
/// original untouched.
pub(crate) fn apply_metadata(path: &Path, write: &MetadataWrite) -> Result<()> {
    if write.is_empty() {
        return Ok(());
    }
    ensure_initialized();

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".meta-tmp");
    let tmp_path = path.with_file_name(&tmp_name);
    std::fs::copy(path, &tmp_path)
        .with_context(|| format!("Copying {} -> {}", path.display(), tmp_path.display()))?;

    let result: Result<()> = (|| {
        let mut file = XmpFile::new().context("creating XmpFile handle")?;
        file.open_file(
            &tmp_path,
            OpenFileOptions::default().for_update().use_smart_handler(),
        )
        .with_context(|| format!("Opening {} for XMP update", tmp_path.display()))?;

        let mut meta = file
            .xmp()
            .unwrap_or_else(|| XmpMeta::new().unwrap_or_default());

        if let Some(dt) = &write.datetime {
            // XMP uses ISO 8601 datetimes; our stored form is the EXIF-style
            // "YYYY:MM:DD HH:MM:SS". Convert for XMP, keep a local EXIF copy
            // so XMP Toolkit's reconciler writes the native block too.
            let iso = exif_datetime_to_iso(dt);
            meta.set_property(xmp_ns::XMP, "CreateDate", &XmpValue::new(iso.clone()))?;
            meta.set_property(xmp_ns::XMP, "ModifyDate", &XmpValue::new(iso.clone()))?;
            meta.set_property(
                xmp_ns::EXIF,
                "DateTimeOriginal",
                &XmpValue::new(iso.clone()),
            )?;
            meta.set_property(xmp_ns::PHOTOSHOP, "DateCreated", &XmpValue::new(iso))?;
        }

        if let Some(r) = write.rating {
            meta.set_property_i32(xmp_ns::XMP, "Rating", &XmpValue::new(i32::from(r.min(5))))?;
        }

        if let Some(gps) = write.gps {
            meta.set_property(
                xmp_ns::EXIF,
                "GPSLatitude",
                &XmpValue::new(encode_gps(gps.latitude, 'N', 'S')),
            )?;
            meta.set_property(
                xmp_ns::EXIF,
                "GPSLongitude",
                &XmpValue::new(encode_gps(gps.longitude, 'E', 'W')),
            )?;
            if let Some(alt) = gps.altitude {
                meta.set_property(
                    xmp_ns::EXIF,
                    "GPSAltitude",
                    &XmpValue::new(encode_altitude(alt)),
                )?;
                meta.set_property(
                    xmp_ns::EXIF,
                    "GPSAltitudeRef",
                    &XmpValue::new(if alt < 0.0 { "1" } else { "0" }.to_string()),
                )?;
            }
        }

        if let Some(title) = &write.title {
            meta.set_localized_text(xmp_ns::DC, "title", None, "x-default", title)?;
        }

        if let Some(desc) = &write.description {
            meta.set_localized_text(xmp_ns::DC, "description", None, "x-default", desc)?;
        }

        if !write.keywords.is_empty() {
            // Clear existing dc:subject so we don't accumulate stale entries on
            // re-writes. XMP Toolkit has no bulk set for bags.
            let _ = meta.delete_property(xmp_ns::DC, "subject");
            for kw in &write.keywords {
                meta.append_array_item(
                    xmp_ns::DC,
                    &XmpValue::new("subject".to_string()).set_is_array(true),
                    &XmpValue::new(kw.clone()),
                )?;
            }
        }

        if !write.people.is_empty() {
            let _ = meta.delete_property(xmp_ns::IPTC_EXT, "PersonInImage");
            for name in &write.people {
                meta.append_array_item(
                    xmp_ns::IPTC_EXT,
                    &XmpValue::new("PersonInImage".to_string()).set_is_array(true),
                    &XmpValue::new(name.clone()),
                )?;
            }
        }

        if write.is_hidden {
            meta.set_property_bool(KEI_XMP_NS, "hidden", &XmpValue::new(true))?;
        }
        if write.is_archived {
            meta.set_property_bool(KEI_XMP_NS, "archived", &XmpValue::new(true))?;
        }
        if let Some(subtype) = &write.media_subtype {
            meta.set_property(KEI_XMP_NS, "mediaSubtype", &XmpValue::new(subtype.clone()))?;
        }
        if let Some(burst) = &write.burst_id {
            meta.set_property(KEI_XMP_NS, "burstId", &XmpValue::new(burst.clone()))?;
        }

        if !file.can_put_xmp(&meta) {
            anyhow::bail!(
                "format handler for {} does not support writing XMP",
                tmp_path.display()
            );
        }
        file.put_xmp(&meta)
            .with_context(|| format!("Writing XMP into {}", tmp_path.display()))?;
        file.try_close()
            .with_context(|| format!("Closing {} after XMP update", tmp_path.display()))?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            std::fs::rename(&tmp_path, path).with_context(|| {
                format!("Renaming {} -> {}", tmp_path.display(), path.display())
            })?;
            tracing::debug!(path = %path.display(), "Applied metadata");
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

/// EXIF stores datetimes as `"YYYY:MM:DD HH:MM:SS"`; XMP wants ISO 8601
/// `"YYYY-MM-DDTHH:MM:SS"`. Best-effort conversion — on malformed input we
/// return the original so XMP Toolkit can reject it with a clear error.
fn exif_datetime_to_iso(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() == 19 && bytes[4] == b':' && bytes[7] == b':' && bytes[10] == b' ' {
        let mut out = s.to_owned();
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

/// Encode decimal degrees in the EXIF-in-XMP form `"DEG,MIN.FRACHEMI"` used
/// by [Xmp.exif.GPSLatitude] / `Xmp.exif.GPSLongitude`.
fn encode_gps(decimal: f64, pos: char, neg: char) -> String {
    let hemisphere = if decimal >= 0.0 { pos } else { neg };
    let abs = decimal.abs();
    let deg = abs.floor();
    let min = (abs - deg) * 60.0;
    format!("{},{:.4}{}", deg as u32, min, hemisphere)
}

/// XMP `exif:GPSAltitude` is a rational; we use `meters/1` (scale of 1).
fn encode_altitude(meters: f64) -> String {
    let scaled = (meters.abs() * 1000.0).round() as u64;
    format!("{scaled}/1000")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_tmp_dir(subdir: &str) -> PathBuf {
        std::env::temp_dir().join("claude").join(subdir)
    }

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI).
    fn minimal_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    fn fresh_jpeg(dir: &Path, name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, minimal_jpeg()).unwrap();
        path
    }

    fn read_meta(path: &Path) -> XmpMeta {
        ensure_initialized();
        let mut file = XmpFile::new().unwrap();
        file.open_file(path, OpenFileOptions::default().for_read())
            .unwrap();
        file.xmp().expect("no XMP in file")
    }

    #[test]
    fn apply_metadata_noop_when_empty() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "noop.jpg");
        let before = fs::read(&path).unwrap();
        apply_metadata(&path, &MetadataWrite::default()).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "empty write must not touch the file");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_datetime_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "dt.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_some());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_rating_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "rating.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let rating = meta.property_i32(xmp_ns::XMP, "Rating").unwrap();
        assert_eq!(rating.value, 4);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_rating_clamps_above_5() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "rating_clamp.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                rating: Some(99),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let rating = meta.property_i32(xmp_ns::XMP, "Rating").unwrap();
        assert_eq!(rating.value, 5);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_gps_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "gps.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: Some(17.0),
                }),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.has_gps);
        let meta = read_meta(&path);
        let lat = meta.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value;
        assert!(lat.contains('N'), "lat should end with N: {lat}");
        let lng = meta.property(xmp_ns::EXIF, "GPSLongitude").unwrap().value;
        assert!(lng.contains('W'), "lng should end with W: {lng}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_description_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "desc.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                description: Some("Beach day".to_string()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let (desc, _lang) = meta
            .localized_text(xmp_ns::DC, "description", None, "x-default")
            .unwrap();
        assert_eq!(desc.value, "Beach day");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_title_and_keywords_roundtrip() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "tags.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                title: Some("Vacation shot".to_string()),
                keywords: vec!["vacation".into(), "beach".into(), "Favorites".into()],
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let (title, _lang) = meta
            .localized_text(xmp_ns::DC, "title", None, "x-default")
            .unwrap();
        assert_eq!(title.value, "Vacation shot");
        let subjects: Vec<String> = meta
            .property_array(xmp_ns::DC, "subject")
            .map(|v| v.value)
            .collect();
        assert_eq!(subjects.len(), 3);
        assert!(subjects.contains(&"vacation".to_string()));
        assert!(subjects.contains(&"beach".to_string()));
        assert!(subjects.contains(&"Favorites".to_string()));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_people_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "people.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                people: vec!["Alice".into(), "Bob".into()],
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        let names: Vec<String> = meta
            .property_array(xmp_ns::IPTC_EXT, "PersonInImage")
            .map(|v| v.value)
            .collect();
        assert_eq!(names, vec!["Alice".to_string(), "Bob".to_string()]);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_kei_namespace_fields() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "kei_ns.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                is_hidden: true,
                is_archived: true,
                media_subtype: Some("portrait".into()),
                burst_id: Some("burst_abc".into()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        let meta = read_meta(&path);
        assert!(meta.property_bool(KEI_XMP_NS, "hidden").unwrap().value);
        assert!(meta.property_bool(KEI_XMP_NS, "archived").unwrap().value);
        assert_eq!(
            meta.property(KEI_XMP_NS, "mediaSubtype").unwrap().value,
            "portrait"
        );
        assert_eq!(
            meta.property(KEI_XMP_NS, "burstId").unwrap().value,
            "burst_abc"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_all_fields_single_pass() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "all.jpg");
        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                rating: Some(5),
                gps: Some(GpsCoords {
                    latitude: 1.0,
                    longitude: 2.0,
                    altitude: None,
                }),
                title: Some("T".into()),
                description: Some("D".into()),
                keywords: vec!["k".into()],
                people: vec!["Alice".into()],
                is_hidden: false,
                is_archived: true,
                media_subtype: Some("live_photo".into()),
                burst_id: None,
            },
        )
        .unwrap();
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_some());
        assert!(probe.has_gps);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_cleans_up_tmp_on_failure() {
        let dir = test_tmp_dir("meta_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("corrupt.jpg");
        fs::write(&path, b"not a jpeg").unwrap();
        let result = apply_metadata(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        );
        assert!(result.is_err(), "corrupt file should fail metadata write");
        let mut tmp_name = path.file_name().unwrap().to_os_string();
        tmp_name.push(".meta-tmp");
        let tmp_path = path.with_file_name(&tmp_name);
        assert!(
            !tmp_path.exists(),
            ".meta-tmp must be cleaned up after a failed write"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_exif_reports_empty_on_fresh_jpeg() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "probe_empty.jpg");
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_none());
        assert!(!probe.has_gps);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn exif_datetime_to_iso_converts_valid() {
        assert_eq!(
            exif_datetime_to_iso("2024:06:15 10:00:00"),
            "2024-06-15T10:00:00"
        );
    }

    #[test]
    fn exif_datetime_to_iso_leaves_invalid_unchanged() {
        assert_eq!(exif_datetime_to_iso("not a date"), "not a date");
    }

    #[test]
    fn encode_gps_positive_is_north() {
        let s = encode_gps(37.7749, 'N', 'S');
        assert!(s.ends_with('N'));
        assert!(s.starts_with("37,"));
    }

    #[test]
    fn encode_gps_negative_is_west() {
        let s = encode_gps(-122.4194, 'E', 'W');
        assert!(s.ends_with('W'));
        assert!(s.starts_with("122,"));
    }
}
