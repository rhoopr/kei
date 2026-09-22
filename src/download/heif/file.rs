//! Bounded file-backed Exif location without loading media payloads.

use super::HeifExifError;
use std::collections::HashSet;

#[derive(Clone, Copy)]
struct FileAtom {
    kind: [u8; 4],
    body_start: u64,
    end: u64,
}

#[derive(Default)]
struct FileMetaControls {
    pitm: Option<FileAtom>,
    iinf: Option<FileAtom>,
    iloc: Option<FileAtom>,
    iref: Option<FileAtom>,
}

struct FileItemIds {
    known: HashSet<u32>,
    exif: HashSet<u32>,
}

const MAX_FILE_META_BODY_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) fn locate_exif_tiff<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    file_len: u64,
) -> Result<Option<(u64, u64)>, HeifExifError> {
    let mut offset = 0_u64;
    let mut meta = None;
    while let Some(atom) = read_file_atom(source, offset, file_len)? {
        if atom.kind == *b"meta" {
            if meta.is_some() {
                return Err(HeifExifError::Malformed);
            }
            meta = Some(atom);
        }
        offset = atom.end;
    }
    let Some(meta) = meta else {
        return Ok(None);
    };
    if meta.end - meta.body_start > MAX_FILE_META_BODY_BYTES {
        return Err(HeifExifError::Malformed);
    }

    let controls = locate_meta_control_atoms(source, meta)?;
    let (Some(iinf), Some(iloc)) = (controls.iinf, controls.iloc) else {
        return Ok(None);
    };
    let item_ids = read_item_ids(source, iinf)?;
    let Some(pitm) = controls.pitm else {
        return Err(HeifExifError::Malformed);
    };
    let primary_item_id = read_primary_item_id(source, pitm)?;
    if !item_ids.known.contains(&primary_item_id) {
        return Err(HeifExifError::Malformed);
    }
    let item_id = match controls.iref {
        Some(iref) => select_file_exif_item(source, iref, primary_item_id, &item_ids)?,
        None => select_unattributed_exif_item(&item_ids, &HashSet::new())?,
    };
    let Some(item_id) = item_id else {
        return Ok(None);
    };
    let Some((base_offset, extent_offset, extent_length)) =
        read_exif_item_location(source, iloc, item_id)?
    else {
        return Ok(None);
    };

    let extent_start = base_offset
        .checked_add(extent_offset)
        .ok_or(HeifExifError::Malformed)?;
    let extent_end = extent_start
        .checked_add(extent_length)
        .filter(|end| *end <= file_len)
        .ok_or(HeifExifError::Malformed)?;
    if extent_length < 4 {
        return Err(HeifExifError::Malformed);
    }
    let mut prefix = [0_u8; 4];
    read_file_exact(source, file_len, extent_start, &mut prefix)?;
    let tiff_offset = u64::from(u32::from_be_bytes(prefix));
    let tiff_start = extent_start
        .checked_add(4)
        .and_then(|start| start.checked_add(tiff_offset))
        .filter(|start| *start <= extent_end)
        .ok_or(HeifExifError::Malformed)?;
    let tiff_len = extent_end
        .checked_sub(tiff_start)
        .ok_or(HeifExifError::Malformed)?;
    Ok(Some((tiff_start, tiff_len)))
}

fn locate_meta_control_atoms<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    meta: FileAtom,
) -> Result<FileMetaControls, HeifExifError> {
    let mut prefix = [0_u8; 8];
    let prefix_len = usize::try_from((meta.end - meta.body_start).min(8))
        .map_err(|_error| HeifExifError::Malformed)?;
    let prefix_output = prefix
        .get_mut(..prefix_len)
        .ok_or(HeifExifError::Malformed)?;
    read_file_exact(source, meta.end, meta.body_start, prefix_output)?;
    let mut offset = if prefix.get(4..8) == Some(b"hdlr".as_slice()) {
        meta.body_start
    } else {
        meta.body_start
            .checked_add(4)
            .filter(|offset| *offset <= meta.end)
            .ok_or(HeifExifError::Malformed)?
    };
    let Some(handler) = read_file_atom(source, offset, meta.end)? else {
        return Ok(FileMetaControls::default());
    };
    if handler.kind != *b"hdlr" {
        return Err(HeifExifError::Malformed);
    }
    offset = handler.end;

    let mut controls = FileMetaControls::default();
    while let Some(atom) = read_file_atom(source, offset, meta.end)? {
        match &atom.kind {
            b"pitm" if controls.pitm.replace(atom).is_some() => {
                return Err(HeifExifError::Malformed);
            }
            b"iinf" if controls.iinf.replace(atom).is_some() => {
                return Err(HeifExifError::Malformed);
            }
            b"iloc" if controls.iloc.replace(atom).is_some() => {
                return Err(HeifExifError::Malformed);
            }
            b"iref" if controls.iref.replace(atom).is_some() => {
                return Err(HeifExifError::Malformed);
            }
            _ => {}
        }
        offset = atom.end;
    }
    Ok(controls)
}

fn read_primary_item_id<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    pitm: FileAtom,
) -> Result<u32, HeifExifError> {
    let mut body = [0_u8; 8];
    let available = usize::try_from((pitm.end - pitm.body_start).min(body.len() as u64))
        .map_err(|_error| HeifExifError::Malformed)?;
    let output = body.get_mut(..available).ok_or(HeifExifError::Malformed)?;
    read_file_exact(source, pitm.end, pitm.body_start, output)?;
    match body.first().copied() {
        Some(0) if available >= 6 => Ok(u32::from(u16::from_be_bytes([body[4], body[5]]))),
        Some(1) if available >= 8 => Ok(u32::from_be_bytes([body[4], body[5], body[6], body[7]])),
        _ => Err(HeifExifError::Malformed),
    }
}

fn read_item_ids<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    iinf: FileAtom,
) -> Result<FileItemIds, HeifExifError> {
    let mut full_box = [0_u8; 8];
    read_file_exact(source, iinf.end, iinf.body_start, &mut full_box[..6])?;
    let version = full_box[0];
    let (entry_count, mut offset) = if version == 0 {
        (
            u32::from(u16::from_be_bytes([full_box[4], full_box[5]])),
            iinf.body_start
                .checked_add(6)
                .ok_or(HeifExifError::Malformed)?,
        )
    } else if version == 1 {
        read_file_exact(source, iinf.end, iinf.body_start, &mut full_box)?;
        (
            u32::from_be_bytes([full_box[4], full_box[5], full_box[6], full_box[7]]),
            iinf.body_start
                .checked_add(8)
                .ok_or(HeifExifError::Malformed)?,
        )
    } else {
        return Err(HeifExifError::Malformed);
    };

    let mut known = HashSet::new();
    let mut exif = HashSet::new();
    for _ in 0..entry_count {
        let Some(entry) = read_file_atom(source, offset, iinf.end)? else {
            return Err(HeifExifError::Malformed);
        };
        if entry.kind != *b"infe" {
            return Err(HeifExifError::Malformed);
        }
        let mut fields = [0_u8; 14];
        let available = usize::try_from((entry.end - entry.body_start).min(fields.len() as u64))
            .map_err(|_error| HeifExifError::Malformed)?;
        let fields_output = fields
            .get_mut(..available)
            .ok_or(HeifExifError::Malformed)?;
        read_file_exact(source, entry.end, entry.body_start, fields_output)?;
        let (item_id, item_type_offset) = match fields.first().copied() {
            Some(0) if available >= 6 => {
                (u32::from(u16::from_be_bytes([fields[4], fields[5]])), None)
            }
            Some(1) => return Err(HeifExifError::Malformed),
            Some(2) if available >= 12 => (
                u32::from(u16::from_be_bytes([fields[4], fields[5]])),
                Some(8),
            ),
            Some(3) if available >= 14 => (
                u32::from_be_bytes([fields[4], fields[5], fields[6], fields[7]]),
                Some(10),
            ),
            _ => {
                offset = entry.end;
                continue;
            }
        };
        if !known.insert(item_id) {
            return Err(HeifExifError::Malformed);
        }
        if item_type_offset
            .is_some_and(|start| fields.get(start..start + 4) == Some(b"Exif".as_slice()))
        {
            exif.insert(item_id);
        }
        offset = entry.end;
    }
    if offset != iinf.end {
        return Err(HeifExifError::Malformed);
    }
    Ok(FileItemIds { known, exif })
}

fn select_unattributed_exif_item(
    item_ids: &FileItemIds,
    attributed: &HashSet<u32>,
) -> Result<Option<u32>, HeifExifError> {
    let mut candidates = item_ids
        .exif
        .iter()
        .copied()
        .filter(|item_id| !attributed.contains(item_id));
    match (candidates.next(), candidates.next()) {
        (None, _) => Ok(None),
        (Some(item_id), None) => Ok(Some(item_id)),
        (Some(_), Some(_)) => Err(HeifExifError::Malformed),
    }
}

fn select_file_exif_item<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    iref: FileAtom,
    primary_item_id: u32,
    item_ids: &FileItemIds,
) -> Result<Option<u32>, HeifExifError> {
    let mut full_box = [0_u8; 4];
    read_file_exact(source, iref.end, iref.body_start, &mut full_box)?;
    let version = full_box[0];
    let id_width = match version {
        0 => 2_u8,
        1 => 4_u8,
        _ => return Err(HeifExifError::Malformed),
    };
    let mut offset = iref
        .body_start
        .checked_add(4)
        .ok_or(HeifExifError::Malformed)?;
    let mut attributed = HashSet::new();
    let mut primary_exif = None;
    while let Some(reference) = read_file_atom(source, offset, iref.end)? {
        if reference.kind == *b"cdsc" {
            let mut cursor = reference.body_start;
            let from_item_id = u32::try_from(read_sized_integer(
                source,
                reference.end,
                &mut cursor,
                id_width,
            )?)
            .map_err(|_error| HeifExifError::Malformed)?;
            if !item_ids.known.contains(&from_item_id) {
                return Err(HeifExifError::Malformed);
            }
            let count = u16::try_from(read_sized_integer(source, reference.end, &mut cursor, 2)?)
                .map_err(|_error| HeifExifError::Malformed)?;
            let is_exif = item_ids.exif.contains(&from_item_id);
            let mut describes_primary = false;
            for _ in 0..count {
                let target = u32::try_from(read_sized_integer(
                    source,
                    reference.end,
                    &mut cursor,
                    id_width,
                )?)
                .map_err(|_error| HeifExifError::Malformed)?;
                if !item_ids.known.contains(&target) {
                    return Err(HeifExifError::Malformed);
                }
                describes_primary |= is_exif && target == primary_item_id;
            }
            if cursor != reference.end {
                return Err(HeifExifError::Malformed);
            }
            if is_exif {
                attributed.insert(from_item_id);
            }
            if describes_primary
                && primary_exif
                    .replace(from_item_id)
                    .is_some_and(|existing| existing != from_item_id)
            {
                return Err(HeifExifError::Malformed);
            }
        }
        offset = reference.end;
    }
    if primary_exif.is_some() {
        Ok(primary_exif)
    } else {
        select_unattributed_exif_item(item_ids, &attributed)
    }
}

fn read_exif_item_location<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    iloc: FileAtom,
    target_item_id: u32,
) -> Result<Option<(u64, u64, u64)>, HeifExifError> {
    let mut header = [0_u8; 10];
    read_file_exact(source, iloc.end, iloc.body_start, &mut header[..8])?;
    let version = header[0];
    if version > 2 {
        return Err(HeifExifError::Malformed);
    }
    let offset_size = header[4] >> 4;
    let length_size = header[4] & 0x0F;
    let base_offset_size = header[5] >> 4;
    let index_size = if version == 0 { 0 } else { header[5] & 0x0F };
    for size in [offset_size, length_size, base_offset_size, index_size] {
        if !matches!(size, 0 | 4 | 8) {
            return Err(HeifExifError::Malformed);
        }
    }
    let (item_count, mut cursor) = if version < 2 {
        (
            u32::from(u16::from_be_bytes([header[6], header[7]])),
            iloc.body_start
                .checked_add(8)
                .ok_or(HeifExifError::Malformed)?,
        )
    } else {
        read_file_exact(source, iloc.end, iloc.body_start, &mut header)?;
        (
            u32::from_be_bytes([header[6], header[7], header[8], header[9]]),
            iloc.body_start
                .checked_add(10)
                .ok_or(HeifExifError::Malformed)?,
        )
    };

    let mut found = None;
    for _ in 0..item_count {
        let item_id = u32::try_from(read_sized_integer(
            source,
            iloc.end,
            &mut cursor,
            if version < 2 { 2 } else { 4 },
        )?)
        .map_err(|_error| HeifExifError::Malformed)?;
        let construction_method = if version == 0 {
            0
        } else {
            u8::try_from(read_sized_integer(source, iloc.end, &mut cursor, 2)? & 0x0F)
                .map_err(|_error| HeifExifError::Malformed)?
        };
        let data_reference_index =
            u16::try_from(read_sized_integer(source, iloc.end, &mut cursor, 2)?)
                .map_err(|_error| HeifExifError::Malformed)?;
        let base_offset = read_sized_integer(source, iloc.end, &mut cursor, base_offset_size)?;
        let extent_count = u16::try_from(read_sized_integer(source, iloc.end, &mut cursor, 2)?)
            .map_err(|_error| HeifExifError::Malformed)?;
        let per_extent_size = u64::from(index_size)
            .checked_add(u64::from(offset_size))
            .and_then(|size| size.checked_add(u64::from(length_size)))
            .ok_or(HeifExifError::Malformed)?;
        if extent_count > 0 && per_extent_size == 0 {
            return Err(HeifExifError::Malformed);
        }
        cursor
            .checked_add(
                u64::from(extent_count)
                    .checked_mul(per_extent_size)
                    .ok_or(HeifExifError::Malformed)?,
            )
            .filter(|end| *end <= iloc.end)
            .ok_or(HeifExifError::Malformed)?;
        let mut selected_extent = None;
        for extent_index in 0..extent_count {
            let item_reference_index =
                read_sized_integer(source, iloc.end, &mut cursor, index_size)?;
            let extent_offset = read_sized_integer(source, iloc.end, &mut cursor, offset_size)?;
            let extent_length = read_sized_integer(source, iloc.end, &mut cursor, length_size)?;
            if item_id == target_item_id && extent_index == 0 {
                if item_reference_index != 0 {
                    return Err(HeifExifError::Malformed);
                }
                selected_extent = Some((extent_offset, extent_length));
            }
        }
        if item_id == target_item_id {
            if found.is_some()
                || construction_method != 0
                || data_reference_index != 0
                || extent_count != 1
            {
                return Err(HeifExifError::Malformed);
            }
            let Some((extent_offset, extent_length)) = selected_extent else {
                return Err(HeifExifError::Malformed);
            };
            found = Some((base_offset, extent_offset, extent_length));
        }
    }
    if cursor != iloc.end {
        return Err(HeifExifError::Malformed);
    }
    Ok(found)
}

fn read_sized_integer<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    parent_end: u64,
    cursor: &mut u64,
    size: u8,
) -> Result<u64, HeifExifError> {
    let mut bytes = [0_u8; 8];
    let size = usize::from(size);
    if size == 0 {
        return Ok(0);
    }
    let start = 8_usize.checked_sub(size).ok_or(HeifExifError::Malformed)?;
    let output = bytes.get_mut(start..).ok_or(HeifExifError::Malformed)?;
    read_file_exact(source, parent_end, *cursor, output)?;
    *cursor = cursor
        .checked_add(u64::try_from(size).map_err(|_error| HeifExifError::Malformed)?)
        .ok_or(HeifExifError::Malformed)?;
    Ok(u64::from_be_bytes(bytes))
}

fn read_file_atom<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    offset: u64,
    parent_end: u64,
) -> Result<Option<FileAtom>, HeifExifError> {
    if offset == parent_end {
        return Ok(None);
    }
    let mut header = [0_u8; 16];
    read_file_exact(source, parent_end, offset, &mut header[..8])?;
    let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let kind = [header[4], header[5], header[6], header[7]];
    let (header_len, total_len) = match size32 {
        0 => (8_u64, parent_end - offset),
        1 => {
            read_file_exact(source, parent_end, offset, &mut header)?;
            let extended_size: [u8; 8] = header
                .get(8..16)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(HeifExifError::Malformed)?;
            (16, u64::from_be_bytes(extended_size))
        }
        size => (8, u64::from(size)),
    };
    if total_len < header_len {
        return Err(HeifExifError::Malformed);
    }
    let end = offset
        .checked_add(total_len)
        .filter(|end| *end <= parent_end)
        .ok_or(HeifExifError::Malformed)?;
    let body_start = offset
        .checked_add(header_len)
        .ok_or(HeifExifError::Malformed)?;
    Ok(Some(FileAtom {
        kind,
        body_start,
        end,
    }))
}

fn read_file_exact<R: std::io::Read + std::io::Seek>(
    source: &mut R,
    parent_end: u64,
    offset: u64,
    output: &mut [u8],
) -> Result<(), HeifExifError> {
    let output_len = u64::try_from(output.len()).map_err(|_error| HeifExifError::Malformed)?;
    offset
        .checked_add(output_len)
        .filter(|end| *end <= parent_end)
        .ok_or(HeifExifError::Malformed)?;
    source.seek(std::io::SeekFrom::Start(offset))?;
    source.read_exact(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::HeifExifError;
    use super::super::test_support::{
        apple_multi_exif_heic, apple_multi_xmp_spec, atom, build_heic, exif_iinf_entries,
        exif_iloc, exif_iloc_options, exif_infe, exif_item, heif_with_exif, heif_with_exif_options,
        ref_box, set_test_primary_item_id,
    };
    use super::{
        FileAtom, locate_exif_tiff, read_exif_item_location, read_file_atom, read_file_exact,
        read_item_ids,
    };
    use std::collections::HashSet;

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
    fn locate_exif_tiff_streams_control_data_and_honours_prefix_offset() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let bytes = heif_with_exif(&tiff, 7, 1, 1024 * 1024);
        let len = bytes.len() as u64;
        let mut source = CountingCursor {
            inner: std::io::Cursor::new(bytes),
            bytes_read: 0,
        };

        let (start, tiff_len) = locate_exif_tiff(&mut source, len)
            .expect("locate HEIF EXIF")
            .expect("HEIF EXIF extent");
        let mut actual = vec![0_u8; tiff.len()];
        read_file_exact(&mut source, len, start, &mut actual).expect("read located TIFF");

        assert_eq!(tiff_len, tiff.len() as u64);
        assert_eq!(actual, tiff);
        assert!(
            source.bytes_read < 512,
            "HEIF EXIF location read {} bytes",
            source.bytes_read
        );
    }

    #[test]
    fn locate_exif_tiff_selects_the_primary_associated_exif_item() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let bytes = apple_multi_exif_heic(&tiff);
        let len = bytes.len() as u64;
        let mut source = std::io::Cursor::new(bytes);

        let (start, tiff_len) = locate_exif_tiff(&mut source, len)
            .expect("locate primary HEIF Exif")
            .expect("primary HEIF Exif extent");
        let mut actual = vec![0_u8; usize::try_from(tiff_len).expect("TIFF length")];
        read_file_exact(&mut source, len, start, &mut actual).expect("read primary TIFF");

        assert_eq!(actual, tiff);
    }

    #[test]
    fn locate_exif_tiff_rejects_ambiguous_and_dangling_associations() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let mut ambiguous = apple_multi_xmp_spec(
            Vec::new(),
            vec![ref_box(b"cdsc", 5, &[1]), ref_box(b"cdsc", 6, &[1])],
        );
        ambiguous
            .items
            .extend([exif_item(5, &tiff), exif_item(6, &tiff)]);

        let mut dangling = apple_multi_xmp_spec(Vec::new(), vec![ref_box(b"cdsc", 5, &[99])]);
        dangling.items.push(exif_item(5, &tiff));
        let mut dangling_source = apple_multi_xmp_spec(
            Vec::new(),
            vec![ref_box(b"cdsc", 6, &[4]), ref_box(b"cdsc", 99, &[1])],
        );
        dangling_source
            .items
            .extend([exif_item(5, &tiff), exif_item(6, b"MM\0*")]);

        for (name, bytes) in [
            ("ambiguous primary Exif", build_heic(&ambiguous)),
            ("dangling Exif target", build_heic(&dangling)),
            ("dangling descriptive source", build_heic(&dangling_source)),
        ] {
            let len = bytes.len() as u64;
            assert!(
                matches!(
                    locate_exif_tiff(&mut std::io::Cursor::new(bytes), len),
                    Err(HeifExifError::Malformed)
                ),
                "{name}"
            );
        }

        for (name, mut bytes) in [
            (
                "dangling primary without references",
                heif_with_exif(&tiff, 0, 1, 0),
            ),
            (
                "dangling primary with references",
                apple_multi_exif_heic(&tiff),
            ),
        ] {
            set_test_primary_item_id(&mut bytes, 99);
            let len = bytes.len() as u64;
            assert!(
                matches!(
                    locate_exif_tiff(&mut std::io::Cursor::new(bytes), len),
                    Err(HeifExifError::Malformed)
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn locate_exif_tiff_rejects_out_of_file_and_multiple_extents() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let mut out_of_file = heif_with_exif(&tiff, 0, 1, 0);
        let iloc_kind = out_of_file
            .windows(4)
            .position(|window| window == b"iloc")
            .expect("iloc atom");
        out_of_file[iloc_kind + 22..iloc_kind + 26].copy_from_slice(&u32::MAX.to_be_bytes());
        let len = out_of_file.len() as u64;
        assert!(matches!(
            locate_exif_tiff(&mut std::io::Cursor::new(out_of_file), len),
            Err(HeifExifError::Malformed)
        ));

        let multiple = heif_with_exif(&tiff, 0, 2, 0);
        let len = multiple.len() as u64;
        assert!(matches!(
            locate_exif_tiff(&mut std::io::Cursor::new(multiple), len),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn locate_exif_tiff_rejects_missing_primary_item() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let bytes = heif_with_exif_options(&tiff, 0, 1, 0, false);
        let len = bytes.len() as u64;

        assert!(matches!(
            locate_exif_tiff(&mut std::io::Cursor::new(bytes), len),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn read_exif_item_location_supports_versions_and_rejects_non_file_locations() {
        for version in 0..=2 {
            let bytes = exif_iloc_options(123, 45, 1, version, 0, 0);
            let atom = FileAtom {
                kind: *b"iloc",
                body_start: 8,
                end: bytes.len() as u64,
            };
            assert_eq!(
                read_exif_item_location(&mut std::io::Cursor::new(bytes), atom, 1)
                    .expect("parse iloc"),
                Some((0, 123, 45))
            );
        }

        for (construction_method, data_reference_index) in [(1, 0), (0, 1)] {
            let bytes = exif_iloc_options(123, 45, 1, 1, construction_method, data_reference_index);
            let atom = FileAtom {
                kind: *b"iloc",
                body_start: 8,
                end: bytes.len() as u64,
            };
            assert!(matches!(
                read_exif_item_location(&mut std::io::Cursor::new(bytes), atom, 1),
                Err(HeifExifError::Malformed)
            ));
        }
    }

    #[test]
    fn read_exif_item_location_rejects_zero_width_extents_before_iteration() {
        let mut body = vec![0, 0, 0, 0, 0, 0];
        body.extend_from_slice(&1_u16.to_be_bytes());
        body.extend_from_slice(&1_u16.to_be_bytes());
        body.extend_from_slice(&0_u16.to_be_bytes());
        body.extend_from_slice(&u16::MAX.to_be_bytes());
        let bytes = atom(b"iloc", &body);
        let atom = FileAtom {
            kind: *b"iloc",
            body_start: 8,
            end: bytes.len() as u64,
        };

        assert!(matches!(
            read_exif_item_location(&mut std::io::Cursor::new(bytes), atom, 1),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn read_exif_item_location_rejects_unparsed_tail() {
        let mut bytes = exif_iloc(123, 45, 1);
        bytes.extend_from_slice(&[0_u8; 4]);
        let size = u32::try_from(bytes.len()).expect("iloc size");
        bytes[..4].copy_from_slice(&size.to_be_bytes());
        let atom = FileAtom {
            kind: *b"iloc",
            body_start: 8,
            end: bytes.len() as u64,
        };

        assert!(matches!(
            read_exif_item_location(&mut std::io::Cursor::new(bytes), atom, 1),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn read_item_ids_supports_versions_and_rejects_duplicate_ids() {
        for (version, item_id) in [(2, 1), (3, 70_000)] {
            let bytes = exif_iinf_entries(&[exif_infe(item_id, version)]);
            let atom = FileAtom {
                kind: *b"iinf",
                body_start: 8,
                end: bytes.len() as u64,
            };
            let ids = read_item_ids(&mut std::io::Cursor::new(bytes), atom).expect("parse iinf");
            assert_eq!(ids.exif, HashSet::from([item_id]));
            assert_eq!(ids.known, HashSet::from([item_id]));
        }

        let duplicate = exif_iinf_entries(&[exif_infe(1, 2), exif_infe(1, 2)]);
        let atom = FileAtom {
            kind: *b"iinf",
            body_start: 8,
            end: duplicate.len() as u64,
        };
        assert!(matches!(
            read_item_ids(&mut std::io::Cursor::new(duplicate), atom),
            Err(HeifExifError::Malformed)
        ));

        let mut hidden = exif_iinf_entries(&[exif_infe(1, 2), exif_infe(2, 2)]);
        hidden[12..14].copy_from_slice(&1_u16.to_be_bytes());
        let atom = FileAtom {
            kind: *b"iinf",
            body_start: 8,
            end: hidden.len() as u64,
        };
        assert!(matches!(
            read_item_ids(&mut std::io::Cursor::new(hidden), atom),
            Err(HeifExifError::Malformed)
        ));
    }

    #[test]
    fn read_file_atom_handles_extended_and_parent_sized_atoms() {
        let mut extended = Vec::new();
        extended.extend_from_slice(&1_u32.to_be_bytes());
        extended.extend_from_slice(b"meta");
        extended.extend_from_slice(&24_u64.to_be_bytes());
        extended.extend_from_slice(&[0_u8; 8]);
        let atom = read_file_atom(
            &mut std::io::Cursor::new(&extended),
            0,
            extended.len() as u64,
        )
        .expect("read extended atom")
        .expect("extended atom");
        assert_eq!(atom.body_start, 16);
        assert_eq!(atom.end, 24);

        let mut parent_sized = Vec::new();
        parent_sized.extend_from_slice(&0_u32.to_be_bytes());
        parent_sized.extend_from_slice(b"mdat");
        parent_sized.extend_from_slice(&[0_u8; 8]);
        let atom = read_file_atom(
            &mut std::io::Cursor::new(&parent_sized),
            0,
            parent_sized.len() as u64,
        )
        .expect("read parent-sized atom")
        .expect("parent-sized atom");
        assert_eq!(atom.body_start, 8);
        assert_eq!(atom.end, parent_sized.len() as u64);
    }
}
