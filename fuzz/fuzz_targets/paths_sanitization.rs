#![no_main]

// Drives the filename / path-component sanitizers in `src/download/paths.rs`
// (clean_filename, sanitize_path_component, expand_album_token,
// add_dedup_suffix, strip_python_wrapper, remove_unicode_chars) through one
// `kei::__fuzz::paths_sanitization` call. Production callers pass UTF-8
// strings derived from CloudKit JSON, so non-UTF-8 bytes don't reach this
// code path.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    kei::__fuzz::paths_sanitization(s);
});
