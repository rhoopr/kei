//! Bounded source EXIF and QuickTime GPS decoding.

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
    read_source_gps_from_file_inner(source, path, true)
}

fn read_source_gps_from_file_inner(
    source: &mut std::fs::File,
    path: &Path,
    allow_companion_lookup: bool,
) -> Result<SourceGpsMetadata> {
    use std::io::Seek;
    source.rewind()?;
    let mut head = [0_u8; 12];
    let bytes_read = std::io::Read::read(source, &mut head)
        .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?;
    let prefix = head.get(..bytes_read).unwrap_or_default();
    if is_movie_content(prefix) {
        let source_len = source
            .metadata()
            .with_context(|| format!("Could not read {} for source GPS metadata", path.display()))?
            .len();
        let mut metadata = match read_movie_source_gps(source, source_len) {
            Ok(metadata) => metadata,
            Err(TiffGpsError::Malformed) => SourceGpsMetadata::default(),
            Err(TiffGpsError::Io(error)) => {
                return Err(error).with_context(|| {
                    format!("Could not read {} for source GPS metadata", path.display())
                });
            }
        };
        if allow_companion_lookup && (metadata.datetime.is_none() || metadata.speed.is_none()) {
            merge_companion_still_source_gps(&mut metadata, path);
        }
        return Ok(metadata);
    }
    let Some(file_type) = source_file_type(prefix) else {
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
            Err(heif::HeifExifError::ResourceUnsupported(error)) => {
                return Err(error.into());
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

fn is_movie_content(bytes: &[u8]) -> bool {
    !heif::is_heif_content(bytes)
        && bytes.get(4..8).is_some_and(|kind| {
            matches!(
                kind,
                b"ftyp" | b"moov" | b"mdat" | b"wide" | b"free" | b"skip"
            )
        })
}

fn strip_ascii_case_suffix<'a>(value: &'a str, suffix: &str) -> Option<&'a str> {
    let start = value.len().checked_sub(suffix.len())?;
    let tail = value.get(start..)?;
    tail.eq_ignore_ascii_case(suffix)
        .then(|| value.get(..start))
        .flatten()
        .filter(|base| !base.is_empty())
}

fn merge_companion_still_source_gps(metadata: &mut SourceGpsMetadata, movie_path: &Path) {
    if metadata.latitude.is_none() || metadata.longitude.is_none() {
        return;
    }
    let Some(parent) = movie_path.parent() else {
        return;
    };
    let Some(stem) = movie_path.file_stem().and_then(|s| s.to_str()) else {
        return;
    };
    let mut candidate_stems = Vec::with_capacity(3);
    candidate_stems.push(stem);
    for suffix in ["_HEVC", "_edited"] {
        if let Some(base) = strip_ascii_case_suffix(stem, suffix)
            && !candidate_stems.contains(&base)
        {
            candidate_stems.push(base);
        }
    }
    const STILL_EXTENSIONS: [&str; 12] = [
        "HEIC", "heic", "HEIF", "heif", "JPG", "jpg", "JPEG", "jpeg", "DNG", "dng", "PNG", "png",
    ];
    for candidate_stem in candidate_stems {
        for ext in STILL_EXTENSIONS {
            let candidate = parent.join(format!("{candidate_stem}.{ext}"));
            if candidate == movie_path {
                continue;
            }
            if !std::fs::symlink_metadata(&candidate).is_ok_and(|m| m.file_type().is_file()) {
                continue;
            }
            let Ok(mut file) = std::fs::File::open(&candidate) else {
                continue;
            };
            let Ok(still_gps) = read_source_gps_from_file_inner(&mut file, &candidate, false)
            else {
                continue;
            };
            if (still_gps.datetime.is_none() && still_gps.speed.is_none())
                || !metadata.matches_provider_coordinates(still_gps.latitude, still_gps.longitude)
            {
                continue;
            }
            if metadata.datetime.is_none() {
                metadata.datetime = still_gps.datetime;
            }
            if metadata.speed.is_none() {
                metadata.speed = still_gps.speed;
                metadata.speed_ref = still_gps.speed_ref;
            }
            return;
        }
    }
}

fn read_box_header<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    source_len: u64,
    offset: u64,
    end: u64,
    budget: &mut usize,
) -> Result<(u64, u64, [u8; 4]), TiffGpsError> {
    if *budget == 0 || end > source_len || end.saturating_sub(offset) < 8 {
        return Err(TiffGpsError::Malformed);
    }
    *budget -= 1;
    let mut header = [0_u8; 8];
    read_exact_file_range(source, source_len, offset, &mut header)?;
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let kind = [header[4], header[5], header[6], header[7]];
    let (size, header_size) = match size32 {
        0 => (end - offset, 8_u64),
        1 => {
            if end.saturating_sub(offset) < 16 {
                return Err(TiffGpsError::Malformed);
            }
            let mut ext = [0_u8; 8];
            read_exact_file_range(source, source_len, offset + 8, &mut ext)?;
            (u64::from_be_bytes(ext), 16_u64)
        }
        2..=7 => return Err(TiffGpsError::Malformed),
        n => (u64::from(n), 8_u64),
    };
    if size < header_size || offset.checked_add(size).is_none_or(|next| next > end) {
        return Err(TiffGpsError::Malformed);
    }
    Ok((size, header_size, kind))
}

fn read_movie_source_gps<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    source_len: u64,
) -> Result<SourceGpsMetadata, TiffGpsError> {
    let mut budget = 4096_usize;
    let mut offset = 0_u64;
    while offset < source_len {
        let (size, header_size, kind) =
            read_box_header(source, source_len, offset, source_len, &mut budget)?;
        let next = offset + size;
        if &kind == b"moov" {
            let mut moov_offset = offset + header_size;
            while moov_offset < next {
                let (child_size, child_hdr, child_kind) =
                    read_box_header(source, source_len, moov_offset, next, &mut budget)?;
                let child_next = moov_offset + child_size;
                if &child_kind == b"meta" {
                    let metadata = read_mdta_meta_box(
                        source,
                        source_len,
                        moov_offset + child_hdr,
                        child_next,
                        &mut budget,
                    )?;
                    if metadata != SourceGpsMetadata::default() {
                        return Ok(metadata);
                    }
                }
                moov_offset = child_next;
            }
        }
        offset = next;
    }
    Ok(SourceGpsMetadata::default())
}

fn read_mdta_meta_box<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    source_len: u64,
    start: u64,
    end: u64,
    budget: &mut usize,
) -> Result<SourceGpsMetadata, TiffGpsError> {
    let mut offset = start;
    if end.saturating_sub(start) >= 12 {
        let mut probe = [0_u8; 12];
        read_exact_file_range(source, source_len, start, &mut probe)?;
        if &probe[0..4] == b"\0\0\0\0"
            && matches!(&probe[8..12], b"hdlr" | b"keys" | b"ilst" | b"free")
        {
            offset = start + 4;
        }
    }
    let mut is_mdta = false;
    let mut keys_range = None;
    let mut ilst_range = None;
    while offset < end {
        let (size, header_size, kind) = read_box_header(source, source_len, offset, end, budget)?;
        let body_start = offset + header_size;
        let next = offset + size;
        match &kind {
            b"hdlr" if next.saturating_sub(body_start) >= 12 => {
                let mut hdlr = [0_u8; 12];
                read_exact_file_range(source, source_len, body_start, &mut hdlr)?;
                if &hdlr[8..12] == b"mdta" {
                    is_mdta = true;
                }
            }
            b"keys" if keys_range.is_none() => keys_range = Some((body_start, next)),
            b"ilst" if ilst_range.is_none() => ilst_range = Some((body_start, next)),
            _ => {}
        }
        offset = next;
    }
    if !is_mdta {
        return Ok(SourceGpsMetadata::default());
    }
    let (Some((keys_start, keys_end)), Some((ilst_start, ilst_end))) = (keys_range, ilst_range)
    else {
        return Ok(SourceGpsMetadata::default());
    };
    if keys_end.saturating_sub(keys_start) < 8 {
        return Err(TiffGpsError::Malformed);
    }
    let mut keys_hdr = [0_u8; 8];
    read_exact_file_range(source, source_len, keys_start, &mut keys_hdr)?;
    if &keys_hdr[0..4] != b"\0\0\0\0" {
        return Err(TiffGpsError::Malformed);
    }
    let count = u32::from_be_bytes([keys_hdr[4], keys_hdr[5], keys_hdr[6], keys_hdr[7]]);
    if count > 4096 {
        return Err(TiffGpsError::Malformed);
    }
    let mut key_offset = keys_start + 8;
    let mut iso6709_idx = None;
    let mut accuracy_idx = None;
    for index in 1..=count {
        if keys_end.saturating_sub(key_offset) < 8 {
            return Err(TiffGpsError::Malformed);
        }
        let mut entry_hdr = [0_u8; 8];
        read_exact_file_range(source, source_len, key_offset, &mut entry_hdr)?;
        let key_size = u64::from(u32::from_be_bytes([
            entry_hdr[0],
            entry_hdr[1],
            entry_hdr[2],
            entry_hdr[3],
        ]));
        if key_size < 8
            || key_offset
                .checked_add(key_size)
                .is_none_or(|n| n > keys_end)
        {
            return Err(TiffGpsError::Malformed);
        }
        let name_len = usize::try_from(key_size - 8).map_err(|_e| TiffGpsError::Malformed)?;
        let mut name_buf = [0_u8; 128];
        if &entry_hdr[4..8] == b"mdta"
            && let Some(slice) = name_buf.get_mut(..name_len)
        {
            read_exact_file_range(source, source_len, key_offset + 8, slice)?;
            match &*slice {
                b"com.apple.quicktime.location.ISO6709" if iso6709_idx.is_none() => {
                    iso6709_idx = Some(index);
                }
                b"com.apple.quicktime.location.accuracy.horizontal" if accuracy_idx.is_none() => {
                    accuracy_idx = Some(index);
                }
                _ => {}
            }
        }
        key_offset += key_size;
    }
    if iso6709_idx.is_none() && accuracy_idx.is_none() {
        return Ok(SourceGpsMetadata::default());
    }

    let mut coordinates = None;
    let mut horizontal_positioning_error = None;
    let mut item_offset = ilst_start;
    while item_offset < ilst_end {
        let (item_size, item_hdr_size, item_kind) =
            read_box_header(source, source_len, item_offset, ilst_end, budget)?;
        let item_next = item_offset + item_size;
        let item_index = u32::from_be_bytes(item_kind);
        if Some(item_index) == iso6709_idx || Some(item_index) == accuracy_idx {
            let mut data_offset = item_offset + item_hdr_size;
            while data_offset < item_next {
                let (data_size, data_hdr_size, data_kind) =
                    read_box_header(source, source_len, data_offset, item_next, budget)?;
                let data_next = data_offset + data_size;
                let data_body_start = data_offset + data_hdr_size;
                let data_body_len = data_next.saturating_sub(data_body_start);
                if &data_kind == b"data" && (8..=136).contains(&data_body_len) {
                    let mut type_hdr = [0_u8; 8];
                    read_exact_file_range(source, source_len, data_body_start, &mut type_hdr)?;
                    let data_type =
                        u32::from_be_bytes([type_hdr[0], type_hdr[1], type_hdr[2], type_hdr[3]]);
                    let val_len =
                        usize::try_from(data_body_len - 8).map_err(|_e| TiffGpsError::Malformed)?;
                    let mut val_buf = [0_u8; 128];
                    if let Some(val_slice) = val_buf.get_mut(..val_len) {
                        read_exact_file_range(source, source_len, data_body_start + 8, val_slice)?;
                        if (data_type == 1 || data_type == 0)
                            && let Ok(raw_text) = std::str::from_utf8(val_slice)
                        {
                            let text = raw_text.trim_matches('\0').trim();
                            if Some(item_index) == iso6709_idx && coordinates.is_none() {
                                coordinates = parse_iso6709_coordinates(text);
                            } else if Some(item_index) == accuracy_idx
                                && horizontal_positioning_error.is_none()
                            {
                                horizontal_positioning_error = parse_decimal_rational(text);
                            }
                        }
                    }
                }
                data_offset = data_next;
            }
        }
        item_offset = item_next;
    }

    let ((latitude, latitude_tolerance), (longitude, longitude_tolerance)) = match coordinates {
        Some(((lat, lat_tol), (lon, lon_tol))) => ((Some(lat), lat_tol), (Some(lon), lon_tol)),
        None => ((None, 0.0), (None, 0.0)),
    };
    Ok(SourceGpsMetadata {
        latitude,
        longitude,
        latitude_tolerance,
        longitude_tolerance,
        datetime: None,
        speed: None,
        speed_ref: None,
        horizontal_positioning_error,
    })
}

fn parse_iso6709_component(component: &str, axis: GpsAxis) -> Option<(f64, f64)> {
    let bytes = component.as_bytes();
    let (&sign, rest) = bytes.split_first()?;
    if sign != b'+' && sign != b'-' || rest.is_empty() {
        return None;
    }
    let rest_str = std::str::from_utf8(rest).ok()?;
    let (int_part, frac_part) = match rest_str.split_once('.') {
        Some((int_part, frac_part)) => (int_part, Some(frac_part)),
        None => (rest_str, None),
    };
    let max_int_digits = match axis {
        GpsAxis::Latitude => 2,
        GpsAxis::Longitude => 3,
    };
    if int_part.is_empty()
        || int_part.len() > max_int_digits
        || !int_part.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let decimals = match frac_part {
        Some(frac) => {
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            u8::try_from(frac.len()).ok()?
        }
        None => 0,
    };
    let value = component.parse::<f64>().ok()?;
    let maximum = match axis {
        GpsAxis::Latitude => 90.0,
        GpsAxis::Longitude => 180.0,
    };
    if !value.is_finite() || value.abs() > maximum {
        return None;
    }
    let tolerance = if (4..=9).contains(&decimals) {
        0.5 / 10_f64.powi(i32::from(decimals)) + 1e-9
    } else {
        0.0
    };
    Some((value, tolerance))
}

fn parse_iso6709_coordinates(text: &str) -> Option<((f64, f64), (f64, f64))> {
    let trimmed = text.trim_end_matches('/').trim();
    let bytes = trimmed.as_bytes();
    if !bytes.first().is_some_and(|&b| b == b'+' || b == b'-') {
        return None;
    }
    let mut signs = bytes
        .iter()
        .enumerate()
        .skip(1)
        .filter_map(|(idx, &b)| (b == b'+' || b == b'-').then_some(idx));
    let lon_start = signs.next()?;
    let lon_end = signs.next().unwrap_or(trimmed.len());
    let lat_str = trimmed.get(..lon_start)?;
    let lon_str = trimmed.get(lon_start..lon_end)?;
    Some((
        parse_iso6709_component(lat_str, GpsAxis::Latitude)?,
        parse_iso6709_component(lon_str, GpsAxis::Longitude)?,
    ))
}

fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let r = a % b;
        a = b;
        b = r;
    }
    a
}

fn parse_decimal_rational(text: &str) -> Option<XmpRational> {
    let trimmed = text.trim().strip_prefix('+').unwrap_or(text.trim());
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return None;
    }
    let (whole_str, frac_str) = match trimmed.split_once('.') {
        Some((w, f)) => (w, f),
        None => (trimmed, ""),
    };
    if (whole_str.is_empty() && frac_str.is_empty())
        || !whole_str.bytes().all(|b| b.is_ascii_digit())
        || !frac_str.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let capped_frac = frac_str.get(..frac_str.len().min(9))?;
    let frac_digits = u32::try_from(capped_frac.len()).ok()?;
    let denominator = 10_u64.checked_pow(frac_digits)?;
    let whole = if whole_str.is_empty() {
        0_u64
    } else {
        whole_str.parse::<u64>().ok()?
    };
    let frac = if capped_frac.is_empty() {
        0_u64
    } else {
        capped_frac.parse::<u64>().ok()?
    };
    let numerator = whole.checked_mul(denominator)?.checked_add(frac)?;
    if numerator == 0 {
        return None;
    }
    let divisor = gcd_u64(numerator, denominator).max(1);
    Some(XmpRational {
        numerator: u32::try_from(numerator / divisor).ok()?,
        denominator: u32::try_from(denominator / divisor).ok()?,
    })
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
    let (latitude, latitude_tolerance) = match latitude
        .zip(latitude_ref)
        .and_then(|(value, reference)| source_gps_degrees(&value, reference, GpsAxis::Latitude))
    {
        Some((deg, tolerance)) => (Some(deg), tolerance),
        None => (None, 0.0),
    };
    let (longitude, longitude_tolerance) = match longitude
        .zip(longitude_ref)
        .and_then(|(value, reference)| source_gps_degrees(&value, reference, GpsAxis::Longitude))
    {
        Some((deg, tolerance)) => (Some(deg), tolerance),
        None => (None, 0.0),
    };
    Ok(SourceGpsMetadata {
        latitude,
        longitude,
        latitude_tolerance,
        longitude_tolerance,
        datetime,
        speed,
        speed_ref,
        horizontal_positioning_error,
    })
}

#[derive(Clone, Copy)]
enum GpsAxis {
    Latitude,
    Longitude,
}

fn source_gps_degrees(
    values: &[XmpRational; 3],
    reference: [u8; 2],
    axis: GpsAxis,
) -> Option<(f64, f64)> {
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
    let signed = if reference[0] == negative {
        -decimal
    } else {
        decimal
    };
    // iOS EXIF quantizes arcseconds to 1/100" (`denominator >= 100`).
    let tolerance =
        if values[0].denominator == 1 && values[1].denominator == 1 && values[2].denominator >= 100
        {
            0.5 / (f64::from(values[2].denominator) * SECONDS_PER_DEGREE) + 1e-12
        } else {
            0.0
        };
    Some((signed, tolerance))
}

impl SourceGpsMetadata {
    pub(crate) fn matches_provider_coordinates(
        &self,
        provider_latitude: Option<f64>,
        provider_longitude: Option<f64>,
    ) -> bool {
        let (Some(native_lat), Some(native_lon), Some(provider_lat), Some(provider_lon)) = (
            self.latitude,
            self.longitude,
            provider_latitude,
            provider_longitude,
        ) else {
            return false;
        };
        coordinate_matches_axis(
            provider_lat,
            native_lat,
            self.latitude_tolerance,
            GpsAxis::Latitude,
        ) && coordinate_matches_axis(
            provider_lon,
            native_lon,
            self.longitude_tolerance,
            GpsAxis::Longitude,
        )
    }
}

fn coordinate_matches_axis(provider: f64, native: f64, tolerance: f64, axis: GpsAxis) -> bool {
    let maximum = match axis {
        GpsAxis::Latitude => 90.0,
        GpsAxis::Longitude => 180.0,
    };
    if !provider.is_finite() || !native.is_finite() || provider.abs() > maximum {
        return false;
    }
    if Some(provider) == Some(native) {
        return true;
    }
    tolerance > 0.0
        && provider.is_sign_negative() == native.is_sign_negative()
        && (provider - native).abs() <= tolerance
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

    fn quicktime_movie_with_gps(
        iso6709: Option<&str>,
        accuracy: Option<&str>,
        full_box_meta: bool,
    ) -> Vec<u8> {
        let atom = |kind: &[u8; 4], body: &[u8]| {
            let mut bytes = u32::try_from(body.len() + 8)
                .unwrap()
                .to_be_bytes()
                .to_vec();
            bytes.extend_from_slice(kind);
            bytes.extend_from_slice(body);
            bytes
        };
        let mut hdlr = vec![0_u8; 24];
        hdlr[8..12].copy_from_slice(b"mdta");
        let mut entries: Vec<(&str, &str)> = Vec::new();
        if let Some(acc) = accuracy {
            entries.push(("com.apple.quicktime.location.accuracy.horizontal", acc));
        }
        if let Some(iso) = iso6709 {
            entries.push(("com.apple.quicktime.location.ISO6709", iso));
        }
        let mut keys_body = vec![0, 0, 0, 0];
        keys_body.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_be_bytes());
        let mut ilst_body = Vec::new();
        for (idx, (key, val)) in entries.iter().enumerate() {
            let key_bytes = key.as_bytes();
            keys_body.extend_from_slice(&u32::try_from(key_bytes.len() + 8).unwrap().to_be_bytes());
            keys_body.extend_from_slice(b"mdta");
            keys_body.extend_from_slice(key_bytes);

            let mut data_body = vec![0, 0, 0, 1, 0, 0, 0, 0];
            data_body.extend_from_slice(val.as_bytes());
            let data_box = atom(b"data", &data_body);
            let item_idx = u32::try_from(idx + 1).unwrap().to_be_bytes();
            ilst_body.extend(atom(&item_idx, &data_box));
        }
        let mut meta_body = Vec::new();
        if full_box_meta {
            meta_body.extend_from_slice(&[0; 4]);
        }
        meta_body.extend(atom(b"hdlr", &hdlr));
        meta_body.extend(atom(b"keys", &keys_body));
        meta_body.extend(atom(b"ilst", &ilst_body));
        let mut moov_body = atom(b"mvhd", &[0; 100]);
        moov_body.extend(atom(b"meta", &meta_body));
        let mut movie = atom(b"ftyp", b"qt  \0\0\0\0qt  ");
        movie.extend(atom(b"moov", &moov_body));
        movie.extend(atom(b"mdat", b"video-payload"));
        movie
    }

    #[test]
    fn movie_source_gps_reads_quicktime_mdta_and_companion_still() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        for full_box_meta in [false, true] {
            let mov_path = dir.path().join("IMG_2555_HEVC.MOV");
            let mov_bytes = quicktime_movie_with_gps(
                Some("-33.7602+151.2178+122.204/"),
                Some("7.109572"),
                full_box_meta,
            );
            std::fs::write(&mov_path, &mov_bytes).expect("write MOV");
            let standalone = read_source_gps(&mov_path).expect("read standalone MOV GPS");
            assert_eq!(standalone.datetime, None);
            assert_eq!(standalone.speed, None);
            assert_eq!(
                standalone
                    .horizontal_positioning_error
                    .as_ref()
                    .map(XmpRational::encode)
                    .as_deref(),
                Some("1777393/250000")
            );
            assert!(
                standalone
                    .matches_provider_coordinates(Some(-33.760_188_333_333_33), Some(151.21785),),
                "ISO 6709 4-decimal coordinates must match provider coordinates"
            );
            assert!(
                !standalone.matches_provider_coordinates(Some(-33.7610), Some(151.21785)),
                "edited coordinates outside the ISO 6709 rounding step must not match"
            );

            // Adding the paired Live Photo still in the same directory supplies
            // GPSTimeStamp and speed when its coordinates match the movie.
            let mut heic = std::fs::read("tests/data/sample.heic").expect("read sample HEIC");
            let mut still_exif = exif_with_source_gps();
            still_exif.set_tag(ExifTag::GPSLatitudeRef("S".into()));
            still_exif.set_tag(ExifTag::GPSLatitude(vec![
                ur64(33, 1),
                ur64(45, 1),
                ur64(3668, 100),
            ]));
            still_exif.set_tag(ExifTag::GPSLongitudeRef("E".into()));
            still_exif.set_tag(ExifTag::GPSLongitude(vec![
                ur64(151, 1),
                ur64(13, 1),
                ur64(426, 100),
            ]));
            still_exif
                .write_to_vec(&mut heic, FileExtension::HEIF)
                .expect("write HEIC EXIF");
            let heic_path = dir.path().join("IMG_2555.HEIC");
            std::fs::write(&heic_path, &heic).expect("write companion HEIC");
            let unified = read_source_gps(&mov_path).expect("read unified Live Photo MOV GPS");
            assert_eq!(unified.datetime.as_deref(), Some(SOURCE_GPS_DATETIME));
            assert_eq!(
                unified.speed.as_ref().map(XmpRational::encode).as_deref(),
                Some(SOURCE_GPS_SPEED)
            );
            assert_eq!(unified.speed_ref.as_deref(), Some(SOURCE_GPS_SPEED_REF));
            assert_eq!(
                unified
                    .horizontal_positioning_error
                    .as_ref()
                    .map(XmpRational::encode)
                    .as_deref(),
                Some("1777393/250000")
            );

            // An unrelated sibling still at different coordinates must not
            // bleed its GPS timestamp or speed into the movie.
            let mut unrelated_heic =
                std::fs::read("tests/data/sample.heic").expect("read sample HEIC");
            let mut unrelated_exif = exif_with_source_gps();
            unrelated_exif.set_tag(ExifTag::GPSLatitudeRef("N".into()));
            unrelated_exif.set_tag(ExifTag::GPSLatitude(vec![
                ur64(35, 1),
                ur64(40, 1),
                ur64(0, 100),
            ]));
            unrelated_exif.set_tag(ExifTag::GPSLongitudeRef("E".into()));
            unrelated_exif.set_tag(ExifTag::GPSLongitude(vec![
                ur64(139, 1),
                ur64(45, 1),
                ur64(0, 100),
            ]));
            unrelated_exif
                .write_to_vec(&mut unrelated_heic, FileExtension::HEIF)
                .expect("write unrelated HEIC EXIF");
            std::fs::write(&heic_path, &unrelated_heic).expect("write unrelated HEIC");
            let unmerged = read_source_gps(&mov_path).expect("read MOV with unrelated still");
            assert_eq!(unmerged.datetime, None);
            assert_eq!(unmerged.speed, None);
            std::fs::remove_file(&heic_path).ok();
        }

        // Non-positive or malformed QuickTime accuracy is ignored.
        for bad_accuracy in ["0", "0.0", "-1.0", "invalid"] {
            let mov_path = dir.path().join("bad-accuracy.MOV");
            std::fs::write(
                &mov_path,
                quicktime_movie_with_gps(Some("-33.7602+151.2178/"), Some(bad_accuracy), false),
            )
            .unwrap();
            let gps = read_source_gps(&mov_path).unwrap();
            assert_eq!(gps.horizontal_positioning_error, None, "{bad_accuracy}");
        }
    }
}
