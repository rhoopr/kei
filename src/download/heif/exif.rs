//! Native Exif extraction and in-place TIFF capture timestamp repair.

use super::boxes::{extent_is_within_mdat_payload, find_meta_layout};
use super::items::{item_extent_is_shared, parse_iinf, parse_iloc, resolve_item_extents};
use super::relationships::select_exif_item_id;
use super::{ExifCaptureTime, HeifError, invalid_layout};
use std::collections::HashSet;
use std::ops::Range;

/// Resolve the TIFF payload from the HEIF `Exif` item associated with the
/// primary image.
///
/// HEIF prefixes an Exif item with a four-byte big-endian offset from the end
/// of that field to the TIFF header. Only construction-method-0 items are
/// supported because their extents address file bytes directly.
pub(crate) fn extract_exif_tiff_bytes(bytes: &[u8]) -> Result<Option<Vec<u8>>, HeifError> {
    let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(bytes)?;
    let iinf_layout = parse_iinf(bytes, iinf)?;
    let Some(exif_item_id) = select_exif_item_id(
        bytes,
        iref,
        primary_item_id,
        &iinf_layout.exif_item_ids,
        &iinf_layout.item_ids,
    )?
    else {
        return Ok(None);
    };
    let iloc_layout = parse_iloc(bytes, iloc)?;
    let item = iloc_layout
        .items
        .iter()
        .find(|item| item.item_id == exif_item_id)
        .ok_or_else(|| invalid_layout("Exif item has no iloc entry"))?;
    if item.construction_method != 0 {
        return Err(invalid_layout(
            "Exif item uses an unsupported construction method",
        ));
    }
    let extents = resolve_item_extents(bytes, item)?;
    if extents.is_empty() {
        return Err(invalid_layout("Exif item has no extents"));
    }
    let payload_len = extents.iter().try_fold(0usize, |total, extent| {
        total
            .checked_add(extent.len())
            .filter(|length| *length <= bytes.len())
            .ok_or_else(|| invalid_layout("Exif item payload is too large"))
    })?;
    let mut payload = Vec::with_capacity(payload_len);
    for extent in extents {
        payload.extend_from_slice(extent);
    }
    let offset = payload
        .get(..4)
        .ok_or_else(|| invalid_layout("Exif item offset is truncated"))?;
    let offset = usize::try_from(u32::from_be_bytes(
        offset
            .try_into()
            .map_err(|_| invalid_layout("Exif item offset is invalid"))?,
    ))
    .map_err(|_| invalid_layout("Exif item offset is too large"))?;
    let tiff_start = 4usize
        .checked_add(offset)
        .ok_or_else(|| invalid_layout("Exif TIFF offset overflows"))?;
    let tiff = payload
        .get(tiff_start..)
        .ok_or_else(|| invalid_layout("Exif TIFF offset is outside the item"))?;
    Ok(Some(tiff.to_vec()))
}

#[derive(Clone, Copy)]
enum TiffEndian {
    Little,
    Big,
}

#[derive(Clone, Copy)]
struct TiffEntry {
    start: usize,
    tag: u16,
    field_type: u16,
    count: u32,
    value_or_offset: u32,
}

fn tiff_u16_at(bytes: &[u8], offset: usize, endian: TiffEndian) -> Result<u16, HeifError> {
    let value = bytes
        .get(offset..offset.saturating_add(2))
        .ok_or_else(|| invalid_layout("TIFF field is truncated"))?
        .try_into()
        .map_err(|_| invalid_layout("TIFF field is invalid"))?;
    Ok(match endian {
        TiffEndian::Little => u16::from_le_bytes(value),
        TiffEndian::Big => u16::from_be_bytes(value),
    })
}

fn tiff_u32_at(bytes: &[u8], offset: usize, endian: TiffEndian) -> Result<u32, HeifError> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| invalid_layout("TIFF field is truncated"))?
        .try_into()
        .map_err(|_| invalid_layout("TIFF field is invalid"))?;
    Ok(match endian {
        TiffEndian::Little => u32::from_le_bytes(value),
        TiffEndian::Big => u32::from_be_bytes(value),
    })
}

fn read_tiff_ifd(
    tiff: &[u8],
    ifd_offset: usize,
    endian: TiffEndian,
) -> Result<(Range<usize>, Vec<TiffEntry>, Option<usize>), HeifError> {
    let count = usize::from(tiff_u16_at(tiff, ifd_offset, endian)?);
    let entries_start = ifd_offset
        .checked_add(2)
        .ok_or_else(|| invalid_layout("TIFF IFD offset overflows"))?;
    let next_ifd_pos = count
        .checked_mul(12)
        .and_then(|length| entries_start.checked_add(length))
        .filter(|end| end.saturating_add(4) <= tiff.len())
        .ok_or_else(|| invalid_layout("TIFF IFD is truncated"))?;
    let structure_end = next_ifd_pos
        .checked_add(4)
        .filter(|end| *end <= tiff.len())
        .ok_or_else(|| invalid_layout("TIFF IFD is truncated"))?;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let start = index
            .checked_mul(12)
            .and_then(|offset| entries_start.checked_add(offset))
            .ok_or_else(|| invalid_layout("TIFF entry offset overflows"))?;
        entries.push(TiffEntry {
            start,
            tag: tiff_u16_at(tiff, start, endian)?,
            field_type: tiff_u16_at(tiff, start + 2, endian)?,
            count: tiff_u32_at(tiff, start + 4, endian)?,
            value_or_offset: tiff_u32_at(tiff, start + 8, endian)?,
        });
    }
    let next_ifd = usize::try_from(tiff_u32_at(tiff, next_ifd_pos, endian)?)
        .map_err(|_| invalid_layout("TIFF next-IFD offset is too large"))?;
    Ok((
        ifd_offset..structure_end,
        entries,
        (next_ifd != 0).then_some(next_ifd),
    ))
}

fn find_tiff_entry(
    tiff: &[u8],
    ifd_offset: usize,
    tag: u16,
    endian: TiffEndian,
) -> Result<Option<TiffEntry>, HeifError> {
    let (_, entries, _) = read_tiff_ifd(tiff, ifd_offset, endian)?;
    find_tiff_entry_in(&entries, tag)
}

fn find_tiff_entry_in(entries: &[TiffEntry], tag: u16) -> Result<Option<TiffEntry>, HeifError> {
    let mut found = None;
    for entry in entries.iter().copied() {
        if entry.tag != tag {
            continue;
        }
        if found.is_some() {
            return Err(invalid_layout("TIFF contains duplicate metadata tags"));
        }
        found = Some(entry);
    }
    Ok(found)
}

fn tiff_entry_data_range(tiff: &[u8], entry: TiffEntry) -> Result<Option<Range<usize>>, HeifError> {
    let width = match entry.field_type {
        1 | 2 | 6 | 7 => 1usize,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => return Err(invalid_layout("TIFF entry has an unsupported field type")),
    };
    let length = usize::try_from(entry.count)
        .ok()
        .and_then(|count| count.checked_mul(width))
        .ok_or_else(|| invalid_layout("TIFF entry payload is too large"))?;
    if length <= 4 {
        return Ok(None);
    }
    let start = usize::try_from(entry.value_or_offset)
        .map_err(|_| invalid_layout("TIFF entry payload offset is too large"))?;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= tiff.len())
        .ok_or_else(|| invalid_layout("TIFF entry payload is truncated"))?;
    Ok(Some(start..end))
}

fn tiff_unsigned_values(
    tiff: &[u8],
    entry: TiffEntry,
    endian: TiffEndian,
) -> Result<Vec<u64>, HeifError> {
    let width = match entry.field_type {
        3 => 2usize,
        4 => 4,
        _ => {
            return Err(invalid_layout(
                "TIFF image-data offset or length has an unsupported type",
            ));
        }
    };
    let count = usize::try_from(entry.count)
        .map_err(|_| invalid_layout("TIFF image-data value count is too large"))?;
    let length = count
        .checked_mul(width)
        .ok_or_else(|| invalid_layout("TIFF image-data values are too large"))?;
    let start = if length <= 4 {
        entry
            .start
            .checked_add(8)
            .ok_or_else(|| invalid_layout("TIFF inline value offset overflows"))?
    } else {
        usize::try_from(entry.value_or_offset)
            .map_err(|_| invalid_layout("TIFF image-data value offset is too large"))?
    };
    start
        .checked_add(length)
        .filter(|end| *end <= tiff.len())
        .ok_or_else(|| invalid_layout("TIFF image-data values are truncated"))?;
    let mut values = Vec::with_capacity(count);
    for index in 0..count {
        let position = index
            .checked_mul(width)
            .and_then(|offset| start.checked_add(offset))
            .ok_or_else(|| invalid_layout("TIFF image-data value offset overflows"))?;
        values.push(match width {
            2 => u64::from(tiff_u16_at(tiff, position, endian)?),
            4 => u64::from(tiff_u32_at(tiff, position, endian)?),
            _ => {
                return Err(invalid_layout("TIFF image-data value width is unsupported"));
            }
        });
    }
    Ok(values)
}

fn tiff_referenced_data_ranges(
    tiff: &[u8],
    entries: &[TiffEntry],
    endian: TiffEndian,
    offset_tag: u16,
    length_tag: u16,
) -> Result<Vec<Range<usize>>, HeifError> {
    let offset_entry = find_tiff_entry_in(entries, offset_tag)?;
    let length_entry = find_tiff_entry_in(entries, length_tag)?;
    let (Some(offset_entry), Some(length_entry)) = (offset_entry, length_entry) else {
        if offset_entry.is_some() || length_entry.is_some() {
            return Err(invalid_layout(
                "TIFF image-data offset and length tags are incomplete",
            ));
        }
        return Ok(Vec::new());
    };
    let offsets = tiff_unsigned_values(tiff, offset_entry, endian)?;
    let lengths = tiff_unsigned_values(tiff, length_entry, endian)?;
    if offsets.len() != lengths.len() {
        return Err(invalid_layout(
            "TIFF image-data offset and length counts differ",
        ));
    }
    offsets
        .into_iter()
        .zip(lengths)
        .filter(|(_, length)| *length != 0)
        .map(|(offset, length)| {
            let start = usize::try_from(offset)
                .map_err(|_| invalid_layout("TIFF image-data offset is too large"))?;
            let length = usize::try_from(length)
                .map_err(|_| invalid_layout("TIFF image-data length is too large"))?;
            let end = start
                .checked_add(length)
                .filter(|end| *end <= tiff.len())
                .ok_or_else(|| invalid_layout("TIFF image-data range is truncated"))?;
            Ok(start..end)
        })
        .collect()
}

fn ranges_overlap(left: &Range<usize>, right: &Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn validate_tiff_patch_ranges(
    tiff: &[u8],
    endian: TiffEndian,
    ifd0_offset: usize,
    targets: &[(TiffEntry, Range<usize>)],
) -> Result<(), HeifError> {
    for (index, (_, target)) in targets.iter().enumerate() {
        if targets
            .iter()
            .skip(index + 1)
            .any(|(_, other)| ranges_overlap(target, other))
        {
            return Err(invalid_layout("HEIF Exif datetime fields overlap"));
        }
    }

    let target_entries = targets
        .iter()
        .map(|(entry, _)| entry.start)
        .collect::<HashSet<_>>();
    let mut pending = vec![ifd0_offset];
    let mut visited = HashSet::new();
    let mut protected = Vec::new();
    protected.push(0..8);
    while let Some(ifd_offset) = pending.pop() {
        if !visited.insert(ifd_offset) {
            continue;
        }
        if visited.len() > 64 {
            return Err(invalid_layout("TIFF contains too many linked IFDs"));
        }
        let (structure, entries, next_ifd) = read_tiff_ifd(tiff, ifd_offset, endian)?;
        protected.push(structure);
        for (offset_tag, length_tag) in [(0x0111, 0x0117), (0x0144, 0x0145), (0x0201, 0x0202)] {
            protected.extend(tiff_referenced_data_ranges(
                tiff, &entries, endian, offset_tag, length_tag,
            )?);
        }
        if let Some(next_ifd) = next_ifd {
            pending.push(next_ifd);
        }
        for entry in entries {
            if matches!(entry.tag, 0x8769 | 0x8825 | 0xA005)
                && entry.field_type == 4
                && entry.count == 1
                && let Ok(offset) = usize::try_from(entry.value_or_offset)
            {
                pending.push(offset);
            }
            if entry.tag == 0x014A && entry.field_type == 4 {
                if entry.count == 1 {
                    if let Ok(offset) = usize::try_from(entry.value_or_offset) {
                        pending.push(offset);
                    }
                } else if let Some(range) = tiff_entry_data_range(tiff, entry)? {
                    let mut position = range.start;
                    for _ in 0..entry.count {
                        let offset = usize::try_from(tiff_u32_at(tiff, position, endian)?)
                            .map_err(|_| invalid_layout("TIFF SubIFD offset is too large"))?;
                        pending.push(offset);
                        position = position
                            .checked_add(4)
                            .ok_or_else(|| invalid_layout("TIFF SubIFD offset overflows"))?;
                    }
                }
            }
            if !target_entries.contains(&entry.start)
                && let Some(range) = tiff_entry_data_range(tiff, entry)?
            {
                protected.push(range);
            }
        }
    }

    if targets.iter().any(|(_, target)| {
        protected
            .iter()
            .any(|protected| ranges_overlap(target, protected))
    }) {
        return Err(invalid_layout(
            "HEIF Exif datetime storage overlaps other TIFF data",
        ));
    }
    Ok(())
}

fn tiff_ascii_range(tiff: &[u8], entry: TiffEntry) -> Result<Range<usize>, HeifError> {
    if entry.field_type != 2 || entry.count == 0 {
        return Err(invalid_layout(
            "HEIF Exif datetime tag is not a non-empty ASCII value",
        ));
    }
    let length = usize::try_from(entry.count)
        .map_err(|_| invalid_layout("HEIF Exif datetime tag is too large"))?;
    let start = if length <= 4 {
        entry
            .start
            .checked_add(8)
            .ok_or_else(|| invalid_layout("TIFF inline value offset overflows"))?
    } else {
        usize::try_from(entry.value_or_offset)
            .map_err(|_| invalid_layout("TIFF value offset is too large"))?
    };
    let end = start
        .checked_add(length)
        .filter(|end| *end <= tiff.len())
        .ok_or_else(|| invalid_layout("HEIF Exif datetime value is truncated"))?;
    Ok(start..end)
}

fn tiff_capture_ifd_offsets(tiff: &[u8]) -> Result<(TiffEndian, usize, Option<usize>), HeifError> {
    let endian = match tiff.get(..2) {
        Some(b"II") => TiffEndian::Little,
        Some(b"MM") => TiffEndian::Big,
        _ => return Err(invalid_layout("HEIF Exif TIFF byte order is invalid")),
    };
    if tiff_u16_at(tiff, 2, endian)? != 42 {
        return Err(invalid_layout("HEIF Exif TIFF header is invalid"));
    }
    let ifd0_offset = usize::try_from(tiff_u32_at(tiff, 4, endian)?)
        .map_err(|_| invalid_layout("HEIF Exif IFD0 offset is too large"))?;
    let Some(exif_pointer) = find_tiff_entry(tiff, ifd0_offset, 0x8769, endian)? else {
        return Ok((endian, ifd0_offset, None));
    };
    if exif_pointer.field_type != 4 || exif_pointer.count != 1 {
        return Err(invalid_layout("HEIF Exif IFD pointer is invalid"));
    }
    let exif_ifd_offset = usize::try_from(exif_pointer.value_or_offset)
        .map_err(|_| invalid_layout("HEIF Exif IFD offset is too large"))?;
    Ok((endian, ifd0_offset, Some(exif_ifd_offset)))
}

fn tiff_ascii_value(tiff: &[u8], entry: TiffEntry) -> Result<String, HeifError> {
    let range = tiff_ascii_range(tiff, entry)?;
    let value = tiff
        .get(range)
        .ok_or_else(|| invalid_layout("HEIF Exif datetime value is truncated"))?;
    let value = std::str::from_utf8(value)
        .map_err(|_| invalid_layout("HEIF Exif datetime value is not UTF-8"))?
        .trim_matches('\0')
        .trim()
        .to_string();
    Ok(value)
}

pub(crate) fn extract_exif_capture_time(
    bytes: &[u8],
) -> Result<Option<ExifCaptureTime>, HeifError> {
    let Some(tiff) = extract_exif_tiff_bytes(bytes)? else {
        return Ok(None);
    };
    let (endian, _, exif_ifd_offset) = tiff_capture_ifd_offsets(&tiff)?;
    let Some(exif_ifd_offset) = exif_ifd_offset else {
        return Ok(Some(ExifCaptureTime {
            datetime_original: None,
            offset_time_original: None,
        }));
    };
    let datetime_original = find_tiff_entry(&tiff, exif_ifd_offset, 0x9003, endian)?
        .map(|entry| tiff_ascii_value(&tiff, entry))
        .transpose()?;
    let offset_time_original = find_tiff_entry(&tiff, exif_ifd_offset, 0x9011, endian)?
        .map(|entry| tiff_ascii_value(&tiff, entry))
        .transpose()?;
    Ok(Some(ExifCaptureTime {
        datetime_original,
        offset_time_original,
    }))
}

fn replace_tiff_ascii(tiff: &mut [u8], range: Range<usize>, value: &str) -> Result<(), HeifError> {
    if !value.is_ascii() || value.as_bytes().contains(&0) || value.len() + 1 > range.len() {
        return Err(invalid_layout(
            "replacement HEIF Exif datetime does not fit its existing field",
        ));
    }
    let target = tiff
        .get_mut(range)
        .ok_or_else(|| invalid_layout("HEIF Exif datetime value is truncated"))?;
    target.fill(0);
    target
        .get_mut(..value.len())
        .ok_or_else(|| invalid_layout("HEIF Exif datetime value is truncated"))?
        .copy_from_slice(value.as_bytes());
    Ok(())
}

/// Replace an existing native HEIF capture timestamp and offset without
/// resizing or re-encoding the Exif item. Files without a native
/// `DateTimeOriginal` need no native repair. A native timestamp without a
/// safely writable paired offset is rejected so its durable marker remains.
pub(crate) fn rewrite_exif_capture_time(
    bytes: &[u8],
    datetime: &str,
    offset: &str,
) -> Result<Option<Vec<u8>>, HeifError> {
    if datetime.len() != 19 || offset.len() != 6 {
        return Err(invalid_layout(
            "replacement HEIF Exif capture time has an invalid length",
        ));
    }
    let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(bytes)?;
    let iinf_layout = parse_iinf(bytes, iinf)?;
    let Some(exif_item_id) = select_exif_item_id(
        bytes,
        iref,
        primary_item_id,
        &iinf_layout.exif_item_ids,
        &iinf_layout.item_ids,
    )?
    else {
        return Ok(None);
    };
    let iloc_layout = parse_iloc(bytes, iloc)?;
    if iloc_layout
        .items
        .iter()
        .any(|item| item.construction_method == 2)
    {
        return Err(invalid_layout(
            "HEIF Exif repair cannot prove item-offset dependants remain unchanged",
        ));
    }
    let item = iloc_layout
        .items
        .iter()
        .find(|item| item.item_id == exif_item_id)
        .ok_or_else(|| invalid_layout("Exif item has no iloc entry"))?;
    if item.construction_method != 0 || item.data_reference_index != 0 || item.extents.len() != 1 {
        return Err(invalid_layout(
            "HEIF Exif item cannot be safely updated in place",
        ));
    }
    let extent = item
        .extents
        .first()
        .ok_or_else(|| invalid_layout("HEIF Exif item has no extent"))?;
    let extent_start = item
        .base_offset
        .checked_add(extent.offset)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or_else(|| invalid_layout("HEIF Exif item offset overflows"))?;
    let extent_length = usize::try_from(extent.length)
        .map_err(|_| invalid_layout("HEIF Exif item is too large"))?;
    let extent_end = extent_start
        .checked_add(extent_length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid_layout("HEIF Exif item is outside the file"))?;
    if item_extent_is_shared(
        &iloc_layout,
        exif_item_id,
        u64::try_from(extent_start)
            .map_err(|_| invalid_layout("HEIF Exif item offset overflows"))?,
        extent.length,
    ) || !extent_is_within_mdat_payload(bytes, extent_start, extent_length)?
    {
        return Err(invalid_layout(
            "HEIF Exif item does not own a writable mdat extent",
        ));
    }
    let prefix = bytes
        .get(extent_start..extent_start.saturating_add(4))
        .ok_or_else(|| invalid_layout("HEIF Exif item offset is truncated"))?;
    let tiff_offset = usize::try_from(u32::from_be_bytes(
        prefix
            .try_into()
            .map_err(|_| invalid_layout("HEIF Exif item offset is invalid"))?,
    ))
    .map_err(|_| invalid_layout("HEIF Exif TIFF offset is too large"))?;
    let tiff_start = extent_start
        .checked_add(4)
        .and_then(|start| start.checked_add(tiff_offset))
        .filter(|start| *start <= extent_end)
        .ok_or_else(|| invalid_layout("HEIF Exif TIFF offset is outside the item"))?;
    let mut tiff = bytes
        .get(tiff_start..extent_end)
        .ok_or_else(|| invalid_layout("HEIF Exif TIFF data is truncated"))?
        .to_vec();
    let (endian, ifd0_offset, exif_ifd_offset) = tiff_capture_ifd_offsets(&tiff)?;
    let Some(exif_ifd_offset) = exif_ifd_offset else {
        return Ok(None);
    };
    let Some(datetime_original) = find_tiff_entry(&tiff, exif_ifd_offset, 0x9003, endian)? else {
        return Ok(None);
    };
    let offset_time_original = find_tiff_entry(&tiff, exif_ifd_offset, 0x9011, endian)?
        .ok_or_else(|| invalid_layout("HEIF Exif timestamp has no writable paired offset"))?;

    let datetime_original_range = tiff_ascii_range(&tiff, datetime_original)?;
    let offset_time_original_range = tiff_ascii_range(&tiff, offset_time_original)?;

    let targets = [
        (datetime_original, datetime_original_range.clone()),
        (offset_time_original, offset_time_original_range.clone()),
    ];
    validate_tiff_patch_ranges(&tiff, endian, ifd0_offset, &targets)?;

    replace_tiff_ascii(&mut tiff, datetime_original_range, datetime)?;
    replace_tiff_ascii(&mut tiff, offset_time_original_range, offset)?;

    let mut output = bytes.to_vec();
    output
        .get_mut(tiff_start..extent_end)
        .ok_or_else(|| invalid_layout("HEIF Exif TIFF data is truncated"))?
        .copy_from_slice(&tiff);
    Ok(Some(output))
}

#[cfg(test)]
mod tests {
    use super::super::HeifError;
    use super::super::test_support::{
        HeicSpec, ItemSpec, build_heic, capture_exif_payload,
        capture_exif_payload_with_thumbnail_alias, heic_with_exif_items, ref_box,
    };
    use super::{extract_exif_capture_time, extract_exif_tiff_bytes, rewrite_exif_capture_time};

    #[test]
    fn extract_exif_tiff_bytes_resolves_sample_item() {
        let tiff = extract_exif_tiff_bytes(include_bytes!("../../../tests/data/sample.heic"))
            .expect("sample HEIC item map")
            .expect("sample HEIC Exif item");
        assert!(
            tiff.starts_with(b"MM\0*") || tiff.starts_with(b"II*\0"),
            "resolved Exif item must start with a TIFF header"
        );
    }

    #[test]
    fn extract_exif_capture_time_rejects_duplicate_native_tags() {
        let exif = capture_exif_payload(true);
        let input = heic_with_exif_items(&[(2, &exif)], vec![ref_box(b"cdsc", 2, &[1])]);

        assert!(
            matches!(
                extract_exif_capture_time(&input),
                Err(HeifError::InvalidLayout { reason })
                    if reason.contains("duplicate metadata tags")
            ),
            "duplicate native timestamps must be ambiguous"
        );
    }

    #[test]
    fn rewrite_exif_capture_time_rejects_aliased_tiff_storage() {
        let exif = capture_exif_payload_with_thumbnail_alias();
        let input = heic_with_exif_items(&[(2, &exif)], vec![ref_box(b"cdsc", 2, &[1])]);

        assert!(
            matches!(
                rewrite_exif_capture_time(&input, "2024:06:15 10:00:00", "+11:00"),
                Err(HeifError::InvalidLayout { reason })
                    if reason.contains("overlaps other TIFF data")
            ),
            "native timestamp storage shared with another tag must not be patched"
        );
    }

    #[test]
    fn rewrite_exif_capture_time_changes_only_native_datetime_values() {
        use little_exif::exif_tag::ExifTag;
        use little_exif::filetype::FileExtension;
        use little_exif::metadata::Metadata;

        const OLD_DATETIME: &[u8] = b"2023:09:03 09:28:14";
        const NEW_DATETIME: &[u8] = b"2024:06:15 10:00:00";
        const OLD_OFFSET: &[u8] = b"+03:00";
        const NEW_OFFSET: &[u8] = b"+11:00";

        fn occurrences(bytes: &[u8], value: &[u8]) -> usize {
            bytes
                .windows(value.len())
                .filter(|candidate| *candidate == value)
                .count()
        }

        let input = include_bytes!("../../../tests/data/sample.heic");
        let output = rewrite_exif_capture_time(
            input,
            std::str::from_utf8(NEW_DATETIME).unwrap(),
            std::str::from_utf8(NEW_OFFSET).unwrap(),
        )
        .expect("native Exif capture time rewrite")
        .expect("sample HEIC has a native capture timestamp");
        assert_eq!(input.len(), output.len());
        assert_eq!(occurrences(&output, OLD_DATETIME), 2);
        assert_eq!(occurrences(&output, NEW_DATETIME), 1);
        assert_eq!(occurrences(&output, OLD_OFFSET), 2);
        assert_eq!(occurrences(&output, NEW_OFFSET), 1);
        let expected_changes = OLD_DATETIME
            .iter()
            .zip(NEW_DATETIME)
            .chain(OLD_OFFSET.iter().zip(NEW_OFFSET))
            .filter(|(before, after)| before != after)
            .count();
        assert_eq!(
            input
                .iter()
                .zip(&output)
                .filter(|(before, after)| before != after)
                .count(),
            expected_changes,
            "only DateTimeOriginal and OffsetTimeOriginal may change"
        );

        let tiff = extract_exif_tiff_bytes(&output)
            .expect("rewritten HEIC item map")
            .expect("rewritten native Exif item");
        let metadata = Metadata::new_from_vec(&tiff, FileExtension::TIFF).unwrap();
        assert!(metadata
            .get_tag(&ExifTag::DateTimeOriginal(String::new()))
            .any(|tag| matches!(tag, ExifTag::DateTimeOriginal(value) if value == "2024:06:15 10:00:00")));
        assert!(
            metadata
                .get_tag(&ExifTag::OffsetTimeOriginal(String::new()))
                .any(|tag| matches!(tag, ExifTag::OffsetTimeOriginal(value) if value == "+11:00"))
        );
    }

    #[test]
    fn rewrite_exif_capture_time_refuses_an_unpaired_native_timestamp() {
        use little_exif::exif_tag::ExifTag;
        use little_exif::filetype::FileExtension;
        use little_exif::metadata::Metadata;

        let mut metadata = Metadata::new();
        metadata.set_tag(ExifTag::DateTimeOriginal("2023:09:03 09:28:14".to_string()));
        let exif = metadata.as_u8_vec(FileExtension::HEIF).unwrap();
        let input = heic_with_exif_items(&[(2, &exif)], vec![ref_box(b"cdsc", 2, &[1])]);

        let result = rewrite_exif_capture_time(&input, "2024:06:15 10:00:00", "+11:00");
        assert!(
            matches!(result, Err(HeifError::InvalidLayout { reason }) if reason.contains("paired offset")),
            "a timestamp without a writable native offset must remain pending"
        );
    }

    #[test]
    fn rewrite_exif_capture_time_refuses_item_offset_dependants() {
        use little_exif::exif_tag::ExifTag;
        use little_exif::filetype::FileExtension;
        use little_exif::metadata::Metadata;

        let mut metadata = Metadata::new();
        metadata.set_tag(ExifTag::DateTimeOriginal("2023:09:03 09:28:14".to_string()));
        metadata.set_tag(ExifTag::OffsetTimeOriginal("+03:00".to_string()));
        let exif = metadata.as_u8_vec(FileExtension::HEIF).unwrap();
        let input = build_heic(&HeicSpec {
            iloc_version: 1,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..32).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"Exif",
                    infe_version: 2,
                    construction_method: 0,
                    data: exif,
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 3,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 2,
                    data: Vec::new(),
                    offset: 0,
                    length: 16,
                },
            ],
            idat: None,
            iref_children: vec![ref_box(b"cdsc", 2, &[1]), ref_box(b"dimg", 1, &[3])],
        });

        let result = rewrite_exif_capture_time(&input, "2024:06:15 10:00:00", "+11:00");
        assert!(
            matches!(result, Err(HeifError::InvalidLayout { reason }) if reason.contains("item-offset")),
            "item-offset dependants make in-place Exif repair unsafe"
        );
    }

    #[test]
    fn exif_probe_selects_item_associated_with_primary_image() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..32).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"Exif",
                    infe_version: 2,
                    construction_method: 0,
                    data: b"\0\0\0\0MM\0*".to_vec(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 3,
                    item_type: *b"Exif",
                    infe_version: 2,
                    construction_method: 0,
                    data: b"\0\0\0\0II*\0".to_vec(),
                    offset: 0,
                    length: 0,
                },
            ],
            idat: None,
            iref_children: vec![ref_box(b"cdsc", 3, &[1])],
        };
        let input = build_heic(&spec);

        assert_eq!(
            extract_exif_tiff_bytes(&input).unwrap().as_deref(),
            Some(b"II*\0".as_slice())
        );
    }

    #[test]
    fn exif_probe_ignores_lone_item_associated_with_an_auxiliary_image() {
        let input = heic_with_exif_items(&[(2, b"\0\0\0\0MM\0*")], vec![ref_box(b"cdsc", 2, &[4])]);

        assert!(
            extract_exif_tiff_bytes(&input).unwrap().is_none(),
            "an auxiliary image's Exif item must not answer for the primary image"
        );
    }

    #[test]
    fn exif_probe_accepts_lone_unattributed_item() {
        let input = heic_with_exif_items(&[(2, b"\0\0\0\0MM\0*")], vec![ref_box(b"dimg", 1, &[4])]);

        assert_eq!(
            extract_exif_tiff_bytes(&input).unwrap().as_deref(),
            Some(b"MM\0*".as_slice()),
            "a lone unattributed Exif item remains compatible with ordinary HEIF files"
        );
    }

    #[test]
    fn exif_probe_rejects_dangling_association() {
        let input = heic_with_exif_items(&[(2, b"\0\0\0\0MM\0*")], vec![ref_box(b"cdsc", 2, &[5])]);

        assert!(
            extract_exif_tiff_bytes(&input).is_err(),
            "a dangling association cannot prove whether Exif belongs to the primary image"
        );
    }
}
