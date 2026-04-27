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

use std::io::Write;
use std::path::Path;

use mp4_atom::{
    Any, Atom, Buf, DecodeMaybe, Encode, FourCC, Header, Iinf, Iloc, ItemInfoEntry, ItemLocation,
    ItemLocationExtent, Mdat, Meta,
};

/// Typed failures from the HEIC writer. Each variant names the precise mode
/// so call-site logging and any future fall-back logic can distinguish
/// "file is truncated" from "kei's own re-encoder failed" from "this isn't
/// a HEIC at all" — instead of grepping anyhow strings.
///
/// `Decode`/`Encode` wrap the underlying [`mp4_atom::Error`] so the original
/// failure detail is preserved (`UnderDecode("infe")`, `OutOfBounds`, etc.)
/// while kei adds the byte offset / atom kind context.
#[derive(Debug, thiserror::Error)]
pub(crate) enum HeifError {
    #[error("ISO-BMFF decode failed at byte offset {offset}/{total}: {source}")]
    Decode {
        offset: u64,
        total: u64,
        #[source]
        source: mp4_atom::Error,
    },

    #[error("Unparsable trailing bytes at offset {offset}/{total} (file likely truncated)")]
    UnparsableTail { offset: u64, total: u64 },

    #[error("HEIC has no `meta` box at the top level ({input_len} bytes scanned)")]
    MissingMeta { input_len: usize },

    #[error("Failed to re-encode `{kind}` atom: {source}")]
    Encode {
        kind: FourCC,
        #[source]
        source: mp4_atom::Error,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Whether this path's extension is HEIF / HEIC / HIF / AVIF — formats
/// that XMP Toolkit's bundled handlers can't open, handled here instead.
///
/// Used for pre-download decisions where the file doesn't exist yet, so
/// content sniffing isn't possible. For post-download dispatch on a file
/// that may have a temp suffix shadowing its real extension (`.kei-tmp`),
/// use [`is_heif_content`] instead.
pub(crate) fn is_heif_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            matches!(lower.as_str(), "heic" | "heif" | "hif" | "avif")
        })
        .unwrap_or(false)
}

/// Whether `bytes` starts with an ISO-BMFF `ftyp` box whose major brand is
/// in the HEIF family. Robust to part-file naming where the path extension
/// has been replaced by a temp suffix — the byte signature is the only
/// reliable way to dispatch HEIF vs the formats XMP Toolkit can sniff
/// itself (JPEG/PNG/TIFF/MP4/MOV).
///
/// Brands per ISO/IEC 23008-12 §A.6 (HEIF) and AV1 Image File Format
/// (`avif`/`avis`). Only the first 12 bytes are inspected: 4-byte size,
/// `ftyp` fourCC, then 4-byte major brand.
pub(crate) fn is_heif_content(bytes: &[u8]) -> bool {
    let Some(box_type) = bytes.get(4..8) else {
        return false;
    };
    if box_type != b"ftyp" {
        return false;
    }
    let Some(brand) = bytes.get(8..12) else {
        return false;
    };
    matches!(
        brand,
        b"heic"
            | b"heix"
            | b"heim"
            | b"heis"
            | b"hevc"
            | b"hevm"
            | b"hevs"
            | b"mif1"
            | b"msf1"
            | b"avif"
            | b"avis"
    )
}

/// Locate the XMP packet bytes embedded in a HEIC file, if any. Returns the
/// raw RDF/XML payload referenced by the first `mime`-type item with
/// content_type `"application/rdf+xml"`. Used by the write path to preserve
/// existing XMP on rewrite (symmetric with xmp_toolkit's `file.xmp()`).
///
/// Walks top-level boxes by header only and descends into `meta` directly,
/// rather than using `Any::decode_maybe` which dispatches into mp4-atom's
/// full type table on every box kind. That dispatch is unsafe for
/// kei: parsers like `Dfla::decode_body` (`flac.rs::parse_vorbis_comment`)
/// and `Avcc::decode_body` allocate from attacker-controlled length fields
/// without a `min(..)` cap, so a malformed sub-100-byte HEIC turns into a
/// 20+ GiB allocation. Filed upstream as kixelated/mp4-atom#154; this
/// function avoids the problem regardless of when that lands by only
/// invoking the iinf / iloc decoders we actually need.
pub(crate) fn extract_xmp_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut cursor: &[u8] = bytes;
    while cursor.has_remaining() {
        let header = Header::decode_maybe(&mut cursor).ok().flatten()?;
        let body_size = header.size.unwrap_or(cursor.remaining());
        if body_size > cursor.remaining() {
            return None;
        }
        if header.kind == FourCC::new(b"meta") {
            // HEIC has at most one top-level `meta` box; stop either way.
            return extract_xmp_from_meta(bytes, cursor.get(..body_size)?);
        }
        cursor.advance(body_size);
    }
    None
}

/// Pull the iinf + iloc out of a `meta` box body and resolve the XMP extent
/// against the original file bytes. Walks the meta sub-boxes by header only
/// and decodes only `iinf` and `iloc`, so a hostile sub-atom (e.g. a nested
/// `dfLa`) can't reach the unbounded-allocation parsers either.
fn extract_xmp_from_meta(file_bytes: &[u8], meta_body: &[u8]) -> Option<Vec<u8>> {
    let mut cursor: &[u8] = meta_body;

    // Two on-the-wire formats for `meta`:
    //   - ISO/IEC 14496-12: 4-byte version+flags, then sub-boxes (first is hdlr).
    //   - Apple QuickTime: starts with hdlr directly, no version+flags.
    // Detect by peeking offset 4..8 for "hdlr"; same heuristic as
    // mp4_atom::Meta::decode_body.
    if cursor.len() >= 8 && cursor.get(4..8) != Some(b"hdlr".as_slice()) {
        if cursor.len() < 4 {
            return None;
        }
        cursor.advance(4);
    }

    // Skip hdlr; we don't need its contents.
    let hdlr = Header::decode_maybe(&mut cursor).ok().flatten()?;
    let hdlr_size = hdlr.size.unwrap_or(cursor.remaining());
    if hdlr_size > cursor.remaining() {
        return None;
    }
    cursor.advance(hdlr_size);

    let mut iinf: Option<Iinf> = None;
    let mut iloc: Option<Iloc> = None;
    while cursor.has_remaining() {
        let h = Header::decode_maybe(&mut cursor).ok().flatten()?;
        let sz = h.size.unwrap_or(cursor.remaining());
        if sz > cursor.remaining() {
            return None;
        }
        let body = cursor.get(..sz)?;
        if h.kind == FourCC::new(b"iinf") {
            iinf = Iinf::decode_body(&mut &body[..]).ok();
        } else if h.kind == FourCC::new(b"iloc") {
            iloc = Iloc::decode_body(&mut &body[..]).ok();
        }
        cursor.advance(sz);
    }

    let iinf = iinf?;
    let iloc = iloc?;
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
    file_bytes.get(start..end).map(<[u8]>::to_vec)
}

/// Surgically insert (or replace) the XMP `mime` item inside a HEIC file,
/// streaming the rewrite to `writer`.
///
/// The HEIC container is ISO-BMFF — a sequence of top-level atoms. XMP lives
/// inside the `meta` atom as an item with `item_type = "mime"` and
/// `content_type = "application/rdf+xml"`. We append the XMP bytes as a new
/// trailing `mdat` (construction_method 0, file-absolute offsets), so the
/// encoded image bytes in the original `mdat` stay byte-for-byte identical
/// even after `meta` grows.
///
/// Writing to a `Write` (typically `BufWriter<File>`) rather than returning
/// a `Vec<u8>` eliminates one full-file-sized allocation on the metadata-
/// embed path — meaningful for large ProRAW HEICs under high concurrency.
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

    /// Build a minimal ftyp prefix with the given major brand for tests.
    fn ftyp_prefix(brand: &[u8; 4]) -> Vec<u8> {
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"mif1");
        bytes.extend_from_slice(b"heic");
        bytes
    }

    #[test]
    fn is_heif_content_accepts_all_known_brands() {
        for brand in [
            b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevm", b"hevs", b"mif1", b"msf1",
            b"avif", b"avis",
        ] {
            assert!(
                is_heif_content(&ftyp_prefix(brand)),
                "expected brand {:?} to be HEIF",
                std::str::from_utf8(brand).unwrap()
            );
        }
    }

    #[test]
    fn is_heif_content_rejects_non_heif_iso_bmff() {
        // mp4/mov: ftyp present but brand is not in the HEIF family.
        for brand in [b"mp42", b"isom", b"qt  ", b"M4V "] {
            assert!(
                !is_heif_content(&ftyp_prefix(brand)),
                "expected brand {:?} to NOT be HEIF",
                std::str::from_utf8(brand).unwrap()
            );
        }
    }

    #[test]
    fn is_heif_content_rejects_jpeg_magic() {
        // SOI + APP0 prefix; bytes 4..8 are not "ftyp".
        let bytes = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01,
        ];
        assert!(!is_heif_content(&bytes));
    }

    #[test]
    fn is_heif_content_rejects_short_or_empty_input() {
        assert!(!is_heif_content(&[]));
        assert!(!is_heif_content(&[0; 11]));
    }

    #[test]
    fn is_heif_content_rejects_garbage_with_no_ftyp() {
        let blob: Vec<u8> = (0..32_u8).collect();
        assert!(!is_heif_content(&blob));
    }

    // ── extract_xmp_bytes: malformed input must not panic, must return None ──
    //
    // The original suite only covered `is_heif_path`; the parser entry points
    // (`extract_xmp_bytes`, `insert_xmp`) had no malformed-input regression.
    // A regression that panicked on truncated bytes would crash the metadata
    // worker on any partial download — silent data loss in the surrounding
    // sync. These pin the "return None / bail" contract for the universe of
    // garbage inputs the wild can produce.

    #[test]
    fn extract_xmp_bytes_empty_input_returns_none() {
        // Zero bytes is the most basic malformed case.
        assert!(extract_xmp_bytes(&[]).is_none());
    }

    #[test]
    fn extract_xmp_bytes_random_bytes_returns_none() {
        // Plausible-looking-but-not-HEIF blob: must not panic, must return
        // None. The previous mp4_atom decode loop swallowed errors via
        // `if let Ok(Some(...)) = ...`, but a future refactor that switched
        // to `.unwrap()` would explode on this input.
        let blob: Vec<u8> = (0..256_u16).map(|i| (i & 0xff) as u8).collect();
        assert!(extract_xmp_bytes(&blob).is_none());
    }

    #[test]
    fn extract_xmp_bytes_truncated_atom_header_returns_none() {
        // 4 bytes is shorter than any valid ISO-BMFF box header (8 bytes).
        // Decoder must not panic on the short read.
        let bytes = [0x00, 0x00, 0x00, 0x18];
        assert!(extract_xmp_bytes(&bytes).is_none());
    }

    #[test]
    fn extract_xmp_bytes_no_meta_box_returns_none() {
        // A syntactically valid `ftyp` atom with no following `meta` — there
        // is no XMP to find, so the function must return None without error.
        // ftyp box: size=0x18 (24), kind=ftyp, major_brand=heic, minor_version=0,
        // compatible_brands=[heic, mif1].
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0x18_u32.to_be_bytes());
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(b"heic");
        bytes.extend_from_slice(b"mif1");
        assert_eq!(bytes.len(), 0x18);
        assert!(extract_xmp_bytes(&bytes).is_none());
    }

    #[test]
    fn extract_xmp_bytes_atom_with_oversized_length_field_returns_none() {
        // size field claims 0xFFFFFFFF bytes (way past end of buffer). A
        // robust parser must reject this, not allocate or read out of bounds.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&0xFFFF_FFFF_u32.to_be_bytes());
        bytes.extend_from_slice(b"meta");
        bytes.extend_from_slice(&[0; 16]); // payload tail (will be cut short)
        assert!(extract_xmp_bytes(&bytes).is_none());
    }

    #[test]
    fn extract_xmp_bytes_top_level_dfla_does_not_oom() {
        // Regression: this 110-byte input was the first OOM repro from the
        // libfuzzer harness (`fuzz/seeds/heif_atoms/regression-iloc-oom`).
        // Pre-fix, `Any::decode_maybe` saw a top-level `dfLa` FourCC,
        // dispatched to `Dfla::decode_body` -> `parse_vorbis_comment`, and
        // tried to `Vec::with_capacity(~876_000_000)` for a `Vec<String>`
        // (~21 GiB) - upstream kixelated/mp4-atom#154. The kei-side fix is
        // to walk top-level boxes by header and only descend into `meta`,
        // so this input must return None promptly without allocating.
        const REPRO: &[u8] = &[
            0x00, 0x00, 0x00, 0x08, 0x00, 0x1d, 0x00, 0x22, 0x00, 0x00, 0x00, 0x00, 0x64, 0x66,
            0x4c, 0x61, 0x00, 0x00, 0x00, 0xf6, 0x6a, 0x00, 0x00, 0x10, 0x0d, 0xaa, 0x6b, 0x9d,
            0xbb, 0xff, 0xff, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x0c, 0x0c, 0x1b, 0x00, 0x04, 0x00,
            0x00, 0x1d, 0x00, 0x00, 0x00, 0x00, 0x66, 0x6c, 0x36, 0x34, 0x00, 0x32, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x4f, 0xe0,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x00, 0x22, 0x00, 0x00, 0x00,
            0x64, 0x66, 0x4c, 0x61, 0x00, 0x00, 0x00, 0xf6, 0x6a, 0x00, 0x00, 0x10, 0x0d, 0xaa,
            0x6b, 0x9d, 0xbb, 0xff, 0xff, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x00, 0x00,
        ];
        assert_eq!(REPRO.len(), 110);
        assert!(extract_xmp_bytes(REPRO).is_none());
    }

    #[test]
    fn extract_xmp_bytes_meta_with_nested_dfla_is_safe() {
        // The same upstream OOM is reachable from a `meta` box that contains
        // a nested `dfLa` sub-atom, because mp4_atom::Meta::decode_body uses
        // `Any::decode_maybe` internally on every item. The kei-side fix
        // also walks meta sub-boxes by header so only `iinf`/`iloc` decoders
        // run; an attacker-supplied `dfLa` inside `meta` is skipped.
        //
        // Layout: <meta box header> <version+flags> <hdlr box> <dfLa box>.
        // The dfLa body declares 0xFFFF_FFFF Vorbis comment fields; pre-fix
        // (or with a future Meta::decode_body-using rewrite) this would
        // allocate ~103 GiB.
        let mut hdlr: Vec<u8> = Vec::new();
        hdlr.extend_from_slice(&0x21_u32.to_be_bytes()); // size = header(8) + body(25) = 33
        hdlr.extend_from_slice(b"hdlr");
        hdlr.extend_from_slice(&[0; 4]); // version+flags
        hdlr.extend_from_slice(&[0; 4]); // pre_defined
        hdlr.extend_from_slice(b"pict"); // handler_type
        hdlr.extend_from_slice(&[0; 12]); // reserved
        hdlr.push(0); // empty name (null-terminated)

        let mut dfla: Vec<u8> = Vec::new();
        dfla.extend_from_slice(&0x18_u32.to_be_bytes()); // size = 24
        dfla.extend_from_slice(b"dfLa");
        dfla.extend_from_slice(&[0; 4]); // version+flags
                                         // metadata block header: last_block=1, type=4 (vorbis_comment), length=8
        dfla.extend_from_slice(&[0x84, 0x00, 0x00, 0x08]);
        // vorbis comment body: vendor_string_length=0, number_of_fields=0xFFFF_FFFF
        dfla.extend_from_slice(&0_u32.to_le_bytes());
        dfla.extend_from_slice(&u32::MAX.to_le_bytes());

        let mut meta_body: Vec<u8> = Vec::new();
        meta_body.extend_from_slice(&[0; 4]); // version+flags
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&dfla);

        let mut meta_box: Vec<u8> = Vec::new();
        let total = (8 + meta_body.len()) as u32;
        meta_box.extend_from_slice(&total.to_be_bytes());
        meta_box.extend_from_slice(b"meta");
        meta_box.extend_from_slice(&meta_body);

        // Must return None instead of allocating gigabytes.
        assert!(extract_xmp_bytes(&meta_box).is_none());
    }

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
    fn heif_error_io_variant_carries_underlying_io_error() {
        // Sanity-check the Io variant — the writer used by insert_xmp is
        // any std::io::Write, and io::Error must convert via From.
        let io_err = std::io::Error::other("disk full");
        let err: HeifError = io_err.into();
        assert!(matches!(err, HeifError::Io(_)));
    }
}
