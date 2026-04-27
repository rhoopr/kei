#![no_main]

// Mirrors `download/metadata.rs::probe_exif_heif`: extract the XMP packet
// from HEIC bytes via kei's filtered header walk, then parse the packet
// through xmp_toolkit. Catches integration bugs that the per-stage targets
// (`heif_atoms`, `xmp_packet`) miss.

use libfuzzer_sys::fuzz_target;
use xmp_toolkit::XmpMeta;

fuzz_target!(|data: &[u8]| {
    let Some(xmp_bytes) = kei::__fuzz::heif_extract_xmp(data) else {
        return;
    };
    let Ok(s) = std::str::from_utf8(&xmp_bytes) else {
        return;
    };
    let _ = s.parse::<XmpMeta>();
});
