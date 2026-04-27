#![no_main]

// Drives every CloudKit response struct (`ZoneListResponse`, `QueryResponse`,
// `Record`, `ChangesDatabaseResponse`, etc.) through `serde_json::from_slice`
// in one pass via `kei::__fuzz::cloudkit_try_all`. Apple controls these
// payloads, so any panic is reachable by a hostile or malformed iCloud
// response.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kei::__fuzz::cloudkit_try_all(data);
});
