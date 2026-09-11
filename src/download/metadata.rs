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

use std::path::{Path, PathBuf};
#[cfg(feature = "xmp")]
use std::sync::Arc;
#[cfg(feature = "xmp")]
use std::sync::Once;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, NaiveDateTime};
use little_exif::exif_tag::ExifTag;
use little_exif::filetype::FileExtension;
#[cfg(not(feature = "xmp"))]
use little_exif::ifd::ExifTagGroup;
use little_exif::metadata::Metadata;
use little_exif::rational::uR64;
#[cfg(feature = "xmp")]
use xmp_toolkit::{
    FromStrOptions, OpenFileOptions, XmpErrorType, XmpFile, XmpMeta, XmpValue, xmp_ns,
};

#[cfg(feature = "xmp")]
use super::heif;

/// Custom XMP namespace for kei-specific fields that don't fit standard
/// schemas (`hidden`, `archived`, `mediaSubtype`, `burstId`). Consumers that
/// care about these know to look for the `kei` prefix.
#[cfg(feature = "xmp")]
const KEI_XMP_NS: &str = "https://github.com/rhoopr/kei/ns/1.0/";
#[cfg(feature = "xmp")]
const KEI_XMP_PREFIX: &str = "kei";
#[cfg(feature = "xmp")]
const EXIF_EX_XMP_NS: &str = "http://cipa.jp/exif/1.0/";
#[cfg(feature = "xmp")]
const EXIF_EX_XMP_PREFIX: &str = "exifEX";

#[cfg(feature = "xmp")]
const KEI_MANAGED_FIELDS: &str = "managedFields";

static EMBED_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "xmp")]
static SIDECAR_TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "xmp")]
#[derive(Clone, Copy)]
enum ManagedXmpField {
    CreateDate,
    ModifyDate,
    DateTimeOriginal,
    DateCreated,
    OffsetTimeOriginal,
    GpsDateTime,
    GpsSpeed,
    GpsSpeedRef,
    GpsHPositioningError,
    Rating,
    GpsLatitude,
    GpsLongitude,
    GpsAltitude,
    GpsAltitudeRef,
    Title,
    Description,
    Keywords,
    People,
    Hidden,
    Archived,
    MediaSubtype,
    BurstId,
}

#[cfg(feature = "xmp")]
const MANAGED_XMP_FIELDS: [ManagedXmpField; 22] = [
    ManagedXmpField::CreateDate,
    ManagedXmpField::ModifyDate,
    ManagedXmpField::DateTimeOriginal,
    ManagedXmpField::DateCreated,
    ManagedXmpField::OffsetTimeOriginal,
    ManagedXmpField::GpsDateTime,
    ManagedXmpField::GpsSpeed,
    ManagedXmpField::GpsSpeedRef,
    ManagedXmpField::GpsHPositioningError,
    ManagedXmpField::Rating,
    ManagedXmpField::GpsLatitude,
    ManagedXmpField::GpsLongitude,
    ManagedXmpField::GpsAltitude,
    ManagedXmpField::GpsAltitudeRef,
    ManagedXmpField::Title,
    ManagedXmpField::Description,
    ManagedXmpField::Keywords,
    ManagedXmpField::People,
    ManagedXmpField::Hidden,
    ManagedXmpField::Archived,
    ManagedXmpField::MediaSubtype,
    ManagedXmpField::BurstId,
];

#[cfg(feature = "xmp")]
impl ManagedXmpField {
    const fn token(self) -> &'static str {
        match self {
            Self::CreateDate => "xmp:CreateDate",
            Self::ModifyDate => "xmp:ModifyDate",
            Self::DateTimeOriginal => "exif:DateTimeOriginal",
            Self::DateCreated => "photoshop:DateCreated",
            Self::OffsetTimeOriginal => "exifEX:OffsetTimeOriginal",
            Self::GpsDateTime => "exif:GPSTimeStamp",
            Self::GpsSpeed => "exif:GPSSpeed",
            Self::GpsSpeedRef => "exif:GPSSpeedRef",
            // CIPA Table 17 and Apple Photos retain GPS tag 31 in exif,
            // unlike the exifEX-only OffsetTime fields above.
            Self::GpsHPositioningError => "exif:GPSHPositioningError",
            Self::Rating => "xmp:Rating",
            Self::GpsLatitude => "exif:GPSLatitude",
            Self::GpsLongitude => "exif:GPSLongitude",
            Self::GpsAltitude => "exif:GPSAltitude",
            Self::GpsAltitudeRef => "exif:GPSAltitudeRef",
            Self::Title => "dc:title[x-default]",
            Self::Description => "dc:description[x-default]",
            Self::Keywords => "dc:subject",
            Self::People => "iptcExt:PersonInImage",
            Self::Hidden => "kei:hidden",
            Self::Archived => "kei:archived",
            Self::MediaSubtype => "kei:mediaSubtype",
            Self::BurstId => "kei:burstId",
        }
    }

    fn is_present(self, write: &MetadataWrite) -> bool {
        match self {
            Self::CreateDate | Self::ModifyDate | Self::DateTimeOriginal | Self::DateCreated => {
                write.datetime.is_some()
            }
            Self::OffsetTimeOriginal => write.offset_time_original.is_some(),
            Self::GpsDateTime => write.gps_datetime.is_some(),
            Self::GpsSpeed => write.gps_speed.is_some(),
            Self::GpsSpeedRef => write.gps_speed_ref.is_some(),
            Self::GpsHPositioningError => write.gps_h_positioning_error.is_some(),
            Self::Rating => write.rating.is_some(),
            Self::GpsLatitude | Self::GpsLongitude => write.gps.is_some(),
            Self::GpsAltitude | Self::GpsAltitudeRef => {
                write.gps.is_some_and(|gps| gps.altitude.is_some())
            }
            Self::Title => write.title.is_some(),
            Self::Description => write.description.is_some(),
            Self::Keywords => !write.keywords.is_empty(),
            Self::People => !write.people.is_empty(),
            Self::Hidden => write.is_hidden,
            Self::Archived => write.is_archived,
            Self::MediaSubtype => write.media_subtype.is_some(),
            Self::BurstId => write.burst_id.is_some(),
        }
    }

    const fn is_source_gps(self) -> bool {
        matches!(
            self,
            Self::GpsDateTime | Self::GpsSpeed | Self::GpsSpeedRef | Self::GpsHPositioningError
        )
    }

    fn delete(self, meta: &mut XmpMeta) -> xmp_toolkit::XmpResult<()> {
        let (namespace, path) = match self {
            Self::CreateDate => (xmp_ns::XMP, "CreateDate"),
            Self::ModifyDate => (xmp_ns::XMP, "ModifyDate"),
            Self::DateTimeOriginal => (xmp_ns::EXIF, "DateTimeOriginal"),
            Self::DateCreated => (xmp_ns::PHOTOSHOP, "DateCreated"),
            Self::OffsetTimeOriginal => (EXIF_EX_XMP_NS, "OffsetTimeOriginal"),
            Self::GpsDateTime => (xmp_ns::EXIF, "GPSTimeStamp"),
            Self::GpsSpeed => (xmp_ns::EXIF, "GPSSpeed"),
            Self::GpsSpeedRef => (xmp_ns::EXIF, "GPSSpeedRef"),
            Self::GpsHPositioningError => (xmp_ns::EXIF, "GPSHPositioningError"),
            Self::Rating => (xmp_ns::XMP, "Rating"),
            Self::GpsLatitude => (xmp_ns::EXIF, "GPSLatitude"),
            Self::GpsLongitude => (xmp_ns::EXIF, "GPSLongitude"),
            Self::GpsAltitude => (xmp_ns::EXIF, "GPSAltitude"),
            Self::GpsAltitudeRef => (xmp_ns::EXIF, "GPSAltitudeRef"),
            Self::Keywords => (xmp_ns::DC, "subject"),
            Self::People => (xmp_ns::IPTC_EXT, "PersonInImage"),
            Self::Hidden => (KEI_XMP_NS, "hidden"),
            Self::Archived => (KEI_XMP_NS, "archived"),
            Self::MediaSubtype => (KEI_XMP_NS, "mediaSubtype"),
            Self::BurstId => (KEI_XMP_NS, "burstId"),
            Self::Title => {
                let path = XmpMeta::compose_lang_selector(xmp_ns::DC, "title", "x-default")?;
                return meta.delete_property(xmp_ns::DC, &path);
            }
            Self::Description => {
                let path = XmpMeta::compose_lang_selector(xmp_ns::DC, "description", "x-default")?;
                return meta.delete_property(xmp_ns::DC, &path);
            }
        };
        meta.delete_property(namespace, path)
    }
}

#[cfg(feature = "xmp")]
static INIT: Once = Once::new();

#[cfg(feature = "xmp")]
fn ensure_initialized() {
    INIT.call_once(|| {
        // Registering the same namespace twice is fine; XMP Toolkit returns
        // the existing prefix. Ignore failures so standard XMP output still
        // works if a custom namespace cannot be registered.
        let _ = XmpMeta::register_namespace(KEI_XMP_NS, KEI_XMP_PREFIX);
        let _ = XmpMeta::register_namespace(EXIF_EX_XMP_NS, EXIF_EX_XMP_PREFIX);
    });
}

/// Snapshot of existing metadata fields that gate write decisions. HEIF
/// probes merge XMP with the standalone Exif item. Other formats use XMP
/// Toolkit in default builds or native EXIF without the `xmp` feature.
#[derive(Debug, Clone, Default)]
pub(super) enum HeifNativeCaptureTime {
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
    pub(super) native_heif_capture_time: HeifNativeCaptureTime,
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
        let Some((wall_clock, zone)) = parse_capture_timestamp(existing) else {
            return false;
        };
        wall_clock == created_local.naive_local()
            && zone.is_none_or(|zone| zone == *created_local.offset())
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
                let denotes_capture_time =
                    parse_capture_timestamp(datetime).is_some_and(|(wall_clock, zone)| {
                        wall_clock == created_local.naive_local()
                            && zone.is_none_or(|zone| zone == *created_local.offset())
                    });
                Ok(Some(!denotes_capture_time || offset != expected_offset))
            }
        }
    }
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
        "%Y:%m:%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
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
            tracing::warn!(path = %path.display(), error = %e, "Failed to read HEIF for probe");
            return ExifProbe::default();
        }
    };
    let mut probe = match heif::extract_xmp_bytes(&bytes) {
        Some(xmp_bytes) => match std::str::from_utf8(&xmp_bytes) {
            Ok(text) => match text.parse::<XmpMeta>() {
                Ok(meta) => probe_from_meta(&meta),
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "Failed to parse HEIF XMP packet");
                    ExifProbe::default()
                }
            },
            Err(_) => {
                tracing::warn!(path = %path.display(), "HEIF XMP packet is not UTF-8; treating it as absent");
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
            tracing::warn!(path = %path.display(), %error, "Failed to inspect native HEIF Exif capture time");
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
                tracing::warn!(path = %path.display(), %error, "Failed to parse HEIF Exif item");
                mark_heif_exif_unreadable(&mut probe);
            }
        },
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "Failed to resolve HEIF Exif item");
            mark_heif_exif_unreadable(&mut probe);
        }
    }
    probe
}

#[cfg(feature = "xmp")]
fn probe_from_meta(meta: &XmpMeta) -> ExifProbe {
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
pub(crate) fn read_source_gps(path: &Path) -> Result<SourceGpsMetadata> {
    let mut source = std::fs::File::open(path)
        .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?;
    read_source_gps_from_file(&mut source, path)
}

#[cfg(feature = "xmp")]
pub(super) fn read_source_gps_from_file(
    source: &mut std::fs::File,
    path: &Path,
) -> Result<SourceGpsMetadata> {
    use std::io::Seek;
    source.rewind()?;
    let mut head = [0_u8; 12];
    let bytes_read = std::io::Read::read(source, &mut head)
        .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?;
    let Some(file_type) = source_file_type(head.get(..bytes_read).unwrap_or_default()) else {
        return Ok(SourceGpsMetadata::default());
    };

    if file_type == FileExtension::JPEG {
        let source_len = source
            .metadata()
            .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?
            .len();
        return match read_jpeg_source_gps(source, source_len) {
            Ok(metadata) => Ok(metadata),
            Err(TiffGpsError::Malformed) => Ok(SourceGpsMetadata::default()),
            Err(TiffGpsError::Io(error)) => Err(error).with_context(|| {
                format!("Could not read {} for source GPS metadata", path.display())
            }),
        };
    }

    if file_type == FileExtension::TIFF {
        let source_len = source
            .metadata()
            .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?
            .len();
        return match read_tiff_source_gps(source, 0, source_len) {
            Ok(metadata) => Ok(metadata),
            Err(TiffGpsError::Malformed) => Ok(SourceGpsMetadata::default()),
            Err(TiffGpsError::Io(error)) => Err(error).with_context(|| {
                format!("Could not read {} for source GPS metadata", path.display())
            }),
        };
    }

    if matches!(file_type, FileExtension::PNG { .. }) {
        let source_len = source
            .metadata()
            .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?
            .len();
        return match read_png_source_gps(source, source_len) {
            Ok(metadata) => Ok(metadata),
            Err(TiffGpsError::Malformed) => Ok(SourceGpsMetadata::default()),
            Err(TiffGpsError::Io(error)) => Err(error).with_context(|| {
                format!("Could not read {} for source GPS metadata", path.display())
            }),
        };
    }

    if file_type == FileExtension::HEIF {
        let source_len = source
            .metadata()
            .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?
            .len();
        let extent = match heif::locate_exif_tiff(source, source_len) {
            Ok(extent) => extent,
            Err(heif::HeifExifError::Malformed) => {
                return Ok(SourceGpsMetadata::default());
            }
            Err(heif::HeifExifError::Io(error)) => {
                return Err(error).with_context(|| {
                    format!("Could not read {} for source GPS metadata", path.display())
                });
            }
        };
        let Some((tiff_start, tiff_len)) = extent else {
            return Ok(SourceGpsMetadata::default());
        };
        return match read_tiff_source_gps(source, tiff_start, tiff_len) {
            Ok(metadata) => Ok(metadata),
            Err(TiffGpsError::Malformed) => Ok(SourceGpsMetadata::default()),
            Err(TiffGpsError::Io(error)) => Err(error).with_context(|| {
                format!("Could not read {} for source GPS metadata", path.display())
            }),
        };
    }

    Ok(SourceGpsMetadata::default())
}

#[cfg(feature = "xmp")]
fn source_file_type(bytes: &[u8]) -> Option<FileExtension> {
    if heif::is_heif_content(bytes) {
        return Some(FileExtension::HEIF);
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(FileExtension::JPEG);
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return Some(FileExtension::TIFF);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(FileExtension::PNG {
            as_zTXt_chunk: true,
        });
    }
    None
}

#[cfg(feature = "xmp")]
fn read_jpeg_source_gps<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    source_len: u64,
) -> Result<SourceGpsMetadata, TiffGpsError> {
    let mut offset = 2_u64;
    while offset < source_len {
        let mut marker = [0_u8; 1];
        read_exact_file_range(source, source_len, offset, &mut marker)?;
        if marker[0] != 0xFF {
            return Err(TiffGpsError::Malformed);
        }
        while marker[0] == 0xFF {
            offset = offset.checked_add(1).ok_or(TiffGpsError::Malformed)?;
            read_exact_file_range(source, source_len, offset, &mut marker)?;
        }
        let marker_code = marker[0];
        offset = offset.checked_add(1).ok_or(TiffGpsError::Malformed)?;
        if marker_code == 0xD9 || marker_code == 0xDA {
            return Ok(SourceGpsMetadata::default());
        }
        if marker_code == 0x01 || (0xD0..=0xD7).contains(&marker_code) {
            continue;
        }

        let mut length = [0_u8; 2];
        read_exact_file_range(source, source_len, offset, &mut length)?;
        let segment_len = u64::from(u16::from_be_bytes(length));
        if segment_len < 2 {
            return Err(TiffGpsError::Malformed);
        }
        let data_start = offset.checked_add(2).ok_or(TiffGpsError::Malformed)?;
        let data_len = segment_len - 2;
        let next = data_start
            .checked_add(data_len)
            .filter(|end| *end <= source_len)
            .ok_or(TiffGpsError::Malformed)?;
        if marker_code == 0xE1 && data_len >= 6 {
            let mut signature = [0_u8; 6];
            read_exact_file_range(source, source_len, data_start, &mut signature)?;
            if signature == *b"Exif\0\0" {
                return read_tiff_source_gps(source, data_start + 6, data_len - 6);
            }
        }
        offset = next;
    }
    Ok(SourceGpsMetadata::default())
}

#[cfg(feature = "xmp")]
fn read_png_source_gps<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    source_len: u64,
) -> Result<SourceGpsMetadata, TiffGpsError> {
    let mut offset = 8_u64;
    while offset < source_len {
        let mut header = [0_u8; 8];
        read_exact_file_range(source, source_len, offset, &mut header)?;
        let data_len = u64::from(u32::from_be_bytes([
            header[0], header[1], header[2], header[3],
        ]));
        let data_start = offset.checked_add(8).ok_or(TiffGpsError::Malformed)?;
        let next = data_start
            .checked_add(data_len)
            .and_then(|end| end.checked_add(4))
            .filter(|end| *end <= source_len)
            .ok_or(TiffGpsError::Malformed)?;
        if &header[4..8] == b"eXIf" {
            return read_tiff_source_gps(source, data_start, data_len);
        }
        if &header[4..8] == b"IEND" {
            return Ok(SourceGpsMetadata::default());
        }
        offset = next;
    }
    Ok(SourceGpsMetadata::default())
}

#[cfg(feature = "xmp")]
fn read_exact_file_range<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    source_len: u64,
    offset: u64,
    output: &mut [u8],
) -> Result<(), TiffGpsError> {
    let output_len = u64::try_from(output.len()).map_err(|_error| TiffGpsError::Malformed)?;
    offset
        .checked_add(output_len)
        .filter(|end| *end <= source_len)
        .ok_or(TiffGpsError::Malformed)?;
    source.seek(std::io::SeekFrom::Start(offset))?;
    source.read_exact(output)?;
    Ok(())
}

#[cfg(feature = "xmp")]
#[derive(Clone, Copy)]
enum TiffEndian {
    Little,
    Big,
}

#[cfg(feature = "xmp")]
#[derive(Debug)]
enum TiffGpsError {
    Io(std::io::Error),
    Malformed,
}

#[cfg(feature = "xmp")]
impl From<std::io::Error> for TiffGpsError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(feature = "xmp")]
struct TiffGpsReader<'a, R> {
    source: &'a mut R,
    base: u64,
    len: u64,
    endian: TiffEndian,
}

#[cfg(feature = "xmp")]
impl<R: std::io::Read + std::io::Seek> TiffGpsReader<'_, R> {
    fn read_exact_at(&mut self, offset: u64, output: &mut [u8]) -> Result<(), TiffGpsError> {
        let length = u64::try_from(output.len()).map_err(|_error| TiffGpsError::Malformed)?;
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= self.len)
            .ok_or(TiffGpsError::Malformed)?;
        let absolute = self
            .base
            .checked_add(offset)
            .filter(|_| end <= self.len)
            .ok_or(TiffGpsError::Malformed)?;
        self.source.seek(std::io::SeekFrom::Start(absolute))?;
        self.source.read_exact(output)?;
        Ok(())
    }

    fn read_ifd_entry(&mut self, ifd_offset: u64, index: u16) -> Result<[u8; 12], TiffGpsError> {
        let entry_offset = u64::from(index)
            .checked_mul(12)
            .and_then(|offset| ifd_offset.checked_add(2)?.checked_add(offset))
            .ok_or(TiffGpsError::Malformed)?;
        let mut entry = [0_u8; 12];
        self.read_exact_at(entry_offset, &mut entry)?;
        Ok(entry)
    }

    fn entry_value<const N: usize>(
        &mut self,
        entry: &[u8; 12],
        tag_name: &'static str,
        expected_type: u16,
        expected_count: u32,
    ) -> Result<Option<[u8; N]>, TiffGpsError> {
        let actual_type = tiff_u16(self.endian, [entry[2], entry[3]]);
        let actual_count = tiff_u32(self.endian, [entry[4], entry[5], entry[6], entry[7]]);
        if actual_type != expected_type || actual_count != expected_count {
            tracing::warn!(
                tag = tag_name,
                expected_type,
                actual_type,
                expected_count,
                actual_count,
                "Source EXIF GPS tag has an unexpected type or count"
            );
            return Ok(None);
        }
        let mut value = [0_u8; N];
        if N <= 4 {
            let inline = entry
                .get(8..)
                .and_then(|bytes| bytes.get(..N))
                .ok_or(TiffGpsError::Malformed)?;
            value.copy_from_slice(inline);
        } else {
            let offset = u64::from(tiff_u32(
                self.endian,
                [entry[8], entry[9], entry[10], entry[11]],
            ));
            self.read_exact_at(offset, &mut value)?;
        }
        Ok(Some(value))
    }
}

#[cfg(feature = "xmp")]
fn read_tiff_source_gps<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    base: u64,
    len: u64,
) -> Result<SourceGpsMetadata, TiffGpsError> {
    let mut header = [0_u8; 8];
    let mut reader = TiffGpsReader {
        source,
        base,
        len,
        endian: TiffEndian::Little,
    };
    reader.read_exact_at(0, &mut header)?;
    reader.endian = match &header[..2] {
        b"II" => TiffEndian::Little,
        b"MM" => TiffEndian::Big,
        _ => return Err(TiffGpsError::Malformed),
    };
    if tiff_u16(reader.endian, [header[2], header[3]]) != 42 {
        return Err(TiffGpsError::Malformed);
    }
    let ifd0_offset = u64::from(tiff_u32(
        reader.endian,
        [header[4], header[5], header[6], header[7]],
    ));
    let ifd0_count = read_tiff_ifd_count(&mut reader, ifd0_offset)?;
    let mut gps_ifd_offset = None;
    for index in 0..ifd0_count {
        let entry = reader.read_ifd_entry(ifd0_offset, index)?;
        if tiff_u16(reader.endian, [entry[0], entry[1]]) == 0x8825
            && tiff_u16(reader.endian, [entry[2], entry[3]]) == 4
            && tiff_u32(reader.endian, [entry[4], entry[5], entry[6], entry[7]]) == 1
        {
            gps_ifd_offset = Some(u64::from(tiff_u32(
                reader.endian,
                [entry[8], entry[9], entry[10], entry[11]],
            )));
            break;
        }
    }
    let Some(gps_ifd_offset) = gps_ifd_offset else {
        return Ok(SourceGpsMetadata::default());
    };

    let gps_count = read_tiff_ifd_count(&mut reader, gps_ifd_offset)?;
    let mut date = None;
    let mut time = None;
    let mut speed = None;
    let mut speed_ref = None;
    let mut horizontal_positioning_error = None;
    let mut latitude = None;
    let mut latitude_ref = None;
    let mut longitude = None;
    let mut longitude_ref = None;
    for index in 0..gps_count {
        let entry = reader.read_ifd_entry(gps_ifd_offset, index)?;
        match tiff_u16(reader.endian, [entry[0], entry[1]]) {
            0x0001 => {
                latitude_ref = reader.entry_value::<2>(&entry, "GPSLatitudeRef", 2, 2)?;
            }
            0x0002 => {
                latitude = reader
                    .entry_value::<24>(&entry, "GPSLatitude", 5, 3)?
                    .and_then(|value| tiff_rational_triplet(reader.endian, &value));
            }
            0x0003 => {
                longitude_ref = reader.entry_value::<2>(&entry, "GPSLongitudeRef", 2, 2)?;
            }
            0x0004 => {
                longitude = reader
                    .entry_value::<24>(&entry, "GPSLongitude", 5, 3)?
                    .and_then(|value| tiff_rational_triplet(reader.endian, &value));
            }
            0x0007 if time.is_none() => {
                if let Some(value) = reader.entry_value::<24>(&entry, "GPSTimeStamp", 5, 3)? {
                    time = tiff_rational_triplet(reader.endian, &value);
                }
            }
            0x000C if speed_ref.is_none() => {
                if let Some(value) = reader.entry_value::<2>(&entry, "GPSSpeedRef", 2, 2)? {
                    speed_ref = std::str::from_utf8(&value)
                        .ok()
                        .map(|value| value.trim_matches('\0').trim().to_ascii_uppercase())
                        .filter(|value| matches!(value.as_str(), "K" | "M" | "N"));
                }
            }
            0x000D if speed.is_none() => {
                if let Some(value) = reader.entry_value::<8>(&entry, "GPSSpeed", 5, 1)? {
                    speed = tiff_rational(reader.endian, &value);
                }
            }
            0x001D if date.is_none() => {
                if let Some(value) = reader.entry_value::<11>(&entry, "GPSDateStamp", 2, 11)? {
                    date = std::str::from_utf8(&value)
                        .ok()
                        .map(|value| value.trim_matches('\0').trim().to_owned());
                }
            }
            0x001F if horizontal_positioning_error.is_none() => {
                if let Some(value) =
                    reader.entry_value::<8>(&entry, "GPSHPositioningError", 5, 1)?
                {
                    horizontal_positioning_error = tiff_rational(reader.endian, &value);
                }
            }
            _ => {}
        }
    }

    let datetime = date
        .as_deref()
        .zip(time.as_ref())
        .and_then(|(date, time)| source_gps_datetime_values(date, time));
    let speed = match (speed, speed_ref.as_ref()) {
        (Some(value), Some(_)) => Some(value),
        (Some(_), None) => {
            tracing::warn!("Source EXIF GPSSpeedRef is missing or unsupported");
            None
        }
        (None, _) => None,
    };
    let speed_ref = speed.as_ref().and(speed_ref);
    Ok(SourceGpsMetadata {
        latitude: latitude.zip(latitude_ref).and_then(|(value, reference)| {
            source_gps_degrees(&value, reference, GpsAxis::Latitude)
        }),
        longitude: longitude.zip(longitude_ref).and_then(|(value, reference)| {
            source_gps_degrees(&value, reference, GpsAxis::Longitude)
        }),
        datetime,
        speed,
        speed_ref,
        horizontal_positioning_error,
    })
}

#[cfg(feature = "xmp")]
enum GpsAxis {
    Latitude,
    Longitude,
}

#[cfg(feature = "xmp")]
fn source_gps_degrees(values: &[XmpRational; 3], reference: [u8; 2], axis: GpsAxis) -> Option<f64> {
    const MINUTES_PER_DEGREE: f64 = 60.0;
    const SECONDS_PER_DEGREE: f64 = 3600.0;
    let (positive, negative, maximum) = match axis {
        GpsAxis::Latitude => (b'N', b'S', 90.0),
        GpsAxis::Longitude => (b'E', b'W', 180.0),
    };
    if reference[1] != 0 || ![positive, negative].contains(&reference[0]) {
        return None;
    }
    let [degrees, minutes, seconds] = values.each_ref().map(XmpRational::as_f64);
    if minutes >= MINUTES_PER_DEGREE || seconds >= MINUTES_PER_DEGREE {
        return None;
    }
    let decimal = degrees + minutes / MINUTES_PER_DEGREE + seconds / SECONDS_PER_DEGREE;
    if decimal > maximum {
        return None;
    }
    Some(if reference[0] == negative {
        -decimal
    } else {
        decimal
    })
}

#[cfg(feature = "__fuzz_internals")]
pub(crate) fn fuzz_tiff_source_gps(bytes: &[u8]) {
    let mut source = std::io::Cursor::new(bytes);
    let _ = read_tiff_source_gps(&mut source, 0, bytes.len() as u64);
}

#[cfg(feature = "xmp")]
fn read_tiff_ifd_count<R: std::io::Read + std::io::Seek>(
    reader: &mut TiffGpsReader<'_, R>,
    offset: u64,
) -> Result<u16, TiffGpsError> {
    let mut count = [0_u8; 2];
    reader.read_exact_at(offset, &mut count)?;
    Ok(tiff_u16(reader.endian, count))
}

#[cfg(feature = "xmp")]
const fn tiff_u16(endian: TiffEndian, bytes: [u8; 2]) -> u16 {
    match endian {
        TiffEndian::Little => u16::from_le_bytes(bytes),
        TiffEndian::Big => u16::from_be_bytes(bytes),
    }
}

#[cfg(feature = "xmp")]
const fn tiff_u32(endian: TiffEndian, bytes: [u8; 4]) -> u32 {
    match endian {
        TiffEndian::Little => u32::from_le_bytes(bytes),
        TiffEndian::Big => u32::from_be_bytes(bytes),
    }
}

#[cfg(feature = "xmp")]
fn tiff_rational(endian: TiffEndian, bytes: &[u8; 8]) -> Option<XmpRational> {
    let numerator = tiff_u32(endian, [bytes[0], bytes[1], bytes[2], bytes[3]]);
    let denominator = tiff_u32(endian, [bytes[4], bytes[5], bytes[6], bytes[7]]);
    (denominator != 0).then_some(XmpRational {
        numerator,
        denominator,
    })
}

#[cfg(feature = "xmp")]
fn tiff_rational_triplet(endian: TiffEndian, bytes: &[u8; 24]) -> Option<[XmpRational; 3]> {
    Some([
        tiff_rational(endian, bytes[0..8].try_into().ok()?)?,
        tiff_rational(endian, bytes[8..16].try_into().ok()?)?,
        tiff_rational(endian, bytes[16..24].try_into().ok()?)?,
    ])
}

#[cfg(feature = "xmp")]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "validated GPS hour, minute, and second ranges make these conversions bounded and non-negative"
)]
fn source_gps_datetime_values(date: &str, time: &[XmpRational; 3]) -> Option<String> {
    let date = chrono::NaiveDate::parse_from_str(date, "%Y:%m:%d")
        .ok()
        .or_else(|| {
            tracing::warn!("Source EXIF GPSDateStamp is malformed");
            None
        })?;
    let [hour_value, minute_value, seconds_value] = time;
    let hour = hour_value.as_f64();
    let minute = minute_value.as_f64();
    let seconds = seconds_value.as_f64();
    if !hour.is_finite() || !minute.is_finite() || !seconds.is_finite() {
        tracing::warn!("Source EXIF GPSTimeStamp is malformed");
        return None;
    }
    if hour.fract() != 0.0
        || minute.fract() != 0.0
        || !(0.0..=23.0).contains(&hour)
        || !(0.0..=59.0).contains(&minute)
        || !(0.0..60.0).contains(&seconds)
    {
        tracing::warn!("Source EXIF GPSTimeStamp is malformed");
        return None;
    }
    let whole_seconds = seconds.trunc() as u32;
    let nanos = ((seconds.fract() * 1_000_000_000.0).round() as u32).min(999_999_999);
    let fraction = if nanos == 0 {
        String::new()
    } else {
        format!(".{nanos:09}").trim_end_matches('0').to_string()
    };
    Some(format!(
        "{}T{:02}:{:02}:{:02}{}Z",
        date.format("%Y-%m-%d"),
        hour as u32,
        minute as u32,
        whole_seconds,
        fraction
    ))
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

fn probe_from_native_metadata(meta: &Metadata) -> ExifProbe {
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

/// GPS triple passed to [`apply_metadata`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct GpsCoords {
    pub(crate) latitude: f64,
    pub(crate) longitude: f64,
    pub(crate) altitude: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmpRational {
    numerator: u32,
    denominator: u32,
}

#[cfg(feature = "xmp")]
impl XmpRational {
    fn as_f64(&self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }

    fn encode(&self) -> String {
        format!("{}/{}", self.numerator, self.denominator)
    }
}

#[cfg(feature = "xmp")]
#[derive(Debug, Default, PartialEq)]
pub(crate) struct SourceGpsMetadata {
    pub(crate) latitude: Option<f64>,
    pub(crate) longitude: Option<f64>,
    pub(crate) datetime: Option<String>,
    pub(crate) speed: Option<XmpRational>,
    pub(crate) speed_ref: Option<String>,
    pub(crate) horizontal_positioning_error: Option<XmpRational>,
}

/// Bundle of every field the writer knows how to embed. Empty / default
/// fields are skipped.
#[derive(Debug, Default, Clone)]
pub(crate) struct MetadataWrite {
    /// `"YYYY:MM:DD HH:MM:SS"` EXIF-style datetime string.
    pub(crate) datetime: Option<String>,
    /// EXIF `OffsetTimeOriginal`, formatted as `+HH:MM` or `-HH:MM`.
    pub(crate) offset_time_original: Option<String>,
    /// Remove offset tags before writing a replacement timestamp.
    pub(crate) clear_datetime_offsets: bool,
    /// Require a native HEIF Exif timestamp pair to be updated or verified.
    pub(crate) require_native_heif_capture_time: bool,
    /// GPS fix timestamp in ISO 8601 UTC form.
    pub(crate) gps_datetime: Option<String>,
    /// GPS receiver speed as an XMP rational in the units named by `gps_speed_ref`.
    pub(crate) gps_speed: Option<XmpRational>,
    /// GPS receiver speed units: K, M, or N.
    pub(crate) gps_speed_ref: Option<String>,
    /// Horizontal positioning error in metres as an XMP rational.
    pub(crate) gps_h_positioning_error: Option<XmpRational>,
    /// Preserve prior kei-owned source GPS fields because the source could not
    /// be read and their current absence is unknown.
    pub(crate) preserve_source_gps: bool,
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
            && self.offset_time_original.is_none()
            && !self.clear_datetime_offsets
            && self.gps_datetime.is_none()
            && self.gps_speed.is_none()
            && self.gps_speed_ref.is_none()
            && self.gps_h_positioning_error.is_none()
            && !self.preserve_source_gps
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
pub(super) fn apply_metadata_with_expected_fingerprint(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<super::file::ExistingFileFingerprint>,
) -> Result<Option<super::file::ExistingFileFingerprint>> {
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

pub(super) fn prepare_metadata_with_expected_fingerprint(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<super::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    #[cfg(not(feature = "xmp"))]
    {
        prepare_metadata_native(path, write, temp_suffix, expected_fingerprint)
    }
    #[cfg(feature = "xmp")]
    let is_heif = if let Some(expected) = expected_fingerprint {
        let snapshot = match super::file::fingerprint_regular_file_snapshot_blocking(path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Err(error).context(
                    super::file::ConditionalPublishTargetChanged::Unverifiable {
                        path: path.to_path_buf(),
                    },
                );
            }
        };
        if snapshot.fingerprint != expected {
            return Err(
                super::file::ConditionalPublishTargetChanged::AfterPlanning {
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

fn publish_prepared_embed(
    temp_path: &Path,
    final_path: &Path,
    expected: super::file::ExistingFileFingerprint,
    expected_replacement: super::file::ExistingFileFingerprint,
) -> anyhow::Result<()> {
    // CONTRACT: METADATA_EMBED_REWRITE_REQUIRES_STABLE_INPUT
    super::file::publish_file_if_unchanged_blocking(
        temp_path,
        final_path,
        expected,
        expected_replacement,
    )
}

#[cfg(feature = "xmp")]
fn prepare_metadata_heif(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<super::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    use std::io::Read;

    // CONTRACT: HEIF_EMBED_REWRITE_REQUIRES_STABLE_INPUT
    ensure_initialized();
    let mut source = std::fs::File::open(path)
        .with_context(|| format!("Could not open {} for HEIC XMP update", path.display()))?;
    let source_permissions = source
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", path.display()))?
        .permissions();
    let mut input = Vec::new();
    source
        .read_to_end(&mut input)
        .with_context(|| format!("Could not read {} for HEIC XMP update", path.display()))?;
    let expected = fingerprint_bytes(&input)?;
    if expected_fingerprint.is_some_and(|approved| approved != expected) {
        return Err(
            super::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }
    let existing = heif::extract_xmp_strict(&input)
        .with_context(|| format!("Could not inspect existing XMP in {}", path.display()))?;
    let mut meta = match existing.as_deref() {
        Some(bytes) => {
            let text = std::str::from_utf8(bytes)
                .with_context(|| format!("Existing HEIC XMP in {} is not UTF-8", path.display()))?;
            text.parse::<XmpMeta>().with_context(|| {
                format!("Existing HEIC XMP in {} is not valid XMP", path.display())
            })?
        }
        None => XmpMeta::new().context("Could not create XMP metadata")?,
    };
    apply_to_xmp(&mut meta, write)?;
    let xmp = meta.to_string().into_bytes();
    let native_input = if write.require_native_heif_capture_time {
        heif::validate_capture_repair_item_ownership(&input).with_context(|| {
            format!(
                "Could not prove HEIC capture metadata belongs only to the primary image in {}",
                path.display()
            )
        })?;
        let (Some(datetime), Some(offset)) = (
            write.datetime.as_deref(),
            write.offset_time_original.as_deref(),
        ) else {
            anyhow::bail!(
                "HEIC capture timestamp repair has no complete timestamp pair for {}",
                path.display()
            );
        };
        heif::rewrite_exif_capture_time(&input, datetime, offset).with_context(|| {
            format!(
                "Could not update native HEIC Exif capture time in {}",
                path.display()
            )
        })?
    } else {
        None
    };
    if write.require_native_heif_capture_time && native_input.is_none() {
        anyhow::bail!(
            "HEIC capture timestamp repair could not update a native Exif timestamp pair in {}",
            path.display()
        );
    }
    let rewrite_input = native_input.as_deref().unwrap_or(&input);

    let (file, tmp_path) = create_unique_embed_temp(path, temp_suffix)?;
    let cleanup_permissions = file
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", tmp_path.display()))?
        .permissions();
    let guard = TmpGuard::with_cleanup_permissions(&tmp_path, cleanup_permissions);
    let mut writer = std::io::BufWriter::new(file);
    heif::rewrite_xmp(rewrite_input, &xmp, &mut writer)
        .with_context(|| format!("Could not update XMP in {}", path.display()))?;
    let file = writer
        .into_inner()
        .with_context(|| format!("Could not flush {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Could not fsync {}", tmp_path.display()))?;
    let mut file = file;
    validate_heif_post_rewrite(&mut file, &tmp_path)?;
    let rewritten = std::fs::read(&tmp_path)
        .with_context(|| format!("Could not validate {}", tmp_path.display()))?;
    let rewritten_fingerprint = fingerprint_bytes(&rewritten)?;
    heif::validate_rewrite_preserves_non_xmp_items(rewrite_input, &rewritten).with_context(
        || {
            format!(
                "HEIC metadata rewrite changed non-XMP media data in {}",
                tmp_path.display()
            )
        },
    )?;
    if native_input.is_some() {
        let native_tiff = heif::extract_exif_tiff_bytes(&rewritten)
            .with_context(|| {
                format!(
                    "Could not inspect rewritten native HEIC Exif in {}",
                    tmp_path.display()
                )
            })?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Rewritten HEIC lost its native Exif item: {}",
                    tmp_path.display()
                )
            })?;
        let native_metadata = Metadata::new_from_vec(&native_tiff, FileExtension::TIFF)
            .with_context(|| {
                format!(
                    "Could not parse rewritten native HEIC Exif in {}",
                    tmp_path.display()
                )
            })?;
        let native_probe = probe_from_native_metadata(&native_metadata);
        if native_probe.datetime_original.as_deref() != write.datetime.as_deref()
            || native_probe.offset_time_original.as_deref() != write.offset_time_original.as_deref()
        {
            anyhow::bail!(
                "Rewritten HEIC native Exif does not contain the requested capture timestamp and offset: {}",
                tmp_path.display()
            );
        }
    }
    let Some(rewritten_xmp) = heif::extract_xmp_strict(&rewritten)
        .with_context(|| format!("Could not inspect rewritten XMP in {}", tmp_path.display()))?
    else {
        anyhow::bail!(
            "HEIC XMP rewrite produced no XMP item: {}",
            tmp_path.display()
        );
    };
    std::str::from_utf8(&rewritten_xmp)
        .with_context(|| format!("Rewritten HEIC XMP is not UTF-8: {}", tmp_path.display()))?
        .parse::<XmpMeta>()
        .with_context(|| format!("Rewritten HEIC XMP is invalid: {}", tmp_path.display()))?;
    // A multi-image HEIC holds one XMP packet per image, so reader and writer
    // must resolve the same item or a merge moves an auxiliary image's metadata
    // onto the photograph. Replacing in place leaves the extent length alone and
    // pads the tail with spaces, so the written packet is a prefix rather than
    // the whole extent.
    let resolves_to_written_packet = rewritten_xmp
        .split_at_checked(xmp.len())
        .is_some_and(|(written, padding)| written == xmp && padding.iter().all(|b| *b == b' '));
    if !resolves_to_written_packet {
        anyhow::bail!(
            "HEIC XMP rewrite resolved a different item than it wrote in {}: wrote {} bytes, read back {} bytes. Refusing to replace the user's file.",
            tmp_path.display(),
            xmp.len(),
            rewritten_xmp.len()
        );
    }
    file.set_permissions(source_permissions)
        .with_context(|| format!("Could not preserve permissions on {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Could not fsync permissions on {}", tmp_path.display()))?;
    drop(file);
    Ok(PreparedMetadataFile {
        guard,
        expected_input: expected,
        expected_output: rewritten_fingerprint,
    })
}

/// Read the first 12 bytes of `path` and dispatch to [`heif::is_heif_content`].
/// On read error, fall back to extension-based detection — preserves the
/// pre-content-sniff behavior for any caller that hands us an unreadable
/// path, rather than misclassifying every such call as non-HEIF.
#[cfg(feature = "xmp")]
fn is_heif_file(path: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 12];
    match std::fs::File::open(path).and_then(|mut f| f.read(&mut head)) {
        Ok(n) => heif::is_heif_content(head.get(..n).unwrap_or(&[])),
        Err(_) => heif::is_heif_path(path),
    }
}

/// Whether the in-place embedded-metadata writer can patch this file.
///
/// The first 12 bytes take precedence over the extension so downloaded part
/// files and misleading extensions route safely. HEIC / HEIF / AVIF use the
/// byte-preserving item-map writer. JPEG / PNG / TIFF / MP4 / MOV continue
/// through XMP Toolkit.
#[must_use]
pub(crate) fn is_embed_writable_path(path: &Path) -> bool {
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

#[cfg(all(test, feature = "xmp"))]
fn temp_path_for(path: &Path, temp_suffix: &str) -> PathBuf {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(temp_suffix);
    path.with_file_name(tmp_name)
}

fn fingerprint_bytes(bytes: &[u8]) -> Result<super::file::ExistingFileFingerprint> {
    use sha2::{Digest, Sha256};

    Ok(super::file::ExistingFileFingerprint {
        size: u64::try_from(bytes.len()).context("Metadata source is too large to fingerprint")?,
        sha256: Sha256::digest(bytes).into(),
    })
}

fn embed_temp_path(path: &Path, temp_suffix: &str, sequence: u64) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".kei-metadata-{}-{sequence}{temp_suffix}",
        std::process::id()
    ))
}

fn create_unique_embed_temp(path: &Path, temp_suffix: &str) -> Result<(std::fs::File, PathBuf)> {
    create_unique_embed_temp_with_sequence(path, temp_suffix, || {
        EMBED_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    })
}

fn create_unique_embed_temp_with_sequence(
    path: &Path,
    temp_suffix: &str,
    mut next_sequence: impl FnMut() -> u64,
) -> Result<(std::fs::File, PathBuf)> {
    loop {
        let candidate = embed_temp_path(path, temp_suffix, next_sequence());
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not create temporary embedded metadata file {}",
                        candidate.display()
                    )
                });
            }
        }
    }
}

#[cfg(feature = "xmp")]
struct ReconciledSidecarSnapshot {
    path: crate::fs_util::ConfinedPath,
    file: std::fs::File,
    bytes: Vec<u8>,
}

#[cfg(feature = "xmp")]
impl ReconciledSidecarSnapshot {
    fn read(path: crate::fs_util::ConfinedPath) -> Result<Option<Self>> {
        let Some(mut file) = path.open_optional_regular()? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut bytes)?;
        path.validate_identity(crate::fs_util::file_identity(&file)?)?;
        Ok(Some(Self { path, file, bytes }))
    }

    fn validate(&self) -> Result<()> {
        let identity = crate::fs_util::file_identity(&self.file)?;
        let mut current = self.path.validate_identity(identity)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut current, &mut bytes)?;
        anyhow::ensure!(
            bytes == self.bytes,
            "Reconciled sidecar changed before state finalization"
        );
        self.path.validate_identity(identity)?;
        Ok(())
    }
}

/// Keep source and destination sidecar evidence live through catalogue finalization.
#[cfg(feature = "xmp")]
pub(super) struct ReconciledSidecar {
    source_path: crate::fs_util::ConfinedPath,
    source: Option<ReconciledSidecarSnapshot>,
    destination: ReconciledSidecarSnapshot,
}

#[cfg(feature = "xmp")]
impl ReconciledSidecar {
    pub(super) async fn validate(self: &Arc<Self>) -> Result<()> {
        let sidecar = Arc::clone(self);
        tokio::task::spawn_blocking(move || sidecar.validate_blocking()).await?
    }

    fn validate_blocking(&self) -> Result<()> {
        if let Some(source) = &self.source {
            source.validate()?;
        } else {
            anyhow::ensure!(
                self.source_path.open_optional_regular()?.is_none(),
                "Source sidecar appeared during reconciliation"
            );
        }
        self.destination.validate()
    }
}

/// Preserve an existing packet byte-for-byte. Generate a configured packet only
/// when no source sidecar exists. Never overwrite a conflicting destination.
#[cfg(feature = "xmp")]
pub(super) fn write_reconciled_sidecar(
    copy: &super::file::ReconciledFile,
    write_missing: impl FnOnce() -> Result<MetadataWrite>,
    temp_suffix: &str,
) -> Result<Arc<ReconciledSidecar>> {
    fn sidecar_name(path: &Path) -> Result<PathBuf> {
        let mut name = path
            .file_name()
            .context("Reconciled media has no filename")?
            .to_os_string();
        name.push(".xmp");
        Ok(path.with_file_name(name))
    }
    let source_path = copy.source.sibling(&sidecar_name(copy.source.path())?)?;
    // Sibling creates a second retained capability without resolving ancestors.
    let source = ReconciledSidecarSnapshot::read(copy.source.sibling(source_path.path())?)?;
    let bytes = if let Some(source) = &source {
        ensure_initialized();
        parse_existing_sidecar(
            std::str::from_utf8(&source.bytes).context("Source sidecar is not UTF-8")?,
        )?;
        source.bytes.clone()
    } else {
        let write = write_missing()?;
        ensure_initialized();
        let mut meta = XmpMeta::new().context("creating reconciled XMP packet")?;
        apply_to_owned_sidecar(&mut meta, &write)?;
        meta.to_string().into_bytes()
    };
    if let Some(source) = &source {
        source.validate()?;
    }
    copy.validate_blocking()?;
    let destination = copy
        .destination
        .sibling(&sidecar_name(copy.destination.path())?)?;
    // CONTRACT: FILE_PUBLISH_NO_OVERWRITE
    let existing = ReconciledSidecarSnapshot::read(copy.destination.sibling(destination.path())?)?;
    let destination = if let Some(existing) = existing {
        anyhow::ensure!(
            existing.bytes == bytes,
            "Reconciled sidecar destination conflicts with source metadata"
        );
        existing
    } else {
        let temp_path = destination.path().with_file_name(format!(
            ".kei-xmp-reconcile-{}{temp_suffix}",
            uuid::Uuid::new_v4()
        ));
        let temp = destination.sibling(&temp_path)?;
        let mut file = temp.create_new_regular()?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        temp.validate_identity(crate::fs_util::file_identity(&file)?)?;
        super::file::publish_reconciliation_part_blocking(&temp, &destination)?;
        let installed = ReconciledSidecarSnapshot::read(destination)?
            .context("Reconciled sidecar disappeared")?;
        anyhow::ensure!(
            installed.bytes == bytes,
            "Reconciled sidecar changed during publication"
        );
        installed
    };
    destination.path.sync_parent()?;
    let receipt = Arc::new(ReconciledSidecar {
        source_path,
        source,
        destination,
    });
    receipt.validate_blocking()?;
    Ok(receipt)
}

/// Write `write` as a `.xmp` sidecar next to the media file, atomically.
///
/// If a sidecar already exists (e.g., from Darktable / Lightroom / digiKam),
/// its existing XMP properties are read and kei's fields are layered on top
/// rather than overwriting the whole packet. Existing bytes must remain
/// readable, parseable, and unchanged through publication. Otherwise the
/// write fails without replacing them.
#[cfg(feature = "xmp")]
pub(crate) async fn write_sidecar(
    media_path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
) -> Result<()> {
    let media_path = media_path.to_path_buf();
    let write = write.clone();
    let temp_suffix = temp_suffix.to_owned();
    let prepared = tokio::task::spawn_blocking(move || {
        prepare_sidecar_write(&media_path, &write, &temp_suffix)
    })
    .await
    .context("XMP sidecar preparation task panicked")??;
    let Some(prepared) = prepared else {
        return Ok(());
    };

    // CONTRACT: XMP_SIDECAR_REWRITE_REQUIRES_STABLE_INPUT
    super::file::publish_file_if_unchanged(
        &prepared.tmp_path,
        &prepared.sidecar_path,
        prepared.expected,
    )
    .await
    .with_context(|| {
        format!(
            "Could not install XMP sidecar {} -> {}",
            prepared.tmp_path.display(),
            prepared.sidecar_path.display()
        )
    })?;
    tracing::debug!(path = %prepared.sidecar_path.display(), "Wrote XMP sidecar");
    Ok(())
}

#[cfg(feature = "xmp")]
struct PreparedSidecar {
    tmp_path: PathBuf,
    sidecar_path: PathBuf,
    expected: Option<super::file::ExistingFileFingerprint>,
}

#[cfg(feature = "xmp")]
fn prepare_sidecar_write(
    media_path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
) -> Result<Option<PreparedSidecar>> {
    ensure_initialized();

    let Some(name) = media_path.file_name() else {
        if write.is_empty() {
            return Ok(None);
        }
        anyhow::bail!(
            "Cannot write an XMP sidecar because the media path has no filename: {}",
            media_path.display()
        );
    };
    let mut sidecar_name = name.to_os_string();
    sidecar_name.push(".xmp");
    let sidecar_path = media_path.with_file_name(&sidecar_name);
    // Seed the packet with any existing sidecar content so user-authored
    // ratings / keywords / develop settings from another tool survive.
    let (mut meta, expected) = match std::fs::read(&sidecar_path) {
        Ok(existing_bytes) => {
            let existing = std::str::from_utf8(&existing_bytes).with_context(|| {
                format!(
                    "Existing XMP sidecar is not valid UTF-8: {}",
                    sidecar_path.display()
                )
            })?;
            let parsed = parse_existing_sidecar(existing).with_context(|| {
                format!(
                    "Could not parse existing XMP sidecar {}",
                    sidecar_path.display()
                )
            })?;
            let fingerprint = fingerprint_bytes(&existing_bytes)?;
            (parsed, Some(fingerprint))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (XmpMeta::new().context("creating XmpMeta")?, None)
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "Could not read existing XMP sidecar {}",
                    sidecar_path.display()
                )
            });
        }
    };
    if write.is_empty() && !meta.contains_property(KEI_XMP_NS, KEI_MANAGED_FIELDS) {
        return Ok(None);
    }
    apply_to_owned_sidecar(&mut meta, write)?;
    let bytes = meta.to_string().into_bytes();

    let (mut temp, tmp_path) = create_unique_sidecar_temp(&sidecar_path, temp_suffix)?;
    let guard = TmpGuard::new(&tmp_path);
    std::io::Write::write_all(&mut temp, &bytes).with_context(|| {
        format!(
            "Could not write temporary XMP sidecar {}",
            tmp_path.display()
        )
    })?;
    temp.sync_all().with_context(|| {
        format!(
            "Could not sync temporary XMP sidecar {}",
            tmp_path.display()
        )
    })?;
    guard.disarm();
    Ok(Some(PreparedSidecar {
        tmp_path,
        sidecar_path,
        expected,
    }))
}

#[cfg(feature = "xmp")]
fn sidecar_temp_path(sidecar_path: &Path, temp_suffix: &str, sequence: u64) -> PathBuf {
    let parent = sidecar_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".kei-xmp-{}-{sequence}{temp_suffix}",
        std::process::id()
    ))
}

#[cfg(feature = "xmp")]
fn create_unique_sidecar_temp(
    sidecar_path: &Path,
    temp_suffix: &str,
) -> Result<(std::fs::File, PathBuf)> {
    loop {
        let candidate = sidecar_temp_path(
            sidecar_path,
            temp_suffix,
            SIDECAR_TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        );
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((file, candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "Could not create temporary XMP sidecar {}",
                        candidate.display()
                    )
                });
            }
        }
    }
}

#[cfg(feature = "xmp")]
fn parse_existing_sidecar(existing: &str) -> Result<XmpMeta> {
    match XmpMeta::from_str_with_options(existing, FromStrOptions::default().require_xmp_meta()) {
        Ok(parsed) => Ok(parsed),
        Err(error) if error.error_type == XmpErrorType::XmpMetaElementMissing => {
            let parsed = existing.parse::<XmpMeta>()?;
            anyhow::ensure!(
                parsed
                    .iter(xmp_toolkit::IterOptions::default())
                    .next()
                    .is_some(),
                "sidecar has no recognizable XMP properties"
            );
            Ok(parsed)
        }
        Err(error) => Err(error.into()),
    }
}

/// Apply a complete sidecar snapshot without deleting metadata that kei cannot
/// prove it wrote. The ownership list records exact properties, so a later
/// clear can remove only the values established by an earlier kei write.
#[cfg(feature = "xmp")]
fn apply_to_owned_sidecar(meta: &mut XmpMeta, write: &MetadataWrite) -> xmp_toolkit::XmpResult<()> {
    let previous = meta
        .property(KEI_XMP_NS, KEI_MANAGED_FIELDS)
        .map(|value| value.value)
        .unwrap_or_default();
    let previous_tokens: Vec<&str> = previous
        .split(',')
        .filter(|token| !token.is_empty())
        .collect();

    for field in MANAGED_XMP_FIELDS {
        if previous_tokens.contains(&field.token())
            && !field.is_present(write)
            && !(write.preserve_source_gps && field.is_source_gps())
        {
            field.delete(meta)?;
        }
    }

    apply_to_xmp(meta, write)?;

    let mut current_tokens: Vec<String> = previous_tokens
        .into_iter()
        .filter(|token| {
            !MANAGED_XMP_FIELDS.iter().any(|field| {
                field.token() == *token && !(write.preserve_source_gps && field.is_source_gps())
            })
        })
        .map(str::to_owned)
        .collect();
    current_tokens.extend(
        MANAGED_XMP_FIELDS
            .iter()
            .copied()
            .filter(|field| field.is_present(write))
            .map(|field| field.token().to_owned()),
    );

    if current_tokens.is_empty() {
        meta.delete_property(KEI_XMP_NS, KEI_MANAGED_FIELDS)?;
    } else {
        meta.set_property(
            KEI_XMP_NS,
            KEI_MANAGED_FIELDS,
            &XmpValue::new(current_tokens.join(",")),
        )?;
    }
    Ok(())
}

/// Remove the tmp file on drop unless disarmed. Protects metadata temp files
/// against panics or writer failures; no orphan sweep matches this suffix.
#[derive(Debug)]
struct TmpGuard {
    path: PathBuf,
    armed: bool,
    cleanup_permissions: Option<std::fs::Permissions>,
}

impl TmpGuard {
    #[cfg(any(test, feature = "xmp"))]
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: true,
            cleanup_permissions: None,
        }
    }

    fn with_cleanup_permissions(path: &Path, permissions: std::fs::Permissions) -> Self {
        Self {
            path: path.to_path_buf(),
            armed: true,
            cleanup_permissions: Some(permissions),
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TmpGuard {
    fn drop(&mut self) {
        if self.armed {
            if self.path.exists()
                && let Some(permissions) = self.cleanup_permissions.take()
                && let Err(error) = std::fs::set_permissions(&self.path, permissions)
            {
                tracing::warn!(
                    path = %self.path.display(),
                    %error,
                    "Could not restore temporary metadata permissions for cleanup; retaining file"
                );
                return;
            }
            crate::fs_util::log_remove(&self.path);
        }
    }
}

pub(super) struct PreparedMetadataFile {
    guard: TmpGuard,
    expected_input: super::file::ExistingFileFingerprint,
    expected_output: super::file::ExistingFileFingerprint,
}

impl PreparedMetadataFile {
    pub(super) fn output_fingerprint(&self) -> super::file::ExistingFileFingerprint {
        self.expected_output
    }

    pub(super) fn publish(self, final_path: &Path) -> Result<super::file::ExistingFileFingerprint> {
        self.publish_with(final_path, publish_prepared_embed)
    }

    fn publish_with(
        self,
        final_path: &Path,
        install: impl FnOnce(
            &Path,
            &Path,
            super::file::ExistingFileFingerprint,
            super::file::ExistingFileFingerprint,
        ) -> anyhow::Result<()>,
    ) -> Result<super::file::ExistingFileFingerprint> {
        let Self {
            guard,
            expected_input,
            expected_output,
        } = self;
        let temp_path = guard.path.clone();
        if let Err(error) = install(&temp_path, final_path, expected_input, expected_output) {
            let disposition = super::file::classify_conditional_publish_error(&error);
            if disposition
                .retained_paths
                .iter()
                .any(|retained| retained == &temp_path)
            {
                guard.disarm();
            }
            return Err(error).with_context(|| {
                format!(
                    "Could not install metadata update {} -> {}",
                    temp_path.display(),
                    final_path.display()
                )
            });
        }
        guard.disarm();
        Ok(expected_output)
    }
}

#[cfg(feature = "xmp")]
fn prepare_metadata_xmp_toolkit(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<super::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    ensure_initialized();

    let expected = super::file::fingerprint_regular_file_snapshot_blocking(path)
        .with_context(|| {
            format!(
                "Could not fingerprint {} for metadata update",
                path.display()
            )
        })?
        .fingerprint;
    if expected_fingerprint.is_some_and(|approved| approved != expected) {
        return Err(
            super::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }
    let (file, tmp_path) = create_unique_embed_temp(path, temp_suffix)?;
    let cleanup_permissions = file
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", tmp_path.display()))?
        .permissions();
    let guard = TmpGuard::with_cleanup_permissions(&tmp_path, cleanup_permissions);
    drop(file);
    std::fs::copy(path, &tmp_path).with_context(|| {
        format!(
            "Could not copy {} to {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    let copied = super::file::fingerprint_regular_file_snapshot_blocking(&tmp_path)
        .with_context(|| format!("Could not fingerprint {}", tmp_path.display()))?
        .fingerprint;
    if copied != expected {
        return Err(
            super::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }

    let result: Result<()> = (|| {
        let mut file = XmpFile::new().context("Could not create XMP handle")?;
        file.open_file(
            &tmp_path,
            OpenFileOptions::default().for_update().use_smart_handler(),
        )
        .with_context(|| format!("Could not open {} for XMP update", tmp_path.display()))?;

        let mut meta = file
            .xmp()
            .unwrap_or_else(|| XmpMeta::new().unwrap_or_default());
        apply_to_xmp(&mut meta, write)?;

        if !file.can_put_xmp(&meta) {
            anyhow::bail!(
                "The XMP format handler cannot write metadata to {}",
                tmp_path.display()
            );
        }
        file.put_xmp(&meta)
            .with_context(|| format!("Could not write XMP metadata into {}", tmp_path.display()))?;
        file.try_close()
            .with_context(|| format!("Could not close {} after XMP update", tmp_path.display()))?;
        Ok(())
    })();

    result?;
    let temp = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&tmp_path)
        .with_context(|| {
            format!(
                "Could not reopen {} after metadata update",
                tmp_path.display()
            )
        })?;
    temp.sync_all()
        .with_context(|| format!("Could not fsync {}", tmp_path.display()))?;
    drop(temp);
    let output = super::file::fingerprint_regular_file_snapshot_blocking(&tmp_path)
        .with_context(|| format!("Could not fingerprint {}", tmp_path.display()))?
        .fingerprint;
    Ok(PreparedMetadataFile {
        guard,
        expected_input: expected,
        expected_output: output,
    })
}

#[cfg(not(feature = "xmp"))]
fn prepare_metadata_native(
    path: &Path,
    write: &MetadataWrite,
    temp_suffix: &str,
    expected_fingerprint: Option<super::file::ExistingFileFingerprint>,
) -> Result<PreparedMetadataFile> {
    let input = std::fs::read(path)
        .with_context(|| format!("Could not read {} for native EXIF update", path.display()))?;
    let expected = fingerprint_bytes(&input)?;
    if expected_fingerprint.is_some_and(|approved| approved != expected) {
        return Err(
            super::file::ConditionalPublishTargetChanged::AfterPlanning {
                path: path.to_path_buf(),
            }
            .into(),
        );
    }
    let source_permissions = std::fs::metadata(path)
        .with_context(|| format!("Could not inspect permissions of {}", path.display()))?
        .permissions();
    let Some(file_type) = native_file_type(&input, path) else {
        tracing::debug!(
            path = %path.display(),
            "Native EXIF writer supports JPEG/TIFF only; skipping metadata write"
        );
        anyhow::bail!(
            "Native EXIF writer does not support embedded metadata in {}",
            path.display()
        );
    };

    let mut metadata = match Metadata::new_from_vec(&input, file_type) {
        Ok(metadata) => metadata,
        Err(e) if e.to_string().contains("No EXIF data found") => Metadata::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("Could not read EXIF from {}", path.display()));
        }
    };
    if let Some(dt) = &write.datetime {
        metadata.set_tag(ExifTag::DateTimeOriginal(dt.clone()));
        metadata.set_tag(ExifTag::CreateDate(dt.clone()));
        metadata.set_tag(ExifTag::ModifyDate(dt.clone()));
    }
    if write.clear_datetime_offsets {
        metadata.remove_tag(ExifTag::OffsetTimeOriginal(String::new()));
        metadata.remove_tag(ExifTag::OffsetTimeDigitized(String::new()));
        metadata.remove_tag(ExifTag::OffsetTime(String::new()));
    }
    if let Some(offset) = &write.offset_time_original {
        metadata.set_tag(ExifTag::OffsetTimeOriginal(offset.clone()));
        if write.datetime.is_some() {
            // CreateDate and ModifyDate carry their own offset tags. They
            // share this offset only when this pass wrote all three
            // timestamps from the same capture-local instant.
            metadata.set_tag(ExifTag::OffsetTimeDigitized(offset.clone()));
            metadata.set_tag(ExifTag::OffsetTime(offset.clone()));
        }
    }
    if let Some(desc) = &write.description {
        metadata.set_tag(ExifTag::ImageDescription(desc.clone()));
    }
    if let Some(gps) = write.gps {
        metadata.set_tag(ExifTag::GPSLatitudeRef(if gps.latitude >= 0.0 {
            "N".to_string()
        } else {
            "S".to_string()
        }));
        metadata.set_tag(ExifTag::GPSLatitude(dms_rational(gps.latitude)));
        metadata.set_tag(ExifTag::GPSLongitudeRef(if gps.longitude >= 0.0 {
            "E".to_string()
        } else {
            "W".to_string()
        }));
        metadata.set_tag(ExifTag::GPSLongitude(dms_rational(gps.longitude)));
        if let Some(alt) = gps.altitude {
            metadata.set_tag(ExifTag::GPSAltitudeRef(vec![u8::from(alt < 0.0)]));
            metadata.set_tag(ExifTag::GPSAltitude(vec![uR64::from(alt.abs())]));
        }
    }
    if let Some(rating) = write.rating {
        let rating = u16::from(rating.min(5));
        metadata.set_tag(ExifTag::UnknownINT16U(
            vec![rating],
            WINDOWS_RATING_TAG,
            ExifTagGroup::GENERIC,
        ));
        metadata.set_tag(ExifTag::UnknownINT16U(
            vec![windows_rating_percent(rating)],
            WINDOWS_RATING_PERCENT_TAG,
            ExifTagGroup::GENERIC,
        ));
    }

    let mut output = input;
    metadata
        .write_to_vec(&mut output, file_type)
        .with_context(|| format!("Could not write native EXIF into {}", path.display()))?;
    let output_fingerprint = fingerprint_bytes(&output)?;
    let (file, tmp_path) = create_unique_embed_temp(path, temp_suffix)?;
    let cleanup_permissions = file
        .metadata()
        .with_context(|| format!("Could not inspect permissions of {}", tmp_path.display()))?
        .permissions();
    let guard = TmpGuard::with_cleanup_permissions(&tmp_path, cleanup_permissions);
    let mut writer = std::io::BufWriter::new(file);
    std::io::Write::write_all(&mut writer, &output).with_context(|| {
        format!(
            "Could not write native EXIF temp file {}",
            tmp_path.display()
        )
    })?;
    let file = writer
        .into_inner()
        .with_context(|| format!("Could not flush {}", tmp_path.display()))?;
    file.set_permissions(source_permissions)
        .with_context(|| format!("Could not preserve permissions on {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("Could not fsync {}", tmp_path.display()))?;
    drop(file);
    Ok(PreparedMetadataFile {
        guard,
        expected_input: expected,
        expected_output: output_fingerprint,
    })
}

#[cfg(not(feature = "xmp"))]
fn native_file_type(bytes: &[u8], path: &Path) -> Option<FileExtension> {
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

#[cfg(not(feature = "xmp"))]
fn dms_rational(decimal: f64) -> Vec<uR64> {
    let abs = decimal.abs();
    let degrees = abs.floor();
    let minutes_full = (abs - degrees) * 60.0;
    let minutes = minutes_full.floor();
    let seconds = (minutes_full - minutes) * 60.0;
    vec![
        uR64::from(degrees),
        uR64::from(minutes),
        uR64::from(seconds),
    ]
}

#[cfg(not(feature = "xmp"))]
const WINDOWS_RATING_TAG: u16 = 0x4746;
#[cfg(not(feature = "xmp"))]
const WINDOWS_RATING_PERCENT_TAG: u16 = 0x4749;

#[cfg(not(feature = "xmp"))]
const fn windows_rating_percent(rating: u16) -> u16 {
    match rating {
        0 => 0,
        1 => 1,
        2 => 25,
        3 => 50,
        4 => 75,
        _ => 99,
    }
}

/// Apply the requested metadata fields to an `XmpMeta`. Single source of
/// truth — both the xmp_toolkit-backed and ISO-BMFF-backed writers route
/// through here so the two paths produce identical XMP content.
#[cfg(feature = "xmp")]
fn apply_to_xmp(meta: &mut XmpMeta, write: &MetadataWrite) -> xmp_toolkit::XmpResult<()> {
    if write.clear_datetime_offsets {
        for path in ["OffsetTimeOriginal", "OffsetTimeDigitized", "OffsetTime"] {
            meta.delete_property(EXIF_EX_XMP_NS, path)?;
        }
    }
    if let Some(dt) = &write.datetime {
        // XMP uses ISO 8601; our stored form is EXIF-style "YYYY:MM:DD HH:MM:SS".
        // Convert for XMP, keep a local EXIF copy so XMP Toolkit's reconciler
        // writes the native block too on formats that have one.
        let iso = exif_datetime_to_iso(dt);
        let iso_with_offset = write
            .offset_time_original
            .as_ref()
            .map_or_else(|| iso.clone(), |offset| format!("{iso}{offset}"));
        meta.set_property(
            xmp_ns::XMP,
            "CreateDate",
            &XmpValue::new(iso_with_offset.clone()),
        )?;
        meta.set_property(
            xmp_ns::XMP,
            "ModifyDate",
            &XmpValue::new(iso_with_offset.clone()),
        )?;
        meta.set_property(
            xmp_ns::EXIF,
            "DateTimeOriginal",
            &XmpValue::new(iso_with_offset.clone()),
        )?;
        meta.set_property(
            xmp_ns::PHOTOSHOP,
            "DateCreated",
            &XmpValue::new(iso_with_offset),
        )?;
    }
    if let Some(offset) = &write.offset_time_original {
        meta.set_property(
            EXIF_EX_XMP_NS,
            "OffsetTimeOriginal",
            &XmpValue::new(offset.clone()),
        )?;
    }

    if let Some(dt) = &write.gps_datetime {
        meta.set_property(xmp_ns::EXIF, "GPSTimeStamp", &XmpValue::new(dt.clone()))?;
    }

    if let Some(speed) = &write.gps_speed {
        meta.set_property(xmp_ns::EXIF, "GPSSpeed", &XmpValue::new(speed.encode()))?;
    }
    if let Some(speed_ref) = &write.gps_speed_ref {
        meta.set_property(
            xmp_ns::EXIF,
            "GPSSpeedRef",
            &XmpValue::new(speed_ref.clone()),
        )?;
    }
    if let Some(error) = &write.gps_h_positioning_error {
        meta.set_property(
            xmp_ns::EXIF,
            "GPSHPositioningError",
            &XmpValue::new(error.encode()),
        )?;
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

    Ok(())
}

/// Read the first 12 bytes of `file` and verify it starts with an
/// ISO-BMFF `ftyp` box whose major brand is in the HEIF family. Used as
/// a sanity check between `rewrite_xmp` and the atomic rename so a
/// malformed rewrite never lands on disk. Reads from the still-open
/// rewrite handle (seeks back to 0) to avoid reopening `tmp_path`
/// immediately after `sync_all`; the path is only used for diagnostics.
#[cfg(feature = "xmp")]
fn validate_heif_post_rewrite(file: &mut std::fs::File, tmp_path: &Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(0)).with_context(|| {
        format!(
            "Could not rewind {} for media validation",
            tmp_path.display()
        )
    })?;
    let mut head = [0u8; 12];
    file.read_exact(&mut head)
        .with_context(|| format!("Could not read magic bytes of {}", tmp_path.display()))?;
    if !heif::is_heif_content(&head) {
        anyhow::bail!(
            "HEIC metadata rewrite produced invalid media at {}: the first 12 bytes did not include an ISO-BMFF ftyp/HEIF brand (got {:02x?}). Refusing to replace the user's file.",
            tmp_path.display(),
            head
        );
    }
    Ok(())
}

/// Build a standalone XMP packet from a bundle of fields. Thin convenience
/// over [`apply_to_xmp`] for callers (mostly tests) that want the serialized
/// packet bytes directly.
#[cfg(all(test, feature = "xmp"))]
fn build_xmp_packet(write: &MetadataWrite) -> Result<Vec<u8>> {
    ensure_initialized();
    let mut meta = XmpMeta::new().context("Could not create XMP metadata")?;
    apply_to_xmp(&mut meta, write)?;
    Ok(meta.to_string().into_bytes())
}

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

/// Encode decimal degrees in the EXIF-in-XMP form `"DEG,MIN.FRACHEMI"` used
/// by [Xmp.exif.GPSLatitude] / `Xmp.exif.GPSLongitude`.
#[cfg(feature = "xmp")]
fn encode_gps(decimal: f64, pos: char, neg: char) -> String {
    let hemisphere = if decimal >= 0.0 { pos } else { neg };
    let abs = decimal.abs();
    let deg = abs.floor();
    let min = (abs - deg) * 60.0;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "deg is floor of abs(lat|lon) so 0..=180; always fits in u32 with no sign"
    )]
    let deg_u32 = deg as u32;
    format!("{deg_u32},{min:.4}{hemisphere}")
}

/// Preserve provider altitude precision in the rational form required by EXIF reconciliation.
#[cfg(feature = "xmp")]
fn encode_altitude(meters: f64) -> String {
    let rational = uR64::from(meters.abs());
    format!("{}/{}", rational.nominator, rational.denominator)
}

#[cfg(all(test, feature = "xmp"))]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    fn apply_metadata_with_default_suffix(
        path: &std::path::Path,
        write: &super::MetadataWrite,
    ) -> super::Result<()> {
        super::apply_metadata(path, write, ".meta-tmp")
    }

    #[cfg(feature = "xmp")]
    fn write_sidecar_with_default_suffix(
        path: &std::path::Path,
        write: &super::MetadataWrite,
    ) -> super::Result<()> {
        static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
        RUNTIME
            .get_or_init(|| tokio::runtime::Runtime::new().expect("sidecar test runtime"))
            .block_on(super::write_sidecar(path, write, ".meta-tmp"))
    }

    fn xmp_rational(numerator: u32, denominator: u32) -> XmpRational {
        XmpRational {
            numerator,
            denominator,
        }
    }

    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use crate::test_helpers::{
        SOURCE_GPS_DATETIME, SOURCE_GPS_H_POSITIONING_ERROR, SOURCE_GPS_SPEED,
        SOURCE_GPS_SPEED_REF, exif_with_source_gps, heif_ftyp_without_meta_bytes,
        minimal_big_endian_tiff_with_source_gps, minimal_jpeg_with_source_gps,
        minimal_tiff_with_source_gps, source_gps_without_time_stamp, ur64,
    };

    fn test_tmp_dir(subdir: &str) -> PathBuf {
        std::env::temp_dir().join("claude").join(subdir)
    }

    #[test]
    fn tmp_guard_cleans_up_on_drop() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("armed.meta-tmp");
        fs::write(&path, b"pending").unwrap();
        {
            let _guard = TmpGuard::new(&path);
            assert!(path.exists(), "precondition: tmp file exists");
        }
        assert!(
            !path.exists(),
            "TmpGuard Drop must remove the tmp file on scope exit"
        );
    }

    #[test]
    fn tmp_guard_disarm_keeps_file() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disarmed.meta-tmp");
        fs::write(&path, b"keep me").unwrap();
        {
            let guard = TmpGuard::new(&path);
            guard.disarm();
        }
        assert!(path.exists(), "disarmed TmpGuard must not delete the file");
        fs::remove_file(&path).ok();
    }

    /// MS-6: validate_heif_post_rewrite must accept a real HEIC head and
    /// reject anything that doesn't begin with an ISO-BMFF ftyp/HEIF
    /// brand. The probe reads only the first 12 bytes, so a minimal
    /// fixture is sufficient.
    #[test]
    fn validate_heif_post_rewrite_accepts_known_heif_brand() {
        let dir = test_tmp_dir("ms6_validate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("good.heic");
        // ftyp box: size=0x18, kind=ftyp, major_brand=heic, minor_version=0,
        // compatible_brands=[heic, mif1].
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        fs::write(&path, &bytes).unwrap();
        let mut f = fs::File::open(&path).unwrap();
        validate_heif_post_rewrite(&mut f, &path).expect("known-good heic head must validate");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn validate_heif_post_rewrite_rejects_jpeg_magic() {
        let dir = test_tmp_dir("ms6_validate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad-jpeg.heic");
        // 12 bytes of JPEG SOI + FFD8FFE0 + JFIF header — definitely not HEIF.
        fs::write(
            &path,
            [
                0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01,
            ],
        )
        .unwrap();
        let mut f = fs::File::open(&path).unwrap();
        let err = validate_heif_post_rewrite(&mut f, &path).unwrap_err();
        assert!(err.to_string().contains("ftyp/HEIF brand"), "msg: {err}");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn validate_heif_post_rewrite_rejects_non_heif_iso_bmff() {
        let dir = test_tmp_dir("ms6_validate");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mp4.heic");
        // ftyp present but with mp42 brand — valid ISO-BMFF, not HEIF.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"mp42");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"mp42");
        bytes.extend_from_slice(b"isom");
        fs::write(&path, &bytes).unwrap();
        let mut f = fs::File::open(&path).unwrap();
        let err = validate_heif_post_rewrite(&mut f, &path).unwrap_err();
        assert!(err.to_string().contains("ftyp/HEIF brand"), "msg: {err}");
        fs::remove_file(&path).ok();
    }

    /// The xmp_toolkit writer runs closures across an FFI boundary, so a
    /// panic out of that FFI must still clean up `.meta-tmp`.
    #[test]
    fn tmp_guard_cleans_up_even_on_panic() {
        let dir = test_tmp_dir("tmp_guard");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("panic.meta-tmp");
        fs::write(&path, b"about to panic").unwrap();

        let path_for_closure = path.clone();
        let joined = std::panic::catch_unwind(move || {
            let _guard = TmpGuard::new(&path_for_closure);
            panic!("simulated xmp_toolkit FFI panic");
        });
        assert!(joined.is_err(), "closure was expected to panic");
        assert!(
            !path.exists(),
            "tmp file must be removed even when the work panics"
        );
    }

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI).
    fn minimal_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    #[test]
    fn source_gps_omits_absent_heic_fixture_fields() {
        let path = Path::new("tests/data/sample.heic");
        let gps = read_source_gps(path).expect("sample HEIC source metadata");
        assert_eq!(gps, SourceGpsMetadata::default());
    }

    #[test]
    fn source_gps_rejects_readable_unsupported_content() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let path = dir.path().join("video.mov");
        std::fs::write(&path, b"\0\0\0\x14ftypqt  \0\0\0\0qt  ").expect("write QuickTime header");
        let gps = read_source_gps(&path).expect("unsupported media should be ignored");
        assert_eq!(gps, SourceGpsMetadata::default());
    }

    #[test]
    fn source_gps_read_errors_preserve_retry_evidence_for_every_path() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        for name in [
            "video.mov",
            "source.DNG",
            "source.NEF",
            "source.SRW",
            "extensionless",
        ] {
            let path = dir.path().join(name);
            std::fs::create_dir(&path).expect("create unreadable source");
            assert!(
                read_source_gps(&path).is_err(),
                "{name} must retain retry evidence after a read failure"
            );
        }
    }

    #[test]
    fn source_gps_degrades_unparsable_media_to_empty() {
        // A recognised format whose bytes cannot yield an EXIF block must
        // degrade to no source GPS rather than an error, so the sidecar is
        // still written from the CloudKit payload and no retry marker is
        // stranded. Covers a truncated JPEG, an EXIF-less PNG, an `ftyp`-only
        // HEIC, and a DNG recognised by TIFF magic.
        let cases: [(&str, Vec<u8>); 5] = [
            ("truncated.jpg", vec![0xFF, 0xD8, 0xFF]),
            (
                "no-exif.png",
                vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            ),
            (
                "oversized-exif-chunk.png",
                [
                    b"\x89PNG\r\n\x1a\n".as_slice(),
                    &u32::MAX.to_be_bytes(),
                    b"eXIf",
                ]
                .concat(),
            ),
            ("ftyp-only.heic", heif_ftyp_without_meta_bytes()),
            (
                "recognised.DNG",
                vec![b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00],
            ),
        ];
        let dir = tempfile::tempdir().expect("metadata temp dir");
        for (name, bytes) in cases {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).expect("write case media");
            let gps = read_source_gps(&path).expect("unparsable media must not error");
            assert_eq!(gps, SourceGpsMetadata::default(), "{name}");
        }
    }

    fn assert_standard_source_gps(gps: &SourceGpsMetadata) {
        assert_eq!(gps.datetime.as_deref(), Some(SOURCE_GPS_DATETIME));
        assert_eq!(
            gps.speed.as_ref().map(XmpRational::encode).as_deref(),
            Some(SOURCE_GPS_SPEED)
        );
        assert_eq!(gps.speed_ref.as_deref(), Some(SOURCE_GPS_SPEED_REF));
        assert_eq!(
            gps.horizontal_positioning_error
                .as_ref()
                .map(XmpRational::encode)
                .as_deref(),
            Some(SOURCE_GPS_H_POSITIONING_ERROR)
        );
    }

    fn read_source_gps_from_test_exif(metadata: &Metadata, name: &str) -> SourceGpsMetadata {
        let mut bytes = minimal_jpeg();
        metadata
            .write_to_vec(&mut bytes, FileExtension::JPEG)
            .expect("serialise source EXIF");
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write source media");
        read_source_gps(&path).expect("read source EXIF through production parser")
    }

    fn read_source_gps_from_test_tiff(bytes: &[u8], name: &str) -> SourceGpsMetadata {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).expect("write source TIFF");
        read_source_gps(&path).expect("read source TIFF through production parser")
    }

    fn png_with_source_gps() -> Vec<u8> {
        fn crc32(bytes: &[u8]) -> u32 {
            let mut crc = u32::MAX;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = if crc & 1 == 0 {
                        crc >> 1
                    } else {
                        (crc >> 1) ^ 0xEDB8_8320
                    };
                }
            }
            !crc
        }

        let mut png = std::fs::read("assets/logo3.png").expect("read PNG fixture");
        let tiff = minimal_tiff_with_source_gps();
        let ihdr_len = u32::from_be_bytes(png[8..12].try_into().expect("PNG IHDR length")) as usize;
        let insert_at = 8 + 12 + ihdr_len;
        let mut chunk = Vec::with_capacity(12 + tiff.len());
        chunk.extend_from_slice(
            &u32::try_from(tiff.len())
                .expect("TIFF fixture length")
                .to_be_bytes(),
        );
        chunk.extend_from_slice(b"eXIf");
        chunk.extend_from_slice(&tiff);
        chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
        png.splice(insert_at..insert_at, chunk);
        png
    }

    struct CountingCursor {
        inner: std::io::Cursor<Vec<u8>>,
        bytes_read: usize,
    }

    impl std::io::Read for CountingCursor {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let read = std::io::Read::read(&mut self.inner, output)?;
            self.bytes_read += read;
            Ok(read)
        }
    }

    impl std::io::Seek for CountingCursor {
        fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
            std::io::Seek::seek(&mut self.inner, position)
        }
    }

    #[test]
    fn tiff_source_gps_reads_fixed_metadata_not_trailing_media() {
        let mut bytes = minimal_tiff_with_source_gps();
        bytes.resize(1024 * 1024, 0);
        let len = bytes.len() as u64;
        let mut source = CountingCursor {
            inner: std::io::Cursor::new(bytes),
            bytes_read: 0,
        };

        let gps = read_tiff_source_gps(&mut source, 0, len).expect("stream TIFF GPS");

        assert_standard_source_gps(&gps);
        assert!(
            source.bytes_read < 256,
            "TIFF GPS parsing read {} bytes",
            source.bytes_read
        );
    }

    #[test]
    fn tiff_source_gps_rejects_attacker_sized_ifd_count_without_allocation() {
        let mut bytes = minimal_tiff_with_source_gps();
        bytes[26..28].copy_from_slice(&u16::MAX.to_le_bytes());
        let len = bytes.len() as u64;
        let mut source = std::io::Cursor::new(bytes);

        assert!(matches!(
            read_tiff_source_gps(&mut source, 0, len),
            Err(TiffGpsError::Malformed)
        ));
    }

    #[test]
    fn source_gps_composes_utc_timestamps() {
        assert_standard_source_gps(&read_source_gps_from_test_exif(
            &exif_with_source_gps(),
            "standard.jpg",
        ));
        // A whole-second GPS timestamp renders without a fractional part.
        let mut metadata = exif_with_source_gps();
        metadata.set_tag(ExifTag::GPSTimeStamp(vec![
            ur64(11, 1),
            ur64(22, 1),
            ur64(33, 1),
        ]));
        let gps = read_source_gps_from_test_exif(&metadata, "whole-second.jpg");
        assert_eq!(gps.datetime.as_deref(), Some("2024-06-15T11:22:33Z"));
    }

    #[test]
    fn source_gps_reads_exif_from_media_path() {
        // `read_source_gps` recovers the same facts from JPEG, HEIC, and
        // TIFF-based DNG media. A DNG path holding JPEG bytes proves content
        // selects the parser instead of the extension hint.
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let mut heic = std::fs::read("tests/data/sample.heic").expect("read sample HEIC");
        exif_with_source_gps()
            .write_to_vec(&mut heic, FileExtension::HEIF)
            .expect("write HEIC EXIF");
        for (name, bytes) in [
            ("source.jpg", minimal_jpeg_with_source_gps()),
            ("jpeg-content.DNG", minimal_jpeg_with_source_gps()),
            ("source.DNG", minimal_tiff_with_source_gps()),
            ("source.NEF", minimal_tiff_with_source_gps()),
            ("big-endian.DNG", minimal_big_endian_tiff_with_source_gps()),
            ("source.png", png_with_source_gps()),
            ("source.heic", heic),
            (
                "multi-exif.heic",
                heif::apple_multi_exif_heic(&crate::test_helpers::minimal_tiff_with_source_gps()),
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).expect("write source media");
            let gps = read_source_gps(&path).expect("read source EXIF");
            assert_eq!(
                gps.datetime.as_deref(),
                Some(SOURCE_GPS_DATETIME),
                "{name}: {gps:?}"
            );
            assert_standard_source_gps(&gps);
        }
    }

    #[test]
    fn jpeg_source_gps_does_not_substitute_camera_datetime_for_gps_date() {
        let mut metadata = Metadata::new();
        metadata.set_tag(ExifTag::DateTimeOriginal("2024:06:16 08:00:00".into()));
        metadata.set_tag(ExifTag::GPSTimeStamp(vec![
            ur64(21, 1),
            ur64(0, 1),
            ur64(0, 1),
        ]));
        metadata.set_tag(ExifTag::GPSSpeedRef("K".into()));
        metadata.set_tag(ExifTag::GPSSpeed(vec![ur64(12_345, 100)]));
        let mut bytes = minimal_jpeg();
        metadata
            .write_to_vec(&mut bytes, FileExtension::JPEG)
            .expect("write JPEG EXIF");
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let path = dir.path().join("source.jpg");
        std::fs::write(&path, bytes).expect("write source JPEG");

        let gps = read_source_gps(&path).expect("read source EXIF");

        assert!(gps.datetime.is_none());
        assert_eq!(
            gps.speed.as_ref().map(XmpRational::encode).as_deref(),
            Some(SOURCE_GPS_SPEED)
        );
    }

    #[test]
    fn source_gps_omits_malformed_timestamp_variants() {
        // A valid date with a malformed GPSTimeStamp, or a malformed date,
        // yields no GPS datetime while sibling source fields prove the GPS IFD
        // was serialised and parsed. `not-a-date` is deliberately ten bytes so
        // its EXIF ASCII count is the required 11 including the trailing NUL.
        let bad = |n: u32| ur64(n, 0);
        let cases: [(&str, Option<&str>, Vec<uR64>); 8] = [
            (
                "malformed date",
                Some("not-a-date"),
                vec![ur64(10, 1), ur64(20, 1), ur64(30, 1)],
            ),
            ("wrong arity", None, vec![ur64(10, 1), ur64(20, 1)]),
            (
                "zero denominator",
                None,
                vec![bad(10), ur64(20, 1), ur64(30, 1)],
            ),
            (
                "fractional hour",
                None,
                vec![ur64(21, 2), ur64(20, 1), ur64(30, 1)],
            ),
            (
                "minute out of range",
                None,
                vec![ur64(10, 1), ur64(60, 1), ur64(30, 1)],
            ),
            (
                "seconds out of range",
                None,
                vec![ur64(10, 1), ur64(20, 1), ur64(60, 1)],
            ),
            (
                "hour out of range",
                None,
                vec![ur64(25, 1), ur64(20, 1), ur64(30, 1)],
            ),
            (
                "minute zero denominator",
                None,
                vec![ur64(10, 1), bad(20), ur64(30, 1)],
            ),
        ];
        for (name, date, times) in cases {
            let mut metadata = exif_with_source_gps();
            if let Some(date) = date {
                metadata.set_tag(ExifTag::GPSDateStamp(date.into()));
            }
            metadata.set_tag(ExifTag::GPSTimeStamp(times));
            let gps = read_source_gps_from_test_exif(&metadata, &format!("{name}.jpg"));
            assert_eq!(gps.datetime, None, "{name}");
            assert_eq!(
                gps.speed.as_ref().map(XmpRational::encode).as_deref(),
                Some(SOURCE_GPS_SPEED),
                "{name}"
            );
            assert_eq!(
                gps.speed_ref.as_deref(),
                Some(SOURCE_GPS_SPEED_REF),
                "{name}"
            );
        }

        // A date with no GPSTimeStamp at all cannot compose a timestamp.
        let gps =
            read_source_gps_from_test_exif(&source_gps_without_time_stamp(), "missing-time.jpg");
        assert_eq!(gps.datetime, None);
        assert_eq!(
            gps.speed.as_ref().map(XmpRational::encode).as_deref(),
            Some(SOURCE_GPS_SPEED)
        );
    }

    #[test]
    fn source_gps_omits_unsupported_speed_ref_and_malformed_error() {
        let mut metadata = exif_with_source_gps();
        // An unsupported speed unit drops both the speed and the unit.
        metadata.set_tag(ExifTag::GPSSpeedRef("Y".into()));
        // A zero-denominator positioning error is dropped.
        metadata.set_tag(ExifTag::GPSHPositioningError(vec![ur64(1, 0)]));

        let gps = read_source_gps_from_test_exif(&metadata, "invalid-speed-error.jpg");
        assert_eq!(gps.speed, None);
        assert_eq!(gps.speed_ref, None);
        assert_eq!(gps.horizontal_positioning_error, None);
        assert_eq!(gps.datetime.as_deref(), Some(SOURCE_GPS_DATETIME));
    }

    #[test]
    fn source_gps_rejects_wrong_tiff_types_and_counts() {
        let cases = [
            ("timestamp type", 30, 2_u16, None, true),
            (
                "speed reference type",
                42,
                5_u16,
                Some(SOURCE_GPS_DATETIME),
                false,
            ),
            ("speed type", 54, 2_u16, Some(SOURCE_GPS_DATETIME), false),
        ];
        for (name, type_offset, value_type, datetime, speed_survives) in cases {
            let mut bytes = minimal_tiff_with_source_gps();
            bytes[type_offset..type_offset + 2].copy_from_slice(&value_type.to_le_bytes());
            let gps = read_source_gps_from_test_tiff(&bytes, &format!("{name}.DNG"));
            assert_eq!(gps.datetime.as_deref(), datetime, "{name}");
            assert_eq!(gps.speed.is_some(), speed_survives, "{name}");
            assert!(
                gps.horizontal_positioning_error.is_some(),
                "{name}: sibling field must survive"
            );
        }

        for (name, count_offset, count) in
            [("timestamp count", 32, 2_u32), ("date count", 68, 10_u32)]
        {
            let mut bytes = minimal_tiff_with_source_gps();
            bytes[count_offset..count_offset + 4].copy_from_slice(&count.to_le_bytes());
            let gps = read_source_gps_from_test_tiff(&bytes, &format!("{name}.DNG"));
            assert_eq!(gps.datetime, None, "{name}");
            assert_eq!(
                gps.speed.as_ref().map(XmpRational::encode).as_deref(),
                Some(SOURCE_GPS_SPEED),
                "{name}: sibling field must survive"
            );
        }
    }

    #[test]
    fn gps_datetime_serialises_as_typed_xmp_date() {
        // The sidecar-write test proves the GPS property values reach XMP. This
        // pins the standard property ID and its typed date value.
        let gps = read_source_gps_from_test_exif(&exif_with_source_gps(), "typed-date.jpg");
        let packet = build_xmp_packet(&MetadataWrite {
            gps_datetime: gps.datetime,
            ..MetadataWrite::default()
        })
        .expect("XMP packet");
        let packet = std::str::from_utf8(&packet).expect("XMP UTF-8");
        let meta = packet.parse::<XmpMeta>().expect("parse XMP packet");
        assert!(packet.contains("<exif:GPSTimeStamp>"));
        assert!(meta.property_date(xmp_ns::EXIF, "GPSTimeStamp").is_some());
        assert!(!meta.contains_property(xmp_ns::EXIF, "GPSDateTime"));
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
        apply_metadata_with_default_suffix(&path, &MetadataWrite::default()).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(before, after, "empty write must not touch the file");
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_uses_configured_temp_suffix() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "custom_suffix.jpg");
        let default_tmp = dir.join("custom_suffix.jpg.meta-tmp");
        let configured_tmp = dir.join("custom_suffix.jpg.kei-tmp");
        fs::write(&default_tmp, b"sentinel").unwrap();
        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();
        assert_eq!(
            fs::read(&default_tmp).unwrap(),
            b"sentinel",
            "metadata rewrite must not use the old .meta-tmp suffix"
        );
        assert!(
            !configured_tmp.exists(),
            "configured metadata temp path must be installed or cleaned up"
        );
        fs::remove_file(&default_tmp).ok();
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_datetime_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "dt.jpg");
        apply_metadata_with_default_suffix(
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

    #[cfg(feature = "xmp")]
    #[test]
    fn xmp_datetime_includes_capture_offset() {
        let packet = build_xmp_packet(&MetadataWrite {
            datetime: Some("2026:02:01 09:31:59".to_string()),
            offset_time_original: Some("+11:00".to_string()),
            ..MetadataWrite::default()
        })
        .unwrap();
        let meta = std::str::from_utf8(&packet)
            .unwrap()
            .parse::<XmpMeta>()
            .unwrap();
        assert_eq!(
            meta.property(super::EXIF_EX_XMP_NS, "OffsetTimeOriginal")
                .unwrap()
                .value,
            "+11:00"
        );
        let probe = super::probe_from_meta(&meta);
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
        assert!(meta.property(xmp_ns::EXIF, "OffsetTimeOriginal").is_none());
        assert_eq!(
            meta.property(xmp_ns::EXIF, "DateTimeOriginal")
                .unwrap()
                .value,
            "2026-02-01T09:31:59+11:00"
        );
    }

    /// An offset names the zone of one specific timestamp. Replacing the
    /// timestamp orphans every offset already in the file, so they have to go
    /// before the resolved one lands, or a stale zone qualifies a capture time
    /// it never described.
    #[cfg(feature = "xmp")]
    #[test]
    fn xmp_write_clears_offsets_orphaned_from_a_replaced_timestamp() {
        ensure_initialized();
        let mut meta = XmpMeta::new().unwrap();
        for property in ["OffsetTimeOriginal", "OffsetTimeDigitized", "OffsetTime"] {
            meta.set_property(
                super::EXIF_EX_XMP_NS,
                property,
                &XmpValue::new("+02:00".to_string()),
            )
            .unwrap();
        }

        super::apply_to_xmp(
            &mut meta,
            &MetadataWrite {
                datetime: Some("2026:02:01 09:31:59".to_string()),
                offset_time_original: Some("+11:00".to_string()),
                clear_datetime_offsets: true,
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        assert_eq!(
            meta.property(super::EXIF_EX_XMP_NS, "OffsetTimeOriginal")
                .unwrap()
                .value,
            "+11:00"
        );
        for property in ["OffsetTimeDigitized", "OffsetTime"] {
            assert!(
                !meta.contains_property(super::EXIF_EX_XMP_NS, property),
                "{property} must not survive to qualify a timestamp it never described"
            );
        }
    }

    #[test]
    fn apply_metadata_rating_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "rating.jpg");
        apply_metadata_with_default_suffix(
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
        apply_metadata_with_default_suffix(
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
        const ALTITUDE: f64 = 9.250_123_456_789;
        const ALTITUDE_RATIONAL: &str = "2032686204/219746927";

        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "gps.jpg");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: Some(ALTITUDE),
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
        assert_eq!(
            meta.property(xmp_ns::EXIF, "GPSAltitude").unwrap().value,
            ALTITUDE_RATIONAL
        );

        let native = Metadata::new_from_path(&path).unwrap();
        let native_altitude = native
            .get_tag(&ExifTag::GPSAltitude(Vec::new()))
            .next()
            .and_then(|tag| match tag {
                ExifTag::GPSAltitude(values) => values.first(),
                _ => None,
            });
        assert_eq!(native_altitude, Some(&ur64(2_032_686_204, 219_746_927)));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_description_roundtrips() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "desc.jpg");
        apply_metadata_with_default_suffix(
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
        apply_metadata_with_default_suffix(
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
        apply_metadata_with_default_suffix(
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
        apply_metadata_with_default_suffix(
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
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                offset_time_original: None,
                clear_datetime_offsets: false,
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
                ..MetadataWrite::default()
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
        let result = apply_metadata_with_default_suffix(
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
    fn xmp_toolkit_rewrite_fsyncs_temp_before_part_replace() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "durable_install.jpg");
        let original = fs::read(&path).unwrap();
        let install_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let install_called_in_closure = Arc::clone(&install_called);
        let expected_path = path.clone();

        let prepared = prepare_metadata_xmp_toolkit(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, move |tmp, dst, _expected, _replacement| {
            install_called_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
            assert!(
                tmp.exists(),
                "metadata temp file must exist before durable install"
            );
            assert_eq!(dst, expected_path.as_path());
            Err(std::io::Error::other("simulated durable install failure").into())
        });

        assert!(
            result.is_err(),
            "durable install failure must surface to leave the asset retryable"
        );
        assert!(
            install_called.load(std::sync::atomic::Ordering::SeqCst),
            "XMP rewrite must route final replacement through the durable install primitive"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "failed metadata publish must leave original bytes intact"
        );
        assert!(
            embed_temp_entries(&dir, ".meta-tmp").is_empty(),
            "metadata temp file must be cleaned up on durable install failure"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn contract_metadata_embed_rewrite_requires_stable_input() {
        let dir = test_tmp_dir("meta_tests");
        let path = fresh_jpeg(&dir, "concurrent_edit.jpg");
        let external = b"external edit".to_vec();

        let prepared = prepare_metadata_xmp_toolkit(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, dst, expected, expected_replacement| {
            fs::write(dst, &external)?;
            publish_prepared_embed(src, dst, expected, expected_replacement)
        });

        let error = result.expect_err("a concurrent edit must block metadata publication");
        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), external);
        assert!(embed_temp_entries(&dir, ".meta-tmp").is_empty());
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

    // ── HEIC tests ──────────────────────────────────────────────────────

    /// `build_xmp_packet` emits a packet bytes blob that libheif can accept.
    /// Verifies the packet contains the rdf:RDF wrapper and our data.
    #[test]
    fn build_xmp_packet_is_deterministic() {
        let w = MetadataWrite {
            rating: Some(3),
            title: Some("X".into()),
            ..MetadataWrite::default()
        };
        let a = build_xmp_packet(&w).unwrap();
        let b = build_xmp_packet(&w).unwrap();
        assert_eq!(a.len(), b.len(), "XMP packet size must be deterministic");
        assert_eq!(a, b, "XMP packet bytes must be deterministic");
    }

    #[test]
    fn build_xmp_packet_contains_requested_fields() {
        let bytes = build_xmp_packet(&MetadataWrite {
            rating: Some(4),
            title: Some("Beach".into()),
            keywords: vec!["vacation".into(), "sand".into()],
            ..MetadataWrite::default()
        })
        .unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("rdf:RDF"), "missing rdf:RDF wrapper");
        assert!(s.contains("xmp:Rating"), "missing xmp:Rating");
        assert!(s.contains("Beach"), "missing title value");
        assert!(s.contains("vacation"), "missing keyword");
    }

    const SAMPLE_HEIC: &[u8] = include_bytes!("../../tests/data/sample.heic");
    const SAMPLE_AVIF: &[u8] = include_bytes!("../../tests/data/white_1x1.avif");

    fn fresh_heic(dir: &Path, name: &str) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, SAMPLE_HEIC).unwrap();
        path
    }

    fn embed_temp_entries(dir: &Path, temp_suffix: &str) -> Vec<PathBuf> {
        let prefix = format!(".kei-metadata-{}-", std::process::id());
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                (name.starts_with(&prefix) && name.ends_with(temp_suffix)).then(|| entry.path())
            })
            .collect()
    }

    #[test]
    fn create_unique_embed_temp_preserves_regular_file_collision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        let collision = embed_temp_path(&path, ".meta-tmp", 31);
        let unique = embed_temp_path(&path, ".meta-tmp", 32);
        fs::write(&collision, b"unrelated bytes").unwrap();
        let mut sequences = [31, 32].into_iter();

        let (file, created) = create_unique_embed_temp_with_sequence(&path, ".meta-tmp", || {
            sequences.next().unwrap()
        })
        .unwrap();
        drop(file);

        assert_eq!(created, unique);
        assert_eq!(fs::read(&collision).unwrap(), b"unrelated bytes");
        fs::remove_file(created).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn create_unique_embed_temp_preserves_symlink_collision() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.heic");
        let target = dir.path().join("unrelated");
        let collision = embed_temp_path(&path, ".meta-tmp", 51);
        let unique = embed_temp_path(&path, ".meta-tmp", 52);
        fs::write(&target, b"unrelated bytes").unwrap();
        symlink(&target, &collision).unwrap();
        let mut sequences = [51, 52].into_iter();

        let (file, created) = create_unique_embed_temp_with_sequence(&path, ".meta-tmp", || {
            sequences.next().unwrap()
        })
        .unwrap();
        drop(file);

        assert_eq!(created, unique);
        assert_eq!(fs::read_link(&collision).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"unrelated bytes");
        fs::remove_file(created).unwrap();
    }

    fn heic_with_xmp_packet(xmp: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        heif::rewrite_xmp(SAMPLE_HEIC, xmp, &mut bytes)
            .expect("sample HEIC should accept a seed XMP packet");
        bytes
    }

    fn write_seeded_heic(dir: &Path, name: &str, seed: &MetadataWrite) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        let seed_xmp = build_xmp_packet(seed).expect("seed XMP packet");
        fs::write(&path, heic_with_xmp_packet(&seed_xmp)).unwrap();
        path
    }

    fn read_heif_meta(bytes: &[u8]) -> XmpMeta {
        let xmp = extract_xmp_from_heic(bytes).expect("HEIF XMP missing");
        std::str::from_utf8(&xmp)
            .expect("HEIF XMP is not UTF-8")
            .parse()
            .expect("HEIF XMP is not valid")
    }

    fn heif_ftyp_without_meta() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&24u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        bytes
    }

    #[test]
    fn apply_metadata_heic_rating_and_title() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "rating.heic");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(5),
                title: Some("Vacation".into()),
                keywords: vec!["beach".into()],
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 5);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Vacation"
        );
        assert_eq!(
            meta.property_array(xmp_ns::DC, "subject")
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec!["beach"]
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_gps_roundtrips() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "gps.heic");
        apply_metadata_with_default_suffix(
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
        .expect("HEIC GPS metadata write");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert!(meta.property(xmp_ns::EXIF, "GPSLatitude").is_some());
        assert!(meta.property(xmp_ns::EXIF, "GPSLongitude").is_some());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_preserves_image_data() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "preserve.heic");
        let source = fs::read(&path).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC rating metadata write");
        let rewritten = fs::read(&path).unwrap();
        assert_eq!(
            find_mdat_bytes(&source),
            find_mdat_bytes(&rewritten),
            "HEIC image payload must remain byte-for-byte stable"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_avif_inserts_xmp_and_preserves_image_data() {
        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("white_1x1.avif");
        fs::write(&path, SAMPLE_AVIF).unwrap();
        let source = fs::read(&path).unwrap();

        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(4),
                title: Some("White pixel".into()),
                ..MetadataWrite::default()
            },
        )
        .expect("AVIF metadata write");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "White pixel"
        );
        heif::validate_rewrite_preserves_non_xmp_items(&source, &rewritten)
            .expect("AVIF image payload and item metadata must remain byte-for-byte stable");
        fs::remove_file(&path).ok();
    }

    /// Existing XMP is layered with the requested fields without changing the
    /// HEIC item count or image payload.
    #[test]
    fn apply_metadata_heic_preserves_existing_xmp() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "preserve_xmp.heic",
            &MetadataWrite {
                title: Some("First".into()),
                ..MetadataWrite::default()
            },
        );
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata update");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "First"
        );
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
        assert_eq!(count_xmp_items_in_heic(&rewritten), 1);
        fs::remove_file(&path).ok();
    }

    /// An iOS HDR capture carries an XMP packet per image. Writing the
    /// photograph's metadata into an auxiliary image's packet would attach it
    /// to a gain map, and no later read could tell.
    #[test]
    fn apply_metadata_heic_writes_past_auxiliary_image_xmp() {
        const AUX_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>";
        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        let seed = build_xmp_packet(&MetadataWrite {
            title: Some("First".into()),
            ..MetadataWrite::default()
        })
        .expect("seed XMP packet");

        for (name, primary) in [
            ("aux_and_primary_xmp.heic", Some(seed.as_slice())),
            ("aux_xmp_only.heic", None),
        ] {
            let path = dir.join(name);
            fs::write(&path, heif::apple_multi_xmp_heic(primary, AUX_XMP)).unwrap();

            apply_metadata_with_default_suffix(
                &path,
                &MetadataWrite {
                    rating: Some(4),
                    ..MetadataWrite::default()
                },
            )
            .expect("HEIC metadata update");

            let rewritten = fs::read(&path).unwrap();
            let packets = xmp_packets_in_heic(&rewritten);
            assert_eq!(packets.len(), 2, "{name} must keep both XMP items");
            assert!(
                packets.iter().any(|(_, packet)| packet == AUX_XMP),
                "{name} must leave the auxiliary image's packet byte-for-byte"
            );
            let photo = packets
                .iter()
                .find(|(_, packet)| packet != AUX_XMP)
                .map(|(_, packet)| packet.clone())
                .expect("photograph's packet");
            let meta: XmpMeta = std::str::from_utf8(&photo).unwrap().parse().unwrap();
            assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
            if primary.is_some() {
                assert_eq!(
                    meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                        .unwrap()
                        .0
                        .value,
                    "First",
                    "{name} must merge into the photograph's own packet"
                );
            }
            fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn apply_metadata_heic_preserves_fixture_with_seeded_xmp_item() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "seeded_xmp.heic",
            &MetadataWrite {
                description: Some("Original iOS caption".into()),
                people: vec!["Casey".into()],
                media_subtype: Some("portrait".into()),
                ..MetadataWrite::default()
            },
        );
        let source = fs::read(&path).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(5),
                title: Some("Kei rewrite".into()),
                keywords: vec!["Favorites".into()],
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata update");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 5);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "description", None, "x-default")
                .unwrap()
                .0
                .value,
            "Original iOS caption"
        );
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Kei rewrite"
        );
        assert_eq!(
            meta.property_array(xmp_ns::DC, "subject")
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec!["Favorites"]
        );
        assert_eq!(
            meta.property_array(xmp_ns::IPTC_EXT, "PersonInImage")
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec!["Casey"]
        );
        assert!(meta.property(KEI_XMP_NS, "mediaSubtype").is_some());
        assert_eq!(
            count_xmp_items_in_heic(&rewritten),
            1,
            "repeat HEIC metadata updates must retain one XMP item"
        );
        assert_eq!(
            find_mdat_bytes(&source),
            find_mdat_bytes(&rewritten),
            "HEIC image data must remain byte-for-byte stable"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_heic_failure_leaves_media_bytes_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing_meta.heic");
        let original = heif_ftyp_without_meta_bytes();
        fs::write(&path, &original).unwrap();

        let result = apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        );
        assert!(
            result.is_err(),
            "HEIC files without a meta box must be rejected"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "rejected HEIC rewrites must leave original media bytes untouched"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "rejected HEIC rewrites must clean the metadata temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_rejects_conflicting_tone_map_xmp_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflicting_tone_map_xmp.heic");
        let original = heif::apple_tmap_conflicting_xmp_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
            b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>",
        );
        fs::write(&path, &original).unwrap();

        let result = apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                ..MetadataWrite::default()
            },
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "conflicting XMP ownership must leave original media bytes untouched"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "a refused rewrite must clean its uniquely owned temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_install_failure_leaves_original_intact_and_cleans_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "install_fault.heic");
        let original = fs::read(&path).unwrap();

        // Inject a failure at the exact boundary where the temp file has been
        // written, fsynced and validated, but the atomic install has not yet
        // renamed over the original.
        let install_calls = std::cell::Cell::new(0u32);
        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, _dst, _expected, _expected_replacement| {
            install_calls.set(install_calls.get() + 1);
            assert!(src.exists(), "temp file must exist at the install boundary");
            Err(std::io::Error::other("simulated crash before publication").into())
        });

        assert!(
            result.is_err(),
            "an install failure must surface as an error"
        );
        assert_eq!(
            install_calls.get(),
            1,
            "the install boundary must be reached"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            original,
            "a crash before publication must leave the original byte-identical"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "a failure before exchange must clean its uniquely owned temp file"
        );
    }

    #[test]
    fn contract_heif_embed_rewrite_requires_stable_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "concurrent_edit.heic");
        let external = b"external edit at the install boundary";

        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, dst, expected, expected_replacement| {
            fs::write(dst, external)?;
            crate::download::file::publish_file_if_unchanged_blocking(
                src,
                dst,
                expected,
                expected_replacement,
            )
        });

        let error = result.expect_err("a concurrent edit must block HEIC publication");
        assert!(
            crate::download::file::classify_conditional_publish_error(&error).target_changed,
            "the caller must preserve checksum evidence for a concurrent edit"
        );
        let error_chain = format!("{error:#}");
        assert!(error_chain.contains("bytes changed"), "{error_chain}");
        assert_eq!(
            fs::read(&path).unwrap(),
            external,
            "a concurrent edit must not be overwritten by the stale HEIC rewrite"
        );
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "a refused pre-exchange publication must clean its uniquely owned temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_refuses_temp_changed_after_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "changed_temp.heic");
        let original = fs::read(&path).unwrap();

        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let error = prepared
            .publish_with(&path, |src, dst, expected, expected_replacement| {
                fs::write(src, b"unapproved temporary bytes")?;
                crate::download::file::publish_file_if_unchanged_blocking(
                    src,
                    dst,
                    expected,
                    expected_replacement,
                )
            })
            .expect_err("temporary bytes changed after validation must not be published");

        assert_eq!(fs::read(&path).unwrap(), original);
        let retained = embed_temp_entries(dir.path(), ".meta-tmp");
        assert_eq!(retained.len(), 1);
        let disposition = crate::download::file::classify_conditional_publish_error(&error);
        assert!(disposition.retained_paths.contains(&retained[0]));
        fs::remove_file(&retained[0]).unwrap();
    }

    #[test]
    fn apply_metadata_heic_refuses_change_after_approved_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "changed_before_read.heic");
        let original = fs::read(&path).unwrap();
        let approved = fingerprint_bytes(&original).unwrap();
        let changed = heic_with_xmp_packet(
            &build_xmp_packet(&MetadataWrite {
                title: Some("External edit".into()),
                ..MetadataWrite::default()
            })
            .unwrap(),
        );
        fs::write(&path, &changed).unwrap();

        let error = apply_metadata_with_expected_fingerprint(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            Some(approved),
        )
        .expect_err("bytes changed after retry approval must not be rewritten");

        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), changed);
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }

    #[test]
    fn apply_metadata_heic_refuses_format_change_after_approved_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "changed_format.heic");
        let approved = fingerprint_bytes(&fs::read(&path).unwrap()).unwrap();
        let changed = minimal_jpeg();
        fs::write(&path, &changed).unwrap();

        let error = apply_metadata_with_expected_fingerprint(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            Some(approved),
        )
        .expect_err("a format change after retry approval must not be rewritten");

        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), changed);
        assert!(!temp_path_for(&path, ".meta-tmp").exists());
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }

    #[test]
    fn apply_metadata_heic_returns_installed_output_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "output_fingerprint.heic");
        let approved = fingerprint_bytes(&fs::read(&path).unwrap()).unwrap();

        let output = apply_metadata_with_expected_fingerprint(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            Some(approved),
        )
        .expect("HEIC metadata write")
        .expect("HEIC writes return their installed fingerprint");

        assert_eq!(
            fingerprint_bytes(&fs::read(&path).unwrap()).unwrap(),
            output
        );
    }

    #[test]
    fn apply_metadata_heic_temp_preserves_readonly_permission() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "readonly.heic");
        let original_permissions = fs::metadata(&path).unwrap().permissions();
        let mut readonly_permissions = original_permissions.clone();
        readonly_permissions.set_readonly(true);
        fs::set_permissions(&path, readonly_permissions).unwrap();

        let prepared = prepare_metadata_heif(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, _dst, _expected, _expected_replacement| {
            assert!(
                fs::metadata(src).unwrap().permissions().readonly(),
                "the validated temp file must carry the source read-only permission"
            );
            Err(std::io::Error::other("stop after permission check").into())
        });

        assert!(result.is_err(), "the injected install failure must surface");
        fs::set_permissions(&path, original_permissions).unwrap();
        assert!(embed_temp_entries(dir.path(), ".meta-tmp").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn apply_metadata_heic_preserves_unix_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = fresh_heic(dir.path(), "mode.heic");
        let original_permissions = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(5),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write");

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640,
            "the installed file must preserve the source mode"
        );
        fs::set_permissions(&path, original_permissions).unwrap();
        assert!(
            embed_temp_entries(dir.path(), ".meta-tmp").is_empty(),
            "the mode check must not leave a temp file"
        );
    }

    #[test]
    fn apply_metadata_heic_is_idempotent_on_rewrite() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = fresh_heic(&dir, "idempotent.heic");
        let write = MetadataWrite {
            rating: Some(4),
            title: Some("Repeat".into()),
            ..MetadataWrite::default()
        };
        let original = fs::read(&path).unwrap();

        apply_metadata_with_default_suffix(&path, &write).unwrap();
        let first = fs::read(&path).unwrap();
        apply_metadata_with_default_suffix(&path, &write).unwrap();
        let second = fs::read(&path).unwrap();

        assert_ne!(
            first, original,
            "HEIC metadata write must update the XMP item"
        );
        assert_eq!(
            second, first,
            "repeated HEIC metadata writes must stay idempotent"
        );
        fs::remove_file(&path).ok();
    }

    /// Each write merges into the packet the reader resolved, so unrelated
    /// fields must survive. A shrinking packet also reuses the existing extent
    /// and pads the tail, which is the layout the post-write identity check has
    /// to tolerate.
    #[test]
    fn apply_metadata_heic_changes_one_field_without_rewriting_other_xmp() {
        let dir = test_tmp_dir("meta_heic_tests");
        let path = write_seeded_heic(
            &dir,
            "change_rating.heic",
            &MetadataWrite {
                rating: Some(5),
                title: Some("Keep this".into()),
                keywords: vec![
                    "alpha".into(),
                    "beta".into(),
                    "gamma".into(),
                    "delta".into(),
                ],
                ..MetadataWrite::default()
            },
        );
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(2),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC rating change");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 2);
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Keep this"
        );
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(0),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC rating clear");
        let cleared = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(
            cleared.property_i32(xmp_ns::XMP, "Rating").unwrap().value,
            0
        );

        let before = extract_xmp_from_heic(&fs::read(&path).unwrap()).expect("packet before");
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                keywords: vec!["alpha".into()],
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC keyword removal");
        let after = extract_xmp_from_heic(&fs::read(&path).unwrap()).expect("packet after");
        assert_eq!(
            after.len(),
            before.len(),
            "a shrinking packet must reuse the existing extent"
        );
        assert!(
            after.ends_with(b" "),
            "the reused extent must be padded to its original length"
        );
        let shrunk = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(
            shrunk
                .localized_text(xmp_ns::DC, "title", None, "x-default")
                .unwrap()
                .0
                .value,
            "Keep this",
            "removing a keyword must not disturb the rest of the packet"
        );
        assert_eq!(
            shrunk.property_array(xmp_ns::DC, "subject").count(),
            1,
            "stale keywords must not accumulate"
        );
        fs::remove_file(&path).ok();
    }

    /// The first XMP packet in a HEIC file. Single-packet fixtures only; use
    /// [`xmp_packets_in_heic`] where several images each carry one.
    fn extract_xmp_from_heic(bytes: &[u8]) -> Option<Vec<u8>> {
        xmp_packets_in_heic(bytes)
            .into_iter()
            .next()
            .map(|(_, packet)| packet)
    }

    /// Walk a HEIC file's top-level atoms and return every XMP packet with its
    /// item ID. The write path puts XMP in a trailing `mdat`; the iloc entry is
    /// construction_method=0 with a file-absolute offset, so we slice the
    /// file bytes directly. Parsed independently of kei's own reader so a test
    /// cannot confirm the reader against itself.
    fn xmp_packets_in_heic(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
        use mp4_atom::{Any, DecodeMaybe, FourCC, Iinf, Iloc};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            let Any::Meta(meta) = atom else {
                continue;
            };
            let (Some(iinf), Some(iloc)) = (meta.get::<Iinf>(), meta.get::<Iloc>()) else {
                return Vec::new();
            };
            return iinf
                .item_infos
                .iter()
                .filter(|e| {
                    e.item_type == Some(FourCC::new(b"mime"))
                        && e.content_type.as_deref() == Some("application/rdf+xml")
                })
                .filter_map(|entry| {
                    let loc = iloc
                        .item_locations
                        .iter()
                        .find(|l| l.item_id == entry.item_id)?;
                    if loc.construction_method != 0 {
                        return None;
                    }
                    let extent = loc.extents.first()?;
                    let start = loc.base_offset.saturating_add(extent.offset) as usize;
                    let end = start + extent.length as usize;
                    Some((entry.item_id, bytes.get(start..end)?.to_vec()))
                })
                .collect();
        }
        Vec::new()
    }

    fn count_xmp_items_in_heic(bytes: &[u8]) -> usize {
        use mp4_atom::{Any, DecodeMaybe, FourCC, Iinf};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            if let Any::Meta(meta) = atom
                && let Some(iinf) = meta.get::<Iinf>()
            {
                return iinf
                    .item_infos
                    .iter()
                    .filter(|e| {
                        e.item_type == Some(FourCC::new(b"mime"))
                            && e.content_type.as_deref() == Some("application/rdf+xml")
                    })
                    .count();
            }
        }
        0
    }

    /// Locate the raw `mdat` box payload bytes in a HEIC file. Used to prove
    /// that the image data didn't change when we modified metadata.
    fn find_mdat_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
        // `mdat` is one of the atoms the `mp4-atom::Any` decoder recognises.
        use mp4_atom::{Any, DecodeMaybe, Encode};
        let mut cursor: &[u8] = bytes;
        while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
            if let Any::Mdat(_) = &atom {
                // Re-encode so the test compares the full box bytes (header + body).
                let mut buf = Vec::new();
                atom.encode(&mut buf).ok()?;
                return Some(buf);
            }
        }
        None
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

    // ── Regression: HEIC writes must work on part-file paths (issue #552) ──
    //
    // The download pipeline writes embedded metadata onto the `<base32>.kei-tmp`
    // part file before the atomic rename to the final `.HEIC` name. Content
    // sniffing must route that extension-shadowed part file to the HEIF writer.

    #[test]
    fn apply_metadata_skips_heif_on_extension_less_part_file() {
        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        // Mimic the download part-file: base32-ish stem with `.kei-tmp`
        // suffix shadowing the real `.heic` extension.
        let path = dir.join("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA.kei-tmp");
        fs::write(&path, SAMPLE_HEIC).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(4),
                title: Some("PartFile".into()),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write must succeed on .kei-tmp part file");
        let meta = read_heif_meta(&fs::read(&path).unwrap());
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 4);
        assert!(
            !temp_path_for(&path, ".meta-tmp").exists(),
            "extension-shadowed HEIC part files must not leave a temp file"
        );
        fs::remove_file(&path).ok();
    }

    // ── Regression: iOS 17+ HEICs with `uri ` infe items (issue #274) ──
    //
    // Apple started embedding `uri ` item-info entries (item_type="uri ",
    // item_uri_type="tag:apple.com,2023:photos/<id>") in HEICs from iOS 17.
    // The mp4-atom 0.10.1 release didn't read the trailing `item_uri_type`
    // cstr, so its strict end-check rejected every such infe entry as
    // `UnderDecode("infe")` — which surfaced to users as the metadata-embed
    // pass failing on every iOS 17+ HEIC. The fix lives on mp4-atom's `main`
    // (PR #123, 2026-01-26); kei pins past that commit. This test pins the
    // round-trip so a future bump that drops the `uri ` decoder regresses
    // visibly rather than reverting silently.

    #[test]
    fn apply_metadata_succeeds_on_heic_with_uri_infe_item() {
        use mp4_atom::{Any, DecodeMaybe, Encode, FourCC, Iinf, Iloc, ItemInfoEntry};

        // Build a HEIC variant that carries a `uri ` infe entry by parsing
        // the sample, injecting a synthetic Apple-style entry into iinf,
        // and re-serializing. This produces bytes byte-shape-identical to
        // what an iOS 17+ camera writes for the failing case.
        let mut atoms: Vec<Any> = Vec::new();
        let mut cursor: &[u8] = SAMPLE_HEIC;
        while let Some(atom) = Any::decode_maybe(&mut cursor).expect("sample HEIC must parse") {
            atoms.push(atom);
        }
        let meta = atoms
            .iter_mut()
            .find_map(|a| if let Any::Meta(m) = a { Some(m) } else { None })
            .expect("sample HEIC has a meta box");
        let iinf = meta
            .get_mut::<Iinf>()
            .expect("sample HEIC has an iinf inside meta");
        iinf.item_infos.push(ItemInfoEntry {
            item_id: 9999,
            item_protection_index: 0,
            item_type: Some(FourCC::new(b"uri ")),
            item_name: "metadata".to_string(),
            content_type: None,
            content_encoding: None,
            item_uri_type: Some("tag:apple.com,2023:photos/UNIT-TEST".to_string()),
            item_not_in_presentation: false,
        });
        let mut bytes: Vec<u8> = Vec::new();
        for atom in &atoms {
            atom.encode(&mut bytes)
                .expect("re-encode of synthetic uri-bearing HEIC must succeed");
        }

        // mp4-atom writes iloc offsets verbatim, so adding the uri entry grows
        // meta and shifts the mdat without repointing the item offsets. Repoint
        // every item at the re-encoded mdat so the fixture stays a valid HEIC
        // whose image data lives after meta, not inside it.
        let mdat_start = {
            let mut pos = 0usize;
            loop {
                let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
                if &bytes[pos + 4..pos + 8] == b"mdat" {
                    break pos as u64;
                }
                pos += size;
            }
        };
        let meta = atoms
            .iter_mut()
            .find_map(|a| if let Any::Meta(m) = a { Some(m) } else { None })
            .expect("sample HEIC has a meta box");
        let iloc = meta
            .get_mut::<Iloc>()
            .expect("sample HEIC has an iloc inside meta");
        let min_base = iloc
            .item_locations
            .iter()
            .map(|loc| loc.base_offset)
            .min()
            .expect("iloc has at least one item");
        let shift = (mdat_start + 8) - min_base;
        for loc in &mut iloc.item_locations {
            loc.base_offset += shift;
        }
        let mut bytes: Vec<u8> = Vec::new();
        for atom in &atoms {
            atom.encode(&mut bytes)
                .expect("re-encode of repointed uri-bearing HEIC must succeed");
        }

        let dir = test_tmp_dir("meta_heic_tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("uri_infe.heic");
        fs::write(&path, &bytes).unwrap();

        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(3),
                title: Some("UriItem".into()),
                ..MetadataWrite::default()
            },
        )
        .expect("HEIC metadata write must preserve a `uri ` infe item");

        let rewritten = fs::read(&path).unwrap();
        let meta = read_heif_meta(&rewritten);
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 3);
        assert_eq!(
            find_mdat_bytes(&bytes),
            find_mdat_bytes(&rewritten),
            "HEIC image data must survive a `uri ` item update"
        );
        fs::remove_file(&path).ok();
    }

    #[test]
    fn apply_metadata_dispatches_xmp_toolkit_on_extension_less_jpeg_part_file() {
        // Negative case: extension-less ≠ HEIF. A JPEG-bearing part file
        // must still route to XMP Toolkit and succeed.
        let dir = test_tmp_dir("meta_tests");
        let path = dir.join("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB.kei-tmp");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&path, minimal_jpeg()).unwrap();
        apply_metadata_with_default_suffix(
            &path,
            &MetadataWrite {
                rating: Some(2),
                ..MetadataWrite::default()
            },
        )
        .expect("JPEG metadata write must succeed on .kei-tmp part file");
        let meta = read_meta(&path);
        assert_eq!(
            meta.property(xmp_ns::XMP, "Rating").map(|v| v.value),
            Some("2".to_string()),
        );
        fs::remove_file(&path).ok();
    }

    // ── Sidecar + format-dispatch tests ────────────────────────────────

    fn read_sidecar_meta(media_path: &Path) -> XmpMeta {
        let mut sidecar_path = media_path.as_os_str().to_os_string();
        sidecar_path.push(".xmp");
        fs::read_to_string(PathBuf::from(sidecar_path))
            .unwrap()
            .parse()
            .unwrap()
    }

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
    fn write_sidecar_is_noop_on_empty_write() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("empty.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();
        write_sidecar_with_default_suffix(&media_path, &MetadataWrite::default()).unwrap();
        let sidecar = dir.join("empty.jpg.xmp");
        assert!(
            !sidecar.exists(),
            "empty metadata write must not create a sidecar"
        );
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_creates_xmp_file_next_to_media() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("photo.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let write = MetadataWrite {
            rating: Some(5),
            title: Some("Vacation".to_string()),
            keywords: vec!["beach".into(), "sun".into()],
            people: vec!["Alice".into()],
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).expect("sidecar write");
        let sidecar = dir.join("photo.jpg.xmp");
        assert!(sidecar.exists(), "sidecar should be written next to media");

        let bytes = fs::read(&sidecar).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("rdf:RDF"));
        assert!(s.contains("xmp:Rating"));
        assert!(s.contains("Vacation"));
        assert!(s.contains("beach"));
        assert!(s.contains("Alice"));

        fs::remove_file(&sidecar).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_is_atomic_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("rewrite.jpg");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let first = MetadataWrite {
            title: Some("Before".into()),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &first).unwrap();

        let second = MetadataWrite {
            title: Some("After".into()),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &second).unwrap();

        let sidecar = dir.path().join("rewrite.jpg.xmp");
        let s = fs::read_to_string(&sidecar).unwrap();
        assert!(s.contains("After"), "second write should replace first");
        assert!(
            !s.contains("Before"),
            "previous title must not leak through"
        );

        let retained_temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".kei-xmp-"))
            .collect();
        assert!(
            retained_temps.is_empty(),
            "successful writes must remove their unique temp files: {retained_temps:?}"
        );
    }

    #[tokio::test]
    async fn write_sidecar_refuses_existing_file_changed_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("changed.jpg");
        let sidecar_path = dir.path().join("changed.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        ensure_initialized();
        let mut original = XmpMeta::new().unwrap();
        original
            .set_property(
                xmp_ns::DC,
                "creator",
                &XmpValue::new("Initial author".to_string()),
            )
            .unwrap();
        std::fs::write(&sidecar_path, original.to_string().into_bytes()).unwrap();

        let prepared = prepare_sidecar_write(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .unwrap()
        .expect("sidecar update should be prepared");
        let temp_path = prepared.tmp_path.clone();

        let external = b"external replacement after initial read";
        std::fs::write(&sidecar_path, external).unwrap();
        let error = crate::download::file::publish_file_if_unchanged(
            &prepared.tmp_path,
            &prepared.sidecar_path,
            prepared.expected,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("bytes changed"), "{error:#}");
        assert_eq!(
            std::fs::read(&sidecar_path).unwrap(),
            external,
            "an external change after the initial read must not be replaced"
        );
        assert!(
            temp_path.exists(),
            "refused publication retains the prepared file because an exchange failure could have displaced user bytes there"
        );
    }

    #[tokio::test]
    async fn write_sidecar_refuses_file_created_after_missing_read() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("appeared.jpg");
        let sidecar_path = dir.path().join("appeared.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let prepared = prepare_sidecar_write(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .unwrap()
        .expect("new sidecar should be prepared");
        let temp_path = prepared.tmp_path.clone();

        let external = b"sidecar created by another application";
        std::fs::write(&sidecar_path, external).unwrap();
        let error = crate::download::file::publish_file_if_unchanged(
            &prepared.tmp_path,
            &prepared.sidecar_path,
            prepared.expected,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("appeared"), "{error:#}");
        assert_eq!(std::fs::read(&sidecar_path).unwrap(), external);
        assert!(
            temp_path.exists(),
            "refused publication retains the prepared file for operator inspection"
        );
    }

    #[tokio::test]
    async fn write_sidecar_retries_after_retained_conflict_temp() {
        let dir = tempfile::tempdir().unwrap();
        let media_path = dir.path().join("temp-conflict.jpg");
        let sidecar_path = dir.path().join("temp-conflict.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        let prepared = prepare_sidecar_write(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .unwrap()
        .expect("initial sidecar should be prepared");
        let retained_path = prepared.tmp_path.clone();
        let retained_bytes = std::fs::read(&retained_path).unwrap();

        ensure_initialized();
        let mut external = XmpMeta::new().unwrap();
        external
            .set_property(
                xmp_ns::DC,
                "creator",
                &XmpValue::new("External author".to_string()),
            )
            .unwrap();
        std::fs::write(&sidecar_path, external.to_string().into_bytes()).unwrap();
        crate::download::file::publish_file_if_unchanged(
            &prepared.tmp_path,
            &prepared.sidecar_path,
            prepared.expected,
        )
        .await
        .unwrap_err();

        super::write_sidecar(
            &media_path,
            &MetadataWrite {
                rating: Some(4),
                ..MetadataWrite::default()
            },
            ".meta-tmp",
        )
        .await
        .expect("a retained conflict file must not block a later attempt");

        assert_eq!(
            std::fs::read(&retained_path).unwrap(),
            retained_bytes,
            "the later attempt must not overwrite or delete ambiguous retained bytes"
        );
        let written = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(written.contains("External author"), "{written}");
        assert!(written.contains("Rating"), "{written}");
    }

    #[test]
    fn write_sidecar_clears_only_fields_previously_owned_by_kei() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("owned.jpg");
        let sidecar_path = dir.join("owned.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        const THIRD_PARTY_NS: &str = "https://example.invalid/xmp/third-party/";
        ensure_initialized();
        XmpMeta::register_namespace(THIRD_PARTY_NS, "thirdParty").unwrap();
        let mut seed = XmpMeta::new().unwrap();
        seed.set_property(
            xmp_ns::DC,
            "creator",
            &XmpValue::new("User-Photographer".to_string()),
        )
        .unwrap();
        seed.set_property(
            THIRD_PARTY_NS,
            "developSettings",
            &XmpValue::new("opaque-user-data".to_string()),
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        let first = MetadataWrite {
            datetime: Some("2024:06:15 10:00:00".into()),
            offset_time_original: Some("+10:00".into()),
            clear_datetime_offsets: false,
            require_native_heif_capture_time: false,
            gps_datetime: Some("2024-06-15T10:00:01Z".into()),
            gps_speed: Some(xmp_rational(25, 2)),
            gps_speed_ref: Some("K".into()),
            gps_h_positioning_error: Some(xmp_rational(13, 4)),
            preserve_source_gps: false,
            rating: Some(5),
            gps: Some(GpsCoords {
                latitude: 37.7,
                longitude: -122.4,
                altitude: Some(10.0),
            }),
            title: Some("Owned title".into()),
            description: Some("Owned description".into()),
            keywords: vec!["vacation".into()],
            people: vec!["Alice".into()],
            is_hidden: true,
            is_archived: true,
            media_subtype: Some("portrait".into()),
            burst_id: Some("burst-1".into()),
        };
        write_sidecar_with_default_suffix(&media_path, &first).unwrap();

        let cleared = MetadataWrite {
            datetime: first.datetime.clone(),
            gps: Some(GpsCoords {
                latitude: 37.7,
                longitude: -122.4,
                altitude: None,
            }),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &cleared).unwrap();

        let meta = read_sidecar_meta(&media_path);
        assert!(meta.contains_property(xmp_ns::XMP, "CreateDate"));
        assert!(meta.contains_property(xmp_ns::EXIF, "GPSLatitude"));
        for (namespace, property) in [
            (xmp_ns::XMP, "Rating"),
            (EXIF_EX_XMP_NS, "OffsetTimeOriginal"),
            (xmp_ns::EXIF, "GPSTimeStamp"),
            (xmp_ns::EXIF, "GPSSpeed"),
            (xmp_ns::EXIF, "GPSSpeedRef"),
            (xmp_ns::EXIF, "GPSHPositioningError"),
            (xmp_ns::EXIF, "GPSAltitude"),
            (xmp_ns::EXIF, "GPSAltitudeRef"),
            (xmp_ns::DC, "subject"),
            (xmp_ns::IPTC_EXT, "PersonInImage"),
            (KEI_XMP_NS, "hidden"),
            (KEI_XMP_NS, "archived"),
            (KEI_XMP_NS, "mediaSubtype"),
            (KEI_XMP_NS, "burstId"),
        ] {
            assert!(
                !meta.contains_property(namespace, property),
                "cleared kei-owned property must be removed: {property}"
            );
        }
        for property in ["title", "description"] {
            let path = XmpMeta::compose_lang_selector(xmp_ns::DC, property, "x-default").unwrap();
            assert!(
                !meta.contains_property(xmp_ns::DC, &path),
                "cleared kei-owned localized property must be removed: {property}"
            );
        }
        let managed = meta.property(KEI_XMP_NS, KEI_MANAGED_FIELDS).unwrap().value;
        assert!(managed.contains("exif:GPSLatitude"));
        assert!(!managed.contains("xmp:Rating"));
        assert!(!managed.contains("exifEX:OffsetTimeOriginal"));
        assert!(!managed.contains("exif:GPSTimeStamp"));
        assert!(!managed.contains("exif:GPSSpeed"));
        assert!(!managed.contains("exif:GPSSpeedRef"));
        assert!(!managed.contains("exif:GPSHPositioningError"));
        assert!(!managed.contains("exif:GPSAltitude"));
        assert_eq!(
            meta.property(THIRD_PARTY_NS, "developSettings")
                .unwrap()
                .value,
            "opaque-user-data"
        );
        assert!(
            std::fs::read_to_string(&sidecar_path)
                .unwrap()
                .contains("User-Photographer"),
            "clearing kei-owned fields must preserve dc:creator"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_unmarked_standard_fields_when_source_is_empty() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("legacy.jpg");
        let sidecar_path = dir.join("legacy.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        ensure_initialized();
        let mut seed = XmpMeta::new().unwrap();
        seed.set_property_i32(xmp_ns::XMP, "Rating", &XmpValue::new(5))
            .unwrap();
        seed.set_property(
            xmp_ns::EXIF,
            "GPSTimeStamp",
            &XmpValue::new("2024-06-15T10:00:01Z".to_string()),
        )
        .unwrap();
        seed.set_property(xmp_ns::EXIF, "GPSSpeed", &XmpValue::new("25/2".to_string()))
            .unwrap();
        seed.set_property(xmp_ns::EXIF, "GPSSpeedRef", &XmpValue::new("K".to_string()))
            .unwrap();
        seed.set_property(
            xmp_ns::EXIF,
            "GPSHPositioningError",
            &XmpValue::new("13/4".to_string()),
        )
        .unwrap();
        seed.set_localized_text(
            xmp_ns::DC,
            "description",
            None,
            "x-default",
            "User description",
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        write_sidecar_with_default_suffix(
            &media_path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".into()),
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        let meta = read_sidecar_meta(&media_path);
        assert_eq!(
            meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value,
            5,
            "an unmarked rating may belong to another application"
        );
        for property in ["GPSTimeStamp", "GPSSpeed", "GPSSpeedRef"] {
            assert!(
                meta.contains_property(xmp_ns::EXIF, property),
                "an unmarked GPS property may belong to another application: {property}"
            );
        }
        assert!(
            meta.property(xmp_ns::EXIF, "GPSHPositioningError")
                .is_some_and(|property| property.value == "13/4"),
            "an unmarked GPS property may belong to another application: GPSHPositioningError"
        );
        assert_eq!(
            meta.localized_text(xmp_ns::DC, "description", None, "x-default")
                .unwrap()
                .0
                .value,
            "User description",
            "an unmarked description may belong to the user"
        );
        let managed = meta.property(KEI_XMP_NS, KEI_MANAGED_FIELDS).unwrap().value;
        assert!(!managed.contains("xmp:Rating"));
        assert!(!managed.contains("dc:description"));
        assert!(!managed.contains("exif:GPSTimeStamp"));
        assert!(!managed.contains("exif:GPSSpeed"));
        assert!(!managed.contains("exif:GPSSpeedRef"));
        assert!(!managed.contains("exif:GPSHPositioningError"));

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_owned_source_gps_when_source_state_is_unknown() {
        let dir = test_tmp_dir("sidecar_preserve_unknown_source_gps");
        fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("unknown-source.jpg");
        let sidecar_path = dir.join("unknown-source.jpg.xmp");
        fs::write(&media_path, b"placeholder").unwrap();

        write_sidecar_with_default_suffix(
            &media_path,
            &MetadataWrite {
                gps_datetime: Some("2024-06-15T10:00:01Z".into()),
                gps_speed: Some(xmp_rational(25, 2)),
                gps_speed_ref: Some("K".into()),
                gps_h_positioning_error: Some(xmp_rational(13, 4)),
                ..MetadataWrite::default()
            },
        )
        .unwrap();
        write_sidecar_with_default_suffix(
            &media_path,
            &MetadataWrite {
                rating: Some(5),
                preserve_source_gps: true,
                ..MetadataWrite::default()
            },
        )
        .unwrap();

        let meta = read_sidecar_meta(&media_path);
        for property in ["GPSTimeStamp", "GPSSpeed", "GPSSpeedRef"] {
            assert!(
                meta.contains_property(xmp_ns::EXIF, property),
                "unknown source state must preserve {property}"
            );
        }
        assert!(
            meta.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            "unknown source state must preserve GPSHPositioningError"
        );
        let managed = meta.property(KEI_XMP_NS, KEI_MANAGED_FIELDS).unwrap().value;
        for token in [
            "exif:GPSTimeStamp",
            "exif:GPSSpeed",
            "exif:GPSSpeedRef",
            "exif:GPSHPositioningError",
        ] {
            assert!(
                managed.split(',').any(|value| value == token),
                "unknown source state must preserve {token}"
            );
        }
        assert_eq!(meta.property_i32(xmp_ns::XMP, "Rating").unwrap().value, 5);

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_existing_user_fields() {
        // A third-party tool (Darktable, digiKam) wrote a sidecar with
        // dc:creator before kei ever ran. On our write, the creator must
        // survive; kei's rating / keywords layer on top.
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("merge.jpg");
        let sidecar_path = dir.join("merge.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        // Seed an existing sidecar that carries a dc:creator we must keep.
        ensure_initialized();
        let mut seed = XmpMeta::new().unwrap();
        seed.set_property(
            xmp_toolkit::xmp_ns::DC,
            "creator",
            &xmp_toolkit::XmpValue::new("User-Photographer".to_string()),
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        // kei writes its own rating on top.
        let write = MetadataWrite {
            rating: Some(4),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).expect("sidecar merge");

        let merged = fs::read_to_string(&sidecar_path).unwrap();
        assert!(
            merged.contains("User-Photographer"),
            "existing user-authored dc:creator must survive kei's write: {merged}"
        );
        assert!(
            merged.contains("Rating") || merged.contains("rating"),
            "kei's rating must be applied on top: {merged}"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    /// Third-party tools (Darktable, digiKam, Lightroom) attach custom
    /// namespaces to their sidecars. Kei's merge must preserve those too,
    /// not just well-known dc: properties.
    #[test]
    fn write_sidecar_preserves_non_dc_namespaces() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("darktable.jpg");
        let sidecar_path = dir.join("darktable.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();

        ensure_initialized();
        const DARKTABLE_NS: &str = "http://darktable.sf.net/";
        XmpMeta::register_namespace(DARKTABLE_NS, "darktable").unwrap();

        let mut seed = XmpMeta::new().unwrap();
        seed.set_property(DARKTABLE_NS, "history_end", &XmpValue::new("7".to_string()))
            .unwrap();
        seed.set_property(DARKTABLE_NS, "xmp_version", &XmpValue::new("5".to_string()))
            .unwrap();
        // Third-party develop-settings-style blob under a non-dc namespace.
        seed.set_property(
            DARKTABLE_NS,
            "raw_params",
            &XmpValue::new("gc5ghbmY2k8...opaque...".to_string()),
        )
        .unwrap();
        std::fs::write(&sidecar_path, seed.to_string().into_bytes()).unwrap();

        let write = MetadataWrite {
            rating: Some(3),
            keywords: vec!["vacation".to_string()],
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).expect("sidecar merge");

        let merged = fs::read_to_string(&sidecar_path).unwrap();
        for expected in ["history_end", "xmp_version", "raw_params", "gc5ghbmY2k8"] {
            assert!(
                merged.contains(expected),
                "darktable field `{expected}` must survive kei's merge: {merged}"
            );
        }
        assert!(
            merged.contains("Rating") || merged.contains("rating"),
            "kei's rating must be applied on top: {merged}"
        );
        assert!(
            merged.contains("vacation"),
            "kei's keyword must be applied on top: {merged}"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn write_sidecar_preserves_unparsable_existing() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("garbage.jpg");
        let sidecar_path = dir.join("garbage.jpg.xmp");
        std::fs::write(&media_path, b"placeholder").unwrap();
        let original = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF";
        std::fs::write(&sidecar_path, original).unwrap();

        let write = MetadataWrite {
            title: Some("Clean".into()),
            ..MetadataWrite::default()
        };
        let error = write_sidecar_with_default_suffix(&media_path, &write).unwrap_err();

        assert!(
            format!("{error:#}").contains("Could not parse existing XMP sidecar"),
            "{error:#}"
        );
        assert_eq!(
            fs::read(&sidecar_path).unwrap(),
            original,
            "unparsable third-party bytes must remain unchanged"
        );

        fs::remove_file(&sidecar_path).ok();
        fs::remove_file(&media_path).ok();
    }

    #[test]
    fn parse_existing_sidecar_accepts_bare_rdf_with_properties() {
        let bare_rdf = r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
<rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:format>image/jpeg</dc:format>
</rdf:Description>
</rdf:RDF>"#;

        let parsed = parse_existing_sidecar(bare_rdf).unwrap();
        assert_eq!(
            parsed.property(xmp_ns::DC, "format").unwrap().value,
            "image/jpeg"
        );
    }

    #[test]
    fn write_sidecar_does_not_touch_media_file() {
        let dir = test_tmp_dir("sidecar_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let media_path = dir.join("untouched.jpg");
        let original_bytes = b"opaque-bytes-dont-care-about-format";
        std::fs::write(&media_path, original_bytes).unwrap();

        let write = MetadataWrite {
            rating: Some(3),
            ..MetadataWrite::default()
        };
        write_sidecar_with_default_suffix(&media_path, &write).unwrap();

        let after = fs::read(&media_path).unwrap();
        assert_eq!(
            after,
            original_bytes.to_vec(),
            "sidecar write must never alter the media file"
        );

        let sidecar = dir.join("untouched.jpg.xmp");
        fs::remove_file(&sidecar).ok();
        fs::remove_file(&media_path).ok();
    }
}

#[cfg(all(test, not(feature = "xmp")))]
mod native_tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI).
    fn minimal_jpeg() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    fn fresh_jpeg(dir: &Path, name: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, minimal_jpeg()).unwrap();
        path
    }

    fn first_unknown_int16(metadata: &Metadata, tag_id: u16) -> Option<u16> {
        metadata
            .get_tag(&ExifTag::UnknownINT16U(
                Vec::new(),
                tag_id,
                ExifTagGroup::GENERIC,
            ))
            .next()
            .and_then(|tag| match tag {
                ExifTag::UnknownINT16U(values, tag, _) if *tag == tag_id => values.first().copied(),
                _ => None,
            })
    }

    #[test]
    fn native_apply_metadata_writes_jpeg_exif_without_xmp_feature() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native.jpg");

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                description: Some("Beach day".to_string()),
                gps: Some(GpsCoords {
                    latitude: 37.7749,
                    longitude: -122.4194,
                    altitude: Some(17.0),
                }),
                rating: Some(5),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let probe = probe_exif(&path).unwrap();
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2024:06:15 10:00:00")
        );
        assert_eq!(probe.offset_time_original, None);
        assert!(probe.has_gps);

        let metadata = Metadata::new_from_path(&path).unwrap();
        let description = metadata
            .get_tag(&ExifTag::ImageDescription(String::new()))
            .next()
            .and_then(|tag| match tag {
                ExifTag::ImageDescription(s) => Some(s.as_str()),
                _ => None,
            });
        assert_eq!(description, Some("Beach day"));

        assert_eq!(first_unknown_int16(&metadata, WINDOWS_RATING_TAG), Some(5));
        assert_eq!(
            first_unknown_int16(&metadata, WINDOWS_RATING_PERCENT_TAG),
            Some(99)
        );
    }

    #[test]
    fn native_metadata_rewrite_refuses_concurrent_target_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native-concurrent.jpg");
        let external = b"external native edit";

        let prepared = prepare_metadata_native(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
            None,
        )
        .unwrap();
        let result = prepared.publish_with(&path, |src, dst, expected, expected_replacement| {
            fs::write(dst, external)?;
            publish_prepared_embed(src, dst, expected, expected_replacement)
        });

        let error = result.expect_err("a concurrent edit must block native EXIF publication");
        assert!(crate::download::file::classify_conditional_publish_error(&error).target_changed);
        assert_eq!(fs::read(&path).unwrap(), external);
    }

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

    #[test]
    fn native_write_pairs_every_datetime_with_its_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native-offset.jpg");

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2026:02:01 09:31:59".to_string()),
                offset_time_original: Some("+11:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let probe = probe_exif(&path).unwrap();
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
        assert!(probe.has_other_datetime_offset);

        let metadata = Metadata::new_from_path(&path).unwrap();
        for template in [
            ExifTag::OffsetTimeOriginal(String::new()),
            ExifTag::OffsetTimeDigitized(String::new()),
            ExifTag::OffsetTime(String::new()),
        ] {
            let written = metadata
                .get_tag(&template)
                .next()
                .and_then(|tag| match tag {
                    ExifTag::OffsetTimeOriginal(s)
                    | ExifTag::OffsetTimeDigitized(s)
                    | ExifTag::OffsetTime(s) => Some(s.as_str()),
                    _ => None,
                });
            assert_eq!(
                written,
                Some("+11:00"),
                "EXIF tag 0x{:04x} must carry the capture offset",
                template.as_u16()
            );
        }

        // Without a timestamp write, CreateDate and ModifyDate keep whatever
        // the file already held, so only the verified DateTimeOriginal gets an
        // offset.
        let lone = fresh_jpeg(dir.path(), "native-lone-offset.jpg");
        apply_metadata(
            &lone,
            &MetadataWrite {
                offset_time_original: Some("+11:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let metadata = Metadata::new_from_path(&lone).unwrap();
        assert!(
            metadata
                .get_tag(&ExifTag::OffsetTimeOriginal(String::new()))
                .next()
                .is_some()
        );
        for template in [
            ExifTag::OffsetTimeDigitized(String::new()),
            ExifTag::OffsetTime(String::new()),
        ] {
            assert!(
                metadata.get_tag(&template).next().is_none(),
                "EXIF tag 0x{:04x} must not claim an offset for a timestamp this pass did not write",
                template.as_u16()
            );
        }
    }

    /// A camera can leave offset tags behind without the capture timestamp
    /// they qualified. Writing a resolved timestamp over that state has to
    /// drop them first, or the file ends up claiming a zone that describes
    /// nothing in it.
    #[test]
    fn native_write_clears_offsets_orphaned_from_a_replaced_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native-orphaned-offset.jpg");

        let mut seed = Metadata::new();
        seed.set_tag(ExifTag::OffsetTimeOriginal("+02:00".to_string()));
        seed.set_tag(ExifTag::OffsetTimeDigitized("+02:00".to_string()));
        seed.set_tag(ExifTag::OffsetTime("+02:00".to_string()));
        seed.write_to_file(&path).unwrap();

        let seeded = probe_exif(&path).unwrap();
        assert_eq!(seeded.datetime_original, None);
        assert!(seeded.has_any_datetime_offset());

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2026:02:01 09:31:59".to_string()),
                clear_datetime_offsets: true,
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        let probe = probe_exif(&path).unwrap();
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2026:02:01 09:31:59")
        );
        assert!(
            !probe.has_any_datetime_offset(),
            "a stale offset must not survive to qualify the replacement timestamp"
        );

        let metadata = Metadata::new_from_path(&path).unwrap();
        for template in [
            ExifTag::OffsetTimeOriginal(String::new()),
            ExifTag::OffsetTimeDigitized(String::new()),
            ExifTag::OffsetTime(String::new()),
        ] {
            assert!(
                metadata.get_tag(&template).next().is_none(),
                "EXIF tag 0x{:04x} must be cleared",
                template.as_u16()
            );
        }
    }

    #[test]
    fn native_apply_metadata_uses_configured_temp_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = fresh_jpeg(dir.path(), "native_suffix.jpg");
        let default_tmp = dir.path().join("native_suffix.jpg.meta-tmp");
        let configured_tmp = dir.path().join("native_suffix.jpg.kei-tmp");
        fs::write(&default_tmp, b"sentinel").unwrap();

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        assert_eq!(
            fs::read(&default_tmp).unwrap(),
            b"sentinel",
            "native metadata rewrite must not use the old .meta-tmp suffix"
        );
        assert!(
            !configured_tmp.exists(),
            "configured native metadata temp path must be installed or cleaned up"
        );
    }

    #[test]
    fn native_apply_metadata_skips_heic_without_xmp_feature() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.heic");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        fs::write(&path, &bytes).unwrap();

        apply_metadata(
            &path,
            &MetadataWrite {
                datetime: Some("2024:06:15 10:00:00".to_string()),
                ..MetadataWrite::default()
            },
            ".kei-tmp",
        )
        .unwrap();

        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "HEIC should be skipped unchanged when XMP support is disabled"
        );
        assert!(!dir.path().join("image.heic.kei-tmp").exists());
    }

    #[test]
    fn native_tmp_guard_cleans_configured_temp_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guarded.kei-tmp");
        fs::write(&path, b"pending").unwrap();
        {
            let _guard = TmpGuard::new(&path);
            assert!(path.exists(), "precondition: tmp file exists");
        }
        assert!(
            !path.exists(),
            "TmpGuard Drop must remove configured metadata temp files"
        );
    }
}
