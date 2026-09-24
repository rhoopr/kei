//! Bounded source EXIF GPS decoding.

use super::values::{SourceGpsMetadata, XmpRational};
use crate::download::heif;
use anyhow::{Context, Result};
use little_exif::filetype::FileExtension;
use std::path::Path;

pub(crate) fn read_source_gps(path: &Path) -> Result<SourceGpsMetadata> {
    let mut source = std::fs::File::open(path)
        .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?;
    read_source_gps_from_file(&mut source, path)
}

pub(in crate::download) fn read_source_gps_from_file(
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

#[derive(Clone, Copy)]
enum TiffEndian {
    Little,
    Big,
}

#[derive(Debug)]
enum TiffGpsError {
    Io(std::io::Error),
    Malformed,
}

impl From<std::io::Error> for TiffGpsError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

struct TiffGpsReader<'a, R> {
    source: &'a mut R,
    base: u64,
    len: u64,
    endian: TiffEndian,
}

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
            tracing::warn!(target: "kei::download::metadata",
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
            tracing::warn!(target: "kei::download::metadata", "Source EXIF GPSSpeedRef is missing or unsupported");
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

enum GpsAxis {
    Latitude,
    Longitude,
}

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

fn read_tiff_ifd_count<R: std::io::Read + std::io::Seek>(
    reader: &mut TiffGpsReader<'_, R>,
    offset: u64,
) -> Result<u16, TiffGpsError> {
    let mut count = [0_u8; 2];
    reader.read_exact_at(offset, &mut count)?;
    Ok(tiff_u16(reader.endian, count))
}

const fn tiff_u16(endian: TiffEndian, bytes: [u8; 2]) -> u16 {
    match endian {
        TiffEndian::Little => u16::from_le_bytes(bytes),
        TiffEndian::Big => u16::from_be_bytes(bytes),
    }
}

const fn tiff_u32(endian: TiffEndian, bytes: [u8; 4]) -> u32 {
    match endian {
        TiffEndian::Little => u32::from_le_bytes(bytes),
        TiffEndian::Big => u32::from_be_bytes(bytes),
    }
}

fn tiff_rational(endian: TiffEndian, bytes: &[u8; 8]) -> Option<XmpRational> {
    let numerator = tiff_u32(endian, [bytes[0], bytes[1], bytes[2], bytes[3]]);
    let denominator = tiff_u32(endian, [bytes[4], bytes[5], bytes[6], bytes[7]]);
    (denominator != 0).then_some(XmpRational {
        numerator,
        denominator,
    })
}

fn tiff_rational_triplet(endian: TiffEndian, bytes: &[u8; 24]) -> Option<[XmpRational; 3]> {
    Some([
        tiff_rational(endian, bytes[0..8].try_into().ok()?)?,
        tiff_rational(endian, bytes[8..16].try_into().ok()?)?,
        tiff_rational(endian, bytes[16..24].try_into().ok()?)?,
    ])
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "validated GPS hour, minute, and second ranges make these conversions bounded and non-negative"
)]
fn source_gps_datetime_values(date: &str, time: &[XmpRational; 3]) -> Option<String> {
    let date = chrono::NaiveDate::parse_from_str(date, "%Y:%m:%d")
        .ok()
        .or_else(|| {
            tracing::warn!(target: "kei::download::metadata", "Source EXIF GPSDateStamp is malformed");
            None
        })?;
    let [hour_value, minute_value, seconds_value] = time;
    let hour = hour_value.as_f64();
    let minute = minute_value.as_f64();
    let seconds = seconds_value.as_f64();
    if !hour.is_finite() || !minute.is_finite() || !seconds.is_finite() {
        tracing::warn!(target: "kei::download::metadata", "Source EXIF GPSTimeStamp is malformed");
        return None;
    }
    if hour.fract() != 0.0
        || minute.fract() != 0.0
        || !(0.0..=23.0).contains(&hour)
        || !(0.0..=59.0).contains(&minute)
        || !(0.0..60.0).contains(&seconds)
    {
        tracing::warn!(target: "kei::download::metadata", "Source EXIF GPSTimeStamp is malformed");
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

#[cfg(test)]
#[allow(clippy::unused_result_ok, reason = "test cleanup is best-effort")]
mod tests {
    use super::super::test_support::minimal_jpeg;
    use super::super::values::{MetadataWrite, SourceGpsMetadata, XmpRational};
    #[cfg(test)]
    use super::super::xmp_fields::build_xmp_packet;
    use super::{TiffGpsError, read_source_gps, read_tiff_source_gps};
    use crate::download::heif;
    use crate::test_helpers::{
        SOURCE_GPS_DATETIME, SOURCE_GPS_H_POSITIONING_ERROR, SOURCE_GPS_SPEED,
        SOURCE_GPS_SPEED_REF, exif_with_source_gps, heif_ftyp_without_meta_bytes,
        minimal_big_endian_tiff_with_source_gps, minimal_jpeg_with_source_gps,
        minimal_tiff_with_source_gps, source_gps_without_time_stamp, ur64,
    };
    use little_exif::exif_tag::ExifTag;
    use little_exif::filetype::FileExtension;
    use little_exif::metadata::Metadata;
    use little_exif::rational::uR64;
    use std::path::Path;
    use xmp_toolkit::{XmpMeta, xmp_ns};

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
}
