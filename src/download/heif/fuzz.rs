//! Fuzz entry points that exercise the production writer and preservation checks.

use super::boxes::{find_meta_layout, is_heif_content};
use super::preservation::validate_rewrite_preserves_non_xmp_items;
use super::xmp_write::{locate_xmp, rewrite_xmp};

/// Fuzz-only driver that exercises [`rewrite_xmp`] with a fixed XMP marker and
/// asserts the writer's safety contract, rather than merely that it does not
/// crash. A rejected rewrite must emit nothing. An accepted rewrite must keep
/// the container HEIF, make the written packet readable again, preserve every
/// item payload it does not own, and preserve opaque `meta` sub-boxes
/// byte-for-byte.
/// Compiled only for the fuzz harness; absent from production builds.
#[cfg(feature = "__fuzz_internals")]
pub(crate) fn fuzz_rewrite_xmp_preserves(input: &[u8]) {
    const MARKER: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description><xmp:Rating xmlns:xmp='http://ns.adobe.com/xap/1.0/'>3</xmp:Rating></rdf:Description></rdf:RDF></x:xmpmeta>";
    let mut output = Vec::new();
    match rewrite_xmp(input, MARKER, &mut output) {
        Err(_) => assert!(
            output.is_empty(),
            "a rejected HEIC rewrite must not emit bytes"
        ),
        Ok(()) => {
            assert!(
                is_heif_content(&output),
                "writer output must remain HEIF content"
            );
            assert_eq!(
                fuzz_extract_xmp(&output)
                    .as_deref()
                    .map(<[u8]>::trim_ascii_end),
                Some(MARKER),
                "the written XMP packet must be locatable and round-trip"
            );
            let validation = validate_rewrite_preserves_non_xmp_items(input, &output);
            assert!(
                validation.is_ok(),
                "accepted rewrite must preserve non-XMP items and opaque meta sub-boxes: {validation:?}"
            );
        }
    }
}

/// Extract the XMP packet through the writer-side parser, for the fuzz safety
/// check. Independent of the mp4-atom read path so item ids above `u16` and
/// iloc version 2 are covered.
#[cfg(feature = "__fuzz_internals")]
fn fuzz_extract_xmp(bytes: &[u8]) -> Option<Vec<u8>> {
    let (_, iinf, iloc, iref, primary_item_id, _) = find_meta_layout(bytes).ok()?;
    let (location, _, _, _, _, _, _) = locate_xmp(bytes, iinf, iloc, iref, primary_item_id).ok()?;
    let location = location?;
    let end = location.extent_start.checked_add(location.extent_length)?;
    bytes.get(location.extent_start..end).map(<[u8]>::to_vec)
}
