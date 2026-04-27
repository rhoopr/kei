#![no_main]

// Drives the JSON / base64 / binary-plist decoders in
// `src/icloud/photos/enc.rs` through both shapes:
//   - parse the input as a JSON Value and call every decoder on the
//     resulting structure (covers JSON-shape edge cases),
//   - wrap the input as a base64 ENCRYPTED_BYTES payload inside a synthesized
//     JSON envelope (drives the bplist parser without making libfuzzer
//     discover the JSON wrapper itself).

use base64::Engine;
use libfuzzer_sys::fuzz_target;
use serde_json::{json, Value};

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        kei::__fuzz::enc_decoders(&value);
    }

    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    let wrapped = json!({
        "captionEnc":     {"value": b64.clone(), "type": "ENCRYPTED_BYTES"},
        "keywordsEnc":    {"value": b64.clone(), "type": "ENCRYPTED_BYTES"},
        "locationEnc":    {"value": b64.clone(), "type": "ENCRYPTED_BYTES"},
        "locationV2Enc":  {"value": b64,         "type": "ENCRYPTED_BYTES"},
    });
    kei::__fuzz::enc_decoders(&wrapped);
});
