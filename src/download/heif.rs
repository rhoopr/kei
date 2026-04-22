//! ISO-BMFF atom surgery for inserting XMP into HEIC / HEIF / AVIF files.
//!
//! Adobe's XMP Toolkit has no HEIF handler, so the HEIC write path edits the
//! container directly via [`mp4_atom`]. The goal is narrow: add (or replace)
//! an XMP `mime` item inside the `meta` box without touching the encoded
//! image bytes in `mdat` — invariant 2.
//!
//! Strategy: append the XMP payload as a new trailing `mdat`, record it in
//! `iinf` + `iloc` with `construction_method = 0` (file-absolute offsets),
//! and remap every other `iloc` entry so the existing image data stays
//! byte-for-byte identical in its new location after `meta` grows.

use std::path::Path;

use anyhow::Result;
use mp4_atom::{
    Any, DecodeMaybe, Encode, FourCC, Iinf, Iloc, ItemInfoEntry, ItemLocation, ItemLocationExtent,
    Mdat, Meta,
};

/// Whether this path's extension is HEIF / HEIC / HIF / AVIF — formats
/// that XMP Toolkit's bundled handlers can't open, handled here instead.
pub(crate) fn is_heif_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            matches!(lower.as_str(), "heic" | "heif" | "hif" | "avif")
        })
        .unwrap_or(false)
}

/// Locate the XMP packet bytes embedded in a HEIC file, if any. Returns the
/// raw RDF/XML payload referenced by the first `mime`-type item with
/// content_type `"application/rdf+xml"`. Used by the write path to preserve
/// existing XMP on rewrite (symmetric with xmp_toolkit's `file.xmp()`).
pub(crate) fn extract_xmp_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut cursor: &[u8] = bytes;
    while let Ok(Some(atom)) = Any::decode_maybe(&mut cursor) {
        let Any::Meta(meta) = atom else { continue };
        let iinf = meta.get::<Iinf>()?;
        let iloc = meta.get::<Iloc>()?;
        let xmp_entry = iinf.item_infos.iter().find(|e| {
            e.item_type == Some(FourCC::new(b"mime"))
                && e.content_type.as_deref() == Some("application/rdf+xml")
        })?;
        let loc = iloc
            .item_locations
            .iter()
            .find(|l| l.item_id == xmp_entry.item_id)?;
        if loc.construction_method != 0 {
            return None;
        }
        let extent = loc.extents.first()?;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "HEIC file byte offsets/lengths fit in usize on 64-bit; kei targets 64-bit platforms"
        )]
        let start = loc.base_offset.saturating_add(extent.offset) as usize;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "HEIC extent length fits in usize on 64-bit"
        )]
        let end = start.checked_add(extent.length as usize)?;
        return bytes.get(start..end).map(<[u8]>::to_vec);
    }
    None
}

/// Surgically insert (or replace) the XMP `mime` item inside a HEIC file.
///
/// The HEIC container is ISO-BMFF — a sequence of top-level atoms. XMP lives
/// inside the `meta` atom as an item with `item_type = "mime"` and
/// `content_type = "application/rdf+xml"`. We append the XMP bytes as a new
/// trailing `mdat` (construction_method 0, file-absolute offsets), so the
/// encoded image bytes in the original `mdat` stay byte-for-byte identical
/// even after `meta` grows.
// meta_idx is produced by .position() over atoms; new_mdat_idx is
// atoms.len() - 1 after a push; new_positions is built from the same atoms
// vec. All indexing here is in-bounds by construction.
#[allow(clippy::indexing_slicing)]
pub(crate) fn insert_xmp(input: &[u8], xmp: &[u8]) -> Result<Vec<u8>> {
    // Record each top-level atom along with its original byte offset in the
    // input so we can rewrite file-absolute iloc entries correctly — the
    // existing iloc offsets point into the original mdat, and those offsets
    // must be updated so that after re-serialization they still land on the
    // same image bytes even though the meta box grew.
    let mut cursor: &[u8] = input;
    let mut atoms: Vec<Any> = Vec::new();
    let mut original_offsets: Vec<u64> = Vec::new();
    while !cursor.is_empty() {
        let offset_before_atom = (input.len() - cursor.len()) as u64;
        match Any::decode_maybe(&mut cursor)? {
            Some(a) => {
                atoms.push(a);
                original_offsets.push(offset_before_atom);
            }
            None => anyhow::bail!(
                "unparsable tail at offset {} of {}",
                offset_before_atom,
                input.len()
            ),
        }
    }

    let meta_idx = atoms
        .iter()
        .position(|a| matches!(a, Any::Meta(_)))
        .ok_or_else(|| anyhow::anyhow!("HEIC has no meta box"))?;

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
        .filter_map(|(idx, a)| {
            let orig = *original_offsets.get(idx)?;
            let size = encoded_size(a);
            Some((orig, orig + size, new_positions[idx]))
        })
        .collect();

    if let Any::Meta(meta) = &mut atoms[meta_idx] {
        if let Some(iloc) = meta.get_mut::<Iloc>() {
            remap_file_offsets(iloc, &file_offset_map);
            // Now fill in the XMP entry's offset (last iloc entry).
            if let Some(xmp_loc) = iloc
                .item_locations
                .iter_mut()
                .find(|l| l.item_id == new_item_id)
            {
                if let Some(extent) = xmp_loc.extents.first_mut() {
                    extent.offset = xmp_file_offset;
                }
            }
        }
    }

    let mut out = Vec::with_capacity(input.len() + xmp.len() + 512);
    for atom in &atoms {
        atom.encode(&mut out)?;
    }
    Ok(out)
}

/// Walk existing iinf/iloc to find any previously-kei-appended XMP mdat.
/// Criteria: an iinf entry flagged as `mime` + `application/rdf+xml`, its
/// iloc entry references a range that lies entirely within a single trailing
/// mdat atom, and no other iloc entry references into that atom.
// Indexing by meta_idx (caller-validated) and by idx from atoms.iter().enumerate()
// (with original_offsets built 1:1 alongside atoms in insert_xmp) is in-bounds.
#[allow(clippy::indexing_slicing)]
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
            let atom_end = atom_start + encoded_size(atom);
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
fn header_size_of(_atom: &Any) -> u64 {
    8
}

/// Return a vector where entry `i` is the byte offset at which atom `i` will
/// sit in the re-serialized output (i.e. the running sum of preceding atom
/// sizes).
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

fn remap_point(file_offset: u64, ranges: &[(u64, u64, u64)]) -> Option<u64> {
    for &(old_start, old_end, new_start) in ranges {
        if file_offset >= old_start && file_offset < old_end {
            return Some(new_start + (file_offset - old_start));
        }
    }
    None
}

fn encoded_size(atom: &Any) -> u64 {
    let mut sink = Vec::new();
    atom.encode(&mut sink).ok();
    sink.len() as u64
}

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

fn push_iinf_entry(meta: &mut Meta, entry: ItemInfoEntry) {
    match meta.get_mut::<Iinf>() {
        Some(iinf) => iinf.item_infos.push(entry),
        None => meta.push(Iinf {
            item_infos: vec![entry],
        }),
    }
}

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
    use super::*;

    #[test]
    fn is_heif_path_recognises_heic_variants() {
        assert!(is_heif_path(Path::new("/a/b.heic")));
        assert!(is_heif_path(Path::new("/a/b.HEIC")));
        assert!(is_heif_path(Path::new("/a/b.HEIF")));
        assert!(is_heif_path(Path::new("/a/b.hif")));
        assert!(is_heif_path(Path::new("/a/b.avif")));
        assert!(!is_heif_path(Path::new("/a/b.jpg")));
        assert!(!is_heif_path(Path::new("/a/b.mov")));
        assert!(!is_heif_path(Path::new("/a/b")));
    }
}
