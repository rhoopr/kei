//! Bounded control-data reads and streaming HEIF replacement ranges.

use super::boxes::{RawBox, find_meta_layout, is_heif_content};
use super::exif::{extract_tiff_capture_time, rewrite_tiff_capture_time};
use super::items::{IlocItem, item_extent_is_shared, parse_iinf, parse_iloc};
use super::relationships::{select_exif_item_id, select_xmp_item_id};
use super::{ExifCaptureTime, HeifError, invalid_layout};
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;

// Reuse the existing writer's 8 MiB retained-control allowance, not a
// format-size limit. Opaque meta children (including image-bearing idat)
// remain file ranges and do not consume this allowance.
pub(super) const MAX_RETAINED_DATA_BYTES: usize = 8 * 1024 * 1024;
const COPY_BUFFER_BYTES: usize = 64 * 1024;

pub(super) struct Patch {
    pub(super) range: Range<usize>,
    pub(super) bytes: Vec<u8>,
}

pub(crate) struct FileLayout {
    // Compact parser view: synthetic meta/hdlr headers and only the four
    // controls used by the existing item/relationship parsers.
    pub(super) control_bytes: Vec<u8>,
    pub(super) meta_box: RawBox,
    pub(super) len: usize,
    controls: Vec<(RawBox, RawBox)>, // compact coordinates, original coordinates
    children_start: usize,
    insertion_safe: bool,
}

fn resource_limit(kind: &'static str, bytes: usize) -> HeifError {
    HeifError::MetadataLimit {
        kind,
        bytes: bytes as u64,
        budget: MAX_RETAINED_DATA_BYTES,
    }
}

fn read_range<R: Read + Seek>(source: &mut R, range: Range<usize>) -> Result<Vec<u8>, HeifError> {
    let len = range
        .end
        .checked_sub(range.start)
        .ok_or_else(|| invalid_layout("metadata range underflows"))?;
    if len > MAX_RETAINED_DATA_BYTES {
        return Err(resource_limit("retained control box", len));
    }
    source.seek(SeekFrom::Start(range.start as u64))?;
    let mut bytes = vec![0; len];
    source.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_box<R: Read + Seek>(
    source: &mut R,
    offset: usize,
    end: usize,
) -> Result<RawBox, HeifError> {
    if end.saturating_sub(offset) < 8 {
        return Err(invalid_layout("truncated ISO-BMFF box header"));
    }
    source.seek(SeekFrom::Start(offset as u64))?;
    let mut header = [0u8; 16];
    source.read_exact(
        header
            .get_mut(..8)
            .ok_or_else(|| invalid_layout("box header"))?,
    )?;
    let size32 = u32::from_be_bytes(
        header
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| invalid_layout("box size"))?,
    );
    let kind = header
        .get(4..8)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid_layout("box kind"))?;
    let (header_size, size) = match size32 {
        0 => {
            return Err(invalid_layout(
                "ISO-BMFF box has no explicit size and cannot be rewritten",
            ));
        }
        1 => {
            if end - offset < 16 {
                return Err(invalid_layout("truncated large ISO-BMFF box header"));
            }
            source.read_exact(
                header
                    .get_mut(8..16)
                    .ok_or_else(|| invalid_layout("box header"))?,
            )?;
            let size = u64::from_be_bytes(
                header
                    .get(8..16)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| invalid_layout("box size"))?,
            );
            (
                16,
                usize::try_from(size).map_err(|_| invalid_layout("ISO-BMFF box is too large"))?,
            )
        }
        size => (8, size as usize),
    };
    offset
        .checked_add(size)
        .filter(|box_end| size >= header_size && *box_end <= end)
        .ok_or_else(|| invalid_layout("ISO-BMFF box extends past the file"))?;
    Ok(RawBox {
        start: offset,
        size,
        header_size,
        kind,
    })
}

impl FileLayout {
    pub(crate) fn read<R: Read + Seek>(source: &mut R, len: u64) -> Result<Self, HeifError> {
        let len = usize::try_from(len).map_err(|_| invalid_layout("HEIF file size overflows"))?;
        let mut signature = [0; 12];
        source.seek(SeekFrom::Start(0))?;
        source.read_exact(&mut signature)?;
        if !is_heif_content(&signature) {
            return Err(invalid_layout("input is not a HEIF-family file"));
        }
        let mut offset = 0;
        let mut meta_box = None;
        let mut insertion_safe = true;
        while offset < len {
            let atom = read_box(source, offset, len)?;
            insertion_safe &= matches!(&atom.kind, b"ftyp" | b"meta" | b"mdat" | b"free" | b"wide");
            if atom.kind == *b"meta" && meta_box.replace(atom).is_some() {
                return Err(invalid_layout("HEIC has multiple top-level meta boxes"));
            }
            offset = atom.end();
        }
        let meta_box = meta_box.ok_or_else(|| invalid_layout("HEIC has no top-level meta box"))?;
        if meta_box.header_size != 8 {
            return Err(invalid_layout("large-size meta boxes are unsupported"));
        }
        let mut prefix = [0; 12];
        source.seek(SeekFrom::Start(meta_box.body_start() as u64))?;
        source.read_exact(&mut prefix)?;
        let prefix_size = if prefix.get(4..8) == Some(b"hdlr") {
            0
        } else if prefix.get(8..12) == Some(b"hdlr") {
            4
        } else {
            return Err(invalid_layout("HEIC meta box has no handler box"));
        };
        let children_start = meta_box.body_start() + prefix_size;
        let mut meta = vec![0; 8];
        meta.get_mut(4..8)
            .ok_or_else(|| invalid_layout("meta header"))?
            .copy_from_slice(b"meta");
        meta.extend_from_slice(
            prefix
                .get(..prefix_size)
                .ok_or_else(|| invalid_layout("meta prefix"))?,
        );
        meta.extend_from_slice(&8u32.to_be_bytes());
        meta.extend_from_slice(b"hdlr");
        let mut controls: Vec<(RawBox, RawBox)> = Vec::new();
        let mut offset = children_start;
        while offset < meta_box.end() {
            let original = read_box(source, offset, meta_box.end())?;
            if matches!(&original.kind, b"pitm" | b"iinf" | b"iloc" | b"iref") {
                if controls
                    .iter()
                    .any(|(_, previous)| previous.kind == original.kind)
                {
                    return Err(invalid_layout("HEIC has duplicate metadata control boxes"));
                }
                let retained = meta
                    .len()
                    .checked_add(original.size)
                    .ok_or_else(|| invalid_layout("retained control size overflows"))?;
                if retained > MAX_RETAINED_DATA_BYTES {
                    return Err(resource_limit("retained rewrite controls", retained));
                }
                let compact = RawBox {
                    start: meta.len(),
                    ..original
                };
                meta.extend_from_slice(&read_range(source, original.start..original.end())?);
                controls.push((compact, original));
            }
            offset = original.end();
        }
        super::boxes::patch_box_size(&mut meta)?;
        let _ = find_meta_layout(&meta)?;
        Ok(Self {
            control_bytes: meta,
            meta_box,
            len,
            controls,
            children_start,
            insertion_safe,
        })
    }

    pub(super) fn ensure_insertion(&self) -> Result<(), HeifError> {
        if !self.insertion_safe {
            return Err(invalid_layout(
                "HEIC insertion refuses top-level boxes with unhandled file offsets",
            ));
        }
        Ok(())
    }

    pub(super) fn in_mdat<R: Read + Seek>(
        &self,
        source: &mut R,
        start: usize,
        len: usize,
    ) -> Result<bool, HeifError> {
        let Some(end) = start.checked_add(len) else {
            return Ok(false);
        };
        let mut offset = 0;
        while offset < self.len {
            let atom = read_box(source, offset, self.len)?;
            if atom.kind == *b"mdat" && start >= atom.body_start() && end <= atom.end() {
                return Ok(true);
            }
            offset = atom.end();
        }
        Ok(false)
    }

    pub(super) fn meta_patch(
        &self,
        range: Range<usize>,
        bytes: Vec<u8>,
    ) -> Result<Patch, HeifError> {
        let (_, original) = self
            .controls
            .iter()
            .find(|(compact, _)| compact.start == range.start && compact.end() == range.end)
            .ok_or_else(|| invalid_layout("rewrite patch is not a complete metadata control"))?;
        Ok(Patch {
            range: original.start..original.end(),
            bytes,
        })
    }

    fn next_opaque<R: Read + Seek>(
        &self,
        source: &mut R,
        cursor: &mut usize,
    ) -> Result<Option<RawBox>, HeifError> {
        while *cursor < self.meta_box.end() {
            let atom = read_box(source, *cursor, self.meta_box.end())?;
            *cursor = atom.end();
            if !matches!(&atom.kind, b"iinf" | b"iloc" | b"iref") {
                return Ok(Some(atom));
            }
        }
        Ok(None)
    }

    pub(super) fn extents(&self, item: &IlocItem) -> Result<Vec<Range<usize>>, HeifError> {
        item.extents
            .iter()
            .map(|extent| {
                let start = item
                    .base_offset
                    .checked_add(extent.offset)
                    .and_then(|start| usize::try_from(start).ok())
                    .ok_or_else(|| invalid_layout("HEIC item offset overflows"))?;
                let length = usize::try_from(extent.length)
                    .map_err(|_| invalid_layout("HEIC item extent is too large"))?;
                let end = start
                    .checked_add(length)
                    .filter(|end| *end <= self.len)
                    .ok_or_else(|| invalid_layout("HEIC item extent is outside the file"))?;
                Ok(start..end)
            })
            .collect()
    }

    fn item_payload<R: Read + Seek>(
        &self,
        source: &mut R,
        item_id: u32,
        kind: &'static str,
    ) -> Result<Vec<u8>, HeifError> {
        let (_, _, iloc, _, _, _) = find_meta_layout(&self.control_bytes)?;
        let items = parse_iloc(&self.control_bytes, iloc)?;
        let item = items
            .items
            .iter()
            .find(|item| item.item_id == item_id)
            .ok_or_else(|| invalid_layout("metadata item has no iloc entry"))?;
        if item.construction_method != 0 || item.data_reference_index != 0 {
            return Err(invalid_layout(
                "metadata item uses an unsupported iloc layout",
            ));
        }
        let extents = self.extents(item)?;
        let len = extents
            .iter()
            .try_fold(0usize, |len, extent| len.checked_add(extent.len()))
            .ok_or_else(|| invalid_layout("metadata extent lengths overflow"))?;
        if len > MAX_RETAINED_DATA_BYTES {
            return Err(resource_limit(kind, len));
        }
        let mut bytes = Vec::with_capacity(len);
        for extent in extents {
            source.seek(SeekFrom::Start(extent.start as u64))?;
            let start = bytes.len();
            bytes.resize(start + extent.len(), 0);
            source.read_exact(
                bytes
                    .get_mut(start..)
                    .ok_or_else(|| invalid_layout("metadata extent"))?,
            )?;
        }
        Ok(bytes)
    }

    pub(crate) fn xmp<R: Read + Seek>(&self, source: &mut R) -> Result<Option<Vec<u8>>, HeifError> {
        let (_, iinf, _, iref, primary, _) = find_meta_layout(&self.control_bytes)?;
        let info = parse_iinf(&self.control_bytes, iinf)?;
        select_xmp_item_id(&self.control_bytes, iref, primary, &info.xmp_item_ids)?
            .map(|id| self.item_payload(source, id, "selected XMP packet"))
            .transpose()
    }

    fn exif_item_id(&self) -> Result<Option<u32>, HeifError> {
        let (_, iinf, _, iref, primary, _) = find_meta_layout(&self.control_bytes)?;
        let info = parse_iinf(&self.control_bytes, iinf)?;
        select_exif_item_id(
            &self.control_bytes,
            iref,
            primary,
            &info.exif_item_ids,
            &info.item_ids,
        )
    }

    pub(crate) fn tiff<R: Read + Seek>(
        &self,
        source: &mut R,
    ) -> Result<Option<Vec<u8>>, HeifError> {
        let Some(id) = self.exif_item_id()? else {
            return Ok(None);
        };
        let mut payload = self.item_payload(
            source,
            id,
            "native Exif item for TIFF parsing and alias validation",
        )?;
        let offset = u32::from_be_bytes(
            payload
                .get(..4)
                .and_then(|b| b.try_into().ok())
                .ok_or_else(|| invalid_layout("Exif item offset is truncated"))?,
        ) as usize;
        let start = offset
            .checked_add(4)
            .filter(|start| *start <= payload.len())
            .ok_or_else(|| invalid_layout("Exif TIFF offset is outside the item"))?;
        payload.drain(..start);
        Ok(Some(payload))
    }

    pub(crate) fn capture_time<R: Read + Seek>(
        &self,
        source: &mut R,
    ) -> Result<Option<ExifCaptureTime>, HeifError> {
        self.tiff(source)?
            .as_deref()
            .map(extract_tiff_capture_time)
            .transpose()
    }

    pub(super) fn capture_patch<R: Read + Seek>(
        &self,
        source: &mut R,
        datetime: &str,
        offset: &str,
        subseconds: Option<&str>,
    ) -> Result<Patch, HeifError> {
        super::relationships::validate_capture_repair_item_ownership(&self.control_bytes)?;
        let id = self
            .exif_item_id()?
            .ok_or_else(|| invalid_layout("HEIF file has no native Exif item"))?;
        let (_, _, iloc, _, _, _) = find_meta_layout(&self.control_bytes)?;
        let items = parse_iloc(&self.control_bytes, iloc)?;
        if items.items.iter().any(|item| item.construction_method == 2) {
            return Err(invalid_layout(
                "HEIF Exif repair cannot prove item-offset dependants remain unchanged",
            ));
        }
        let item = items
            .items
            .iter()
            .find(|item| item.item_id == id)
            .ok_or_else(|| invalid_layout("Exif item has no iloc entry"))?;
        let ranges = self.extents(item)?;
        let range = ranges
            .first()
            .filter(|_| {
                ranges.len() == 1 && item.construction_method == 0 && item.data_reference_index == 0
            })
            .ok_or_else(|| invalid_layout("HEIF Exif item cannot be safely updated in place"))?;
        if item_extent_is_shared(&items, id, range.start as u64, range.len() as u64)
            || !self.in_mdat(source, range.start, range.len())?
        {
            return Err(invalid_layout(
                "HEIF Exif item does not own a writable mdat extent",
            ));
        }
        let mut tiff = self
            .tiff(source)?
            .ok_or_else(|| invalid_layout("HEIF file has no native Exif item"))?;
        if !rewrite_tiff_capture_time(&mut tiff, datetime, offset, subseconds)? {
            return Err(invalid_layout(
                "HEIF Exif item has no writable capture time",
            ));
        }
        Ok(Patch {
            range: range.end - tiff.len()..range.end,
            bytes: tiff,
        })
    }

    pub(super) fn write<R: Read + Seek, W: Write>(
        &self,
        source: &mut R,
        patches: &[Patch],
        writer: &mut W,
    ) -> Result<(), HeifError> {
        let mut cursor = 0;
        for patch in patches {
            if patch.range.start < cursor
                || patch.range.end < patch.range.start
                || patch.range.end > self.len
            {
                return Err(invalid_layout(
                    "HEIF rewrite patches overlap or exceed the file",
                ));
            }
            copy_range(source, cursor..patch.range.start, writer)?;
            writer.write_all(&patch.bytes)?;
            cursor = patch.range.end;
        }
        copy_range(source, cursor..self.len, writer)
    }

    pub(crate) fn rewrite<R: Read + Seek, W: Write>(
        &self,
        source: &mut R,
        xmp: &[u8],
        capture: Option<(&str, &str, Option<&str>)>,
        writer: &mut W,
    ) -> Result<(), HeifError> {
        if xmp.len() > MAX_RETAINED_DATA_BYTES {
            return Err(resource_limit("replacement XMP packet", xmp.len()));
        }
        let mut patches = super::xmp_write::plan_xmp(self, source, xmp)?;
        if let Some((datetime, offset, subseconds)) = capture {
            patches.push(self.capture_patch(source, datetime, offset, subseconds)?);
        }
        patches.sort_by_key(|patch| patch.range.start);
        self.write(source, &patches, writer)
    }

    pub(crate) fn validate<R: Read + Seek, S: Read + Seek>(
        &self,
        source: &mut R,
        output: &Self,
        rewritten: &mut S,
        capture: Option<(&str, &str, Option<&str>)>,
    ) -> Result<(), HeifError> {
        let (_, iinf, iloc, iref, primary, _) = find_meta_layout(&self.control_bytes)?;
        let (_, out_iinf, out_iloc, out_iref, out_primary, _) =
            find_meta_layout(&output.control_bytes)?;
        let info = parse_iinf(&self.control_bytes, iinf)?;
        let out_info = parse_iinf(&output.control_bytes, out_iinf)?;
        let xmp = select_xmp_item_id(&self.control_bytes, iref, primary, &info.xmp_item_ids)?;
        let out_xmp = select_xmp_item_id(
            &output.control_bytes,
            out_iref,
            out_primary,
            &out_info.xmp_item_ids,
        )?;
        let items = parse_iloc(&self.control_bytes, iloc)?;
        let out_items = parse_iloc(&output.control_bytes, out_iloc)?;
        let protected =
            |item: &&IlocItem| item.construction_method == 0 && Some(item.item_id) != xmp;
        if items.items.iter().filter(protected).count()
            != out_items
                .items
                .iter()
                .filter(|item| item.construction_method == 0 && Some(item.item_id) != out_xmp)
                .count()
        {
            return Err(invalid_layout(
                "HEIC rewrite changed the construction-method-0 item set",
            ));
        }
        let patch = capture
            .map(|(datetime, offset, subseconds)| {
                self.capture_patch(source, datetime, offset, subseconds)
            })
            .transpose()?;
        for item in items.items.iter().filter(protected) {
            let out_item = out_items
                .items
                .iter()
                .find(|other| {
                    other.item_id == item.item_id
                        && other.construction_method == 0
                        && Some(other.item_id) != out_xmp
                })
                .ok_or_else(|| invalid_layout("HEIC rewrite removed a non-XMP item"))?;
            let before = self.extents(item)?;
            let after = output.extents(out_item)?;
            if before.len() != after.len() {
                return Err(invalid_layout(
                    "HEIC rewrite changed a non-XMP item payload",
                ));
            }
            for (before, after) in before.iter().zip(&after) {
                compare_ranges(
                    source,
                    before.clone(),
                    rewritten,
                    after.clone(),
                    patch.as_ref(),
                )?;
            }
        }
        compare_ranges(
            source,
            self.meta_box.body_start()..self.children_start,
            rewritten,
            output.meta_box.body_start()..output.children_start,
            None,
        )?;
        let mut before_cursor = self.children_start;
        let mut after_cursor = output.children_start;
        loop {
            match (
                self.next_opaque(source, &mut before_cursor)?,
                output.next_opaque(rewritten, &mut after_cursor)?,
            ) {
                (None, None) => break,
                (Some(before), Some(after))
                    if before.kind == after.kind && before.size == after.size =>
                {
                    compare_ranges(
                        source,
                        before.start..before.end(),
                        rewritten,
                        after.start..after.end(),
                        None,
                    )?;
                }
                _ => {
                    return Err(invalid_layout(
                        "HEIC rewrite changed an opaque meta sub-box",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn copy_range<R: Read + Seek, W: Write>(
    source: &mut R,
    range: Range<usize>,
    writer: &mut W,
) -> Result<(), HeifError> {
    source.seek(SeekFrom::Start(range.start as u64))?;
    let mut remaining = range.len();
    let mut buffer = [0; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let count = remaining.min(buffer.len());
        let bytes = buffer
            .get_mut(..count)
            .ok_or_else(|| invalid_layout("copy buffer"))?;
        source.read_exact(bytes)?;
        writer.write_all(bytes)?;
        remaining -= count;
    }
    Ok(())
}

fn compare_ranges<R: Read + Seek, S: Read + Seek>(
    source: &mut R,
    before: Range<usize>,
    rewritten: &mut S,
    after: Range<usize>,
    patch: Option<&Patch>,
) -> Result<(), HeifError> {
    if before.len() != after.len() {
        return Err(invalid_layout(
            "HEIC rewrite changed a non-XMP item payload",
        ));
    }
    source.seek(SeekFrom::Start(before.start as u64))?;
    rewritten.seek(SeekFrom::Start(after.start as u64))?;
    let mut position = before.start;
    let mut left = [0; COPY_BUFFER_BYTES];
    let mut right = [0; COPY_BUFFER_BYTES];
    while position < before.end {
        let count = (before.end - position).min(left.len());
        let left = left
            .get_mut(..count)
            .ok_or_else(|| invalid_layout("compare buffer"))?;
        let right = right
            .get_mut(..count)
            .ok_or_else(|| invalid_layout("compare buffer"))?;
        source.read_exact(left)?;
        rewritten.read_exact(right)?;
        if let Some(patch) = patch {
            let start = position.max(patch.range.start);
            let end = (position + count).min(patch.range.end);
            if start < end {
                left.get_mut(start - position..end - position)
                    .ok_or_else(|| invalid_layout("patch extent"))?
                    .copy_from_slice(
                        patch
                            .bytes
                            .get(start - patch.range.start..end - patch.range.start)
                            .ok_or_else(|| invalid_layout("patch bytes"))?,
                    );
            }
        }
        if left != right {
            return Err(invalid_layout(
                "HEIC rewrite changed a non-XMP item payload",
            ));
        }
        position += count;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FileLayout, MAX_RETAINED_DATA_BYTES};
    use crate::download::heif::test_support::{MATRIX_XMP, atom};
    use std::io::{Cursor, Seek, SeekFrom, Write};

    #[test]
    fn bounded_metadata_read_refuses_oversized_and_truncated_boxes() {
        let seed = crate::download::heif::apple_multi_exif_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
        );
        for size in [0, 7, 11] {
            assert!(FileLayout::read(&mut Cursor::new(&seed[..size]), size as u64).is_err());
        }
        for tail in [
            [0, 0, 0, 0, b'f', b'r', b'e', b'e'].to_vec(),
            [0, 0, 0, 7, b'f', b'r', b'e', b'e'].to_vec(),
            [0, 0, 0, 16, b'f', b'r', b'e', b'e'].to_vec(),
            [0, 0, 0, 1, b'f', b'r', b'e', b'e'].to_vec(),
        ] {
            let mut input = seed.to_vec();
            input.extend_from_slice(&tail);
            let len = input.len() as u64;
            assert!(FileLayout::read(&mut Cursor::new(&input), len).is_err());
        }
        let directory = tempfile::tempdir().unwrap();
        let mut file = std::fs::File::create(directory.path().join("oversized.heic")).unwrap();
        file.write_all(&atom(b"ftyp", b"heic\0\0\0\0")).unwrap();
        file.write_all(&((MAX_RETAINED_DATA_BYTES + 1) as u32).to_be_bytes())
            .unwrap();
        file.write_all(b"meta").unwrap();
        let len = 16 + MAX_RETAINED_DATA_BYTES as u64 + 1;
        file.set_len(len).unwrap();
        drop(file);
        let mut file = std::fs::File::open(directory.path().join("oversized.heic")).unwrap();
        let (_, heap) = crate::test_helpers::allocation::measure_heap_high_water(|| {
            assert!(FileLayout::read(&mut file, len).is_err());
        });
        assert!(heap.largest_allocation < 1024 * 1024, "{heap:?}");
    }

    #[test]
    fn metadata_after_large_sparse_media_uses_absolute_offsets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("after-media.heic");
        let prefix = atom(b"ftyp", b"heic\0\0\0\0");
        let seed = crate::download::heif::apple_multi_exif_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
        );
        let layout = FileLayout::read(&mut Cursor::new(&seed), seed.len() as u64).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(&prefix).unwrap();
        let free_size = u32::MAX as u64 + 16;
        file.write_all(&1u32.to_be_bytes()).unwrap();
        file.write_all(b"free").unwrap();
        file.write_all(&free_size.to_be_bytes()).unwrap();
        file.seek(SeekFrom::Start(prefix.len() as u64 + free_size))
            .unwrap();
        file.write_all(&layout.control_bytes).unwrap();
        let len = file.stream_position().unwrap();
        drop(file);
        let mut file = std::fs::File::open(path).unwrap();
        let (parsed, heap) = crate::test_helpers::allocation::measure_heap_high_water(|| {
            FileLayout::read(&mut file, len).unwrap()
        });
        assert_eq!(
            parsed.meta_box.start as u64,
            prefix.len() as u64 + free_size
        );
        assert_eq!(parsed.control_bytes, layout.control_bytes);
        assert!(
            heap.largest_allocation < MAX_RETAINED_DATA_BYTES,
            "{heap:?}"
        );
        // This input retains 32-bit iloc fields. Appending XMP beyond their
        // range fails before the first byte is written, rather than wrapping.
        let mut output = Vec::new();
        assert!(
            parsed
                .rewrite(&mut file, MATRIX_XMP, None, &mut output)
                .is_err()
        );
        assert!(output.is_empty());
    }

    #[test]
    fn streaming_checks_corrupted_payloads_opaque_metadata_and_io_errors() {
        let input = crate::download::heif::apple_multi_exif_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
        );
        let before = FileLayout::read(&mut Cursor::new(&input), input.len() as u64).unwrap();
        let mut output = Vec::new();
        before
            .rewrite(&mut Cursor::new(&input), MATRIX_XMP, None, &mut output)
            .unwrap();
        let after = FileLayout::read(&mut Cursor::new(&output), output.len() as u64).unwrap();
        before
            .validate(
                &mut Cursor::new(&input),
                &after,
                &mut Cursor::new(&output),
                None,
            )
            .unwrap();
        let (_, _, iloc, _, _, _) =
            super::super::boxes::find_meta_layout(&after.control_bytes).unwrap();
        let items = super::super::items::parse_iloc(&after.control_bytes, iloc).unwrap();
        let image = items.items.iter().find(|item| item.item_id == 2).unwrap();
        let range = after.extents(image).unwrap().remove(0);
        output[range.start] ^= 1;
        assert!(
            before
                .validate(
                    &mut Cursor::new(&input),
                    &after,
                    &mut Cursor::new(&output),
                    None
                )
                .is_err()
        );
        output[range.start] ^= 1;
        let (meta, _, _, _, _, prefix) = super::super::boxes::find_meta_layout(&output).unwrap();
        let opaque =
            super::super::boxes::scan_raw_boxes(&output, meta.body_start() + prefix, meta.end())
                .unwrap()
                .into_iter()
                .find(|atom| atom.kind == *b"hdlr")
                .unwrap();
        output[opaque.body_start()] ^= 1;
        let changed_meta =
            FileLayout::read(&mut Cursor::new(&output), output.len() as u64).unwrap();
        assert!(
            before
                .validate(
                    &mut Cursor::new(&input),
                    &changed_meta,
                    &mut Cursor::new(&output),
                    None
                )
                .is_err()
        );
        struct FailingWriter;
        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("injected writer failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(
            before
                .rewrite(
                    &mut Cursor::new(&input),
                    MATRIX_XMP,
                    None,
                    &mut FailingWriter
                )
                .is_err()
        );
        assert!(
            before
                .rewrite(
                    &mut Cursor::new(&input[..input.len() - 1]),
                    MATRIX_XMP,
                    None,
                    &mut Vec::new()
                )
                .is_err()
        );
    }
    #[test]
    fn oversized_selected_xmp_and_exif_refuse_before_packet_allocation() {
        use crate::download::heif::test_support::{
            PRIMARY_XMP, apple_multi_exif_heic, apple_multi_xmp_heic, set_test_file_item_extent,
        };
        for (is_xmp, item_id, mut prefix) in [
            (
                true,
                6,
                apple_multi_xmp_heic(Some(PRIMARY_XMP), PRIMARY_XMP),
            ),
            (
                false,
                6,
                apple_multi_exif_heic(&crate::test_helpers::minimal_tiff_with_source_gps()),
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("oversized.heic");
            let start = prefix.len() + 8;
            let packet_len = MAX_RETAINED_DATA_BYTES + 1;
            set_test_file_item_extent(&mut prefix, item_id, start as u32, packet_len as u32);
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(&prefix).unwrap();
            file.write_all(&((packet_len + 8) as u32).to_be_bytes())
                .unwrap();
            file.write_all(b"mdat").unwrap();
            let len = start + packet_len;
            file.set_len(len as u64).unwrap();
            drop(file);
            let mut file = std::fs::File::open(&path).unwrap();
            let layout = FileLayout::read(&mut file, len as u64).unwrap();
            let (_, heap) = crate::test_helpers::allocation::measure_heap_high_water(|| {
                let packet = if is_xmp {
                    layout.xmp(&mut file)
                } else {
                    layout.tiff(&mut file)
                };
                assert!(matches!(
                    packet,
                    Err(super::HeifError::MetadataLimit { .. })
                ));
                let error = crate::download::metadata::probe_exif(&path).unwrap_err();
                assert!(matches!(
                    error.downcast_ref::<super::HeifError>(),
                    Some(super::HeifError::MetadataLimit { .. })
                ));
            });
            assert!(heap.largest_allocation < 1024 * 1024, "{heap:?}");
            assert_eq!(std::fs::metadata(&path).unwrap().len(), len as u64);
        }
    }

    #[test]
    fn protected_range_comparison_detects_corruption_after_first_buffer() {
        let source = vec![7u8; super::COPY_BUFFER_BYTES * 2 + 33];
        let mut rewritten = source.clone();
        let len = source.len();
        for offset in [
            super::COPY_BUFFER_BYTES - 1,
            super::COPY_BUFFER_BYTES,
            len - 1,
        ] {
            rewritten[offset] ^= 1;
            assert!(
                super::compare_ranges(
                    &mut Cursor::new(&source),
                    0..len,
                    &mut Cursor::new(&rewritten),
                    0..len,
                    None
                )
                .is_err()
            );
            rewritten[offset] ^= 1;
        }
        super::compare_ranges(
            &mut Cursor::new(&source),
            0..len,
            &mut Cursor::new(&rewritten),
            0..len,
            None,
        )
        .unwrap();
    }
}
