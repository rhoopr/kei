#![no_main]

// Drives the inherent `from_str` parsers on the three state-DB enums
// (VersionSizeKey, AssetStatus, MediaType). Inputs come from a sqlite
// state DB on disk that could be replayed, hand-edited, or corrupted; a
// panic here would crash any kei subcommand that loads state.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    kei::__fuzz::state_enums_from_str(s);
});
