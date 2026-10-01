//! Post-rewrite checks for non-XMP item payloads and opaque metadata.

use super::boxes::{find_meta_layout, scan_raw_boxes};
use super::items::{parse_iinf, parse_iloc, resolve_item_extents};
use super::relationships::select_xmp_item_id;
use super::{HeifError, invalid_layout};

fn opaque_meta_sub_boxes(bytes: &[u8]) -> Result<Vec<([u8; 4], usize, usize)>, HeifError> {
    let (meta, _, _, _, _, prefix_size) = find_meta_layout(bytes)?;
    let children = scan_raw_boxes(bytes, meta.body_start() + prefix_size, meta.end())?;
    Ok(children
        .into_iter()
        .filter(|child| !matches!(&child.kind, b"iinf" | b"iloc" | b"iref"))
        .map(|child| (child.kind, child.start, child.end()))
        .collect())
}

/// Verify that a rewritten HEIF preserves every item payload except the
/// primary image's XMP packet, and every opaque `meta` sub-box, byte-for-byte.
///
/// The writer may update `iinf`, `iloc`, and `iref`, and may replace or append
/// the packet it owns. Any other difference is unsafe because it can redirect
/// or damage image data while leaving the XMP item readable. An auxiliary
/// image's XMP is that image's metadata, so it is preserved like any other
/// payload.
pub(crate) fn validate_rewrite_preserves_non_xmp_items(
    input: &[u8],
    rewritten: &[u8],
) -> Result<(), HeifError> {
    let (_, input_iinf, input_iloc, input_iref, input_primary, _) = find_meta_layout(input)?;
    let input_iinf_layout = parse_iinf(input, input_iinf)?;
    let input_iloc_layout = parse_iloc(input, input_iloc)?;
    let input_xmp_item_id = select_xmp_item_id(
        input,
        input_iref,
        input_primary,
        &input_iinf_layout.xmp_item_ids,
    )?;
    let (_, rewritten_iinf, rewritten_iloc, rewritten_iref, rewritten_primary, _) =
        find_meta_layout(rewritten)?;
    let rewritten_iinf_layout = parse_iinf(rewritten, rewritten_iinf)?;
    let rewritten_iloc_layout = parse_iloc(rewritten, rewritten_iloc)?;
    let rewritten_xmp_item_id = select_xmp_item_id(
        rewritten,
        rewritten_iref,
        rewritten_primary,
        &rewritten_iinf_layout.xmp_item_ids,
    )?;

    let input_items = input_iloc_layout
        .items
        .iter()
        .filter(|item| item.construction_method == 0 && Some(item.item_id) != input_xmp_item_id)
        .collect::<Vec<_>>();
    let rewritten_items = rewritten_iloc_layout
        .items
        .iter()
        .filter(|item| item.construction_method == 0 && Some(item.item_id) != rewritten_xmp_item_id)
        .collect::<Vec<_>>();
    if input_items.len() != rewritten_items.len() {
        return Err(invalid_layout(
            "HEIC rewrite changed the construction-method-0 item set",
        ));
    }
    for input_item in input_items {
        let rewritten_item = rewritten_items
            .iter()
            .find(|item| item.item_id == input_item.item_id)
            .ok_or_else(|| invalid_layout("HEIC rewrite removed a non-XMP item"))?;
        let input_extents = resolve_item_extents(input, input_item)?;
        let rewritten_extents = resolve_item_extents(rewritten, rewritten_item)?;
        if input_extents.len() != rewritten_extents.len()
            || input_extents
                .iter()
                .zip(&rewritten_extents)
                .any(|(before, after)| before != after)
        {
            return Err(invalid_layout(
                "HEIC rewrite changed a non-XMP item payload",
            ));
        }
    }

    let input_meta = opaque_meta_sub_boxes(input)?;
    let rewritten_meta = opaque_meta_sub_boxes(rewritten)?;
    if input_meta.len() != rewritten_meta.len() {
        return Err(invalid_layout(
            "HEIC rewrite changed the opaque meta sub-box set",
        ));
    }
    for ((input_kind, input_start, input_end), (rewritten_kind, rewritten_start, rewritten_end)) in
        input_meta.iter().zip(&rewritten_meta)
    {
        if input_kind != rewritten_kind
            || input.get(*input_start..*input_end)
                != rewritten.get(*rewritten_start..*rewritten_end)
        {
            return Err(invalid_layout(
                "HEIC rewrite changed an opaque meta sub-box",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::boxes::{find_meta_layout, scan_raw_boxes};
    use super::super::items::parse_iloc;
    use super::super::test_support::MATRIX_XMP;
    use super::super::xmp_write::rewrite_xmp;
    use super::validate_rewrite_preserves_non_xmp_items;

    #[test]
    fn validate_rewrite_rejects_changed_non_xmp_payload_and_opaque_meta() {
        let input = include_bytes!("../../../tests/data/sample.heic");
        let mut rewritten = Vec::new();
        rewrite_xmp(input, MATRIX_XMP, &mut rewritten).expect("sample HEIC rewrite");
        validate_rewrite_preserves_non_xmp_items(input, &rewritten)
            .expect("writer output must preserve protected bytes");

        let (_, _, iloc, _, _, _) = find_meta_layout(&rewritten).unwrap();
        let layout = parse_iloc(&rewritten, iloc).unwrap();
        let image = layout
            .items
            .iter()
            .find(|item| item.item_id == 1)
            .expect("primary image item");
        let image_start = usize::try_from(
            image.base_offset + image.extents.first().expect("image extent").offset,
        )
        .unwrap();
        let mut changed_payload = rewritten.clone();
        changed_payload[image_start] ^= 1;
        assert!(
            validate_rewrite_preserves_non_xmp_items(input, &changed_payload).is_err(),
            "changed image payload must fail validation"
        );

        let (meta, _, _, _, _, prefix_size) = find_meta_layout(&rewritten).unwrap();
        let children =
            scan_raw_boxes(&rewritten, meta.body_start() + prefix_size, meta.end()).unwrap();
        let iprp = children
            .iter()
            .find(|child| child.kind == *b"iprp")
            .expect("sample HEIC iprp");
        let mut changed_meta = rewritten.clone();
        changed_meta[iprp.body_start()] ^= 1;
        assert!(
            validate_rewrite_preserves_non_xmp_items(input, &changed_meta).is_err(),
            "changed opaque meta bytes must fail validation"
        );
    }
}
