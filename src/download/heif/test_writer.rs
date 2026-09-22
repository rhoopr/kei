//! Legacy typed-atom writer retained only for parser fixture tests.

use super::HeifError;
use mp4_atom::{
    Any, DecodeMaybe, Encode, FourCC, Iinf, Iloc, ItemInfoEntry, ItemLocation, ItemLocationExtent,
    Mdat, Meta,
};
use std::io::Write;

/// Legacy typed-atom XMP writer retained for parser fixture tests.
///
/// The HEIC container is ISO-BMFF, a sequence of top-level atoms. XMP lives
/// inside the `meta` atom as an item with `item_type = "mime"` and
/// `content_type = "application/rdf+xml"`. This helper appends the XMP bytes as a new
/// trailing `mdat` (construction_method 0, file-absolute offsets), so the
/// encoded image bytes in the original `mdat` stay byte-for-byte identical
/// even after `meta` grows.
#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    reason = "meta_idx comes from .position() over atoms; new_mdat_idx is atoms.len() - 1 \
              after a push; new_positions is built from the same atoms vec; all indexing \
              here is in-bounds by construction"
)]
pub(crate) fn insert_xmp<W: Write>(
    input: &[u8],
    xmp: &[u8],
    mut writer: W,
) -> Result<(), HeifError> {
    // Record each top-level atom along with its original byte offset in the
    // input so we can rewrite file-absolute iloc entries correctly — the
    // existing iloc offsets point into the original mdat, and those offsets
    // must be updated so that after re-serialization they still land on the
    // same image bytes even though the meta box grew.
    let total = input.len() as u64;
    let mut cursor: &[u8] = input;
    let mut atoms: Vec<Any> = Vec::new();
    let mut original_offsets: Vec<u64> = Vec::new();
    while !cursor.is_empty() {
        let offset = total - cursor.len() as u64;
        match Any::decode_maybe(&mut cursor).map_err(|source| HeifError::Decode {
            offset,
            total,
            source,
        })? {
            Some(a) => {
                atoms.push(a);
                original_offsets.push(offset);
            }
            None => {
                return Err(HeifError::UnparsableTail { offset, total });
            }
        }
    }

    let meta_idx =
        atoms
            .iter()
            .position(|a| matches!(a, Any::Meta(_)))
            .ok_or(HeifError::MissingMeta {
                input_len: input.len(),
            })?;

    // Step 1: locate and drop the trailing mdat that a prior kei write
    // appended (if any) so we don't accumulate stale XMP payloads on
    // re-sync. We identify it by: (a) the existing XMP iloc entry's
    // extent range, (b) it sitting past the image-data mdat, (c) no
    // other iloc entry pointing into it.
    let stale_mdat_idx = locate_stale_kei_mdat(&atoms, &original_offsets, meta_idx);

    // Step 2: remove the XMP entries from iinf and iloc.
    if let Any::Meta(meta) = &mut atoms[meta_idx] {
        let removed_ids = remove_existing_xmp_items(meta);
        if let Some(iloc) = meta.get_mut::<Iloc>() {
            iloc.item_locations
                .retain(|loc| !removed_ids.contains(&loc.item_id));
        }
    }

    // Step 3: drop the stale mdat atom (indexes shift, recompute meta_idx
    // relative to the surviving atoms).
    let meta_idx = if let Some(stale) = stale_mdat_idx {
        atoms.remove(stale);
        original_offsets.remove(stale);
        if stale < meta_idx {
            meta_idx - 1
        } else {
            meta_idx
        }
    } else {
        meta_idx
    };

    // Step 4: reserve the iinf + iloc entries for the new XMP. The iloc
    // offset is the file offset our appended mdat's DATA will have in the
    // re-serialized output. mp4-atom encodes Iloc at fixed width regardless
    // of offset value, so we can append the mdat atom first, compute the
    // resulting running offsets, then populate the iloc offset.
    let new_item_id = {
        #[allow(
            clippy::unreachable,
            reason = "meta_idx comes from matches!(a, Any::Meta(_)) above"
        )]
        let Any::Meta(meta) = &atoms[meta_idx] else {
            unreachable!()
        };
        next_free_item_id(meta)
    };

    atoms.push(Any::Mdat(Mdat { data: xmp.to_vec() }));
    let new_mdat_idx = atoms.len() - 1;

    // Insert placeholder iloc entry (offset=0) and iinf entry so that running
    // offsets reflect the final meta size.
    {
        #[allow(
            clippy::unreachable,
            reason = "meta_idx comes from matches!(a, Any::Meta(_)) above"
        )]
        let Any::Meta(meta) = &mut atoms[meta_idx] else {
            unreachable!()
        };
        push_iinf_entry(
            meta,
            ItemInfoEntry {
                item_id: new_item_id,
                item_protection_index: 0,
                item_type: Some(FourCC::new(b"mime")),
                item_name: String::new(),
                content_type: Some("application/rdf+xml".to_string()),
                content_encoding: Some(String::new()),
                item_uri_type: None,
                item_not_in_presentation: false,
            },
        );
        push_iloc_entry(
            meta,
            ItemLocation {
                item_id: new_item_id,
                construction_method: 0,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![ItemLocationExtent {
                    item_reference_index: 0,
                    offset: 0,
                    length: xmp.len() as u64,
                }],
            },
        );
    }

    // Step 5: remap pre-existing file-offset iloc entries and fill in the
    // offset for the XMP iloc entry we just pushed.
    let new_positions = running_offsets(&atoms);
    let xmp_file_offset = new_positions[new_mdat_idx] + header_size_of(&atoms[new_mdat_idx]);

    let file_offset_map: Vec<(u64, u64, u64)> = atoms
        .iter()
        .enumerate()
        .take(new_mdat_idx) // skip the mdat we just added; it has no matching original
        .filter_map(|(idx, _a)| {
            let orig = *original_offsets.get(idx)?;
            // Use the original atom's actual extent, not encoded_size(a).
            // Meta::encode_body always writes ISO format (with 4-byte
            // version+flags) even when the input was Apple QuickTime
            // (without version+flags). encoded_size would report the
            // re-encoded ISO size, making this range 4 bytes wider than
            // the original atom — iloc entries in that overshoot region
            // are then captured by the wrong range.
            let orig_end = original_offsets.get(idx + 1).copied().unwrap_or(total);
            Some((orig, orig_end, new_positions[idx]))
        })
        .collect();

    if let Any::Meta(meta) = &mut atoms[meta_idx]
        && let Some(iloc) = meta.get_mut::<Iloc>()
    {
        remap_file_offsets(iloc, &file_offset_map);
        // Now fill in the XMP entry's offset (last iloc entry).
        if let Some(xmp_loc) = iloc
            .item_locations
            .iter_mut()
            .find(|l| l.item_id == new_item_id)
            && let Some(extent) = xmp_loc.extents.first_mut()
        {
            extent.offset = xmp_file_offset;
        }
    }

    // mp4-atom's Encode requires BufMut (bytes), not Write; a reusable
    // per-atom Vec caps in-memory output at one atom (the image mdat
    // is typically the largest) rather than the full serialized file.
    let mut atom_buf: Vec<u8> = Vec::new();
    for atom in &atoms {
        atom_buf.clear();
        let kind = atom.kind();
        atom.encode(&mut atom_buf)
            .map_err(|source| HeifError::Encode { kind, source })?;
        writer.write_all(&atom_buf)?;
    }
    Ok(())
}

/// Walk existing iinf/iloc to find any previously-kei-appended XMP mdat.
/// Criteria: an iinf entry flagged as `mime` + `application/rdf+xml`, its
/// iloc entry references a range that lies entirely within a single trailing
/// mdat atom, and no other iloc entry references into that atom.
#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    reason = "meta_idx is caller-validated and idx comes from atoms.iter().enumerate() \
              with original_offsets built 1:1 alongside atoms in insert_xmp"
)]
fn locate_stale_kei_mdat(
    atoms: &[Any],
    original_offsets: &[u64],
    meta_idx: usize,
) -> Option<usize> {
    let meta = if let Any::Meta(m) = &atoms[meta_idx] {
        m
    } else {
        return None;
    };
    let iinf = meta.get::<Iinf>()?;
    let iloc = meta.get::<Iloc>()?;

    let xmp_item_ids: Vec<u32> = iinf
        .item_infos
        .iter()
        .filter(|e| {
            e.item_type == Some(FourCC::new(b"mime"))
                && e.content_type.as_deref() == Some("application/rdf+xml")
        })
        .map(|e| e.item_id)
        .collect();
    if xmp_item_ids.is_empty() {
        return None;
    }

    for item_id in &xmp_item_ids {
        let Some(loc) = iloc.item_locations.iter().find(|l| l.item_id == *item_id) else {
            continue;
        };
        if loc.construction_method != 0 {
            continue;
        }
        let Some(extent) = loc.extents.first() else {
            continue;
        };
        let abs_start = loc.base_offset.saturating_add(extent.offset);
        let abs_end = abs_start.saturating_add(extent.length);

        for (idx, atom) in atoms.iter().enumerate() {
            if !matches!(atom, Any::Mdat(_)) {
                continue;
            }
            let atom_start = original_offsets[idx];
            let atom_end = original_offsets
                .get(idx + 1)
                .copied()
                .unwrap_or_else(|| atom_start + encoded_size(atom));
            if abs_start < atom_start || abs_end > atom_end {
                continue;
            }
            let other_refs = iloc.item_locations.iter().any(|other| {
                if other.item_id == *item_id || other.construction_method != 0 {
                    return false;
                }
                other.extents.iter().any(|e| {
                    let o_start = other.base_offset.saturating_add(e.offset);
                    o_start >= atom_start && o_start < atom_end
                })
            });
            if !other_refs {
                return Some(idx);
            }
        }
    }
    None
}

/// Byte size of an atom's box header (the length field + 4-byte kind code).
/// mp4-atom always emits a 32-bit-length header for atoms that fit — large
/// mdats (>4GB) would use a 16-byte header, but kei isn't going to hit that.
#[cfg(test)]
fn header_size_of(_atom: &Any) -> u64 {
    8
}

/// Return a vector where entry `i` is the byte offset at which atom `i` will
/// sit in the re-serialized output (i.e. the running sum of preceding atom
/// sizes).
#[cfg(test)]
fn running_offsets(atoms: &[Any]) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(atoms.len());
    let mut running = 0u64;
    for atom in atoms {
        offsets.push(running);
        running += encoded_size(atom);
    }
    offsets
}

/// Translate each construction_method-0 iloc offset from "original file
/// offset" to "new file offset", using the per-atom old_start/old_end/new_start
/// table. An offset that falls within `[old_start, old_end)` is rebased onto
/// `new_start` with the same intra-atom position.
#[cfg(test)]
fn remap_file_offsets(iloc: &mut Iloc, ranges: &[(u64, u64, u64)]) {
    for loc in &mut iloc.item_locations {
        if loc.construction_method != 0 {
            continue;
        }
        // Some encoders put the whole file offset in `base_offset` and leave
        // extent offsets at 0; others leave base_offset 0 and put absolute
        // offsets on each extent. Handle both by remapping either piece that
        // lands in a known original-atom range.
        loc.base_offset = remap_point(loc.base_offset, ranges).unwrap_or(loc.base_offset);
        for extent in &mut loc.extents {
            let absolute = loc.base_offset.saturating_add(extent.offset);
            if let Some(new_abs) = remap_point(absolute, ranges) {
                extent.offset = new_abs.saturating_sub(loc.base_offset);
            }
        }
    }
}

#[cfg(test)]
fn remap_point(file_offset: u64, ranges: &[(u64, u64, u64)]) -> Option<u64> {
    for &(old_start, old_end, new_start) in ranges {
        if file_offset >= old_start && file_offset < old_end {
            return Some(new_start + (file_offset - old_start));
        }
    }
    None
}

#[cfg(test)]
fn encoded_size(atom: &Any) -> u64 {
    let mut sink = Vec::new();
    if let Err(e) = atom.encode(&mut sink) {
        tracing::warn!(
            target: "kei::download::heif",
            error = %e,
            "encoded_size: atom re-encode failed; size estimate may be wrong, \
             downstream offset remap will skip this atom"
        );
    }
    sink.len() as u64
}

#[cfg(test)]
fn remove_existing_xmp_items(meta: &mut Meta) -> Vec<u32> {
    let mut removed = Vec::new();
    if let Some(iinf) = meta.get_mut::<Iinf>() {
        iinf.item_infos.retain(|entry| {
            let is_xmp = entry.item_type == Some(FourCC::new(b"mime"))
                && entry.content_type.as_deref() == Some("application/rdf+xml");
            if is_xmp {
                removed.push(entry.item_id);
                false
            } else {
                true
            }
        });
    }
    removed
}

#[cfg(test)]
fn next_free_item_id(meta: &Meta) -> u32 {
    meta.get::<Iinf>()
        .map(|iinf| {
            iinf.item_infos
                .iter()
                .map(|e| e.item_id)
                .max()
                .map(|m| m + 1)
                .unwrap_or(1)
        })
        .unwrap_or(1)
}

#[cfg(test)]
fn push_iinf_entry(meta: &mut Meta, entry: ItemInfoEntry) {
    match meta.get_mut::<Iinf>() {
        Some(iinf) => iinf.item_infos.push(entry),
        None => meta.push(Iinf {
            item_infos: vec![entry],
        }),
    }
}

#[cfg(test)]
fn push_iloc_entry(meta: &mut Meta, loc: ItemLocation) {
    match meta.get_mut::<Iloc>() {
        Some(iloc) => iloc.item_locations.push(loc),
        None => meta.push(Iloc {
            item_locations: vec![loc],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::super::HeifError;
    use super::super::boxes::is_heif_content;
    use super::super::test_support::{
        build_apple_qt_heic_fixture, decode_iloc_from_heic, find_atom_end, find_mdat,
    };
    use super::super::xmp_read::extract_xmp_bytes;
    use super::insert_xmp;

    // ── insert_xmp: typed errors per failure mode ──
    //
    // Each pin asserts on a specific HeifError variant so a future refactor
    // that drops or reclassifies a failure lands a test failure rather than
    // a silent regression. Variant matching keeps the assertions stable
    // across error-message rewording.

    #[test]
    fn insert_xmp_returns_missing_meta_on_input_with_no_meta_box() {
        // ftyp-only fixture — syntactically valid ISO-BMFF, but no `meta`
        // box, so HEIC surgery has nothing to operate on.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");

        let mut out: Vec<u8> = Vec::new();
        let err = insert_xmp(&bytes, b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>", &mut out)
            .expect_err("insert_xmp must reject input without a meta box");
        assert!(
            matches!(err, HeifError::MissingMeta { input_len } if input_len == bytes.len()),
            "expected MissingMeta with correct input_len, got: {err:?}",
        );
        // Critical: nothing should have been written to the writer.
        assert!(
            out.is_empty(),
            "no bytes should be flushed when input has no meta box; got {} bytes",
            out.len()
        );
    }

    #[test]
    fn insert_xmp_returns_unparsable_tail_on_short_trailing_bytes() {
        // ftyp box followed by 3 stray bytes that can't form a valid atom
        // header. The parser must surface this as UnparsableTail (truncation
        // signal), not as a generic Decode error.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        let total = bytes.len() as u64;

        let mut out: Vec<u8> = Vec::new();
        let err = insert_xmp(&bytes, b"<x/>", &mut out)
            .expect_err("insert_xmp must surface parse errors on unparsable tail");
        assert!(
            matches!(err, HeifError::UnparsableTail { offset: 0x18, total: t } if t == total),
            "expected UnparsableTail at offset 0x18 of {total}, got: {err:?}",
        );
    }

    #[test]
    fn insert_xmp_output_keeps_ftyp_with_known_heif_brand() {
        // The post-download magic-byte check (`is_heif_content`) currently
        // runs on the original bytes; nothing re-checks the file header
        // after `insert_xmp` rewrites it. A regression in the rewriter
        // that produced a malformed prefix (e.g. truncated `ftyp` size,
        // wrong brand, double atom) would land on disk and only surface
        // when downstream tools (Immich, iCloud re-import) refuse the
        // file. This test pins the invariant on the canonical fixture so
        // any prefix-shape regression fails loudly here first.
        const SAMPLE_HEIC: &[u8] = include_bytes!("../../../tests/data/sample.heic");
        // Sanity: the fixture itself has to start with a HEIF brand or
        // the test is meaningless.
        assert!(
            is_heif_content(SAMPLE_HEIC),
            "fixture sample.heic must already be HEIF-shaped"
        );

        let mut out: Vec<u8> = Vec::new();
        insert_xmp(
            SAMPLE_HEIC,
            b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>",
            &mut out,
        )
        .expect("insert_xmp on a valid HEIC fixture must succeed");
        assert!(
            out.len() >= 12,
            "rewritten output must contain at least ftyp(8) + brand(4); got {} bytes",
            out.len()
        );
        // Bytes 4..8 must be the FourCC "ftyp"; bytes 8..12 must be one
        // of the brands `is_heif_content` accepts. This is exactly the
        // contract the post-rewrite magic-byte check would assert.
        assert_eq!(
            &out[4..8],
            b"ftyp",
            "rewritten output must begin with an ftyp box; first 12 bytes: {:?}",
            &out[..12]
        );
        assert!(
            is_heif_content(&out),
            "rewritten output must still pass is_heif_content; first 12 bytes: {:?}",
            &out[..12]
        );
    }

    #[test]
    fn insert_xmp_then_extract_round_trips_payload() {
        let heic = include_bytes!("../../../tests/data/sample.heic");
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF/></x:xmpmeta>";

        let mut rewritten: Vec<u8> = Vec::new();
        insert_xmp(heic.as_slice(), xmp.as_slice(), &mut rewritten)
            .expect("insert_xmp must succeed on a valid HEIC");

        let extracted = extract_xmp_bytes(&rewritten)
            .expect("extract_xmp_bytes must find the XMP we just inserted");
        assert_eq!(extracted, xmp, "round-tripped XMP must be byte-identical");
    }

    #[test]
    fn insert_xmp_twice_retains_only_latest() {
        let heic = include_bytes!("../../../tests/data/sample.heic");
        let first = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><first/></x:xmpmeta>";
        let second = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><second/></x:xmpmeta>";

        let mut after_first: Vec<u8> = Vec::new();
        insert_xmp(heic.as_slice(), first.as_slice(), &mut after_first)
            .expect("first insert must succeed");

        let mut after_second: Vec<u8> = Vec::new();
        insert_xmp(&after_first, second.as_slice(), &mut after_second)
            .expect("second insert must succeed");

        let extracted =
            extract_xmp_bytes(&after_second).expect("extract must find XMP after double insert");
        assert_eq!(
            extracted,
            second.as_slice(),
            "only the latest XMP packet should be present"
        );
    }

    // ── Regression: insert_xmp with Apple QuickTime Meta (no version+flags) ──
    //
    // mp4_atom::Meta::encode_body always writes ISO format (4-byte
    // version+flags) regardless of the input format. When the input Meta
    // is Apple QuickTime format (no version+flags), the re-encoded Meta is
    // 4 bytes larger than the original. The file_offset_map used
    // encoded_size() as old_end, which inflates Meta's range 4 bytes into
    // the next atom's territory. An iloc entry pointing to the start of mdat
    // is captured by Meta's bloated range and incorrectly remapped to the
    // old (pre-growth) position instead of the correct shifted position.
    //
    // Fix: use original_offsets[i+1] (or total for last atom) as old_end.
    #[test]
    fn insert_xmp_remaps_iloc_correctly_with_apple_qt_meta() {
        let input = build_apple_qt_heic_fixture();
        assert!(is_heif_content(&input));

        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF/></x:xmpmeta>";
        let mut output: Vec<u8> = Vec::new();
        insert_xmp(&input, xmp, &mut output).expect("insert_xmp must succeed");

        assert!(is_heif_content(&output));

        // The image mdat atom must have shifted past the (now-larger) meta.
        // Decode the output iloc and verify it points to the actual mdat data.
        let out_iloc = decode_iloc_from_heic(&output).expect("output iloc");
        let out_loc = out_iloc
            .item_locations
            .iter()
            .find(|l| l.item_id == 1)
            .expect("item 1 in output iloc");
        let out_abs = out_loc
            .base_offset
            .saturating_add(out_loc.extents.first().map(|e| e.offset).unwrap_or(0));
        let (out_mdat_start, out_mdat_data) = find_mdat(&output).expect("output mdat");

        assert_eq!(
            out_abs,
            out_mdat_start + 8,
            "iloc must point to output mdat data at offset {}+8, got {out_abs}",
            out_mdat_start,
        );

        // The input mdat must match output mdat.
        let (_in_mdat_start, in_mdat_data) = find_mdat(&input).expect("input mdat");
        assert_eq!(in_mdat_data, out_mdat_data, "mdat data must be preserved");

        // Verify the meta box actually grew (mdat shifted).
        let in_meta_end = find_atom_end(&input, "meta").expect("input meta");
        let out_meta_end = find_atom_end(&output, "meta").expect("output meta");
        assert!(out_meta_end > in_meta_end, "meta must grow on re-encode");
    }
}
