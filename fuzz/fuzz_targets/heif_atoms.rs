#![no_main]

// Drives `extract_xmp_bytes` and `is_heif_content` from
// `src/download/heif.rs` over arbitrary bytes. Bug #274 (panic on `uri `
// infe items in iOS-17 HEICs) lived here, and the upstream mp4-atom
// `parse_vorbis_comment` OOM (kixelated/mp4-atom#154) shows up here too.
// kei has a header-walk filter (PR #286) that closes the OOM, so this
// target now exercises the post-filter path.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = kei::__fuzz::heif_extract_xmp(data);
    let _ = kei::__fuzz::heif_is_heif_content(data);
});
