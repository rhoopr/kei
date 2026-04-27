#![no_main]

// Drives `PhotoAsset::from_records(master, asset)` - the hot path that
// turns two CloudKit `Record` JSON values into a typed PhotoAsset.
// The internal extractors (decode_filename, resolve_item_type,
// extract_versions, metadata::extract) chain through five sibling modules
// plus crate::state::AssetMetadata, so a panic anywhere in there ends up
// here.
//
// `Record` parses as JSON, so the harness needs two JSON values per
// iteration. Split the input on the first NUL into (master, asset). If
// the input doesn't contain a NUL, both halves get the same bytes - still
// useful, since same-shape pairs exercise the version-dedup logic that
// from_records does.

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    let (master_bytes, asset_bytes) = match data.iter().position(|b| *b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, data),
    };
    let Ok(master) = serde_json::from_slice::<Value>(master_bytes) else {
        return;
    };
    let Ok(asset) = serde_json::from_slice::<Value>(asset_bytes) else {
        return;
    };
    kei::__fuzz::photo_asset_from_record_json(master, asset);
});
