//! Byte-preserving insertion and replacement of the selected XMP packet.

use super::boxes::{
    RawBox, box_with_body, extent_is_within_mdat_payload, find_meta_layout, is_heif_content,
    patch_box_size, scan_raw_boxes,
};
use super::items::{
    append_iinf_entry, append_iloc_entry, item_extent_is_shared, parse_iinf, parse_iloc,
    reject_meta_overlapping_items, resolve_item_extents, rewrite_existing_iloc,
};
use super::relationships::{
    append_cdsc_reference, insertion_described_item_ids, select_xmp_item_id, synthesise_cdsc_iref,
};
use super::{HeifError, invalid_layout};
use std::io::Write;

#[derive(Debug, Clone, Copy)]
pub(super) struct XmpLocation {
    pub(super) item_id: u32,
    pub(super) extent_start: usize,
    pub(super) extent_length: usize,
}

fn ensure_insertion_layout(bytes: &[u8]) -> Result<(), HeifError> {
    let top = scan_raw_boxes(bytes, 0, bytes.len())?;
    for atom in top {
        let allowed = atom.kind == *b"ftyp"
            || atom.kind == *b"meta"
            || atom.kind == *b"mdat"
            || atom.kind == *b"free"
            || atom.kind == *b"wide";
        if !allowed {
            return Err(invalid_layout(
                "HEIC insertion refuses top-level boxes with unhandled file offsets",
            ));
        }
    }
    Ok(())
}

pub(super) fn locate_xmp(
    bytes: &[u8],
    iinf: RawBox,
    iloc: RawBox,
    iref: Option<RawBox>,
    primary_item_id: u32,
) -> Result<
    (
        Option<XmpLocation>,
        Option<u32>,
        u32,
        u8,
        usize,
        usize,
        Vec<u32>,
    ),
    HeifError,
> {
    let iinf_layout = parse_iinf(bytes, iinf)?;
    let iloc_layout = parse_iloc(bytes, iloc)?;
    let mut max_item_id = iinf_layout.max_item_id;
    for item in &iloc_layout.items {
        if !iinf_layout.item_ids.contains(&item.item_id) {
            return Err(invalid_layout("iloc contains an item ID absent from iinf"));
        }
        max_item_id = max_item_id.max(item.item_id);
    }
    let xmp_item_id = select_xmp_item_id(bytes, iref, primary_item_id, &iinf_layout.xmp_item_ids)?;
    let xmp = xmp_item_id.and_then(|item_id| {
        iloc_layout
            .items
            .iter()
            .find(|item| item.item_id == item_id)
            .and_then(|item| {
                let extent = item.extents.first()?;
                let start = item.base_offset.checked_add(extent.offset)?;
                let start = usize::try_from(start).ok()?;
                let length = usize::try_from(extent.length).ok()?;
                let end = start.checked_add(length)?;
                if item.construction_method == 0 && end <= bytes.len() {
                    Some(XmpLocation {
                        item_id,
                        extent_start: start,
                        extent_length: length,
                    })
                } else {
                    None
                }
            })
    });
    Ok((
        xmp,
        xmp_item_id,
        max_item_id,
        iinf_layout.version,
        iinf_layout.count_pos,
        iinf_layout.count_size,
        iinf_layout.item_ids,
    ))
}

/// Rewrite an XMP packet without decoding or re-encoding the surrounding
/// HEIC item graph. Existing packets are replaced in place when possible;
/// otherwise only the XMP iloc entry is repointed to an appended mdat. Files
/// without XMP receive one new item, one new location, and a `cdsc` reference
/// to the primary image, synthesising an `iref` box when the file has none.
/// Every unrelated box and payload is copied unchanged.
///
/// Insertion is limited to files whose top-level boxes have no unhandled
/// absolute offsets. Existing-XMP updates do not grow `meta` and can append
/// after otherwise unsupported top-level boxes. The complete rewritten file
/// is materialised before it is written to `writer`.
#[allow(
    clippy::indexing_slicing,
    reason = "All rewrite ranges come from validated box boundaries and checked XMP extent arithmetic."
)]
pub(crate) fn rewrite_xmp<W: Write>(
    input: &[u8],
    xmp: &[u8],
    mut writer: W,
) -> Result<(), HeifError> {
    if !is_heif_content(input) {
        return Err(invalid_layout("input is not a HEIF-family file"));
    }
    let (meta, iinf, iloc, iref, primary_item_id, prefix_size) = find_meta_layout(input)?;
    let iloc_layout = parse_iloc(input, iloc)?;
    let (
        existing,
        xmp_item_id,
        max_item_id,
        iinf_version,
        iinf_count_pos,
        iinf_count_size,
        iinf_item_ids,
    ) = locate_xmp(input, iinf, iloc, iref, primary_item_id)?;

    reject_meta_overlapping_items(&iloc_layout, meta)?;
    for item in &iloc_layout.items {
        if item.construction_method == 0 {
            let _ = resolve_item_extents(input, item)?;
        }
    }

    if let Some(location) = existing {
        let item = iloc_layout
            .items
            .iter()
            .find(|item| item.item_id == location.item_id)
            .ok_or_else(|| invalid_layout("XMP item has no iloc entry"))?;
        if item.extents.len() != 1 {
            return Err(invalid_layout("XMP item uses multiple iloc extents"));
        }
        let end = location
            .extent_start
            .checked_add(location.extent_length)
            .ok_or_else(|| invalid_layout("existing XMP extent overflows"))?;
        if end > input.len() {
            return Err(invalid_layout("existing XMP extent is outside the file"));
        }
        let shared = item_extent_is_shared(
            &iloc_layout,
            location.item_id,
            u64::try_from(location.extent_start)
                .map_err(|_| invalid_layout("existing XMP offset overflows"))?,
            u64::try_from(location.extent_length)
                .map_err(|_| invalid_layout("existing XMP length overflows"))?,
        );
        let in_mdat =
            extent_is_within_mdat_payload(input, location.extent_start, location.extent_length)?;
        if xmp.len() <= location.extent_length && !shared && in_mdat {
            let mut output = input.to_vec();
            output[location.extent_start..location.extent_start + xmp.len()].copy_from_slice(xmp);
            output[location.extent_start + xmp.len()..end].fill(b' ');
            writer.write_all(&output)?;
            return Ok(());
        }
        let new_data_offset = u64::try_from(input.len())
            .ok()
            .and_then(|len| len.checked_add(8))
            .ok_or_else(|| invalid_layout("new XMP offset overflows"))?;
        let new_iloc = rewrite_existing_iloc(
            input,
            iloc,
            &iloc_layout,
            location.item_id,
            new_data_offset,
            u64::try_from(xmp.len()).map_err(|_| invalid_layout("XMP packet is too large"))?,
        )?;
        let mut output = Vec::new();
        output.extend_from_slice(&input[..iloc.start]);
        output.extend_from_slice(&new_iloc);
        output.extend_from_slice(&input[iloc.end()..]);
        output.extend_from_slice(&box_with_body(*b"mdat", xmp)?);
        writer.write_all(&output)?;
        return Ok(());
    }

    if xmp_item_id.is_some() {
        return Err(invalid_layout(
            "existing XMP item uses an unsupported iloc layout",
        ));
    }
    ensure_insertion_layout(input)?;
    let iinf_layout = parse_iinf(input, iinf)?;
    let described_item_ids =
        insertion_described_item_ids(input, iref, primary_item_id, &iinf_layout)?;
    let new_item_id = max_item_id
        .checked_add(1)
        .ok_or_else(|| invalid_layout("no free HEIC item id remains"))?;
    if iinf_version == 0 && new_item_id > u16::MAX as u32 && iloc_layout.version != 2 {
        return Err(invalid_layout(
            "new XMP item id does not fit this HEIC item map",
        ));
    }
    let new_iinf = append_iinf_entry(input, iinf, iinf_count_pos, iinf_count_size, new_item_id)?;
    let (new_iref, old_iref_size) = match iref {
        Some(iref) => (
            append_cdsc_reference(
                input,
                iref,
                new_item_id,
                described_item_ids.as_slice(),
                &iinf_item_ids,
            )?,
            iref.size,
        ),
        None => (
            synthesise_cdsc_iref(new_item_id, described_item_ids.as_slice())?,
            0,
        ),
    };
    let old_meta_len = meta.size;
    let xmp_length =
        u64::try_from(xmp.len()).map_err(|_| invalid_layout("XMP packet is too large"))?;
    let placeholder_iloc = append_iloc_entry(
        input,
        iloc,
        &iloc_layout,
        new_item_id,
        u64::try_from(input.len())
            .ok()
            .and_then(|len| len.checked_add(8))
            .ok_or_else(|| invalid_layout("new XMP offset overflows"))?,
        xmp_length,
        0,
        u64::try_from(meta.end()).map_err(|_| invalid_layout("HEIC meta offset overflows"))?,
    )?;
    let new_meta_len = old_meta_len
        .checked_add(new_iinf.len())
        .and_then(|len| len.checked_sub(iinf.size))
        .and_then(|len| len.checked_add(new_iref.len()))
        .and_then(|len| len.checked_sub(old_iref_size))
        .and_then(|len| len.checked_add(placeholder_iloc.len()))
        .and_then(|len| len.checked_sub(iloc.size))
        .ok_or_else(|| invalid_layout("new HEIC meta size overflows"))?;
    let delta = u64::try_from(new_meta_len)
        .ok()
        .and_then(|new_len| {
            u64::try_from(old_meta_len)
                .ok()
                .and_then(|old_len| new_len.checked_sub(old_len))
        })
        .ok_or_else(|| invalid_layout("new HEIC meta size is invalid"))?;
    let data_offset = u64::try_from(input.len())
        .ok()
        .and_then(|len| len.checked_add(delta))
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| invalid_layout("new XMP offset overflows"))?;
    let new_iloc = append_iloc_entry(
        input,
        iloc,
        &iloc_layout,
        new_item_id,
        data_offset,
        xmp_length,
        delta,
        u64::try_from(meta.end()).map_err(|_| invalid_layout("HEIC meta offset overflows"))?,
    )?;
    let new_meta_len = old_meta_len
        .checked_add(new_iinf.len())
        .and_then(|len| len.checked_sub(iinf.size))
        .and_then(|len| len.checked_add(new_iref.len()))
        .and_then(|len| len.checked_sub(old_iref_size))
        .and_then(|len| len.checked_add(new_iloc.len()))
        .and_then(|len| len.checked_sub(iloc.size))
        .ok_or_else(|| invalid_layout("new HEIC meta size overflows"))?;
    let mut meta_bytes = Vec::with_capacity(new_meta_len);
    meta_bytes.extend_from_slice(&input[meta.start..meta.body_start() + prefix_size]);
    let children = scan_raw_boxes(input, meta.body_start() + prefix_size, meta.end())?;
    for child in children {
        if child.start == iinf.start {
            meta_bytes.extend_from_slice(&new_iinf);
            if iref.is_none() {
                meta_bytes.extend_from_slice(&new_iref);
            }
        } else if iref.is_some_and(|existing| existing.start == child.start) {
            meta_bytes.extend_from_slice(&new_iref);
        } else if child.start == iloc.start {
            meta_bytes.extend_from_slice(&new_iloc);
        } else {
            meta_bytes.extend_from_slice(&input[child.start..child.end()]);
        }
    }
    patch_box_size(&mut meta_bytes)?;
    if meta_bytes.len() != new_meta_len {
        return Err(invalid_layout("rewritten HEIC meta size is inconsistent"));
    }
    let mut output = Vec::new();
    output.extend_from_slice(&input[..meta.start]);
    output.extend_from_slice(&meta_bytes);
    output.extend_from_slice(&input[meta.end()..]);
    output.extend_from_slice(&box_with_body(*b"mdat", xmp)?);
    writer.write_all(&output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::HeifError;
    use super::super::boxes::{find_meta_layout, is_heif_content, scan_raw_boxes};
    use super::super::exif::extract_exif_tiff_bytes;
    #[cfg(feature = "__fuzz_internals")]
    use super::super::fuzz::fuzz_rewrite_xmp_preserves;
    use super::super::insert_xmp;
    use super::super::items::{parse_iloc, write_uint};
    use super::super::preservation::validate_rewrite_preserves_non_xmp_items;
    use super::super::test_support::{
        AUX_XMP, HeicSpec, ItemSpec, MATRIX_XMP, PRIMARY_XMP, apple_multi_xmp_spec,
        apple_tmap_insertion_heic, apple_tmap_insertion_spec,
        apple_tmap_insertion_spec_with_xmp_targets, build_heic, cdsc_references,
        decode_iloc_from_heic, duplicate_meta_child, duplicate_top_level_meta, extract_xmp_raw,
        find_mdat, meta_child_bytes, ref_box, resolve_item_data, tone_map_item,
        xmp_and_primary_item_ids, xmp_item,
    };
    use super::super::xmp_read::{extract_xmp_bytes, extract_xmp_strict};
    use super::{locate_xmp, rewrite_xmp};
    use mp4_atom::{Any, DecodeMaybe, Encode, Iloc, ItemLocation, ItemLocationExtent};

    #[test]
    fn rewrite_xmp_preserves_real_heic_image_data_and_is_idempotent() {
        let input = include_bytes!("../../../tests/data/sample.heic");
        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>5</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut first = Vec::new();
        rewrite_xmp(input, xmp, &mut first).expect("real HEIC XMP insertion");
        assert!(is_heif_content(&first));
        assert_eq!(extract_xmp_bytes(&first).as_deref(), Some(xmp.as_slice()));
        let (_, image_data) = find_mdat(input).expect("source image mdat");
        let (rewritten_image_start, rewritten_image_data) =
            find_mdat(&first).expect("rewritten image mdat");
        assert_eq!(Some(image_data), Some(rewritten_image_data));
        let iloc = decode_iloc_from_heic(&first).expect("rewritten iloc");
        let image_item = iloc
            .item_locations
            .iter()
            .find(|item| item.item_id == 1)
            .expect("source image item");
        let image_offset = image_item
            .base_offset
            .saturating_add(image_item.extents[0].offset);
        assert_eq!(
            image_offset,
            rewritten_image_start + 8,
            "rewritten image item must still point at the image mdat"
        );
        assert!(
            cdsc_references(input)
                .into_iter()
                .all(|reference| cdsc_references(&first).contains(&reference)),
            "pre-existing cdsc associations must remain intact"
        );
        let (xmp_item_id, primary_item_id) = xmp_and_primary_item_ids(&first);
        assert!(
            cdsc_references(&first).contains(&(xmp_item_id, primary_item_id)),
            "new XMP item must describe the primary image through cdsc"
        );

        let mut second = Vec::new();
        rewrite_xmp(&first, xmp, &mut second).expect("repeat HEIC XMP insertion");
        assert_eq!(
            second, first,
            "repeating the same XMP update must be byte-idempotent"
        );

        let changed = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating><dc:description xmlns:dc='http://purl.org/dc/elements/1.1/'>changed</dc:description></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut third = Vec::new();
        rewrite_xmp(&first, changed, &mut third).expect("changed HEIC XMP update");
        assert_eq!(
            extract_xmp_bytes(&third).as_deref(),
            Some(changed.as_slice())
        );
        assert_eq!(Some(image_data), find_mdat(&third).map(|(_, data)| data));
    }

    #[test]
    fn fuzz_seeds_reach_existing_xmp_replacement_paths() {
        const MARKER: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let fits = include_bytes!("../../../fuzz/seeds/heif_rewrite/replacement-fits");
        let grows = include_bytes!("../../../fuzz/seeds/heif_rewrite/replacement-grows");

        let mut replaced_in_place = Vec::new();
        rewrite_xmp(fits, MARKER, &mut replaced_in_place).expect("fitting replacement seed");
        assert_eq!(
            replaced_in_place.len(),
            fits.len(),
            "fitting seed must take the in-place replacement path"
        );

        let mut replaced_by_append = Vec::new();
        rewrite_xmp(grows, MARKER, &mut replaced_by_append).expect("growing replacement seed");
        assert!(
            replaced_by_append.len() > grows.len(),
            "growing seed must repoint XMP to an appended mdat"
        );

        // The fuzzer will not synthesise a valid multi-image item map on its
        // own, so selection stays unreachable without a seed carrying one.
        let multi = include_bytes!("../../../fuzz/seeds/heif_rewrite/multi-xmp");
        assert_eq!(
            extract_xmp_raw(multi).as_deref(),
            Some(PRIMARY_XMP),
            "multi-image seed must resolve to the primary image's packet"
        );
        #[cfg(feature = "__fuzz_internals")]
        fuzz_rewrite_xmp_preserves(multi);

        let conflicting = include_bytes!("../../../fuzz/seeds/heif_rewrite/conflicting-tmap-xmp");
        let mut rejected = Vec::new();
        assert!(rewrite_xmp(conflicting, MARKER, &mut rejected).is_err());
        assert!(
            rejected.is_empty(),
            "conflicting ownership seed must fail before emitting bytes"
        );
        #[cfg(feature = "__fuzz_internals")]
        fuzz_rewrite_xmp_preserves(conflicting);
    }

    #[test]
    fn rewrite_xmp_appends_when_existing_extent_includes_mdat_header() {
        const MARKER: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut input =
            include_bytes!("../../../fuzz/seeds/heif_rewrite/replacement-fits").to_vec();
        let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(&input).unwrap();
        let (location, xmp_item_id, _, _, _, _, _) =
            locate_xmp(&input, iinf, iloc, iref, primary_item_id).unwrap();
        let location = location.expect("seed XMP location");
        let layout = parse_iloc(&input, iloc).unwrap();
        let item = layout
            .items
            .iter()
            .find(|item| Some(item.item_id) == xmp_item_id)
            .expect("seed XMP item");
        let iloc_body = &mut input[iloc.body_start()..iloc.end()];
        if let Some(base_offset_pos) = item.base_offset_pos {
            write_uint(iloc_body, base_offset_pos, layout.base_offset_size, 0).unwrap();
        }
        write_uint(
            iloc_body,
            item.extents[0].offset_pos.expect("XMP extent offset"),
            layout.offset_size,
            u64::try_from(location.extent_start - 8).unwrap(),
        )
        .unwrap();

        let mut output = Vec::new();
        rewrite_xmp(&input, MARKER, &mut output).expect("safe append replacement");

        assert!(
            output.len() > input.len(),
            "an XMP extent that includes an mdat header must not be changed in place"
        );
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MARKER));
        validate_rewrite_preserves_non_xmp_items(&input, &output)
            .expect("append replacement must preserve protected bytes");
    }

    #[test]
    fn rewrite_xmp_rejects_malformed_heic_without_writing() {
        let input = &include_bytes!("../../../tests/data/sample.heic")[..input_len()];
        let mut output = Vec::new();
        let result = rewrite_xmp(input, b"<x:xmpmeta/>", &mut output);
        assert!(result.is_err());
        assert!(output.is_empty());

        fn input_len() -> usize {
            include_bytes!("../../../tests/data/sample.heic").len() - 3
        }
    }

    #[test]
    fn rewrite_xmp_rejects_insertion_when_top_level_offsets_are_unhandled() {
        let mut input = include_bytes!("../../../tests/data/sample.heic").to_vec();
        input.extend_from_slice(&8u32.to_be_bytes());
        input.extend_from_slice(b"moov");

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, b"<x:xmpmeta/>", &mut output);
        assert!(result.is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn rewrite_xmp_rejects_orphan_iloc_item_id() {
        let mut atoms = Vec::new();
        let mut cursor: &[u8] = include_bytes!("../../../tests/data/sample.heic");
        while let Some(atom) = Any::decode_maybe(&mut cursor).expect("sample HEIC") {
            atoms.push(atom);
        }
        let meta = atoms
            .iter_mut()
            .find_map(|atom| match atom {
                Any::Meta(meta) => Some(meta),
                _ => None,
            })
            .expect("sample meta");
        meta.get_mut::<Iloc>()
            .expect("sample iloc")
            .item_locations
            .push(ItemLocation {
                item_id: 999,
                construction_method: 0,
                data_reference_index: 0,
                base_offset: 0,
                extents: vec![ItemLocationExtent {
                    item_reference_index: 0,
                    offset: 0,
                    length: 0,
                }],
            });
        let mut input = Vec::new();
        for atom in atoms {
            atom.encode(&mut input).expect("encode malformed fixture");
        }

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, b"<x:xmpmeta/>", &mut output);
        assert!(result.is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn rewrite_xmp_existing_item_can_append_after_unhandled_top_level_box() {
        let seed = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF/></x:xmpmeta>";
        let mut seeded = Vec::new();
        insert_xmp(
            include_bytes!("../../../tests/data/sample.heic"),
            seed,
            &mut seeded,
        )
        .expect("seed XMP");
        seeded.extend_from_slice(&8u32.to_be_bytes());
        seeded.extend_from_slice(b"moov");

        let replacement =
            b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>5</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let mut output = Vec::new();
        rewrite_xmp(&seeded, replacement, &mut output).expect("existing XMP append");
        assert_eq!(
            extract_xmp_bytes(&output).as_deref(),
            Some(replacement.as_slice())
        );
        assert!(
            output
                .windows(8)
                .any(|window| window == [0, 0, 0, 8, b'm', b'o', b'o', b'v'])
        );
    }

    /// Independent-reader oracle. kei's reader and writer share a code base, so
    /// a packet that round-trips through both proves only self-consistency. This
    /// drives ExifTool, which parses the HEIF item map itself and applies the
    /// same primary-image rule, over the rewritten bytes.
    ///
    /// ExifTool is optional for a local run. `KEI_REQUIRE_HEIF_ORACLE` makes it
    /// mandatory, so CI cannot lose this coverage by failing to install it.
    #[test]
    fn rewrite_xmp_is_readable_by_an_independent_heif_reader() {
        use std::process::Command;

        let available = Command::new("exiftool")
            .arg("-ver")
            .output()
            .is_ok_and(|out| out.status.success());
        if !available {
            let required = std::env::var("KEI_REQUIRE_HEIF_ORACLE")
                .is_ok_and(|value| !value.trim().is_empty());
            assert!(
                !required,
                "KEI_REQUIRE_HEIF_ORACLE is set but exiftool is not installed"
            );
            eprintln!("exiftool unavailable; skipping independent HEIF reader check");
            return;
        }

        let xmp = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>5</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
        let dir = tempfile::tempdir().expect("reader fixture directory");
        let tone_map_insertion = apple_tmap_insertion_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
            AUX_XMP,
        );

        let read_tag = |path: &std::path::Path, tag: &str| -> String {
            let out = Command::new("exiftool")
                .args(["-s3", tag])
                .arg(path)
                .output()
                .expect("run exiftool");
            assert!(
                out.status.success(),
                "exiftool must accept {}: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        for (name, source) in [
            (
                "sample.heic",
                include_bytes!("../../../tests/data/sample.heic").as_slice(),
            ),
            (
                "apple-hdr-gainmap.heic",
                include_bytes!("../../../tests/data/apple-hdr-gainmap.heic").as_slice(),
            ),
            (
                "white_1x1.avif",
                include_bytes!("../../../tests/data/white_1x1.avif").as_slice(),
            ),
            ("tone-map-insertion.heic", tone_map_insertion.as_slice()),
        ] {
            let mut output = Vec::new();
            rewrite_xmp(source, xmp, &mut output).expect("rewrite for reader check");
            let path = dir.path().join(name);
            std::fs::write(&path, &output).expect("write reader fixture");

            assert_eq!(
                read_tag(&path, "-Validate"),
                "OK",
                "{name} must still validate after a rewrite"
            );
            assert_eq!(
                read_tag(&path, "-XMP:Rating"),
                "5",
                "an independent reader must resolve the packet kei wrote into {name}"
            );

            let source_path = dir.path().join(format!("source-{name}"));
            std::fs::write(&source_path, source).expect("write source fixture");
            assert_eq!(
                read_tag(&path, "-HDRGainMapVersion"),
                read_tag(&source_path, "-HDRGainMapVersion"),
                "{name} must keep whatever gain map it arrived with"
            );
        }
    }

    #[test]
    fn rewrite_xmp_inserts_into_grid_dimg_layout() {
        let grid_header = vec![0u8, 0, 1, 1, 0, 0x40, 0, 0x40];
        let spec = HeicSpec {
            iloc_version: 1,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![
                ItemSpec {
                    item_id: 1,
                    item_type: *b"grid",
                    infe_version: 2,
                    construction_method: 1,
                    data: Vec::new(),
                    offset: 0,
                    length: grid_header.len() as u64,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..40).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 3,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (40u8..96).collect(),
                    offset: 0,
                    length: 0,
                },
            ],
            idat: Some(grid_header.clone()),
            iref_children: vec![ref_box(b"dimg", 1, &[2, 3])],
        };
        let input = build_heic(&spec);
        assert!(is_heif_content(&input));

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("grid layout insertion");

        assert_eq!(resolve_item_data(&input, 2), resolve_item_data(&output, 2));
        assert_eq!(resolve_item_data(&input, 3), resolve_item_data(&output, 3));
        assert_eq!(
            meta_child_bytes(&input, b"idat"),
            meta_child_bytes(&output, b"idat"),
            "construction-method-1 grid header must survive"
        );
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        let dimg = ref_box(b"dimg", 1, &[2, 3]);
        assert!(
            output.windows(dimg.len()).any(|window| window == dimg),
            "existing dimg reference must be copied unchanged"
        );
        let (xmp_id, _) = xmp_and_primary_item_ids(&output);
        assert!(cdsc_references(&output).contains(&(xmp_id, 1)));

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("grid layout idempotent");
        assert_eq!(
            again, output,
            "repeated grid rewrite must be byte-idempotent"
        );
    }

    #[test]
    fn rewrite_xmp_targets_the_xmp_item_describing_the_primary_image() {
        let spec = apple_multi_xmp_spec(
            vec![xmp_item(5, AUX_XMP), xmp_item(6, PRIMARY_XMP)],
            vec![ref_box(b"cdsc", 5, &[4]), ref_box(b"cdsc", 6, &[1, 3])],
        );
        let input = build_heic(&spec);

        assert_eq!(
            extract_xmp_raw(&input).as_deref(),
            Some(PRIMARY_XMP),
            "the writer must resolve the packet the cdsc binds to the primary"
        );
        assert_eq!(
            extract_xmp_strict(&input).unwrap().as_deref(),
            Some(PRIMARY_XMP),
            "the reader must resolve the same packet as the writer, or a merge \
             would move auxiliary metadata onto the photograph"
        );

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("multi-XMP rewrite");

        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        assert_eq!(
            resolve_item_data(&output, 5),
            AUX_XMP,
            "the auxiliary image's packet must survive byte-for-byte"
        );
        for tile in [2, 3, 4] {
            assert_eq!(
                resolve_item_data(&input, tile),
                resolve_item_data(&output, tile),
                "item {tile} payload must be untouched"
            );
        }
        validate_rewrite_preserves_non_xmp_items(&input, &output)
            .expect("every item but the selected XMP packet must be preserved");

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("multi-XMP idempotent");
        assert_eq!(again, output, "repeated rewrite must be byte-idempotent");
    }

    /// The synthetic multi-XMP tests above control the item map precisely, but
    /// they are still kei's own idea of an Apple file. This drives the same
    /// selection rule through a genuine iOS 17.6.1 HDR capture: a `grid`
    /// primary over six tiles, a gain map, and two XMP packets, one describing
    /// the photograph and one describing the gain map. Choosing the wrong one
    /// moves the user's rating onto the gain map.
    #[test]
    fn rewrite_xmp_targets_the_primary_packet_in_a_real_apple_hdr_capture() {
        const PRIMARY_XMP_ITEM: u32 = 9;
        const GAIN_MAP_XMP_ITEM: u32 = 11;
        let input = include_bytes!("../../../tests/data/apple-hdr-gainmap.heic");

        let gain_map_packet = resolve_item_data(input, GAIN_MAP_XMP_ITEM);
        let selected = extract_xmp_raw(input).expect("the capture carries XMP");
        assert_eq!(
            selected,
            resolve_item_data(input, PRIMARY_XMP_ITEM),
            "the writer must resolve the packet bound to the primary image"
        );
        assert_ne!(
            selected, gain_map_packet,
            "the gain map's packet must never answer for the photograph"
        );
        assert_eq!(
            extract_xmp_strict(input).unwrap().as_deref(),
            Some(selected.as_slice()),
            "the reader must resolve the same packet as the writer"
        );

        let mut output = Vec::new();
        rewrite_xmp(input, MATRIX_XMP, &mut output).expect("real Apple HDR rewrite");

        // The capture's packet is far larger than the replacement, so this is
        // the in-place branch: the extent is reused and the tail padded.
        let written = extract_xmp_raw(&output).expect("rewritten capture carries XMP");
        assert_eq!(written.trim_ascii_end(), MATRIX_XMP);
        assert_eq!(
            written.len(),
            selected.len(),
            "a shrinking packet must reuse the existing extent"
        );
        assert_eq!(
            resolve_item_data(&output, GAIN_MAP_XMP_ITEM),
            gain_map_packet,
            "the gain map's packet must survive byte-for-byte"
        );
        validate_rewrite_preserves_non_xmp_items(input, &output)
            .expect("every tile, the gain map, and Exif must be preserved");

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("real Apple HDR idempotent");
        assert_eq!(again, output, "repeated rewrite must be byte-idempotent");
    }

    #[test]
    fn rewrite_xmp_inserts_when_every_packet_describes_an_auxiliary_image() {
        // Twelve of sixty-one files in a real library carry one auxiliary
        // packet, and eight carry several. In both the photograph has no XMP,
        // so selecting one would overwrite a gain map's metadata with the
        // photograph's.
        for (xmp_items, cdsc) in [
            (vec![xmp_item(5, AUX_XMP)], vec![ref_box(b"cdsc", 5, &[4])]),
            (
                vec![xmp_item(5, AUX_XMP), xmp_item(6, AUX_XMP)],
                vec![ref_box(b"cdsc", 5, &[4]), ref_box(b"cdsc", 6, &[3])],
            ),
        ] {
            let ids: Vec<u32> = xmp_items.iter().map(|item| item.item_id).collect();
            let spec = apple_multi_xmp_spec(xmp_items, cdsc);
            let input = build_heic(&spec);

            assert!(
                extract_xmp_strict(&input).unwrap().is_none(),
                "an auxiliary packet must not answer for the primary image"
            );

            let mut output = Vec::new();
            rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("insertion beside auxiliary XMP");

            assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
            for id in &ids {
                assert_eq!(
                    resolve_item_data(&output, *id),
                    AUX_XMP,
                    "auxiliary packet {id} must survive byte-for-byte"
                );
            }
            let (xmp_id, _) = xmp_and_primary_item_ids(&output);
            assert!(
                cdsc_references(&output).contains(&(xmp_id, 1)),
                "the inserted item must be bound to the primary image"
            );
            validate_rewrite_preserves_non_xmp_items(&input, &output)
                .expect("insertion must preserve every existing item");

            let mut again = Vec::new();
            rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("insertion idempotent");
            assert_eq!(again, output, "repeated rewrite must be byte-idempotent");
        }
    }

    #[test]
    fn rewrite_xmp_refuses_unproved_tone_map_insertion() {
        let mut insertion = apple_multi_xmp_spec(Vec::new(), Vec::new());
        insertion.items.push(tone_map_item(7));
        let input = build_heic(&insertion);
        assert!(
            extract_xmp_strict(&input).unwrap().is_none(),
            "the fixture must carry no XMP so the writer takes the insertion path"
        );
        let mut output = Vec::new();
        let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
        assert!(
            result.is_err(),
            "insertion must fail until the relevant tone map can be selected from real relationship evidence"
        );
        assert!(output.is_empty(), "a refused rewrite must emit no bytes");

        let mut replacement = apple_multi_xmp_spec(
            vec![xmp_item(6, PRIMARY_XMP)],
            vec![ref_box(b"cdsc", 6, &[1])],
        );
        replacement.items.push(tone_map_item(7));
        let input = build_heic(&replacement);
        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output)
            .expect("replacing an existing packet must still be allowed");
        assert_eq!(
            extract_xmp_raw(&output)
                .as_deref()
                .map(<[u8]>::trim_ascii_end),
            Some(MATRIX_XMP)
        );
        assert_eq!(
            cdsc_references(&output),
            cdsc_references(&input),
            "replacing a packet must leave every existing reference alone"
        );
        assert_eq!(
            resolve_item_data(&output, 7),
            (0u8..24).collect::<Vec<u8>>(),
            "the tone-mapped image must survive byte-for-byte"
        );
    }

    #[test]
    fn rewrite_xmp_inserts_when_primary_exif_proves_the_tone_map() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        for exif_targets in [[1, 7], [7, 1]] {
            let input = build_heic(&apple_tmap_insertion_spec(
                &tiff,
                AUX_XMP,
                &[1, 4],
                &exif_targets,
            ));

            assert!(
                extract_xmp_strict(&input).unwrap().is_none(),
                "only auxiliary XMP must exist before insertion"
            );

            let mut output = Vec::new();
            rewrite_xmp(&input, MATRIX_XMP, &mut output)
                .expect("primary Exif proves the related tone map");

            let (xmp_id, primary_id) = xmp_and_primary_item_ids(&output);
            let references = cdsc_references(&output);
            assert!(references.contains(&(xmp_id, primary_id)));
            assert!(references.contains(&(xmp_id, 7)));
            assert_eq!(
                resolve_item_data(&output, 5),
                AUX_XMP,
                "auxiliary XMP must remain byte-for-byte stable"
            );
            assert_eq!(
                resolve_item_data(&input, 7),
                resolve_item_data(&output, 7),
                "tone-map metadata must remain byte-for-byte stable"
            );
            validate_rewrite_preserves_non_xmp_items(&input, &output)
                .expect("insertion must preserve every pre-existing item");
        }
    }

    #[test]
    fn rewrite_xmp_rejects_ambiguous_tone_map_relationships() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let mut multiple_tone_maps = apple_tmap_insertion_spec(&tiff, AUX_XMP, &[1, 4], &[1, 7]);
        multiple_tone_maps.items.push(tone_map_item(9));
        multiple_tone_maps
            .iref_children
            .push(ref_box(b"dimg", 9, &[1, 4]));
        let mut repeated_exif_scope = apple_tmap_insertion_spec(&tiff, AUX_XMP, &[1, 4], &[1, 7]);
        repeated_exif_scope
            .iref_children
            .push(ref_box(b"cdsc", 8, &[4]));
        let mut primary_is_tone_map = apple_tmap_insertion_spec(&tiff, AUX_XMP, &[7, 4], &[7, 7]);
        primary_is_tone_map.primary_id = 7;
        let cases = [
            (
                "tone map does not use primary as base",
                apple_tmap_insertion_spec(&tiff, AUX_XMP, &[4, 1], &[1, 7]),
            ),
            (
                "Exif does not describe tone map",
                apple_tmap_insertion_spec(&tiff, AUX_XMP, &[1, 4], &[1]),
            ),
            (
                "Exif has an additional target",
                apple_tmap_insertion_spec(&tiff, AUX_XMP, &[1, 4], &[1, 7, 4]),
            ),
            (
                "tone map uses itself as gain map",
                apple_tmap_insertion_spec(&tiff, AUX_XMP, &[1, 7], &[1, 7]),
            ),
            ("primary item is the tone map", primary_is_tone_map),
            ("Exif has an additional cdsc", repeated_exif_scope),
            ("multiple tone maps", multiple_tone_maps),
        ];

        for (name, spec) in cases {
            let input = build_heic(&spec);
            let mut output = Vec::new();
            assert!(
                rewrite_xmp(&input, MATRIX_XMP, &mut output).is_err(),
                "{name}"
            );
            assert!(output.is_empty(), "{name}");
        }
    }

    #[test]
    fn rewrite_xmp_rejects_existing_xmp_ownership_of_the_tone_map() {
        let tiff = crate::test_helpers::minimal_tiff_with_source_gps();
        let mut multiple_descriptors =
            apple_tmap_insertion_spec_with_xmp_targets(&tiff, AUX_XMP, &[7], &[1, 4], &[1, 7]);
        multiple_descriptors.items.push(xmp_item(6, AUX_XMP));
        multiple_descriptors
            .iref_children
            .push(ref_box(b"cdsc", 6, &[7]));
        let cases = [
            (
                "tone-map-only XMP",
                apple_tmap_insertion_spec_with_xmp_targets(&tiff, AUX_XMP, &[7], &[1, 4], &[1, 7]),
            ),
            (
                "multi-target XMP including tone map",
                apple_tmap_insertion_spec_with_xmp_targets(
                    &tiff,
                    AUX_XMP,
                    &[4, 7],
                    &[1, 4],
                    &[1, 7],
                ),
            ),
            ("multiple tone-map XMP descriptors", multiple_descriptors),
        ];

        for (name, spec) in cases {
            let input = build_heic(&spec);
            assert!(
                extract_xmp_strict(&input).unwrap().is_none(),
                "{name} must not answer as primary XMP"
            );
            let mut output = Vec::new();
            assert!(
                rewrite_xmp(&input, MATRIX_XMP, &mut output).is_err(),
                "{name} must fail closed"
            );
            assert!(output.is_empty(), "{name} must emit no bytes");
        }
    }

    #[test]
    fn rewrite_xmp_rejects_duplicate_singleton_boxes() {
        let input = apple_tmap_insertion_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
            AUX_XMP,
        );
        let cases = [
            ("duplicate pitm", duplicate_meta_child(&input, *b"pitm")),
            ("duplicate iinf", duplicate_meta_child(&input, *b"iinf")),
            ("duplicate iloc", duplicate_meta_child(&input, *b"iloc")),
            ("duplicate iref", duplicate_meta_child(&input, *b"iref")),
            ("duplicate meta", duplicate_top_level_meta(&input)),
        ];

        for (name, bytes) in cases {
            let mut output = Vec::new();
            assert!(
                rewrite_xmp(&bytes, MATRIX_XMP, &mut output).is_err(),
                "{name}"
            );
            assert!(output.is_empty(), "{name}");
        }
    }

    #[test]
    fn rewrite_xmp_rejects_external_data_references() {
        for construction_method in 0..=2 {
            let spec = HeicSpec {
                iloc_version: 1,
                offset_size: 4,
                length_size: 4,
                base_offset_size: 4,
                index_size: 0,
                primary_id: 1,
                items: vec![ItemSpec {
                    item_id: 1,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method,
                    data: if construction_method == 0 {
                        (0u8..32).collect()
                    } else {
                        Vec::new()
                    },
                    offset: 0,
                    length: 0,
                }],
                idat: None,
                iref_children: Vec::new(),
            };
            let mut input = build_heic(&spec);
            let (_, _, iloc, _, _, _) = find_meta_layout(&input).expect("meta layout");
            let data_reference_pos = iloc.body_start() + 12;
            input[data_reference_pos..data_reference_pos + 2].copy_from_slice(&1u16.to_be_bytes());
            assert!(
                parse_iloc(&input, iloc).is_err(),
                "construction method {construction_method} must reject an external data reference"
            );

            let mut output = Vec::new();
            let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
            assert!(
                result.is_err(),
                "the writer cannot resolve or preserve externally referenced item bytes"
            );
            assert!(output.is_empty(), "a refused rewrite must emit no bytes");
        }
    }

    #[test]
    fn rewrite_xmp_refuses_undecidable_xmp_item_maps() {
        // Two packets naming the primary, and two naming nothing at all. Both
        // shapes leave no evidence for choosing, so neither may be overwritten.
        let ambiguous = [
            (
                vec![xmp_item(5, AUX_XMP), xmp_item(6, PRIMARY_XMP)],
                vec![ref_box(b"cdsc", 5, &[1]), ref_box(b"cdsc", 6, &[1])],
            ),
            (vec![xmp_item(5, AUX_XMP), xmp_item(6, PRIMARY_XMP)], vec![]),
        ];
        for (xmp_items, cdsc) in ambiguous {
            let spec = apple_multi_xmp_spec(xmp_items, cdsc);
            let input = build_heic(&spec);

            let mut output = Vec::new();
            let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
            assert!(
                result.is_err(),
                "an undecidable item map must not be written"
            );
            assert!(output.is_empty(), "a refused rewrite must emit nothing");
            assert!(
                extract_xmp_strict(&input).is_err(),
                "the reader must refuse whatever the writer refuses"
            );
        }
    }

    #[test]
    fn rewrite_xmp_preserves_construction_method_two_item() {
        let spec = HeicSpec {
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
                    data: (0u8..48).collect(),
                    offset: 0,
                    length: 0,
                },
                ItemSpec {
                    item_id: 2,
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 2,
                    data: Vec::new(),
                    offset: 0,
                    length: 16,
                },
            ],
            idat: None,
            iref_children: vec![ref_box(b"dimg", 1, &[2])],
        };
        let input = build_heic(&spec);

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("construction-method-2 insertion");

        assert_eq!(resolve_item_data(&input, 1), resolve_item_data(&output, 1));
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));

        let before = parse_iloc(&input, find_meta_layout(&input).unwrap().2).unwrap();
        let after = parse_iloc(&output, find_meta_layout(&output).unwrap().2).unwrap();
        let item_before = before.items.iter().find(|item| item.item_id == 2).unwrap();
        let item_after = after.items.iter().find(|item| item.item_id == 2).unwrap();
        assert_eq!(
            (
                item_before.construction_method,
                item_before.base_offset,
                item_before.extents[0].offset,
                item_before.extents[0].length,
            ),
            (
                item_after.construction_method,
                item_after.base_offset,
                item_after.extents[0].offset,
                item_after.extents[0].length,
            ),
            "construction-method-2 item must be copied without shifting"
        );
    }

    #[test]
    fn rewrite_xmp_synthesises_iref_when_absent() {
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
                    item_type: *b"hvc1",
                    infe_version: 2,
                    construction_method: 0,
                    data: (0u8..16).collect(),
                    offset: 0,
                    length: 0,
                },
            ],
            idat: None,
            iref_children: Vec::new(),
        };
        let input = build_heic(&spec);
        assert!(
            find_meta_layout(&input).unwrap().3.is_none(),
            "fixture must have no iref"
        );

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("insertion synthesises an iref");

        assert_eq!(resolve_item_data(&input, 1), resolve_item_data(&output, 1));
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        assert!(
            find_meta_layout(&output).unwrap().3.is_some(),
            "insertion must synthesise an iref"
        );
        let (xmp_id, primary) = xmp_and_primary_item_ids(&output);
        assert_eq!(primary, 1);
        let references = cdsc_references(&output);
        assert!(
            references.contains(&(xmp_id, 1)),
            "synthesised iref must carry a cdsc from the XMP item to the primary"
        );
        assert_eq!(
            references,
            vec![(xmp_id, 1)],
            "a synthesised cdsc must describe only the proven primary image"
        );

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("idempotent synthesised iref");
        assert_eq!(again, output);
    }

    #[test]
    fn rewrite_xmp_refuses_item_data_overlapping_meta() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![ItemSpec {
                item_id: 1,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..32).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let mut input = build_heic(&spec);
        let (meta, _, iloc, _, _, _) = find_meta_layout(&input).unwrap();
        let layout = parse_iloc(&input, iloc).unwrap();
        let base_pos = iloc.body_start() + layout.items[0].base_offset_pos.unwrap();
        let inside_meta = u32::try_from(meta.start + 40).unwrap();
        input[base_pos..base_pos + 4].copy_from_slice(&inside_meta.to_be_bytes());

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
        assert!(
            result.is_err(),
            "an item whose data overlaps the meta box must be refused"
        );
        assert!(output.is_empty(), "a refused rewrite must not emit bytes");
    }

    #[test]
    fn rewrite_xmp_refuses_existing_xmp_data_overlapping_meta() {
        let mut input = Vec::new();
        rewrite_xmp(
            include_bytes!("../../../tests/data/sample.heic"),
            b"<x:xmpmeta>existing packet padding</x:xmpmeta>",
            &mut input,
        )
        .expect("seed existing XMP");
        let (meta, iinf, iloc, iref, primary_item_id, prefix_size) =
            find_meta_layout(&input).unwrap();
        let children = scan_raw_boxes(&input, meta.body_start() + prefix_size, meta.end()).unwrap();
        let iprp = children
            .iter()
            .find(|child| child.kind == *b"iprp")
            .expect("sample HEIC iprp");
        let (_, xmp_item_id, _, _, _, _, _) =
            locate_xmp(&input, iinf, iloc, iref, primary_item_id).unwrap();
        let layout = parse_iloc(&input, iloc).unwrap();
        let xmp_item = layout
            .items
            .iter()
            .find(|item| Some(item.item_id) == xmp_item_id)
            .expect("seeded XMP iloc item");
        let inside_iprp = u64::try_from(iprp.body_start() + 16).unwrap();
        let iloc_body = &mut input[iloc.body_start()..iloc.end()];
        if let Some(base_offset_pos) = xmp_item.base_offset_pos {
            write_uint(iloc_body, base_offset_pos, layout.base_offset_size, 0).unwrap();
        }
        write_uint(
            iloc_body,
            xmp_item.extents[0]
                .offset_pos
                .expect("XMP extent offset field"),
            layout.offset_size,
            inside_iprp,
        )
        .unwrap();

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, b"<x:xmpmeta>short</x:xmpmeta>", &mut output);

        assert!(
            matches!(result, Err(HeifError::InvalidLayout { reason }) if reason.contains("overlaps the meta box")),
            "an existing XMP extent inside meta must be refused, got {result:?}"
        );
        assert!(output.is_empty(), "a refused rewrite must not emit bytes");
    }

    #[test]
    fn multiple_exif_items_do_not_block_xmp_rewrite() {
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
            iref_children: vec![ref_box(b"cdsc", 2, &[1]), ref_box(b"cdsc", 3, &[1])],
        };
        let input = build_heic(&spec);
        let mut output = Vec::new();

        assert!(
            extract_exif_tiff_bytes(&input).is_err(),
            "several Exif items naming the primary image are ambiguous"
        );
        rewrite_xmp(&input, MATRIX_XMP, &mut output)
            .expect("multiple Exif items must not block an unrelated XMP rewrite");
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
    }

    #[test]
    fn rewrite_xmp_refuses_zero_sized_child_box() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![ItemSpec {
                item_id: 1,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..32).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let mut input = build_heic(&spec);
        let (_, iinf, _, _, _, _) = find_meta_layout(&input).unwrap();
        let entries = scan_raw_boxes(&input, iinf.body_start() + 6, iinf.end()).unwrap();
        let infe_start = entries[0].start;
        input[infe_start..infe_start + 4].copy_from_slice(&0u32.to_be_bytes());

        let mut output = Vec::new();
        let result = rewrite_xmp(&input, MATRIX_XMP, &mut output);
        assert!(
            result.is_err(),
            "a box with no explicit size cannot be appended after and must be refused"
        );
        assert!(output.is_empty(), "a refused rewrite must not emit bytes");
    }

    #[test]
    fn rewrite_xmp_inserts_with_item_ids_above_u16() {
        let big_id = 70_000u32;
        let spec = HeicSpec {
            iloc_version: 2,
            offset_size: 4,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: big_id,
            items: vec![ItemSpec {
                item_id: big_id,
                item_type: *b"hvc1",
                infe_version: 3,
                construction_method: 0,
                data: (0u8..64).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let input = build_heic(&spec);
        assert!(is_heif_content(&input));

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("item ids above u16 insertion");

        assert_eq!(
            resolve_item_data(&input, big_id),
            resolve_item_data(&output, big_id)
        );
        assert_eq!(extract_xmp_raw(&output).as_deref(), Some(MATRIX_XMP));
        let (xmp_id, primary) = xmp_and_primary_item_ids(&output);
        assert!(
            xmp_id > u32::from(u16::MAX),
            "new item id must exceed u16 to exercise infe v3 and iloc v2"
        );
        assert_eq!(primary, big_id);
        assert!(cdsc_references(&output).contains(&(xmp_id, big_id)));
    }

    #[test]
    fn rewrite_xmp_inserts_with_base_offset_only_iloc() {
        let spec = HeicSpec {
            iloc_version: 0,
            offset_size: 0,
            length_size: 4,
            base_offset_size: 4,
            index_size: 0,
            primary_id: 1,
            items: vec![ItemSpec {
                item_id: 1,
                item_type: *b"hvc1",
                infe_version: 2,
                construction_method: 0,
                data: (0u8..48).collect(),
                offset: 0,
                length: 0,
            }],
            idat: None,
            iref_children: Vec::new(),
        };
        let input = build_heic(&spec);

        let mut output = Vec::new();
        rewrite_xmp(&input, MATRIX_XMP, &mut output).expect("base-offset-only insertion");

        assert_eq!(resolve_item_data(&input, 1), resolve_item_data(&output, 1));
        assert_eq!(
            extract_xmp_raw(&output).as_deref(),
            Some(MATRIX_XMP),
            "the new XMP item must resolve through the base offset, not offset 0"
        );

        let mut again = Vec::new();
        rewrite_xmp(&output, MATRIX_XMP, &mut again).expect("base-offset-only idempotent");
        assert_eq!(again, output);
    }
}
