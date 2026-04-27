#![no_main]

// Drives Adobe's vendored XMP Toolkit (C++ via FFI) by feeding XMP packets
// straight into `XmpMeta::from_str`. kei reaches this code path in
// `download/metadata.rs::probe_exif_heif` after extracting the packet from
// a HEIC container. The C++ reach is what makes this the highest-value
// target: a panic in Rust is bad, a heap corruption in vendored C++ is
// worse, and ASan (which cargo-fuzz turns on by default) catches both.

use libfuzzer_sys::fuzz_target;
use xmp_toolkit::XmpMeta;

fuzz_target!(|data: &[u8]| {
    // The toolkit's parse impl only accepts UTF-8 (it's RDF/XML). Skip
    // non-UTF-8 inputs so we don't waste budget on the byte layer.
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _ = s.parse::<XmpMeta>();
});
