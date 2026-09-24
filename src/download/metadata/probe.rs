//! EXIF and XMP evidence used by metadata write policy.

#[cfg(feature = "xmp")]
use super::formats::is_heif_file;
#[cfg(not(feature = "xmp"))]
use super::formats::native_file_type;
#[cfg(feature = "xmp")]
use super::xmp_fields::{EXIF_EX_XMP_NS, ensure_initialized};
#[cfg(feature = "xmp")]
use crate::download::heif;
#[cfg(feature = "xmp")]
use anyhow::Context;
use anyhow::Result;
use chrono::{DateTime, FixedOffset, NaiveDateTime, Timelike};
use little_exif::exif_tag::ExifTag;
#[cfg(feature = "xmp")]
use little_exif::filetype::FileExtension;
use little_exif::metadata::Metadata;
use std::path::Path;
#[cfg(feature = "xmp")]
use xmp_toolkit::{OpenFileOptions, XmpFile, XmpMeta, xmp_ns};

/// Snapshot of existing metadata fields that gate write decisions. HEIF
/// probes merge XMP with the standalone Exif item. Other formats use XMP
/// Toolkit in default builds or native EXIF without the `xmp` feature.
#[derive(Debug, Clone, Default)]
pub(in crate::download) enum HeifNativeCaptureTime {
    #[default]
    NotApplicable,
    #[cfg(feature = "xmp")]
    Missing,
    #[cfg(feature = "xmp")]
    Present {
        datetime_original: Option<String>,
        offset_time_original: Option<String>,
    },
    #[cfg(feature = "xmp")]
    Unreadable,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExifProbe {
    pub(crate) datetime_original: Option<String>,
    pub(crate) offset_time_original: Option<String>,
    pub(crate) has_other_datetime_offset: bool,
    pub(crate) has_gps: bool,
    pub(in crate::download) native_heif_capture_time: HeifNativeCaptureTime,
}

impl ExifProbe {
    pub(crate) fn has_any_datetime_offset(&self) -> bool {
        self.offset_time_original.is_some() || self.has_other_datetime_offset
    }

    /// True when the capture timestamp already in the file renders
    /// `created_local`, and carries no zone that contradicts it.
    ///
    /// A resolved offset may only join a timestamp that satisfies this. A file
    /// written before capture-local resolution holds a wall clock in the
    /// backup host's timezone, so pairing Apple's offset with it would assert
    /// an instant the asset never had.
    pub(crate) fn denotes_capture_time(&self, created_local: &DateTime<FixedOffset>) -> bool {
        let Some(existing) = self.datetime_original.as_deref() else {
            return false;
        };
        capture_timestamp_matches(existing, created_local)
    }

    pub(crate) fn native_heif_capture_time_repair_required(
        &self,
        created_local: &DateTime<FixedOffset>,
        expected_offset: &str,
    ) -> std::result::Result<Option<bool>, &'static str> {
        #[cfg(not(feature = "xmp"))]
        let _ = (created_local, expected_offset);
        match &self.native_heif_capture_time {
            HeifNativeCaptureTime::NotApplicable => Ok(None),
            #[cfg(feature = "xmp")]
            HeifNativeCaptureTime::Missing => Err("HEIF file has no native Exif item"),
            #[cfg(feature = "xmp")]
            HeifNativeCaptureTime::Unreadable => Err("HEIF native Exif item is unreadable"),
            #[cfg(feature = "xmp")]
            HeifNativeCaptureTime::Present {
                datetime_original: None,
                ..
            } => Err("HEIF native Exif item has no DateTimeOriginal"),
            #[cfg(feature = "xmp")]
            HeifNativeCaptureTime::Present {
                offset_time_original: None,
                ..
            } => Err("HEIF native Exif item has no OffsetTimeOriginal"),
            #[cfg(feature = "xmp")]
            HeifNativeCaptureTime::Present {
                datetime_original: Some(datetime),
                offset_time_original: Some(offset),
            } => {
                let denotes_capture_time = capture_timestamp_matches(datetime, created_local);
                Ok(Some(!denotes_capture_time || offset != expected_offset))
            }
        }
    }
}

fn capture_timestamp_matches(value: &str, expected: &DateTime<FixedOffset>) -> bool {
    parse_capture_timestamp(value).is_some_and(|(wall_clock, zone)| {
        // Native writers may omit subseconds. Never forgive an incorrect nonzero fraction.
        (wall_clock == expected.naive_local()
            || Some(wall_clock) == expected.naive_local().with_nanosecond(0))
            && zone.is_none_or(|zone| zone == *expected.offset())
    })
}

/// Split a capture timestamp into its wall clock and the zone it names, if
/// any. XMP holds ISO 8601 and the native EXIF block holds
/// `YYYY:MM:DD HH:MM:SS`, so both forms reach the probe, and either may arrive
/// padded with whitespace or trailing NULs.
fn parse_capture_timestamp(value: &str) -> Option<(NaiveDateTime, Option<FixedOffset>)> {
    let value = value.trim_matches(|c: char| c.is_whitespace() || c == '\0');
    if let Ok(zoned) = DateTime::parse_from_rfc3339(value) {
        return Some((zoned.naive_local(), Some(*zoned.offset())));
    }
    [
        "%Y:%m:%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
    ]
    .into_iter()
    .find_map(|format| NaiveDateTime::parse_from_str(value, format).ok())
    .map(|wall_clock| (wall_clock, None))
}

/// Read the existing XMP / EXIF state of a media file.
///
/// Dispatch is content-based, mirroring [`apply_metadata`]: the first 12 bytes
/// are inspected for an ISO-BMFF `ftyp` box with a HEIF-family brand. HEIF
/// inputs get parsed through their XMP and Exif items because XMP Toolkit
/// ships no HEIF handler. Non-HEIF inputs go through the XMP Toolkit smart
/// handler so JPEG/PNG/TIFF/MP4/MOV reconciled EXIF/IPTC is visible. An
/// unreadable HEIF Exif item is treated as existing metadata so a derived
/// datetime or GPS value cannot compete with data kei could not inspect.
pub(crate) fn probe_exif(path: &Path) -> Result<ExifProbe> {
    #[cfg(feature = "xmp")]
    {
        probe_exif_xmp(path)
    }
    #[cfg(not(feature = "xmp"))]
    {
        Ok(probe_exif_native(path))
    }
}

#[cfg(feature = "xmp")]
fn probe_exif_xmp(path: &Path) -> Result<ExifProbe> {
    ensure_initialized();
    if is_heif_file(path) {
        Ok(probe_exif_heif(path))
    } else {
        probe_exif_xmp_toolkit(path)
    }
}

#[cfg(feature = "xmp")]
fn probe_exif_xmp_toolkit(path: &Path) -> Result<ExifProbe> {
    let mut file = XmpFile::new().context("Could not create XMP handle")?;
    if file
        .open_file(path, OpenFileOptions::default().for_read().only_xmp())
        .is_err()
    {
        return Ok(ExifProbe::default());
    }
    let Some(meta) = file.xmp() else {
        return Ok(ExifProbe::default());
    };
    Ok(probe_from_meta(&meta))
}

#[cfg(feature = "xmp")]
fn probe_exif_heif(path: &Path) -> ExifProbe {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(target: "kei::download::metadata", path = %path.display(), error = %e, "Failed to read HEIF for probe");
            return ExifProbe::default();
        }
    };
    let mut probe = match heif::extract_xmp_bytes(&bytes) {
        Some(xmp_bytes) => match std::str::from_utf8(&xmp_bytes) {
            Ok(text) => match text.parse::<XmpMeta>() {
                Ok(meta) => probe_from_meta(&meta),
                Err(error) => {
                    tracing::warn!(target: "kei::download::metadata", path = %path.display(), %error, "Failed to parse HEIF XMP packet");
                    ExifProbe::default()
                }
            },
            Err(_) => {
                tracing::warn!(target: "kei::download::metadata", path = %path.display(), "HEIF XMP packet is not UTF-8; treating it as absent");
                ExifProbe::default()
            }
        },
        None => ExifProbe::default(),
    };
    match heif::extract_exif_capture_time(&bytes) {
        Ok(Some(capture_time)) => {
            probe.native_heif_capture_time = HeifNativeCaptureTime::Present {
                datetime_original: capture_time.datetime_original,
                offset_time_original: capture_time.offset_time_original,
            };
        }
        Ok(None) => probe.native_heif_capture_time = HeifNativeCaptureTime::Missing,
        Err(error) => {
            tracing::warn!(target: "kei::download::metadata", path = %path.display(), %error, "Failed to inspect native HEIF Exif capture time");
            mark_heif_exif_unreadable(&mut probe);
            return probe;
        }
    }
    match heif::extract_exif_tiff_bytes(&bytes) {
        Ok(Some(tiff)) => match Metadata::new_from_vec(&tiff, FileExtension::TIFF) {
            Ok(metadata) => {
                let exif_probe = probe_from_native_metadata(&metadata);
                merge_probe(&mut probe, exif_probe);
            }
            Err(error) => {
                tracing::warn!(target: "kei::download::metadata", path = %path.display(), %error, "Failed to parse HEIF Exif item");
                mark_heif_exif_unreadable(&mut probe);
            }
        },
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(target: "kei::download::metadata", path = %path.display(), %error, "Failed to resolve HEIF Exif item");
            mark_heif_exif_unreadable(&mut probe);
        }
    }
    probe
}

#[cfg(feature = "xmp")]
pub(super) fn probe_from_meta(meta: &XmpMeta) -> ExifProbe {
    let datetime_original = meta
        .property(xmp_ns::EXIF, "DateTimeOriginal")
        .map(|v| v.value);
    let offset_time_original = meta
        .property(EXIF_EX_XMP_NS, "OffsetTimeOriginal")
        .map(|v| v.value);
    let has_other_datetime_offset = ["OffsetTimeDigitized", "OffsetTime"]
        .iter()
        .any(|path| meta.contains_property(EXIF_EX_XMP_NS, path));
    let has_gps = meta.contains_property(xmp_ns::EXIF, "GPSLatitude")
        || meta.contains_property(xmp_ns::EXIF, "GPSLongitude");
    ExifProbe {
        datetime_original,
        offset_time_original,
        has_other_datetime_offset,
        has_gps,
        ..ExifProbe::default()
    }
}

#[cfg(feature = "xmp")]
fn merge_probe(target: &mut ExifProbe, source: ExifProbe) {
    if target.datetime_original.is_none() {
        target.datetime_original = source.datetime_original;
    }
    if target.offset_time_original.is_none() {
        target.offset_time_original = source.offset_time_original;
    }
    target.has_other_datetime_offset |= source.has_other_datetime_offset;
    target.has_gps |= source.has_gps;
}

#[cfg(feature = "xmp")]
fn mark_heif_exif_unreadable(probe: &mut ExifProbe) {
    probe.native_heif_capture_time = HeifNativeCaptureTime::Unreadable;
    probe
        .datetime_original
        .get_or_insert_with(|| "unreadable HEIF Exif item".to_string());
    probe.has_gps = true;
}

pub(super) fn probe_from_native_metadata(meta: &Metadata) -> ExifProbe {
    let datetime_original = meta
        .get_tag(&ExifTag::DateTimeOriginal(String::new()))
        .next()
        .and_then(|tag| match tag {
            ExifTag::DateTimeOriginal(value) => Some(value.clone()),
            _ => None,
        });
    let offset_time_original = meta
        .get_tag(&ExifTag::OffsetTimeOriginal(String::new()))
        .next()
        .and_then(|tag| match tag {
            ExifTag::OffsetTimeOriginal(value) => Some(value.clone()),
            _ => None,
        });
    let has_other_datetime_offset = meta
        .get_tag(&ExifTag::OffsetTimeDigitized(String::new()))
        .next()
        .is_some()
        || meta
            .get_tag(&ExifTag::OffsetTime(String::new()))
            .next()
            .is_some();
    let has_gps = meta
        .get_tag(&ExifTag::GPSLatitude(Vec::new()))
        .next()
        .is_some()
        || meta
            .get_tag(&ExifTag::GPSLongitude(Vec::new()))
            .next()
            .is_some();
    ExifProbe {
        datetime_original,
        offset_time_original,
        has_other_datetime_offset,
        has_gps,
        ..ExifProbe::default()
    }
}

#[cfg(not(feature = "xmp"))]
fn probe_exif_native(path: &Path) -> ExifProbe {
    let Ok(input) = std::fs::read(path) else {
        return ExifProbe::default();
    };
    let Some(file_type) = native_file_type(&input, path) else {
        return ExifProbe::default();
    };
    let Ok(meta) = Metadata::new_from_vec(&input, file_type) else {
        return ExifProbe::default();
    };
    probe_from_native_metadata(&meta)
}

#[cfg(all(test, feature = "xmp"))]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    #[cfg(feature = "xmp")]
    use super::super::test_support::{
        apply_metadata_with_default_suffix, fresh_heic, fresh_jpeg, test_tmp_dir, write_seeded_heic,
    };
    use super::super::values::{GpsCoords, MetadataWrite};
    use super::probe_exif;
    use std::fs;

    #[test]
    fn probe_exif_reports_empty_on_fresh_jpeg() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "probe_empty.jpg");
        let probe = probe_exif(&path).unwrap();
        assert!(probe.datetime_original.is_none());
        assert!(!probe.has_gps);
        fs::remove_file(&path).ok();
    }

    // HEIF probes merge the standalone Exif item with embedded XMP. Content
    // sniffing keeps the same behaviour on final paths and `.kei-tmp` files.
    #[test]
    fn probe_exif_heic_reports_seeded_datetime() {
        let dir = test_tmp_dir("probe_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "probe_dt.heic",
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
        );
        let probe = probe_exif(&path).expect("probe_exif must succeed on HEIC");
        assert!(
            probe.datetime_original.is_some(),
            "probe must read DateTimeOriginal back from HEIC XMP, got {:?}",
            probe.datetime_original,
        );
        assert!(
            !probe.has_gps,
            "probe must report no GPS when none was written"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_exif_heic_reports_seeded_gps() {
        let dir = test_tmp_dir("probe_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "probe_gps.heic",
            &MetadataWrite {
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: None,
                }),
                ..MetadataWrite::default()
            },
        );
        let probe = probe_exif(&path).expect("probe_exif must succeed on HEIC");
        assert!(
            probe.has_gps,
            "probe must report GPS present after writing GPS to HEIC"
        );
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2023:09:03 09:28:14"),
            "probe must retain the native HEIF Exif datetime when XMP does not replace it",
        );
        assert_eq!(probe.offset_time_original.as_deref(), Some("+03:00"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_exif_heic_reads_native_exif_without_xmp() {
        let dir = test_tmp_dir("probe_heic_tests");
        let path = fresh_heic(&dir, "probe_empty.heic");
        let probe = probe_exif(&path).expect("probe_exif must succeed on HEIC");
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2023:09:03 09:28:14"),
            "HEIF probe must retain DateTimeOriginal from the Exif item"
        );
        assert_eq!(probe.offset_time_original.as_deref(), Some("+03:00"));
        assert!(
            !probe.has_gps,
            "sample HEIC Exif carries no GPS coordinates"
        );
        fs::remove_file(&path).ok();
    }

    /// The metadata-rewrite pass calls `probe_exif` on the renamed final
    /// `.HEIC` file, but the in-pipeline embed step calls it on the
    /// `<base32>.kei-tmp` part file. Extension-based dispatch would route
    /// the part file to XMP Toolkit and silently return `default()` —
    /// recreating the bug for every first-pass HEIC. Content sniffing
    /// covers both call sites; pin it.
    #[test]
    fn probe_exif_dispatches_heif_on_extension_less_part_file() {
        let dir = test_tmp_dir("probe_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC.kei-tmp",
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                gps: Some(GpsCoords {
                    latitude: 1.0,
                    longitude: 2.0,
                    altitude: None,
                }),
                ..MetadataWrite::default()
            },
        );
        let probe = probe_exif(&path).expect("probe_exif on .kei-tmp HEIC");
        assert!(
            probe.datetime_original.is_some(),
            "probe must read DateTimeOriginal even when extension is `.kei-tmp`"
        );
        assert!(
            probe.has_gps,
            "probe must read GPS even when extension is `.kei-tmp`"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn probe_exif_jpeg_with_datetime_returns_value() {
        // Non-HEIF branch must still go through XMP Toolkit and see the
        // reconciled EXIF datetime — the refactor doesn't regress JPEG.
        let dir = test_tmp_dir("probe_heic_tests");
        let path = fresh_jpeg(&dir, "probe_jpeg_dt.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
        )
        .expect("JPEG datetime write");
        let probe = probe_exif(&path).expect("probe_exif must succeed on JPEG");
        assert!(
            probe.datetime_original.is_some(),
            "JPEG probe must keep returning DateTimeOriginal post-refactor",
        );
        fs::remove_file(&path).ok();
    }
}

#[cfg(all(test, not(feature = "xmp")))]
mod native_tests {
    #[cfg(test)]
    use super::super::embedded::apply_metadata;
    #[cfg(not(feature = "xmp"))]
    use super::super::test_support::fresh_jpeg;
    use super::super::values::MetadataWrite;
    use super::probe_exif;

    #[test]
    fn native_probe_reads_exif_from_temp_suffix_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "probe.jpg.kei-tmp");

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .unwrap();

        let probe = probe_exif(&path).unwrap();
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2024:06:15 10:00:00")
        );
    }
}
