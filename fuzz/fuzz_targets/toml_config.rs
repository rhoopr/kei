#![no_main]

// Drives kei's TomlConfig deserializer over arbitrary input. Most of the
// surface is `serde::Deserialize` derive logic, but a few fields hit custom
// deserializers (`folder_structure`, `RecentLimit`) and `deny_unknown_fields`
// boundary checks. Users can hand kei a malformed config file at any time;
// a panic here would crash kei before tracing is even installed.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    kei::__fuzz::parse_toml_config(s);
});
