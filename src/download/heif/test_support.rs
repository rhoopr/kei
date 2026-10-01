//! Shared synthetic HEIF fixtures and test-only item-map inspection.

use super::boxes::{find_meta_layout, scan_raw_boxes};
use super::items::{parse_iinf, parse_iloc};
use super::relationships::select_xmp_item_id;
use super::xmp_write::locate_xmp;
use mp4_atom::{Any, Encode, FourCC, Iloc};

pub(super) fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let size = u32::try_from(body.len() + 8).expect("test atom size");
    let mut bytes = Vec::with_capacity(size as usize);
    bytes.extend_from_slice(&size.to_be_bytes());
    bytes.extend_from_slice(kind);
    bytes.extend_from_slice(body);
    bytes
}

pub(super) fn exif_infe(item_id: u32, version: u8) -> Vec<u8> {
    let mut infe_body = vec![version, 0, 0, 0];
    if version == 2 {
        infe_body.extend_from_slice(
            &u16::try_from(item_id)
                .expect("version 2 item id")
                .to_be_bytes(),
        );
    } else {
        infe_body.extend_from_slice(&item_id.to_be_bytes());
    }
    infe_body.extend_from_slice(&0_u16.to_be_bytes());
    infe_body.extend_from_slice(b"Exif");
    infe_body.push(0);
    atom(b"infe", &infe_body)
}

fn exif_iinf() -> Vec<u8> {
    exif_iinf_entries(&[exif_infe(1, 2)])
}

pub(super) fn exif_iinf_entries(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut iinf_body = vec![0, 0, 0, 0];
    iinf_body.extend_from_slice(
        &u16::try_from(entries.len())
            .expect("iinf entry count")
            .to_be_bytes(),
    );
    for entry in entries {
        iinf_body.extend_from_slice(entry);
    }
    atom(b"iinf", &iinf_body)
}

pub(super) fn exif_iloc(extent_offset: u32, extent_length: u32, extent_count: u16) -> Vec<u8> {
    exif_iloc_options(extent_offset, extent_length, extent_count, 0, 0, 0)
}

pub(super) fn exif_iloc_options(
    extent_offset: u32,
    extent_length: u32,
    extent_count: u16,
    version: u8,
    construction_method: u16,
    data_reference_index: u16,
) -> Vec<u8> {
    let mut body = vec![version, 0, 0, 0, 0x44, 0];
    if version == 2 {
        body.extend_from_slice(&1_u32.to_be_bytes());
        body.extend_from_slice(&1_u32.to_be_bytes());
    } else {
        body.extend_from_slice(&1_u16.to_be_bytes());
        body.extend_from_slice(&1_u16.to_be_bytes());
    }
    if version > 0 {
        body.extend_from_slice(&construction_method.to_be_bytes());
    }
    body.extend_from_slice(&data_reference_index.to_be_bytes());
    body.extend_from_slice(&extent_count.to_be_bytes());
    for _ in 0..extent_count {
        body.extend_from_slice(&extent_offset.to_be_bytes());
        body.extend_from_slice(&extent_length.to_be_bytes());
    }
    atom(b"iloc", &body)
}

pub(super) fn heif_with_exif_options(
    tiff: &[u8],
    tiff_header_offset: u32,
    extent_count: u16,
    trailing_media: usize,
    include_primary_item: bool,
) -> Vec<u8> {
    let ftyp = ftyp_prefix(b"heic");
    let build_meta = |extent_offset| {
        let mut body = vec![0, 0, 0, 0];
        body.extend_from_slice(&atom(b"hdlr", &[]));
        if include_primary_item {
            let mut pitm = vec![0_u8; 4];
            pitm.extend_from_slice(&1_u16.to_be_bytes());
            body.extend_from_slice(&atom(b"pitm", &pitm));
        }
        body.extend_from_slice(&exif_iinf());
        body.extend_from_slice(&exif_iloc(
            extent_offset,
            u32::try_from(4 + tiff_header_offset as usize + tiff.len())
                .expect("EXIF extent length"),
            extent_count,
        ));
        atom(b"meta", &body)
    };
    let placeholder_meta = build_meta(0);
    let extent_offset =
        u32::try_from(ftyp.len() + placeholder_meta.len() + 8).expect("EXIF file offset");
    let meta = build_meta(extent_offset);
    assert_eq!(meta.len(), placeholder_meta.len());

    let mut mdat_body = Vec::new();
    mdat_body.extend_from_slice(&tiff_header_offset.to_be_bytes());
    mdat_body.resize(4 + tiff_header_offset as usize, 0);
    mdat_body.extend_from_slice(tiff);
    mdat_body.resize(mdat_body.len() + trailing_media, 0);

    [ftyp, meta, atom(b"mdat", &mdat_body)].concat()
}

pub(super) fn heif_with_exif(
    tiff: &[u8],
    tiff_header_offset: u32,
    extent_count: u16,
    trailing_media: usize,
) -> Vec<u8> {
    heif_with_exif_options(tiff, tiff_header_offset, extent_count, trailing_media, true)
}

/// Build a minimal ftyp prefix with the given major brand for tests.
pub(super) fn ftyp_prefix(brand: &[u8; 4]) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    bytes.extend_from_slice(&0x18_u32.to_be_bytes());
    bytes.extend_from_slice(b"ftyp");
    bytes.extend_from_slice(brand);
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(b"mif1");
    bytes.extend_from_slice(b"heic");
    bytes
}

pub(super) fn malformed_iinf_meta_box() -> Vec<u8> {
    let mut hdlr: Vec<u8> = Vec::new();
    hdlr.extend_from_slice(&0x21_u32.to_be_bytes());
    hdlr.extend_from_slice(b"hdlr");
    hdlr.extend_from_slice(&[0; 4]);
    hdlr.extend_from_slice(&[0; 4]);
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0; 12]);
    hdlr.push(0);

    let mut iinf: Vec<u8> = Vec::new();
    // size = 9 (header 8 + body 1); body is 1 byte but Iinf::decode_body
    // requires at least version+flags+entry_count (6 bytes).
    iinf.extend_from_slice(&0x09_u32.to_be_bytes());
    iinf.extend_from_slice(b"iinf");
    iinf.push(0);

    let mut meta_body: Vec<u8> = Vec::new();
    meta_body.extend_from_slice(&[0; 4]);
    meta_body.extend_from_slice(&hdlr);
    meta_body.extend_from_slice(&iinf);

    let mut meta_box: Vec<u8> = Vec::new();
    let total = (8 + meta_body.len()) as u32;
    meta_box.extend_from_slice(&total.to_be_bytes());
    meta_box.extend_from_slice(b"meta");
    meta_box.extend_from_slice(&meta_body);
    meta_box
}

pub(super) fn xmp_and_primary_item_ids(bytes: &[u8]) -> (u32, u32) {
    let (_, iinf, _, iref, primary_item_id, _) = find_meta_layout(bytes).unwrap();
    let iinf_layout = parse_iinf(bytes, iinf).unwrap();
    (
        select_xmp_item_id(bytes, iref, primary_item_id, &iinf_layout.xmp_item_ids)
            .unwrap()
            .unwrap(),
        primary_item_id,
    )
}

/// Every `cdsc` edge as a `(descriptive item, described image)` pair. A
/// reference may name several images, and each one becomes its own pair.
pub(super) fn cdsc_references(bytes: &[u8]) -> Vec<(u32, u32)> {
    let (_, _, _, Some(iref), _, _) = find_meta_layout(bytes).unwrap() else {
        return Vec::new();
    };
    let body = &bytes[iref.body_start()..iref.end()];
    let version = body[0];
    scan_raw_boxes(body, 4, body.len())
        .unwrap()
        .into_iter()
        .filter(|child| child.kind == *b"cdsc")
        .flat_map(|child| {
            let child_body = &body[child.body_start()..child.end()];
            let width = if version == 0 { 2 } else { 4 };
            let from = if version == 0 {
                u32::from(u16::from_be_bytes([child_body[0], child_body[1]]))
            } else {
                u32::from_be_bytes(child_body[0..4].try_into().unwrap())
            };
            let count = u16::from_be_bytes([child_body[width], child_body[width + 1]]) as usize;
            (0..count)
                .map(|index| {
                    let start = width + 2 + index * width;
                    let to = if version == 0 {
                        u32::from(u16::from_be_bytes([
                            child_body[start],
                            child_body[start + 1],
                        ]))
                    } else {
                        u32::from_be_bytes(child_body[start..start + 4].try_into().unwrap())
                    };
                    (from, to)
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Build a minimal HEIC with Apple QuickTime Meta (no version+flags)
/// and meta-before-mdat layout. Contains one hvc1 image item with
/// a 16-byte payload.
pub(super) fn build_apple_qt_heic_fixture() -> Vec<u8> {
    use mp4_atom::{
        Hdlr, Hvcc, Iinf, Iloc, Ipco, Ipma, Iprp, ItemInfoEntry, ItemLocation, ItemLocationExtent,
        Pitm, PropertyAssociation, PropertyAssociations,
    };

    let payload: Vec<u8> = (0..16).collect();
    let mut tmp: Vec<u8> = Vec::new();

    let hdlr = Hdlr {
        handler: FourCC::new(b"pict"),
        name: String::new(),
    };
    hdlr.encode(&mut tmp).unwrap();
    let hdlr_enc: Vec<u8> = std::mem::take(&mut tmp);

    Pitm { item_id: 1 }.encode(&mut tmp).unwrap();
    let pitm_enc: Vec<u8> = std::mem::take(&mut tmp);

    Iinf {
        item_infos: vec![ItemInfoEntry {
            item_id: 1,
            item_protection_index: 0,
            item_type: Some(FourCC::new(b"hvc1")),
            item_name: String::new(),
            content_type: None,
            content_encoding: None,
            item_uri_type: None,
            item_not_in_presentation: false,
        }],
    }
    .encode(&mut tmp)
    .unwrap();
    let iinf_enc: Vec<u8> = std::mem::take(&mut tmp);

    Iprp {
        ipco: Ipco {
            properties: vec![Any::Hvcc(Hvcc::new())],
        },
        ipma: vec![Ipma {
            item_properties: vec![PropertyAssociations {
                item_id: 1,
                associations: vec![PropertyAssociation {
                    essential: true,
                    property_index: 1,
                }],
            }],
        }],
    }
    .encode(&mut tmp)
    .unwrap();
    let iprp_enc: Vec<u8> = std::mem::take(&mut tmp);

    // Iterate to find iloc size / mdat offset fixed point.
    let non_iloc = hdlr_enc.len() + pitm_enc.len() + iinf_enc.len() + iprp_enc.len();
    let ftyp = 24u64;
    let meta_hdr = 8u64; // no version+flags for Apple QT
    let mut base: u64 = 0;
    let iloc_enc: Vec<u8> = loop {
        let iloc = Iloc {
            item_locations: vec![ItemLocation {
                item_id: 1,
                construction_method: 0,
                data_reference_index: 0,
                base_offset: base,
                extents: vec![ItemLocationExtent {
                    item_reference_index: 0,
                    offset: 0,
                    length: payload.len() as u64,
                }],
            }],
        };
        iloc.encode(&mut tmp).unwrap();
        let ilc = std::mem::take(&mut tmp);
        let correct = ftyp + meta_hdr + non_iloc as u64 + ilc.len() as u64 + 8;
        if correct == base {
            break ilc;
        }
        base = correct;
    };

    let meta_box = 8 + non_iloc + iloc_enc.len();
    let mdat_box = 8 + payload.len();
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(&0x18_u32.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(b"heic");
    buf.extend_from_slice(&0_u32.to_be_bytes());
    buf.extend_from_slice(b"mif1");
    buf.extend_from_slice(b"heic");
    buf.extend_from_slice(&(meta_box as u32).to_be_bytes());
    buf.extend_from_slice(b"meta");
    buf.extend_from_slice(&hdlr_enc);
    buf.extend_from_slice(&pitm_enc);
    buf.extend_from_slice(&iloc_enc);
    buf.extend_from_slice(&iinf_enc);
    buf.extend_from_slice(&iprp_enc);
    buf.extend_from_slice(&(mdat_box as u32).to_be_bytes());
    buf.extend_from_slice(b"mdat");
    buf.extend_from_slice(&payload);
    buf
}

/// Find the first mdat atom: return (file_offset_of_atom, data_bytes).
pub(super) fn find_mdat(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut pos = 0;
    while pos + 8 <= bytes.len() {
        let sz = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        if &bytes[pos + 4..pos + 8] == b"mdat" {
            let end = (pos + sz).min(bytes.len());
            if end > pos + 8 {
                return Some((pos as u64, &bytes[pos + 8..end]));
            }
        }
        if sz == 0 || pos + sz > bytes.len() {
            break;
        }
        pos += sz;
    }
    None
}

/// Decode the Iloc from the first Meta box in an ISO-BMFF file.
pub(super) fn decode_iloc_from_heic(bytes: &[u8]) -> Option<Iloc> {
    use mp4_atom::{Atom, Iloc};
    let mut pos = 0;
    while pos + 8 <= bytes.len() {
        let sz = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        if &bytes[pos + 4..pos + 8] == b"meta" && pos + sz <= bytes.len() {
            let body = &bytes[pos + 8..pos + sz];
            let mut cur: &[u8] = body;
            // Skip version+flags if present (ISO format).
            if cur.len() >= 8 && cur.get(4..8) != Some(b"hdlr".as_slice()) {
                cur = cur.get(4..)?;
            }
            while cur.len() >= 8 {
                let ss = u32::from_be_bytes([cur[0], cur[1], cur[2], cur[3]]) as usize;
                if &cur[4..8] == b"iloc" && cur.len() >= ss {
                    return Iloc::decode_body(&mut &cur[8..ss]).ok();
                }
                if ss == 0 || ss > cur.len() {
                    break;
                }
                cur = &cur[ss..];
            }
            return None;
        }
        if sz == 0 || pos + sz > bytes.len() {
            break;
        }
        pos += sz;
    }
    None
}

/// Return the byte offset of the end (start + size) of the first
/// top-level atom with the given FourCC.
pub(super) fn find_atom_end(bytes: &[u8], tag: &str) -> Option<u64> {
    let tag = tag.as_bytes();
    let mut pos = 0;
    while pos + 8 <= bytes.len() {
        let sz = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        if &bytes[pos + 4..pos + 8] == tag {
            return Some((pos + sz) as u64);
        }
        if sz == 0 || pos + sz > bytes.len() {
            break;
        }
        pos += sz;
    }
    None
}

pub(super) const MATRIX_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>4</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";

/// One HEIF item for [`build_heic`]. `data` is placed in the top-level
/// `mdat` for construction method 0; construction methods 1 and 2 leave it
/// empty and use `offset`/`length` verbatim.
pub(super) struct ItemSpec {
    pub(super) item_id: u32,
    pub(super) item_type: [u8; 4],
    pub(super) infe_version: u8,
    pub(super) construction_method: u8,
    pub(super) data: Vec<u8>,
    pub(super) offset: u64,
    pub(super) length: u64,
}

/// A hand-built HEIF file covering layout shapes the real `sample.heic`
/// fixture cannot express: grid/dimg derivation, construction methods 1
/// and 2, item ids above `u16`, and files with no `iref`.
pub(super) struct HeicSpec {
    pub(super) iloc_version: u8,
    pub(super) offset_size: u8,
    pub(super) length_size: u8,
    pub(super) base_offset_size: u8,
    pub(super) index_size: u8,
    pub(super) primary_id: u32,
    pub(super) items: Vec<ItemSpec>,
    pub(super) idat: Option<Vec<u8>>,
    pub(super) iref_children: Vec<Vec<u8>>,
}

pub(super) fn sbox(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&u32::try_from(body.len() + 8).unwrap().to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn write_width(buf: &mut Vec<u8>, width: u8, value: u64) {
    match width {
        0 => {}
        2 => buf.extend_from_slice(&u16::try_from(value).unwrap().to_be_bytes()),
        4 => buf.extend_from_slice(&u32::try_from(value).unwrap().to_be_bytes()),
        8 => buf.extend_from_slice(&value.to_be_bytes()),
        _ => panic!("unsupported field width"),
    }
}

/// A version-0 single-item-type reference box (`dimg`, `thmb`, ...) with
/// 16-bit ids, matching the shapes `append_cdsc_reference` validates.
pub(super) fn ref_box(kind: &[u8; 4], from: u16, to: &[u16]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&from.to_be_bytes());
    body.extend_from_slice(&u16::try_from(to.len()).unwrap().to_be_bytes());
    for target in to {
        body.extend_from_slice(&target.to_be_bytes());
    }
    sbox(kind, &body)
}

pub(super) fn build_heic(spec: &HeicSpec) -> Vec<u8> {
    let ftyp = {
        let mut body = Vec::new();
        body.extend_from_slice(b"heic");
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(b"mif1");
        sbox(b"ftyp", &body)
    };

    let build_meta = |mdat_data_start: u64| -> Vec<u8> {
        let hdlr = {
            let mut body = vec![0u8, 0, 0, 0, 0, 0, 0, 0];
            body.extend_from_slice(b"pict");
            body.extend_from_slice(&[0u8; 12]);
            body.push(0);
            sbox(b"hdlr", &body)
        };
        let pitm = if spec.primary_id <= u32::from(u16::MAX) {
            let mut body = vec![0u8, 0, 0, 0];
            body.extend_from_slice(&(spec.primary_id as u16).to_be_bytes());
            sbox(b"pitm", &body)
        } else {
            let mut body = vec![1u8, 0, 0, 0];
            body.extend_from_slice(&spec.primary_id.to_be_bytes());
            sbox(b"pitm", &body)
        };
        let iinf = {
            let mut body = vec![0u8, 0, 0, 0];
            body.extend_from_slice(&u16::try_from(spec.items.len()).unwrap().to_be_bytes());
            for item in &spec.items {
                let mut entry = vec![item.infe_version, 0, 0, 0];
                if item.infe_version == 3 {
                    entry.extend_from_slice(&item.item_id.to_be_bytes());
                } else {
                    entry.extend_from_slice(&(item.item_id as u16).to_be_bytes());
                }
                entry.extend_from_slice(&0u16.to_be_bytes());
                entry.extend_from_slice(&item.item_type);
                entry.push(0);
                if item.item_type == *b"mime" {
                    entry.extend_from_slice(b"application/rdf+xml\0");
                    entry.push(0);
                }
                body.extend_from_slice(&sbox(b"infe", &entry));
            }
            sbox(b"iinf", &body)
        };
        let iloc = {
            let mut body = vec![spec.iloc_version, 0, 0, 0];
            body.push((spec.offset_size << 4) | spec.length_size);
            body.push((spec.base_offset_size << 4) | spec.index_size);
            if spec.iloc_version == 2 {
                body.extend_from_slice(&u32::try_from(spec.items.len()).unwrap().to_be_bytes());
            } else {
                body.extend_from_slice(&u16::try_from(spec.items.len()).unwrap().to_be_bytes());
            }
            let mut cm0_cursor = mdat_data_start;
            for item in &spec.items {
                if spec.iloc_version == 2 {
                    body.extend_from_slice(&item.item_id.to_be_bytes());
                } else {
                    body.extend_from_slice(&(item.item_id as u16).to_be_bytes());
                }
                if spec.iloc_version > 0 {
                    body.extend_from_slice(&u16::from(item.construction_method).to_be_bytes());
                }
                body.extend_from_slice(&0u16.to_be_bytes());
                let (base, offset, length) = if item.construction_method == 0 {
                    let base = cm0_cursor;
                    cm0_cursor += item.data.len() as u64;
                    (base, 0u64, item.data.len() as u64)
                } else {
                    (0u64, item.offset, item.length)
                };
                write_width(&mut body, spec.base_offset_size, base);
                body.extend_from_slice(&1u16.to_be_bytes());
                if spec.iloc_version > 0 {
                    write_width(&mut body, spec.index_size, 0);
                }
                write_width(&mut body, spec.offset_size, offset);
                write_width(&mut body, spec.length_size, length);
            }
            sbox(b"iloc", &body)
        };
        let iref = if spec.iref_children.is_empty() {
            None
        } else {
            let mut body = vec![0u8, 0, 0, 0];
            for child in &spec.iref_children {
                body.extend_from_slice(child);
            }
            Some(sbox(b"iref", &body))
        };
        let idat = spec.idat.as_ref().map(|data| sbox(b"idat", data));

        let mut meta_body = vec![0u8, 0, 0, 0];
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&pitm);
        meta_body.extend_from_slice(&iinf);
        meta_body.extend_from_slice(&iloc);
        if let Some(iref) = &iref {
            meta_body.extend_from_slice(iref);
        }
        if let Some(idat) = &idat {
            meta_body.extend_from_slice(idat);
        }
        sbox(b"meta", &meta_body)
    };

    let placeholder = build_meta(0);
    let mdat_data_start = ftyp.len() as u64 + placeholder.len() as u64 + 8;
    let meta = build_meta(mdat_data_start);
    assert_eq!(
        meta.len(),
        placeholder.len(),
        "meta size must not depend on offset values"
    );

    let mut mdat_data = Vec::new();
    for item in &spec.items {
        if item.construction_method == 0 {
            mdat_data.extend_from_slice(&item.data);
        }
    }

    let mut file = Vec::new();
    file.extend_from_slice(&ftyp);
    file.extend_from_slice(&meta);
    if !mdat_data.is_empty() {
        file.extend_from_slice(&sbox(b"mdat", &mdat_data));
    }
    file
}

/// Resolve a construction-method-0 item's payload bytes through kei's raw
/// iloc parser. Used to prove image bytes survive a rewrite and that
/// shifted offsets still point at the same data.
pub(super) fn resolve_item_data(bytes: &[u8], item_id: u32) -> Vec<u8> {
    let (_, _, iloc, _, _, _) = find_meta_layout(bytes).expect("meta layout");
    let layout = parse_iloc(bytes, iloc).expect("iloc");
    let item = layout
        .items
        .iter()
        .find(|item| item.item_id == item_id)
        .expect("item present");
    assert_eq!(
        item.construction_method, 0,
        "resolver handles construction method 0 only"
    );
    let mut out = Vec::new();
    for extent in &item.extents {
        let start = usize::try_from(item.base_offset + extent.offset).unwrap();
        let length = usize::try_from(extent.length).unwrap();
        out.extend_from_slice(&bytes[start..start + length]);
    }
    out
}

/// Extract the XMP packet through kei's own writer-side parser rather than
/// the mp4-atom read path, so item ids above `u16` and iloc version 2 are
/// covered too.
pub(super) fn extract_xmp_raw(bytes: &[u8]) -> Option<Vec<u8>> {
    let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(bytes).ok()?;
    let (location, _, _, _, _, _, _) = locate_xmp(bytes, iinf, iloc, iref, primary_item_id).ok()?;
    let location = location?;
    Some(bytes[location.extent_start..location.extent_start + location.extent_length].to_vec())
}

pub(super) fn meta_child_bytes(bytes: &[u8], kind: &[u8; 4]) -> Option<Vec<u8>> {
    let (meta, _, _, _, _, prefix) = find_meta_layout(bytes).ok()?;
    let children = scan_raw_boxes(bytes, meta.body_start() + prefix, meta.end()).ok()?;
    children
        .iter()
        .find(|child| child.kind == *kind)
        .map(|child| bytes[child.start..child.end()].to_vec())
}

pub(super) fn set_test_primary_item_id(bytes: &mut [u8], item_id: u16) {
    let (meta, _, _, _, _, prefix) = find_meta_layout(bytes).expect("meta layout");
    let pitm = scan_raw_boxes(bytes, meta.body_start() + prefix, meta.end())
        .expect("meta children")
        .into_iter()
        .find(|child| child.kind == *b"pitm")
        .expect("pitm");
    bytes[pitm.body_start() + 4..pitm.body_start() + 6].copy_from_slice(&item_id.to_be_bytes());
}

pub(super) fn duplicate_meta_child(bytes: &[u8], kind: [u8; 4]) -> Vec<u8> {
    let (meta, _, _, _, _, prefix) = find_meta_layout(bytes).expect("meta layout");
    let child = scan_raw_boxes(bytes, meta.body_start() + prefix, meta.end())
        .expect("meta children")
        .into_iter()
        .find(|child| child.kind == kind)
        .expect("meta child");
    let child_bytes = bytes[child.start..child.end()].to_vec();
    let mut output = bytes.to_vec();
    output.splice(meta.end()..meta.end(), child_bytes.iter().copied());
    let size = u32::try_from(meta.size + child_bytes.len()).expect("meta size");
    output[meta.start..meta.start + 4].copy_from_slice(&size.to_be_bytes());
    output
}

pub(super) fn duplicate_top_level_meta(bytes: &[u8]) -> Vec<u8> {
    let (meta, _, _, _, _, _) = find_meta_layout(bytes).expect("meta layout");
    let mut output = bytes.to_vec();
    output.extend_from_slice(&bytes[meta.start..meta.end()]);
    output
}

pub(super) const AUX_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>";

pub(super) const PRIMARY_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:xmp='http://ns.adobe.com/xap/1.0/' xmp:Rating='1'/></rdf:RDF></x:xmpmeta>";

/// The shape iOS writes for an HDR or portrait capture: a `grid` primary
/// over `hvc1` tiles, an auxiliary image, and one XMP item per image. Only
/// the packet whose `cdsc` names the primary is the photograph's.
pub(super) fn apple_multi_xmp_spec(xmp_items: Vec<ItemSpec>, cdsc: Vec<Vec<u8>>) -> HeicSpec {
    let grid_header = vec![0u8, 0, 1, 1, 0, 0x40, 0, 0x40];
    let mut items = vec![
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
        ItemSpec {
            item_id: 4,
            item_type: *b"hvc1",
            infe_version: 2,
            construction_method: 0,
            data: (96u8..128).collect(),
            offset: 0,
            length: 0,
        },
    ];
    items.extend(xmp_items);
    let mut iref_children = vec![ref_box(b"dimg", 1, &[2, 3])];
    iref_children.extend(cdsc);
    HeicSpec {
        iloc_version: 1,
        offset_size: 4,
        length_size: 4,
        base_offset_size: 4,
        index_size: 0,
        primary_id: 1,
        items,
        idat: Some(grid_header),
        iref_children,
    }
}

pub(super) fn xmp_item(item_id: u32, packet: &[u8]) -> ItemSpec {
    ItemSpec {
        item_id,
        item_type: *b"mime",
        infe_version: 2,
        construction_method: 0,
        data: packet.to_vec(),
        offset: 0,
        length: 0,
    }
}

pub(super) fn exif_item(item_id: u32, tiff: &[u8]) -> ItemSpec {
    let mut data = vec![0_u8; 4];
    data.extend_from_slice(tiff);
    ItemSpec {
        item_id,
        item_type: *b"Exif",
        infe_version: 2,
        construction_method: 0,
        data,
        offset: 0,
        length: 0,
    }
}

pub(super) fn tone_map_item(item_id: u32) -> ItemSpec {
    ItemSpec {
        item_id,
        item_type: *b"tmap",
        infe_version: 2,
        construction_method: 0,
        data: (0_u8..24).collect(),
        offset: 0,
        length: 0,
    }
}

pub(super) fn apple_tmap_insertion_spec(
    primary_tiff: &[u8],
    aux_xmp: &[u8],
    tone_map_inputs: &[u16],
    exif_targets: &[u16],
) -> HeicSpec {
    apple_tmap_insertion_spec_with_xmp_targets(
        primary_tiff,
        aux_xmp,
        &[4],
        tone_map_inputs,
        exif_targets,
    )
}

pub(super) fn apple_tmap_insertion_spec_with_xmp_targets(
    primary_tiff: &[u8],
    aux_xmp: &[u8],
    xmp_targets: &[u16],
    tone_map_inputs: &[u16],
    exif_targets: &[u16],
) -> HeicSpec {
    let mut spec = apple_multi_xmp_spec(
        vec![xmp_item(5, aux_xmp)],
        vec![ref_box(b"cdsc", 5, xmp_targets)],
    );
    spec.items.push(tone_map_item(7));
    spec.items.push(exif_item(8, primary_tiff));
    spec.iref_children
        .push(ref_box(b"dimg", 7, tone_map_inputs));
    spec.iref_children.push(ref_box(b"cdsc", 8, exif_targets));
    spec
}

pub(crate) fn apple_tmap_insertion_heic(primary_tiff: &[u8], aux_xmp: &[u8]) -> Vec<u8> {
    build_heic(&apple_tmap_insertion_spec(
        primary_tiff,
        aux_xmp,
        &[1, 4],
        &[1, 7],
    ))
}

pub(crate) fn apple_tmap_conflicting_xmp_heic(primary_tiff: &[u8], aux_xmp: &[u8]) -> Vec<u8> {
    build_heic(&apple_tmap_insertion_spec_with_xmp_targets(
        primary_tiff,
        aux_xmp,
        &[7],
        &[1, 4],
        &[1, 7],
    ))
}

pub(crate) fn apple_multi_exif_heic(primary_tiff: &[u8]) -> Vec<u8> {
    let mut spec = apple_multi_xmp_spec(
        Vec::new(),
        vec![
            ref_box(b"cdsc", 5, &[4]),
            ref_box(b"cdsc", 6, &[1]),
            ref_box(b"cdsc", 7, &[4]),
        ],
    );
    spec.items.extend([
        exif_item(5, b"MM\0*"),
        exif_item(6, primary_tiff),
        exif_item(7, b"II*\0"),
    ]);
    build_heic(&spec)
}

/// A HEIF file shaped like an iOS HDR capture, for tests outside this
/// module: a `grid` primary over `hvc1` tiles, an auxiliary image carrying
/// its own XMP packet, and optionally the photograph's packet bound to the
/// primary by `cdsc`.
pub(crate) fn apple_multi_xmp_heic(primary_xmp: Option<&[u8]>, aux_xmp: &[u8]) -> Vec<u8> {
    let mut xmp_items = vec![xmp_item(5, aux_xmp)];
    let mut cdsc = vec![ref_box(b"cdsc", 5, &[4])];
    if let Some(packet) = primary_xmp {
        xmp_items.push(xmp_item(6, packet));
        cdsc.push(ref_box(b"cdsc", 6, &[1, 3]));
    }
    build_heic(&apple_multi_xmp_spec(xmp_items, cdsc))
}

fn little_tiff_entry(tag: u16, field_type: u16, count: u32, value: u32) -> [u8; 12] {
    let mut entry = [0_u8; 12];
    entry[..2].copy_from_slice(&tag.to_le_bytes());
    entry[2..4].copy_from_slice(&field_type.to_le_bytes());
    entry[4..8].copy_from_slice(&count.to_le_bytes());
    entry[8..12].copy_from_slice(&value.to_le_bytes());
    entry
}

pub(super) fn capture_exif_payload(duplicate_datetime: bool) -> Vec<u8> {
    const IFD0_OFFSET: u32 = 8;
    const EXIF_IFD_OFFSET: u32 = 26;
    let exif_count = 2_u16 + u16::from(duplicate_datetime);
    let data_start = EXIF_IFD_OFFSET + 2 + u32::from(exif_count) * 12 + 4;
    let second_datetime_offset = data_start + 20;
    let offset_value = data_start + if duplicate_datetime { 40 } else { 20 };

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42_u16.to_le_bytes());
    tiff.extend_from_slice(&IFD0_OFFSET.to_le_bytes());
    tiff.extend_from_slice(&1_u16.to_le_bytes());
    tiff.extend_from_slice(&little_tiff_entry(0x8769, 4, 1, EXIF_IFD_OFFSET));
    tiff.extend_from_slice(&0_u32.to_le_bytes());
    tiff.extend_from_slice(&exif_count.to_le_bytes());
    tiff.extend_from_slice(&little_tiff_entry(0x9003, 2, 20, data_start));
    if duplicate_datetime {
        tiff.extend_from_slice(&little_tiff_entry(0x9003, 2, 20, second_datetime_offset));
    }
    tiff.extend_from_slice(&little_tiff_entry(0x9011, 2, 7, offset_value));
    tiff.extend_from_slice(&0_u32.to_le_bytes());
    tiff.extend_from_slice(b"2023:09:03 09:28:14\0");
    if duplicate_datetime {
        tiff.extend_from_slice(b"2023:09:03 09:28:14\0");
    }
    tiff.extend_from_slice(b"+03:00\0");

    let mut payload = vec![0_u8; 4];
    payload.extend_from_slice(&tiff);
    payload
}

pub(super) fn capture_exif_payload_with_thumbnail_alias() -> Vec<u8> {
    const IFD0_OFFSET: u32 = 8;
    const EXIF_IFD_OFFSET: u32 = 50;
    const DATETIME_OFFSET: u32 = 80;
    const OFFSET_TIME_OFFSET: u32 = 100;

    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"II");
    tiff.extend_from_slice(&42_u16.to_le_bytes());
    tiff.extend_from_slice(&IFD0_OFFSET.to_le_bytes());
    tiff.extend_from_slice(&3_u16.to_le_bytes());
    tiff.extend_from_slice(&little_tiff_entry(0x0201, 4, 1, DATETIME_OFFSET));
    tiff.extend_from_slice(&little_tiff_entry(0x0202, 4, 1, 20));
    tiff.extend_from_slice(&little_tiff_entry(0x8769, 4, 1, EXIF_IFD_OFFSET));
    tiff.extend_from_slice(&0_u32.to_le_bytes());
    tiff.extend_from_slice(&2_u16.to_le_bytes());
    tiff.extend_from_slice(&little_tiff_entry(0x9003, 2, 20, DATETIME_OFFSET));
    tiff.extend_from_slice(&little_tiff_entry(0x9011, 2, 7, OFFSET_TIME_OFFSET));
    tiff.extend_from_slice(&0_u32.to_le_bytes());
    tiff.extend_from_slice(b"2023:09:03 09:28:14\0");
    tiff.extend_from_slice(b"+03:00\0");

    let mut payload = vec![0_u8; 4];
    payload.extend_from_slice(&tiff);
    payload
}

pub(super) fn heic_with_exif_items(
    exif_items: &[(u32, &[u8])],
    iref_children: Vec<Vec<u8>>,
) -> Vec<u8> {
    let mut items = vec![
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
            item_id: 4,
            item_type: *b"hvc1",
            infe_version: 2,
            construction_method: 0,
            data: (32u8..48).collect(),
            offset: 0,
            length: 0,
        },
    ];
    items.extend(exif_items.iter().map(|(item_id, data)| ItemSpec {
        item_id: *item_id,
        item_type: *b"Exif",
        infe_version: 2,
        construction_method: 0,
        data: data.to_vec(),
        offset: 0,
        length: 0,
    }));
    build_heic(&HeicSpec {
        iloc_version: 0,
        offset_size: 4,
        length_size: 4,
        base_offset_size: 4,
        index_size: 0,
        primary_id: 1,
        items,
        idat: None,
        iref_children,
    })
}
