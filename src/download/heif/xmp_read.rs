//! Header-only XMP lookup with guarded typed metadata decoding.

use super::boxes::find_meta_layout;
use super::relationships::select_xmp_item_id;
use super::{HeifError, invalid_layout};
use mp4_atom::{Atom, Buf, DecodeMaybe, FourCC, Header, Iinf, Iloc};

/// Locate the primary image's embedded XMP packet, if any. Returns the raw
/// RDF/XML payload of the `mime` item with content_type
/// `"application/rdf+xml"` that [`select_xmp_item_id`] resolves to the primary
/// image. Used by the write path to preserve existing XMP on rewrite
/// (symmetric with xmp_toolkit's `file.xmp()`), so both ends of a
/// read-merge-write agree on which packet they own.
///
/// Walks top-level boxes by header only and descends into `meta` directly,
/// rather than using `Any::decode_maybe` which dispatches into mp4-atom's
/// full type table on every box kind. That dispatch is unsafe for
/// kei: parsers like `Dfla::decode_body` (`flac.rs::parse_vorbis_comment`)
/// and `Avcc::decode_body` allocated from attacker-controlled length fields
/// without a `min(..)` cap, so a malformed sub-100-byte HEIC turned into a
/// 20+ GiB allocation. Fixed upstream in kixelated/mp4-atom#157 (the rev
/// pinned in Cargo.toml includes it); this header-walk is retained as
/// defense-in-depth against the same class of bug surfacing in a sibling
/// decoder we don't actually need.
pub(crate) fn extract_xmp_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_xmp_strict(bytes).unwrap_or_default()
}

/// Strict variant of [`extract_xmp_bytes`] that distinguishes "the primary
/// image has no XMP item" (`Ok(None)`) from a file that cannot be resolved:
/// `Err(MetaSubBoxDecode)` when the iinf/iloc structure fails to decode, and
/// `Err(InvalidLayout)` when several packets are equally plausible candidates
/// for the primary image. Used by the metadata write path so a malformed or
/// undecidable item map fails loudly instead of silently stripping
/// pre-existing XMP.
pub(crate) fn extract_xmp_strict(bytes: &[u8]) -> Result<Option<Vec<u8>>, HeifError> {
    let mut cursor: &[u8] = bytes;
    while cursor.has_remaining() {
        let Some(header) = Header::decode_maybe(&mut cursor).ok().flatten() else {
            return Ok(None);
        };
        let body_size = header.size.unwrap_or(cursor.remaining());
        if body_size > cursor.remaining() {
            return Ok(None);
        }
        if header.kind == FourCC::new(b"meta") {
            let Some(body) = cursor.get(..body_size) else {
                return Ok(None);
            };
            // HEIC has at most one top-level `meta` box; stop either way.
            return extract_xmp_from_meta(bytes, body);
        }
        cursor.advance(body_size);
    }
    Ok(None)
}

/// Pull the iinf + iloc out of a `meta` box body and resolve the XMP extent
/// against the original file bytes. Walks the meta sub-boxes by header only
/// and decodes only `iinf` and `iloc`, so a hostile sub-atom (e.g. a nested
/// `dfLa`) can't reach the unbounded-allocation parsers either.
///
/// Returns `Err(MetaSubBoxDecode)` if iinf or iloc decode fails — the caller
/// can then mark the asset as needing metadata-rewrite. Returns `Ok(None)`
/// for legitimately-absent XMP (no iinf, no XMP item, etc).
fn extract_xmp_from_meta(
    file_bytes: &[u8],
    meta_body: &[u8],
) -> Result<Option<Vec<u8>>, HeifError> {
    let mut cursor: &[u8] = meta_body;

    // Two on-the-wire formats for `meta`:
    //   - ISO/IEC 14496-12: 4-byte version+flags, then sub-boxes (first is hdlr).
    //   - Apple QuickTime: starts with hdlr directly, no version+flags.
    // Detect by peeking offset 4..8 for "hdlr"; same heuristic as
    // mp4_atom::Meta::decode_body.
    if cursor.len() >= 8 && cursor.get(4..8) != Some(b"hdlr".as_slice()) {
        if cursor.len() < 4 {
            return Ok(None);
        }
        cursor.advance(4);
    }

    // Skip hdlr; we don't need its contents.
    let Some(hdlr) = Header::decode_maybe(&mut cursor).ok().flatten() else {
        return Ok(None);
    };
    let hdlr_size = hdlr.size.unwrap_or(cursor.remaining());
    if hdlr_size > cursor.remaining() {
        return Ok(None);
    }
    cursor.advance(hdlr_size);

    let mut iinf: Option<Iinf> = None;
    let mut iloc: Option<Iloc> = None;
    while cursor.has_remaining() {
        let Some(h) = Header::decode_maybe(&mut cursor).ok().flatten() else {
            return Ok(None);
        };
        let sz = h.size.unwrap_or(cursor.remaining());
        if sz > cursor.remaining() {
            return Ok(None);
        }
        let Some(body) = cursor.get(..sz) else {
            return Ok(None);
        };
        // Defense-in-depth cap on the bytes handed to the typed decoders:
        // HEIC iinf/iloc are KB-scale in real-world files. The original
        // `Vec::with_capacity(<attacker count>)` shape that bit
        // `parse_vorbis_comment` is fixed upstream (kixelated/mp4-atom#157,
        // closing #154); this guard remains so the same pattern surfacing
        // later in `ItemInfoEntry::decode_body` or `ItemLocation::decode_body`
        // shorts the OOM before the decoder ever sees the body.
        const MAX_META_SUBBOX_BYTES: usize = 8 * 1024 * 1024;
        if body.len() <= MAX_META_SUBBOX_BYTES {
            if h.kind == FourCC::new(b"iinf") {
                iinf = Some(
                    decode_iinf(body).map_err(|source| HeifError::MetaSubBoxDecode {
                        kind: FourCC::new(b"iinf"),
                        source,
                    })?,
                );
            } else if h.kind == FourCC::new(b"iloc") {
                iloc = Some(Iloc::decode_body(&mut &body[..]).map_err(|source| {
                    HeifError::MetaSubBoxDecode {
                        kind: FourCC::new(b"iloc"),
                        source,
                    }
                })?);
            }
        }
        cursor.advance(sz);
    }

    let (Some(iinf), Some(iloc)) = (iinf, iloc) else {
        return Ok(None);
    };
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
        return Ok(None);
    }
    let xmp_item_id = match find_meta_layout(file_bytes) {
        Ok((_, _, _, iref, primary_item_id, _)) => {
            match select_xmp_item_id(file_bytes, iref, primary_item_id, &xmp_item_ids)? {
                Some(item_id) => item_id,
                None => return Ok(None),
            }
        }
        // Without a resolvable item graph there is no association to read, so a
        // lone packet still answers for the primary image.
        Err(err) => match xmp_item_ids.as_slice() {
            [item_id] => *item_id,
            _ => return Err(err),
        },
    };
    let Some(loc) = iloc
        .item_locations
        .iter()
        .find(|l| l.item_id == xmp_item_id)
    else {
        return Ok(None);
    };
    if loc.data_reference_index != 0 {
        return Err(invalid_layout("XMP item uses an external data reference"));
    }
    if loc.construction_method != 0 {
        return Ok(None);
    }
    let Some(extent) = loc.extents.first() else {
        return Ok(None);
    };
    #[allow(
        clippy::cast_possible_truncation,
        reason = "HEIC file byte offsets/lengths fit in usize on 64-bit; kei targets 64-bit platforms"
    )]
    let start = loc.base_offset.saturating_add(extent.offset) as usize;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "HEIC extent length fits in usize on 64-bit"
    )]
    let Some(end) = start.checked_add(extent.length as usize) else {
        return Ok(None);
    };
    Ok(file_bytes.get(start..end).map(<[u8]>::to_vec))
}

/// Decode `iinf` while shielding kei from known mp4-atom panic paths.
///
/// `mp4-atom` 0.11.0 still has an `unimplemented!` branch for version-1
/// `infe` entries. `iinf` comes from user-controlled HEIF bytes, so kei
/// pre-screens that unsupported shape and converts it to a normal decode
/// error until upstream returns `Err` itself:
/// <https://github.com/kixelated/mp4-atom/issues/164>.
fn decode_iinf(body: &[u8]) -> Result<Iinf, mp4_atom::Error> {
    if contains_unsupported_infe_v1(body) {
        return Err(mp4_atom::Error::Unsupported("infe version 1 extensions"));
    }
    Iinf::decode_body(&mut &body[..])
}

fn contains_unsupported_infe_v1(mut body: &[u8]) -> bool {
    let Some(version) = body.first().copied() else {
        return false;
    };
    let Some(mut entries) = iinf_entry_count(body, version) else {
        return false;
    };

    let count_len = if version == 0 { 2 } else { 4 };
    let Some(entries_body) = body.get(4 + count_len..) else {
        return false;
    };
    body = entries_body;

    while entries > 0 {
        let before = body.len();
        let Some(header) = Header::decode_maybe(&mut body).ok().flatten() else {
            return false;
        };
        let header_len = before - body.len();
        let entry_body_len = header.size.unwrap_or(body.len());
        if entry_body_len > body.len() {
            return false;
        }

        // `mp4-atom` decodes every child declared by the `iinf` entry count
        // as `ItemInfoEntry` without checking the child FourCC first, so a
        // malformed child kind can still reach the version-1 `infe` panic.
        if body.first() == Some(&1) {
            return true;
        }

        let Some(rest) = body.get(entry_body_len..) else {
            return false;
        };
        body = rest;
        entries -= 1;

        if header_len == 0 {
            return false;
        }
    }

    false
}

fn iinf_entry_count(body: &[u8], version: u8) -> Option<u32> {
    match version {
        0 => body
            .get(4..6)
            .and_then(|count| count.try_into().ok())
            .map(|count| u16::from_be_bytes(count) as u32),
        1 => body
            .get(4..8)
            .and_then(|count| count.try_into().ok())
            .map(u32::from_be_bytes),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::HeifError;
    use super::super::test_support::malformed_iinf_meta_box;
    use super::{extract_xmp_bytes, extract_xmp_strict};
    use mp4_atom::FourCC;

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
        // (~21 GiB) - upstream kixelated/mp4-atom#154, fixed in #157. The
        // kei-side fix is independent: walk top-level boxes by header and
        // only descend into `meta`, so a hostile `dfLa` here is skipped
        // even if a future regression reintroduces the upstream bug.
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
        // The same upstream OOM was reachable from a `meta` box containing
        // a nested `dfLa` sub-atom, because mp4_atom::Meta::decode_body uses
        // `Any::decode_maybe` internally on every item. Upstream #157 caps
        // the `parse_vorbis_comment` allocation at the root, but the kei
        // header-walk also descends into meta with only `iinf`/`iloc`
        // decoders, so an attacker-supplied `dfLa` inside `meta` is skipped
        // regardless of upstream regressions.
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

    /// CG-14 / MS-5-full: a malformed iinf inside an otherwise-walkable
    /// meta box previously surfaced as a silent None — indistinguishable
    /// from "no XMP present". The strict variant must surface the
    /// structural failure as a typed `HeifError::MetaSubBoxDecode` so the
    /// metadata-write path can mark the asset for rewrite next sync. The
    /// lenient `extract_xmp_bytes` collapses the same input to None for
    /// callers (e.g. the EXIF probe) that don't care about the cause.
    #[test]
    fn extract_xmp_strict_returns_meta_sub_box_decode_on_malformed_iinf() {
        let meta_box = malformed_iinf_meta_box();

        let err = extract_xmp_strict(&meta_box).unwrap_err();
        match err {
            HeifError::MetaSubBoxDecode { kind, .. } => {
                assert_eq!(kind, FourCC::new(b"iinf"));
            }
            other => panic!("expected MetaSubBoxDecode for iinf, got {other:?}"),
        }

        // Lenient variant: same input, structural failure collapsed to None.
        assert!(extract_xmp_bytes(&meta_box).is_none());
    }

    #[test]
    fn extract_xmp_bytes_unsupported_infe_v1_returns_none_not_panic() {
        // Durable unit regression for fuzz artifact
        // crash-26040ebf1e311287ba7f285b767ac5a6ca9aef5e. The unsupported
        // version-1 `infe` shape must not panic in the lenient probe path.
        const REPRO: &[u8] = &[
            0x00, 0x00, 0x00, 0x00, b'm', b'e', b't', b'a', 0x00, 0x1d, 0x00, 0x22, 0x00, 0x00,
            0x00, 0x08, 0x00, 0x00, 0x00, 0x5b, 0x00, 0x00, 0x00, 0x00, b'i', b'i', b'n', b'f',
            0x00, 0x00, 0x00, 0x00, 0x5b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x41, 0x80,
            0x01, 0x00, 0x00, 0x04, 0x00, b'p', b'y', b't', b'f',
        ];

        let lenient = std::panic::catch_unwind(|| extract_xmp_bytes(REPRO));
        assert!(
            lenient.is_ok(),
            "lenient HEIF XMP probe must not panic on unsupported infe v1"
        );
        assert_eq!(lenient.unwrap(), None);

        let strict = std::panic::catch_unwind(|| extract_xmp_strict(REPRO));
        assert!(
            strict.is_ok(),
            "strict HEIF XMP probe must convert unsupported infe v1 to a typed error"
        );
        match strict.unwrap() {
            Err(HeifError::MetaSubBoxDecode { kind, .. }) => {
                assert_eq!(kind, FourCC::new(b"iinf"));
            }
            other => panic!("expected MetaSubBoxDecode for unsupported infe v1, got {other:?}"),
        }
    }
}
