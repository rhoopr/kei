//! Primary-image metadata ownership and item-reference encoding.

use super::boxes::{RawBox, box_with_body, find_meta_layout, scan_raw_boxes};
use super::items::{IinfLayout, parse_iinf};
use super::{HeifError, invalid_layout};
use std::collections::HashSet;

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser and version-specific size checks prove every iref child field before direct access."
)]
fn item_reference_pairs(
    bytes: &[u8],
    iref: RawBox,
    reference_type: [u8; 4],
) -> Result<Vec<(u32, Vec<u32>)>, HeifError> {
    let body = &bytes[iref.body_start()..iref.end()];
    if body.len() < 4 {
        return Err(invalid_layout("iref box is truncated"));
    }
    let version = body[0];
    let children = scan_raw_boxes(body, 4, body.len())?;
    let mut pairs = Vec::new();
    for child in children {
        if child.kind != reference_type {
            continue;
        }
        let child_body = &body[child.body_start()..child.end()];
        let (from_item_id, count_pos, id_width) = match version {
            0 => {
                if child_body.len() < 4 {
                    return Err(invalid_layout("iref child is truncated"));
                }
                (
                    u32::from(u16::from_be_bytes([child_body[0], child_body[1]])),
                    2,
                    2,
                )
            }
            1 => {
                if child_body.len() < 6 {
                    return Err(invalid_layout("iref version 1 child is truncated"));
                }
                (
                    u32::from_be_bytes(
                        child_body[0..4]
                            .try_into()
                            .map_err(|_| invalid_layout("invalid iref source item ID"))?,
                    ),
                    4,
                    4,
                )
            }
            _ => return Err(invalid_layout("unsupported iref version")),
        };
        let count = usize::from(u16::from_be_bytes(
            child_body[count_pos..count_pos + 2]
                .try_into()
                .map_err(|_| invalid_layout("invalid iref reference count"))?,
        ));
        let expected_len = count_pos
            .checked_add(2)
            .and_then(|length| length.checked_add(count.checked_mul(id_width)?))
            .ok_or_else(|| invalid_layout("iref reference list overflows"))?;
        if child_body.len() != expected_len {
            return Err(invalid_layout("iref reference list is inconsistent"));
        }
        let mut targets = Vec::with_capacity(count);
        for index in 0..count {
            let start = count_pos + 2 + index * id_width;
            let to_item_id = if id_width == 2 {
                u32::from(u16::from_be_bytes([
                    child_body[start],
                    child_body[start + 1],
                ]))
            } else {
                u32::from_be_bytes(
                    child_body[start..start + id_width]
                        .try_into()
                        .map_err(|_| invalid_layout("invalid iref target item ID"))?,
                )
            };
            targets.push(to_item_id);
        }
        pairs.push((from_item_id, targets));
    }
    Ok(pairs)
}

fn cdsc_pairs(bytes: &[u8], iref: RawBox) -> Result<Vec<(u32, Vec<u32>)>, HeifError> {
    item_reference_pairs(bytes, iref, *b"cdsc")
}

pub(super) fn insertion_described_item_ids(
    bytes: &[u8],
    iref: Option<RawBox>,
    primary_item_id: u32,
    iinf: &IinfLayout,
) -> Result<Vec<u32>, HeifError> {
    let [] = iinf.tone_map_item_ids.as_slice() else {
        let [tone_map_item_id] = iinf.tone_map_item_ids.as_slice() else {
            return Err(invalid_layout(
                "HEIC XMP insertion has multiple tone-mapped images",
            ));
        };
        let Some(iref) = iref else {
            return Err(invalid_layout(
                "HEIC XMP insertion cannot prove the tone-mapped image",
            ));
        };
        let known_item_ids: HashSet<u32> = iinf.item_ids.iter().copied().collect();
        let exif_item_ids: HashSet<u32> = iinf.exif_item_ids.iter().copied().collect();
        let mut tone_map_inputs = item_reference_pairs(bytes, iref, *b"dimg")?
            .into_iter()
            .filter_map(|(source, targets)| (source == *tone_map_item_id).then_some(targets));
        let Some(inputs) = tone_map_inputs.next() else {
            return Err(invalid_layout(
                "HEIC tone-mapped image has no derived-image reference",
            ));
        };
        let [base_item_id, gain_map_item_id] = inputs.as_slice() else {
            return Err(invalid_layout(
                "HEIC tone-mapped image does not have exactly two inputs",
            ));
        };
        if tone_map_inputs.next().is_some()
            || *base_item_id != primary_item_id
            || primary_item_id == *tone_map_item_id
            || base_item_id == gain_map_item_id
            || *gain_map_item_id == *tone_map_item_id
            || exif_item_ids.contains(&primary_item_id)
            || iinf.xmp_item_ids.contains(&primary_item_id)
            || iinf.tone_map_item_ids.contains(&primary_item_id)
            || exif_item_ids.contains(gain_map_item_id)
            || iinf.xmp_item_ids.contains(gain_map_item_id)
            || iinf.tone_map_item_ids.contains(gain_map_item_id)
            || inputs
                .iter()
                .any(|item_id| !known_item_ids.contains(item_id))
        {
            return Err(invalid_layout(
                "HEIC tone-mapped image is not uniquely derived from the primary image",
            ));
        }

        let references = cdsc_pairs(bytes, iref)?;
        let xmp_item_ids: HashSet<u32> = iinf.xmp_item_ids.iter().copied().collect();
        if references.iter().any(|(source, targets)| {
            xmp_item_ids.contains(source) && targets.contains(tone_map_item_id)
        }) {
            return Err(invalid_layout(
                "HEIC tone-mapped image already has an XMP descriptor",
            ));
        }
        if references.iter().any(|(source, targets)| {
            exif_item_ids.contains(source)
                && targets
                    .iter()
                    .any(|target| !known_item_ids.contains(target))
        }) {
            return Err(invalid_layout(
                "Exif item describes an item absent from iinf",
            ));
        }
        let mut primary_exif_targets = references
            .iter()
            .filter(|(source, targets)| {
                exif_item_ids.contains(source) && targets.contains(&primary_item_id)
            })
            .map(|(source, targets)| (*source, targets.as_slice()));
        let Some((source_item_id, targets)) = primary_exif_targets.next() else {
            return Err(invalid_layout(
                "HEIC XMP insertion has no primary-image Exif evidence",
            ));
        };
        if primary_exif_targets.next().is_some()
            || targets.len() != 2
            || !targets.contains(&primary_item_id)
            || !targets.contains(tone_map_item_id)
            || references
                .iter()
                .filter(|(source, _)| *source == source_item_id)
                .count()
                != 1
        {
            return Err(invalid_layout(
                "HEIC XMP insertion has ambiguous tone-map metadata scope",
            ));
        }
        return Ok(vec![primary_item_id, *tone_map_item_id]);
    };
    Ok(vec![primary_item_id])
}

/// Resolve which XMP item holds the primary image's metadata.
///
/// A `cdsc` reference binds a descriptive item to the image it describes, so
/// an XMP item that names an auxiliary image (a depth map, a portrait matte, a
/// gain map) is that image's metadata and not the photograph's. Absence of a
/// reference is evidence too: when every packet names an auxiliary image the
/// primary has no XMP of its own, which is the same conclusion as a file with
/// no XMP at all, and `Ok(None)` lets the caller insert one.
///
/// Items with no `cdsc` reference are unattributed. A single unattributed
/// packet is the ordinary single-XMP HEIC and resolves to itself; several are
/// undecidable and fail closed rather than risk overwriting the wrong one.
pub(super) fn select_xmp_item_id(
    bytes: &[u8],
    iref: Option<RawBox>,
    primary_item_id: u32,
    xmp_item_ids: &[u32],
) -> Result<Option<u32>, HeifError> {
    let Some((first, rest)) = xmp_item_ids.split_first() else {
        return Ok(None);
    };
    let Some(iref) = iref else {
        return if rest.is_empty() {
            Ok(Some(*first))
        } else {
            Err(invalid_layout(
                "multiple XMP items have no primary-image association",
            ))
        };
    };
    let pairs = cdsc_pairs(bytes, iref)?;
    let mut primary = xmp_item_ids.iter().copied().filter(|item_id| {
        pairs
            .iter()
            .any(|(from, targets)| from == item_id && targets.contains(&primary_item_id))
    });
    if let Some(item_id) = primary.next() {
        return if primary.next().is_some() {
            Err(invalid_layout(
                "multiple XMP items describe the primary image",
            ))
        } else {
            Ok(Some(item_id))
        };
    }
    let mut unattributed = xmp_item_ids
        .iter()
        .copied()
        .filter(|item_id| !pairs.iter().any(|(from, _)| from == item_id));
    match (unattributed.next(), unattributed.next()) {
        (None, _) => Ok(None),
        (Some(item_id), None) => Ok(Some(item_id)),
        (Some(_), Some(_)) => Err(invalid_layout(
            "multiple XMP items have no primary-image association",
        )),
    }
}

pub(super) fn select_exif_item_id(
    bytes: &[u8],
    iref: Option<RawBox>,
    primary_item_id: u32,
    exif_item_ids: &[u32],
    known_item_ids: &[u32],
) -> Result<Option<u32>, HeifError> {
    let Some((first, rest)) = exif_item_ids.split_first() else {
        return Ok(None);
    };
    let Some(iref) = iref else {
        return if rest.is_empty() {
            Ok(Some(*first))
        } else {
            Err(invalid_layout(
                "multiple Exif items have no primary-image association",
            ))
        };
    };
    let pairs = cdsc_pairs(bytes, iref)?;
    let known_item_ids: HashSet<u32> = known_item_ids.iter().copied().collect();
    if pairs.iter().any(|(from, targets)| {
        exif_item_ids.contains(from)
            && targets
                .iter()
                .any(|target| !known_item_ids.contains(target))
    }) {
        return Err(invalid_layout(
            "Exif item describes an item absent from iinf",
        ));
    }
    let mut primary = exif_item_ids.iter().copied().filter(|item_id| {
        pairs
            .iter()
            .any(|(from, targets)| from == item_id && targets.contains(&primary_item_id))
    });
    if let Some(item_id) = primary.next() {
        return if primary.next().is_some() {
            Err(invalid_layout(
                "multiple Exif items describe the primary image",
            ))
        } else {
            Ok(Some(item_id))
        };
    }
    let mut unattributed = exif_item_ids
        .iter()
        .copied()
        .filter(|item_id| !pairs.iter().any(|(from, _)| from == item_id));
    match (unattributed.next(), unattributed.next()) {
        (None, _) => Ok(None),
        (Some(item_id), None) => Ok(Some(item_id)),
        (Some(_), Some(_)) => Err(invalid_layout(
            "multiple Exif items have no primary-image association",
        )),
    }
}

fn ensure_metadata_item_is_primary_only(
    bytes: &[u8],
    iref: Option<RawBox>,
    item_id: u32,
    primary_item_id: u32,
) -> Result<(), HeifError> {
    let Some(iref) = iref else {
        return Ok(());
    };
    let pairs = cdsc_pairs(bytes, iref)?;
    let mut describes_primary = false;
    let mut describes_other = false;
    for (_, targets) in pairs.iter().filter(|(from, _)| *from == item_id) {
        describes_primary |= targets.contains(&primary_item_id);
        describes_other |= targets.iter().any(|target| *target != primary_item_id);
    }
    if describes_primary && describes_other {
        return Err(invalid_layout(
            "HEIF metadata item is shared by the primary and another image",
        ));
    }
    Ok(())
}

pub(crate) fn validate_capture_repair_item_ownership(bytes: &[u8]) -> Result<(), HeifError> {
    let (_, iinf, _, iref, primary_item_id, _) = find_meta_layout(bytes)?;
    let iinf_layout = parse_iinf(bytes, iinf)?;
    if let Some(exif_item_id) = select_exif_item_id(
        bytes,
        iref,
        primary_item_id,
        &iinf_layout.exif_item_ids,
        &iinf_layout.item_ids,
    )? {
        ensure_metadata_item_is_primary_only(bytes, iref, exif_item_id, primary_item_id)?;
    }
    if let Some(xmp_item_id) =
        select_xmp_item_id(bytes, iref, primary_item_id, &iinf_layout.xmp_item_ids)?
    {
        ensure_metadata_item_is_primary_only(bytes, iref, xmp_item_id, primary_item_id)?;
    }
    Ok(())
}

/// Encode one `cdsc` reference from a descriptive item to the images it
/// describes. `version` is the enclosing `iref` version, which fixes the
/// item-id width at 16 or 32 bits.
fn cdsc_body(version: u8, from_item_id: u32, to_item_ids: &[u32]) -> Result<Vec<u8>, HeifError> {
    let count = u16::try_from(to_item_ids.len())
        .map_err(|_| invalid_layout("cdsc names too many images"))?;
    if count == 0 {
        return Err(invalid_layout("cdsc names no image"));
    }
    let mut body = Vec::new();
    match version {
        0 => {
            let from = u16::try_from(from_item_id).map_err(|_| HeifError::ValueOverflow {
                field: "16-bit cdsc source item id",
            })?;
            body.extend_from_slice(&from.to_be_bytes());
            body.extend_from_slice(&count.to_be_bytes());
            for to_item_id in to_item_ids {
                let to = u16::try_from(*to_item_id).map_err(|_| HeifError::ValueOverflow {
                    field: "16-bit cdsc target item id",
                })?;
                body.extend_from_slice(&to.to_be_bytes());
            }
        }
        1 => {
            body.extend_from_slice(&from_item_id.to_be_bytes());
            body.extend_from_slice(&count.to_be_bytes());
            for to_item_id in to_item_ids {
                body.extend_from_slice(&to_item_id.to_be_bytes());
            }
        }
        _ => return Err(invalid_layout("unsupported iref version")),
    }
    Ok(body)
}

#[allow(
    clippy::indexing_slicing,
    reason = "The raw-box parser and version-specific size checks prove every iref child field before direct access."
)]
pub(super) fn append_cdsc_reference(
    bytes: &[u8],
    iref: RawBox,
    xmp_item_id: u32,
    described_item_ids: &[u32],
    known_item_ids: &[u32],
) -> Result<Vec<u8>, HeifError> {
    let body = &bytes[iref.body_start()..iref.end()];
    if body.len() < 4 {
        return Err(invalid_layout("iref box is truncated"));
    }
    let version = body[0];
    let children = scan_raw_boxes(body, 4, body.len())?;
    let known_item_ids: HashSet<u32> = known_item_ids.iter().copied().collect();
    for child in children {
        let child_body = &body[child.body_start()..child.end()];
        let (from_item_id, count_pos, id_width) = match version {
            0 => {
                if child_body.len() < 4 {
                    return Err(invalid_layout("iref child is truncated"));
                }
                (
                    u32::from(u16::from_be_bytes([child_body[0], child_body[1]])),
                    2,
                    2,
                )
            }
            1 => {
                if child_body.len() < 6 {
                    return Err(invalid_layout("iref version 1 child is truncated"));
                }
                (
                    u32::from_be_bytes(
                        child_body[0..4]
                            .try_into()
                            .map_err(|_| invalid_layout("invalid iref source item ID"))?,
                    ),
                    4,
                    4,
                )
            }
            _ => return Err(invalid_layout("unsupported iref version")),
        };
        let count = u16::from_be_bytes(
            child_body[count_pos..count_pos + 2]
                .try_into()
                .map_err(|_| invalid_layout("invalid iref reference count"))?,
        ) as usize;
        let expected_len = count_pos
            .checked_add(2)
            .and_then(|len| len.checked_add(count.checked_mul(id_width)?))
            .ok_or_else(|| invalid_layout("iref reference list overflows"))?;
        if child_body.len() != expected_len {
            return Err(invalid_layout("iref reference list is inconsistent"));
        }
        if !known_item_ids.contains(&from_item_id) {
            return Err(invalid_layout("iref source item ID is absent from iinf"));
        }
        for index in 0..count {
            let start = count_pos + 2 + index * id_width;
            let to_item_id = if id_width == 2 {
                u32::from(u16::from_be_bytes([
                    child_body[start],
                    child_body[start + 1],
                ]))
            } else {
                u32::from_be_bytes(
                    child_body[start..start + id_width]
                        .try_into()
                        .map_err(|_| invalid_layout("invalid iref target item ID"))?,
                )
            };
            if !known_item_ids.contains(&to_item_id) {
                return Err(invalid_layout("iref target item ID is absent from iinf"));
            }
        }
    }
    let mut new_body = body.to_vec();
    new_body.extend_from_slice(&box_with_body(
        *b"cdsc",
        &cdsc_body(version, xmp_item_id, described_item_ids)?,
    )?);
    box_with_body(iref.kind, &new_body)
}

/// Build a fresh `iref` box carrying a single `cdsc` reference from the new XMP
/// item to the images it describes. Used when a file has no `iref` of its own so
/// insertion can still associate the descriptive XMP with the image, the way
/// Apple's own writer does, rather than refusing the file and leaving a retry
/// marker that never resolves. Version 0 encodes 16-bit ids; version 1 is used
/// when any id exceeds 16 bits.
pub(super) fn synthesise_cdsc_iref(
    xmp_item_id: u32,
    described_item_ids: &[u32],
) -> Result<Vec<u8>, HeifError> {
    let version = if xmp_item_id <= u32::from(u16::MAX)
        && described_item_ids
            .iter()
            .all(|item_id| *item_id <= u32::from(u16::MAX))
    {
        0u8
    } else {
        1u8
    };
    let mut body = vec![version, 0, 0, 0];
    body.extend_from_slice(&box_with_body(
        *b"cdsc",
        &cdsc_body(version, xmp_item_id, described_item_ids)?,
    )?);
    box_with_body(*b"iref", &body)
}

#[cfg(test)]
mod tests {
    use super::super::HeifError;
    use super::super::boxes::scan_raw_boxes;
    use super::super::test_support::{
        PRIMARY_XMP, apple_multi_xmp_spec, build_heic, capture_exif_payload, heic_with_exif_items,
        ref_box, xmp_item,
    };
    use super::validate_capture_repair_item_ownership;

    #[test]
    fn capture_repair_rejects_metadata_shared_with_an_auxiliary_image() {
        let exif = capture_exif_payload(false);
        let shared_exif = heic_with_exif_items(&[(2, &exif)], vec![ref_box(b"cdsc", 2, &[1, 4])]);
        assert!(matches!(
            validate_capture_repair_item_ownership(&shared_exif),
            Err(HeifError::InvalidLayout { reason }) if reason.contains("shared")
        ));

        let shared_xmp = build_heic(&apple_multi_xmp_spec(
            vec![xmp_item(6, PRIMARY_XMP)],
            vec![ref_box(b"cdsc", 6, &[1, 4])],
        ));
        assert!(matches!(
            validate_capture_repair_item_ownership(&shared_xmp),
            Err(HeifError::InvalidLayout { reason }) if reason.contains("shared")
        ));
    }

    #[test]
    fn capture_repair_rejects_duplicate_control_boxes() {
        let input = include_bytes!("../../../tests/data/sample.heic");
        let meta = scan_raw_boxes(input, 0, input.len())
            .unwrap()
            .into_iter()
            .find(|atom| atom.kind == *b"meta")
            .expect("sample HEIC meta box");
        let mut duplicate = input.to_vec();
        duplicate.extend_from_slice(&input[meta.start..meta.end()]);

        assert!(matches!(
            validate_capture_repair_item_ownership(&duplicate),
            Err(HeifError::InvalidLayout { reason }) if reason.contains("multiple top-level meta")
        ));
    }
}
