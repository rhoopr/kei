#![no_main]

// Drives every iCloud auth response struct through `serde_json::from_slice`.
// Apple controls this payload, and `TwoFactorChallenge` has a custom
// deserializer that probes for `fsa_challenge: Option<Value>` and a
// `service_errors` array - exactly the shape that benefits from fuzzing.
// A bug here would mean kei misclassifies a 2FA challenge as a successful
// login (or vice versa) on a hostile or malformed response.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    kei::__fuzz::auth_responses_try_all(data);
});
