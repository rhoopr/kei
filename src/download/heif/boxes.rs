//! HEIF detection, raw box boundaries, and top-level metadata layout.

use super::{HeifError, invalid_layout};
use std::path::Path;

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

#[derive(Debug, Clone, Copy)]
pub(super) struct RawBox {
    pub(super) start: usize,
    pub(super) size: usize,
    pub(super) header_size: usize,
    pub(super) kind: [u8; 4],
}

impl RawBox {
    pub(super) fn body_start(self) -> usize {
        self.start + self.header_size
    }

    pub(super) fn end(self) -> usize {
        self.start + self.size
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "The initial get proves the fixed eight-byte header before its size and type fields are sliced."
)]
pub(super) fn parse_raw_box(bytes: &[u8], start: usize) -> Result<RawBox, HeifError> {
    let header = bytes
        .get(start..start.saturating_add(8))
        .ok_or_else(|| invalid_layout("truncated ISO-BMFF box header"))?;
    let size32 = u32::from_be_bytes(
        header[0..4]
            .try_into()
            .map_err(|_| invalid_layout("invalid ISO-BMFF box size"))?,
    );
    let kind = header[4..8]
        .try_into()
        .map_err(|_| invalid_layout("invalid ISO-BMFF box type"))?;
    let (header_size, size) = match size32 {
        0 => {
            return Err(invalid_layout(
                "ISO-BMFF box has no explicit size and cannot be rewritten",
            ));
        }
        1 => {
            let large = bytes
                .get(start + 8..start + 16)
                .ok_or_else(|| invalid_layout("truncated large ISO-BMFF box header"))?;
            let size = u64::from_be_bytes(
                large
                    .try_into()
                    .map_err(|_| invalid_layout("invalid large ISO-BMFF box size"))?,
            );
            let size =
                usize::try_from(size).map_err(|_| invalid_layout("ISO-BMFF box is too large"))?;
            (16, size)
        }
        size => (8, size as usize),
    };
    if size < header_size {
        return Err(invalid_layout("ISO-BMFF box is smaller than its header"));
    }
    let end = start
        .checked_add(size)
        .ok_or_else(|| invalid_layout("ISO-BMFF box end overflows"))?;
    if end > bytes.len() {
        return Err(invalid_layout("ISO-BMFF box extends past the file"));
    }
    Ok(RawBox {
        start,
        size,
        header_size,
        kind,
    })
}

pub(super) fn scan_raw_boxes(
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<Vec<RawBox>, HeifError> {
    if start > end || end > bytes.len() {
        return Err(invalid_layout("invalid ISO-BMFF box range"));
    }
    let mut boxes = Vec::new();
    let mut cursor = start;
    while cursor < end {
        let atom = parse_raw_box(bytes, cursor)?;
        if atom.end() > end {
            return Err(invalid_layout(
                "nested ISO-BMFF box extends past its parent",
            ));
        }
        boxes.push(atom);
        cursor = atom.end();
    }
    if cursor != end {
        return Err(invalid_layout(
            "nested ISO-BMFF boxes leave a trailing fragment",
        ));
    }
    Ok(boxes)
}

pub(super) fn box_with_body(kind: [u8; 4], body: &[u8]) -> Result<Vec<u8>, HeifError> {
    let size = body
        .len()
        .checked_add(8)
        .ok_or_else(|| invalid_layout("rewritten ISO-BMFF box size overflows"))?;
    let size =
        u32::try_from(size).map_err(|_| invalid_layout("rewritten ISO-BMFF box is too large"))?;
    let mut out = Vec::with_capacity(size as usize);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&kind);
    out.extend_from_slice(body);
    Ok(out)
}

pub(super) fn patch_box_size(box_bytes: &mut [u8]) -> Result<(), HeifError> {
    let size = u32::try_from(box_bytes.len())
        .map_err(|_| invalid_layout("rewritten ISO-BMFF box is too large"))?;
    let size_bytes = box_bytes
        .get_mut(..4)
        .ok_or_else(|| invalid_layout("rewritten ISO-BMFF box has no size field"))?;
    size_bytes.copy_from_slice(&size.to_be_bytes());
    Ok(())
}

fn unique_required_box(
    boxes: &[RawBox],
    kind: [u8; 4],
    missing: &'static str,
    duplicate: &'static str,
) -> Result<RawBox, HeifError> {
    let mut matches = boxes.iter().copied().filter(|atom| atom.kind == kind);
    let first = matches.next().ok_or_else(|| invalid_layout(missing))?;
    if matches.next().is_some() {
        return Err(invalid_layout(duplicate));
    }
    Ok(first)
}

fn unique_optional_box(
    boxes: &[RawBox],
    kind: [u8; 4],
    duplicate: &'static str,
) -> Result<Option<RawBox>, HeifError> {
    let mut matches = boxes.iter().copied().filter(|atom| atom.kind == kind);
    let first = matches.next();
    if matches.next().is_some() {
        return Err(invalid_layout(duplicate));
    }
    Ok(first)
}

pub(super) fn find_meta_layout(
    bytes: &[u8],
) -> Result<(RawBox, RawBox, RawBox, Option<RawBox>, u32, usize), HeifError> {
    let top = scan_raw_boxes(bytes, 0, bytes.len())?;
    let meta = unique_required_box(
        &top,
        *b"meta",
        "HEIC has no top-level meta box",
        "HEIC has multiple top-level meta boxes",
    )?;
    const MAX_REWRITE_META_BYTES: usize = 8 * 1024 * 1024;
    if meta.size > MAX_REWRITE_META_BYTES {
        return Err(invalid_layout(
            "HEIC meta box is too large to rewrite safely",
        ));
    }
    if meta.header_size != 8 {
        return Err(invalid_layout("large-size meta boxes are unsupported"));
    }
    let body_start = meta.body_start();
    let body_end = meta.end();
    let prefix_size = if bytes
        .get(body_start + 8..body_start + 12)
        .is_some_and(|kind| kind == b"hdlr")
    {
        4
    } else if bytes
        .get(body_start + 4..body_start + 8)
        .is_some_and(|kind| kind == b"hdlr")
    {
        0
    } else {
        return Err(invalid_layout("HEIC meta box has no handler box"));
    };
    let children = scan_raw_boxes(bytes, body_start + prefix_size, body_end)?;
    let iinf = unique_required_box(
        &children,
        *b"iinf",
        "HEIC meta box has no iinf box",
        "HEIC meta box has multiple iinf boxes",
    )?;
    let iloc = unique_required_box(
        &children,
        *b"iloc",
        "HEIC meta box has no iloc box",
        "HEIC meta box has multiple iloc boxes",
    )?;
    let iref = unique_optional_box(&children, *b"iref", "HEIC meta box has multiple iref boxes")?;
    let pitm = unique_required_box(
        &children,
        *b"pitm",
        "HEIC meta box has no primary item",
        "HEIC meta box has multiple primary-item boxes",
    )?;
    let primary_item_id = parse_primary_item_id(bytes, pitm)?;
    if iinf.header_size != 8 || iloc.header_size != 8 {
        return Err(invalid_layout(
            "large-size iinf or iloc boxes are unsupported",
        ));
    }
    if let Some(iref) = iref
        && iref.header_size != 8
    {
        return Err(invalid_layout("large-size iref boxes are unsupported"));
    }
    Ok((meta, iinf, iloc, iref, primary_item_id, prefix_size))
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser proves the body range, and the six-byte minimum proves the version-0 pitm fields."
)]
fn parse_primary_item_id(bytes: &[u8], pitm: RawBox) -> Result<u32, HeifError> {
    let body = &bytes[pitm.body_start()..pitm.end()];
    if body.len() < 6 {
        return Err(invalid_layout("pitm box is truncated"));
    }
    match body[0] {
        0 => Ok(u32::from(u16::from_be_bytes([body[4], body[5]]))),
        1 => {
            let item_id = body
                .get(4..8)
                .ok_or_else(|| invalid_layout("pitm version 1 box is truncated"))?;
            Ok(u32::from_be_bytes(
                item_id
                    .try_into()
                    .map_err(|_| invalid_layout("invalid pitm item id"))?,
            ))
        }
        _ => Err(invalid_layout("unsupported pitm version")),
    }
}

pub(super) fn extent_is_within_mdat_payload(
    bytes: &[u8],
    extent_start: usize,
    extent_length: usize,
) -> Result<bool, HeifError> {
    let extent_end = extent_start
        .checked_add(extent_length)
        .ok_or_else(|| invalid_layout("existing XMP extent overflows"))?;
    Ok(scan_raw_boxes(bytes, 0, bytes.len())?
        .into_iter()
        .filter(|atom| atom.kind == *b"mdat")
        .any(|atom| atom.body_start() <= extent_start && extent_end <= atom.end()))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::ftyp_prefix;
    use super::{is_heif_content, is_heif_path};
    use std::path::Path;

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
}
