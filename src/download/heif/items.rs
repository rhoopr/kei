//! Item information, extent maps, and byte-preserving item-table updates.

use super::boxes::{RawBox, box_with_body, scan_raw_boxes};
use super::{HeifError, invalid_layout};
use std::collections::HashSet;

#[derive(Debug, Clone)]
pub(super) struct IlocExtent {
    pub(super) offset_pos: Option<usize>,
    length_pos: Option<usize>,
    pub(super) offset: u64,
    pub(super) length: u64,
}

#[derive(Debug, Clone)]
pub(super) struct IlocItem {
    pub(super) item_id: u32,
    pub(super) construction_method: u8,
    pub(super) data_reference_index: u16,
    pub(super) base_offset_pos: Option<usize>,
    pub(super) base_offset: u64,
    pub(super) extents: Vec<IlocExtent>,
}

#[derive(Debug, Clone)]
pub(super) struct IlocLayout {
    pub(super) version: u8,
    pub(super) offset_size: u8,
    length_size: u8,
    pub(super) base_offset_size: u8,
    index_size: u8,
    count_pos: usize,
    count_size: usize,
    pub(super) items: Vec<IlocItem>,
}

#[derive(Debug)]
pub(super) struct IinfLayout {
    pub(super) version: u8,
    pub(super) count_pos: usize,
    pub(super) count_size: usize,
    pub(super) max_item_id: u32,
    pub(super) xmp_item_ids: Vec<u32>,
    pub(super) exif_item_ids: Vec<u32>,
    pub(super) tone_map_item_ids: Vec<u32>,
    pub(super) item_ids: Vec<u32>,
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the body range, and the explicit minimum lengths prove each fixed iinf field."
)]
pub(super) fn parse_iinf(bytes: &[u8], iinf: RawBox) -> Result<IinfLayout, HeifError> {
    let body = &bytes[iinf.body_start()..iinf.end()];
    if body.len() < 6 {
        return Err(invalid_layout("iinf box is truncated"));
    }
    let version = body[0];
    let (count_pos, count_size, count) = match version {
        0 => (4, 2, u16::from_be_bytes([body[4], body[5]]) as u32),
        1 => {
            if body.len() < 8 {
                return Err(invalid_layout("iinf version 1 box is truncated"));
            }
            (
                4,
                4,
                u32::from_be_bytes(
                    body.get(4..8)
                        .ok_or_else(|| invalid_layout("iinf entry count is truncated"))?
                        .try_into()
                        .map_err(|_| invalid_layout("invalid iinf entry count"))?,
                ),
            )
        }
        _ => return Err(invalid_layout("unsupported iinf version")),
    };
    let entry_start = count_pos + count_size;
    let entries = scan_raw_boxes(body, entry_start, body.len())?;
    if entries.len() != usize::try_from(count).unwrap_or(usize::MAX) {
        return Err(invalid_layout(
            "iinf entry count does not match its contents",
        ));
    }
    let mut max_item_id = 0u32;
    let mut xmp_item_ids = Vec::new();
    let mut exif_item_ids = Vec::new();
    let mut tone_map_item_ids = Vec::new();
    let mut item_ids = HashSet::with_capacity(entries.len());
    for entry in entries {
        if entry.kind != *b"infe" {
            return Err(invalid_layout("iinf contains a non-infe entry"));
        }
        let entry_body = &body[entry.body_start()..entry.end()];
        let (item_id, item_type, content_type) = parse_infe(entry_body)?;
        if !item_ids.insert(item_id) {
            return Err(invalid_layout("iinf contains duplicate item IDs"));
        }
        max_item_id = max_item_id.max(item_id);
        if item_type == Some(*b"mime") && content_type.as_deref() == Some("application/rdf+xml") {
            xmp_item_ids.push(item_id);
        }
        if item_type == Some(*b"Exif") {
            exif_item_ids.push(item_id);
        }
        if item_type == Some(*b"tmap") {
            tone_map_item_ids.push(item_id);
        }
    }
    Ok(IinfLayout {
        version,
        count_pos,
        count_size,
        max_item_id,
        xmp_item_ids,
        exif_item_ids,
        tone_map_item_ids,
        item_ids: item_ids.into_iter().collect(),
    })
}

#[allow(
    clippy::indexing_slicing,
    reason = "The explicit version-specific minimum lengths prove each fixed infe field before access."
)]
fn parse_infe(body: &[u8]) -> Result<(u32, Option<[u8; 4]>, Option<String>), HeifError> {
    if body.len() < 8 {
        return Err(invalid_layout("infe box is truncated"));
    }
    let version = body[0];
    let (item_id, type_pos) = match version {
        0 => (u16::from_be_bytes([body[4], body[5]]) as u32, None),
        1 => return Err(invalid_layout("unsupported infe version 1")),
        2 => (u16::from_be_bytes([body[4], body[5]]) as u32, Some(8)),
        3 => {
            if body.len() < 14 {
                return Err(invalid_layout("infe version 3 box is truncated"));
            }
            (
                u32::from_be_bytes(
                    body.get(4..8)
                        .ok_or_else(|| invalid_layout("infe item id is truncated"))?
                        .try_into()
                        .map_err(|_| invalid_layout("invalid infe item id"))?,
                ),
                Some(10),
            )
        }
        _ => return Err(invalid_layout("unsupported infe version")),
    };
    let Some(type_pos) = type_pos else {
        return Ok((item_id, None, None));
    };
    let item_type = body
        .get(type_pos..type_pos + 4)
        .ok_or_else(|| invalid_layout("infe item type is truncated"))?
        .try_into()
        .map_err(|_| invalid_layout("invalid infe item type"))?;
    let mut cursor = type_pos + 4;
    skip_c_string(body, &mut cursor)?;
    let content_type = if item_type == *b"mime" {
        Some(read_c_string(body, &mut cursor)?.to_string())
    } else if item_type == *b"uri " {
        let _ = read_c_string(body, &mut cursor)?;
        None
    } else {
        None
    };
    Ok((item_id, Some(item_type), content_type))
}

#[allow(
    clippy::indexing_slicing,
    reason = "The terminator position comes from iterating the same slice, so the string range is in bounds."
)]
fn read_c_string<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a str, HeifError> {
    let rest = bytes
        .get(*cursor..)
        .ok_or_else(|| invalid_layout("unterminated HEIC item string"))?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| invalid_layout("unterminated HEIC item string"))?;
    let value = std::str::from_utf8(&rest[..end])
        .map_err(|_| invalid_layout("HEIC item string is not UTF-8"))?;
    *cursor += end + 1;
    Ok(value)
}

fn skip_c_string(bytes: &[u8], cursor: &mut usize) -> Result<(), HeifError> {
    let _ = read_c_string(bytes, cursor)?;
    Ok(())
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the body range, and the explicit minimum lengths prove each fixed iloc field."
)]
pub(super) fn parse_iloc(bytes: &[u8], iloc: RawBox) -> Result<IlocLayout, HeifError> {
    let body = &bytes[iloc.body_start()..iloc.end()];
    if body.len() < 8 {
        return Err(invalid_layout("iloc box is truncated"));
    }
    let version = body[0];
    if version > 2 {
        return Err(invalid_layout("unsupported iloc version"));
    }
    let offset_size = body[4] >> 4;
    let length_size = body[4] & 0x0f;
    let base_offset_size = body[5] >> 4;
    let index_size = if version == 0 { 0 } else { body[5] & 0x0f };
    for size in [offset_size, length_size, base_offset_size, index_size] {
        if !matches!(size, 0 | 4 | 8) {
            return Err(invalid_layout("iloc uses a reserved field width"));
        }
    }
    let (count_pos, count_size, count) = if version == 2 {
        if body.len() < 10 {
            return Err(invalid_layout("iloc version 2 box is truncated"));
        }
        (
            6,
            4,
            u32::from_be_bytes(
                body.get(6..10)
                    .ok_or_else(|| invalid_layout("iloc item count is truncated"))?
                    .try_into()
                    .map_err(|_| invalid_layout("invalid iloc item count"))?,
            ) as u64,
        )
    } else {
        (6, 2, u16::from_be_bytes([body[6], body[7]]) as u64)
    };
    let count =
        usize::try_from(count).map_err(|_| invalid_layout("iloc item count is too large"))?;
    // Every item occupies at least its id, the optional construction and
    // reserved fields, the data-reference index, its base offset, and the
    // extent count. Reject a count that cannot be backed by the remaining
    // body so a crafted iloc cannot force a large speculative allocation.
    let item_id_size = if version == 2 { 4u8 } else { 2 };
    let min_item_bytes = usize::from(item_id_size)
        + usize::from(if version == 0 { 0u8 } else { 2 })
        + 2
        + usize::from(base_offset_size)
        + 2;
    let body_after_count = body.len() - (count_pos + count_size);
    if count > body_after_count / min_item_bytes {
        return Err(invalid_layout("iloc item count exceeds its box body"));
    }
    let mut cursor = count_pos + count_size;
    let mut items = Vec::with_capacity(count);
    let mut item_ids = HashSet::with_capacity(count);
    for _ in 0..count {
        let item_id = u32::try_from(read_uint(body, &mut cursor, item_id_size)?)
            .map_err(|_| invalid_layout("iloc item ID is too large"))?;
        if !item_ids.insert(item_id) {
            return Err(invalid_layout("iloc contains duplicate item IDs"));
        }
        let construction_method = if version == 0 {
            0
        } else {
            let packed = read_uint(body, &mut cursor, 2)?;
            (packed & 0x0f) as u8
        };
        let data_reference_index = u16::try_from(read_uint(body, &mut cursor, 2)?)
            .map_err(|_| invalid_layout("iloc data reference index is too large"))?;
        let base_offset_pos = if base_offset_size == 0 {
            None
        } else {
            Some(cursor)
        };
        let base_offset = read_uint(body, &mut cursor, base_offset_size)?;
        let extent_count = read_uint(body, &mut cursor, 2)?;
        let extent_count = usize::try_from(extent_count)
            .map_err(|_| invalid_layout("iloc extent count is too large"))?;
        // Each extent occupies its optional index plus its offset and length
        // fields (index_size is zero for version 0). When all three widths are
        // zero the extents carry no bytes, so more than one is meaningless;
        // otherwise reject a count the remaining body cannot hold before
        // allocating for it.
        let min_extent_bytes =
            usize::from(index_size) + usize::from(offset_size) + usize::from(length_size);
        let max_extents = match min_extent_bytes {
            0 => 1,
            unit => (body.len() - cursor) / unit,
        };
        if extent_count > max_extents {
            return Err(invalid_layout("iloc extent count exceeds its box body"));
        }
        let mut extents = Vec::with_capacity(extent_count);
        for _ in 0..extent_count {
            if version != 0 {
                let _ = read_uint(body, &mut cursor, index_size)?;
            }
            let offset_pos = if offset_size == 0 { None } else { Some(cursor) };
            let offset = read_uint(body, &mut cursor, offset_size)?;
            let length_pos = if length_size == 0 { None } else { Some(cursor) };
            let length = read_uint(body, &mut cursor, length_size)?;
            extents.push(IlocExtent {
                offset_pos,
                length_pos,
                offset,
                length,
            });
        }
        items.push(IlocItem {
            item_id,
            construction_method,
            data_reference_index,
            base_offset_pos,
            base_offset,
            extents,
        });
    }
    if cursor != body.len() {
        return Err(invalid_layout("iloc contains an unparsed tail"));
    }
    if items.iter().any(|item| item.data_reference_index != 0) {
        return Err(invalid_layout("HEIC item uses an external data reference"));
    }
    Ok(IlocLayout {
        version,
        offset_size,
        length_size,
        base_offset_size,
        index_size,
        count_pos,
        count_size,
        items,
    })
}

fn read_uint(bytes: &[u8], cursor: &mut usize, width: u8) -> Result<u64, HeifError> {
    let width = usize::from(width);
    if width == 0 {
        return Ok(0);
    }
    let end = cursor
        .checked_add(width)
        .ok_or_else(|| invalid_layout("HEIC integer position overflows"))?;
    let value = match width {
        2 => u16::from_be_bytes(
            bytes
                .get(*cursor..end)
                .ok_or_else(|| invalid_layout("truncated HEIC integer"))?
                .try_into()
                .map_err(|_| invalid_layout("invalid HEIC integer"))?,
        ) as u64,
        4 => u32::from_be_bytes(
            bytes
                .get(*cursor..end)
                .ok_or_else(|| invalid_layout("truncated HEIC integer"))?
                .try_into()
                .map_err(|_| invalid_layout("invalid HEIC integer"))?,
        ) as u64,
        8 => u64::from_be_bytes(
            bytes
                .get(*cursor..end)
                .ok_or_else(|| invalid_layout("truncated HEIC integer"))?
                .try_into()
                .map_err(|_| invalid_layout("invalid HEIC integer"))?,
        ),
        _ => return Err(invalid_layout("unsupported HEIC integer width")),
    };
    *cursor = end;
    Ok(value)
}

pub(super) fn write_uint(
    bytes: &mut [u8],
    pos: usize,
    width: u8,
    value: u64,
) -> Result<(), HeifError> {
    let width = usize::from(width);
    if width == 0 {
        if value == 0 {
            return Ok(());
        }
        return Err(invalid_layout(
            "non-zero value cannot use a zero-width field",
        ));
    }
    let end = pos
        .checked_add(width)
        .ok_or_else(|| invalid_layout("HEIC integer position overflows"))?;
    let dst = bytes
        .get_mut(pos..end)
        .ok_or_else(|| invalid_layout("HEIC integer field is truncated"))?;
    match width {
        2 => {
            let value = u16::try_from(value).map_err(|_| HeifError::ValueOverflow {
                field: "16-bit HEIC field",
            })?;
            dst.copy_from_slice(&value.to_be_bytes());
        }
        4 => {
            let value = u32::try_from(value).map_err(|_| HeifError::ValueOverflow {
                field: "32-bit HEIC field",
            })?;
            dst.copy_from_slice(&value.to_be_bytes());
        }
        8 => dst.copy_from_slice(&value.to_be_bytes()),
        _ => return Err(invalid_layout("unsupported HEIC integer width")),
    }
    Ok(())
}

pub(super) fn resolve_item_extents<'a>(
    bytes: &'a [u8],
    item: &IlocItem,
) -> Result<Vec<&'a [u8]>, HeifError> {
    let mut resolved = Vec::with_capacity(item.extents.len());
    for extent in &item.extents {
        let start = item
            .base_offset
            .checked_add(extent.offset)
            .and_then(|offset| usize::try_from(offset).ok())
            .ok_or_else(|| invalid_layout("HEIC item offset overflows"))?;
        let length = usize::try_from(extent.length)
            .map_err(|_| invalid_layout("HEIC item extent is too large"))?;
        let end = start
            .checked_add(length)
            .ok_or_else(|| invalid_layout("HEIC item extent overflows"))?;
        let data = bytes
            .get(start..end)
            .ok_or_else(|| invalid_layout("HEIC item extent is outside the file"))?;
        resolved.push(data);
    }
    Ok(resolved)
}

/// Refuse insertion when a construction-method-0 item's data overlaps the
/// `meta` box. Insertion grows `meta` and relocates everything after it, so an
/// item whose bytes fall inside the region being rewritten cannot be preserved
/// or safely repointed. Such a layout is malformed; fail closed and leave the
/// file untouched rather than emit a file whose item points at changed bytes.
pub(super) fn reject_meta_overlapping_items(
    layout: &IlocLayout,
    meta: RawBox,
) -> Result<(), HeifError> {
    let meta_start = meta.start as u64;
    let meta_end = meta.end() as u64;
    for item in &layout.items {
        if item.construction_method != 0 {
            continue;
        }
        for extent in &item.extents {
            let start = item
                .base_offset
                .checked_add(extent.offset)
                .ok_or_else(|| invalid_layout("HEIC item offset overflows"))?;
            let end = start
                .checked_add(extent.length)
                .ok_or_else(|| invalid_layout("HEIC item extent overflows"))?;
            if start < meta_end && meta_start < end {
                return Err(invalid_layout(
                    "HEIC item data overlaps the meta box and cannot be rewritten",
                ));
            }
        }
    }
    Ok(())
}

fn make_infe(item_id: u32) -> Result<Vec<u8>, HeifError> {
    let mut body = Vec::new();
    body.extend_from_slice(&[if item_id <= u16::MAX as u32 { 2 } else { 3 }, 0, 0, 0]);
    if item_id <= u16::MAX as u32 {
        let item_id = u16::try_from(item_id)
            .map_err(|_| invalid_layout("HEIC item ID does not fit infe version 2"))?;
        body.extend_from_slice(&item_id.to_be_bytes());
    } else {
        body.extend_from_slice(&item_id.to_be_bytes());
    }
    body.extend_from_slice(&0u16.to_be_bytes());
    body.extend_from_slice(b"mime");
    body.extend_from_slice(b"XMP\0");
    body.extend_from_slice(b"application/rdf+xml\0");
    body.push(0);
    box_with_body(*b"infe", &body)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the iinf body range before it is copied for a bounded field update."
)]
pub(super) fn append_iinf_entry(
    bytes: &[u8],
    iinf: RawBox,
    count_pos: usize,
    count_size: usize,
    item_id: u32,
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iinf.body_start()..iinf.end()];
    let entry = make_infe(item_id)?;
    let mut new_body = body.to_vec();
    let mut count_cursor = count_pos;
    let count_size =
        u8::try_from(count_size).map_err(|_| invalid_layout("iinf count width is too large"))?;
    let count = read_uint(new_body.as_slice(), &mut count_cursor, count_size)?;
    let new_count = count
        .checked_add(1)
        .ok_or_else(|| invalid_layout("iinf entry count overflows"))?;
    write_uint(&mut new_body, count_pos, count_size, new_count)?;
    new_body.extend_from_slice(&entry);
    box_with_body(iinf.kind, &new_body)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the iloc body range, and the single-extent check proves the accessed entry."
)]
pub(super) fn rewrite_existing_iloc(
    bytes: &[u8],
    iloc: RawBox,
    layout: &IlocLayout,
    item_id: u32,
    new_offset: u64,
    new_length: u64,
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iloc.body_start()..iloc.end()];
    let mut new_body = body.to_vec();
    let item = layout
        .items
        .iter()
        .find(|item| item.item_id == item_id)
        .ok_or_else(|| invalid_layout("XMP item has no iloc entry"))?;
    if item.construction_method != 0 || item.extents.len() != 1 {
        return Err(invalid_layout("XMP item uses an unsupported iloc layout"));
    }
    let extent = &item.extents[0];
    if let Some(pos) = extent.offset_pos {
        let offset = new_offset
            .checked_sub(item.base_offset)
            .ok_or_else(|| invalid_layout("XMP iloc offset is below its base offset"))?;
        write_uint(&mut new_body, pos, layout.offset_size, offset)?;
    } else if let Some(pos) = item.base_offset_pos {
        write_uint(&mut new_body, pos, layout.base_offset_size, new_offset)?;
    } else if new_offset != 0 {
        return Err(invalid_layout("XMP iloc has no writable offset field"));
    }
    if let Some(pos) = extent.length_pos {
        write_uint(&mut new_body, pos, layout.length_size, new_length)?;
    } else if new_length != 0 {
        return Err(invalid_layout("XMP iloc has no writable length field"));
    }
    box_with_body(iloc.kind, &new_body)
}

pub(super) fn item_extent_is_shared(
    layout: &IlocLayout,
    item_id: u32,
    extent_start: u64,
    extent_length: u64,
) -> bool {
    let Some(extent_end) = extent_start.checked_add(extent_length) else {
        return true;
    };
    layout.items.iter().any(|item| {
        item.item_id != item_id
            && item.construction_method == 0
            && item.extents.iter().any(|extent| {
                let Some(start) = item.base_offset.checked_add(extent.offset) else {
                    return true;
                };
                let Some(end) = start.checked_add(extent.length) else {
                    return true;
                };
                start < extent_end && extent_start < end
            })
    })
}

fn make_new_iloc_entry(
    layout: &IlocLayout,
    item_id: u32,
    data_offset: u64,
    length: u64,
) -> Result<Vec<u8>, HeifError> {
    let id_width = if layout.version == 2 { 4 } else { 2 };
    let mut entry = Vec::new();
    write_uint_vec(&mut entry, id_width, u64::from(item_id))?;
    if layout.version != 0 {
        write_uint_vec(&mut entry, 2, 0)?;
    }
    write_uint_vec(&mut entry, 2, 0)?;
    // The item's file position must land in whichever field can hold it: the
    // extent offset when present, otherwise the base offset. Writing it to a
    // zero-width field would silently drop it and point the item at offset 0.
    let (base_value, offset_value) = if layout.offset_size != 0 {
        (0, data_offset)
    } else if layout.base_offset_size != 0 {
        (data_offset, 0)
    } else {
        return Err(invalid_layout("XMP iloc cannot represent a file offset"));
    };
    write_uint_vec(&mut entry, layout.base_offset_size, base_value)?;
    write_uint_vec(&mut entry, 2, 1)?;
    if layout.version != 0 {
        write_uint_vec(&mut entry, layout.index_size, 0)?;
    }
    write_uint_vec(&mut entry, layout.offset_size, offset_value)?;
    write_uint_vec(&mut entry, layout.length_size, length)?;
    Ok(entry)
}

fn write_uint_vec(bytes: &mut Vec<u8>, width: u8, value: u64) -> Result<(), HeifError> {
    match width {
        0 => {
            if value != 0 {
                return Err(invalid_layout(
                    "non-zero value cannot use a zero-width field",
                ));
            }
        }
        2 => bytes.extend_from_slice(
            &u16::try_from(value)
                .map_err(|_| HeifError::ValueOverflow {
                    field: "16-bit HEIC field",
                })?
                .to_be_bytes(),
        ),
        4 => bytes.extend_from_slice(
            &u32::try_from(value)
                .map_err(|_| HeifError::ValueOverflow {
                    field: "32-bit HEIC field",
                })?
                .to_be_bytes(),
        ),
        8 => bytes.extend_from_slice(&value.to_be_bytes()),
        _ => return Err(invalid_layout("unsupported HEIC integer width")),
    }
    Ok(())
}

/// Whether an extent's absolute start sits at or past the boundary where the
/// appended metadata begins, so it must move with the shift. An offset that
/// overflows `u64` is treated as past the boundary; the shift below uses
/// `checked_add` and rejects an offset it cannot represent rather than writing
/// a wrapped value.
fn extent_at_or_past_boundary(base_offset: u64, extent_offset: u64, boundary: u64) -> bool {
    extent_offset
        .checked_add(base_offset)
        .is_none_or(|absolute| absolute >= boundary)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the iloc body range before the validated field positions are updated."
)]
pub(super) fn append_iloc_entry(
    bytes: &[u8],
    iloc: RawBox,
    layout: &IlocLayout,
    item_id: u32,
    data_offset: u64,
    length: u64,
    delta: u64,
    shift_boundary: u64,
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iloc.body_start()..iloc.end()];
    let mut new_body = body.to_vec();
    for item in &layout.items {
        if item.construction_method != 0 {
            continue;
        }
        let shifted = item
            .extents
            .iter()
            .map(|extent| {
                extent_at_or_past_boundary(item.base_offset, extent.offset, shift_boundary)
            })
            .collect::<Vec<_>>();
        if shifted.iter().any(|shift| *shift) {
            if shifted.iter().all(|shift| *shift) {
                if let Some(pos) = item.base_offset_pos {
                    write_uint(
                        &mut new_body,
                        pos,
                        layout.base_offset_size,
                        item.base_offset
                            .checked_add(delta)
                            .ok_or_else(|| invalid_layout("HEIC iloc base offset overflows"))?,
                    )?;
                } else if item
                    .extents
                    .iter()
                    .any(|extent| extent.offset_pos.is_none())
                {
                    return Err(invalid_layout("HEIC iloc offset cannot be shifted safely"));
                } else {
                    for extent in &item.extents {
                        let shifted_offset = extent
                            .offset
                            .checked_add(delta)
                            .ok_or_else(|| invalid_layout("HEIC iloc offset overflows"))?;
                        if let Some(pos) = extent.offset_pos {
                            write_uint(&mut new_body, pos, layout.offset_size, shifted_offset)?;
                        }
                    }
                }
            } else if item
                .extents
                .iter()
                .any(|extent| extent.offset_pos.is_none())
            {
                return Err(invalid_layout("HEIC iloc extents shift inconsistently"));
            } else {
                for extent in &item.extents {
                    if extent_at_or_past_boundary(item.base_offset, extent.offset, shift_boundary)
                        && let Some(pos) = extent.offset_pos
                    {
                        let shifted_offset = extent
                            .offset
                            .checked_add(delta)
                            .ok_or_else(|| invalid_layout("HEIC iloc offset overflows"))?;
                        write_uint(&mut new_body, pos, layout.offset_size, shifted_offset)?;
                    }
                }
            }
        }
    }
    let mut count_cursor = layout.count_pos;
    let count_size = u8::try_from(layout.count_size)
        .map_err(|_| invalid_layout("iloc count width is too large"))?;
    let count = read_uint(&new_body, &mut count_cursor, count_size)?;
    write_uint(
        &mut new_body,
        layout.count_pos,
        count_size,
        count
            .checked_add(1)
            .ok_or_else(|| invalid_layout("iloc item count overflows"))?,
    )?;
    new_body.extend_from_slice(&make_new_iloc_entry(layout, item_id, data_offset, length)?);
    box_with_body(iloc.kind, &new_body)
}

#[cfg(test)]
mod tests {
    use super::super::HeifError;
    use super::super::boxes::parse_raw_box;
    use super::super::test_support::sbox;
    use super::parse_iloc;

    #[test]
    fn parse_iloc_rejects_item_count_exceeding_body() {
        let mut body = vec![0u8, 0, 0, 0];
        body.push(0x44);
        body.push(0x00);
        body.extend_from_slice(&u16::MAX.to_be_bytes());
        let boxed = sbox(b"iloc", &body);
        let raw = parse_raw_box(&boxed, 0).expect("iloc header");
        let err = parse_iloc(&boxed, raw).unwrap_err();
        assert!(
            matches!(err, HeifError::InvalidLayout { reason } if reason.contains("item count")),
            "a count with no backing bytes must be refused, got {err:?}"
        );
    }

    #[test]
    fn parse_iloc_rejects_extent_count_exceeding_body() {
        let mut body = vec![0u8, 0, 0, 0];
        body.push(0x44);
        body.push(0x00);
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&1u16.to_be_bytes());
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(&u16::MAX.to_be_bytes());
        let boxed = sbox(b"iloc", &body);
        let raw = parse_raw_box(&boxed, 0).expect("iloc header");
        let err = parse_iloc(&boxed, raw).unwrap_err();
        assert!(
            matches!(err, HeifError::InvalidLayout { reason } if reason.contains("extent count")),
            "an extent count with no backing bytes must be refused, got {err:?}"
        );
    }
}
