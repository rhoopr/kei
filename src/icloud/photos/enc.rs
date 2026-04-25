//! Decoders for iCloud `*Enc` metadata fields.
//!
//! Despite the `Enc` suffix, these fields are **not client-side encrypted** for
//! non-ADP accounts. Apple's servers decrypt them before returning them via the
//! web API. What arrives is base64-encoded plaintext or binary plist data.
//!
//! kei already handles this pattern for `filenameEnc`. For ADP accounts these
//! fields would be genuinely end-to-end encrypted, but `i_cdp_enabled` checks
//! cause kei to bail on ADP accounts before these decoders are reached.

use std::borrow::Cow;
use std::io::Cursor;

use base64::Engine;
use plist::Value as PlistValue;
use serde_json::Value;

/// Read the `value` string from a `fieldEnc` JSON entry.
///
/// Accepts both Apple variants: `type: "STRING"` (plain UTF-8 text in
/// `value`, returned borrowed) and `type: "ENCRYPTED_BYTES"` (base64-
/// encoded bytes, returned owned after decode). Plist fields whose
/// `value` is always base64 fall through to the ENCRYPTED_BYTES arm.
fn decoded_bytes(entry: &Value) -> Option<Cow<'_, [u8]>> {
    let value = entry.get("value")?.as_str()?;
    let enc_type = entry
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("ENCRYPTED_BYTES");
    match enc_type {
        "STRING" => Some(Cow::Borrowed(value.as_bytes())),
        "ENCRYPTED_BYTES" => base64::engine::general_purpose::STANDARD
            .decode(value)
            .ok()
            .map(Cow::Owned),
        other => {
            tracing::warn!(enc_type = %other, "Unsupported Enc field type");
            None
        }
    }
}

/// Decode a UTF-8 `*Enc` string field (e.g. `captionEnc`, `extendedDescEnc`).
///
/// On the common `type: "STRING"` branch this avoids the
/// `Vec<u8>` round-trip that `decoded_bytes()` performs.
pub fn decode_string(fields: &Value, key: &str) -> Option<String> {
    let entry = fields.get(key)?;
    if entry.is_null() {
        return None;
    }
    let value = entry.get("value")?.as_str()?;
    match entry
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("ENCRYPTED_BYTES")
    {
        "STRING" => Some(value.to_owned()),
        "ENCRYPTED_BYTES" => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(value)
                .ok()?;
            String::from_utf8(bytes).ok()
        }
        other => {
            tracing::warn!(enc_type = %other, "Unsupported Enc field type");
            None
        }
    }
}

/// Decode `keywordsEnc`: base64 -> binary plist containing an array of strings.
/// Returns the keyword list or `None` if absent / malformed.
pub fn decode_keywords(fields: &Value) -> Option<Vec<String>> {
    let entry = fields.get("keywordsEnc")?;
    if entry.is_null() {
        return None;
    }
    let bytes = decoded_bytes(entry)?;
    let value = PlistValue::from_reader(Cursor::new(&bytes)).ok()?;
    let array = value.into_array()?;
    Some(
        array
            .into_iter()
            .filter_map(|v| v.into_string())
            .collect::<Vec<_>>(),
    )
}

/// GPS triple decoded from `locationEnc` / `locationV2Enc` binary plists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    pub altitude: Option<f64>,
}

/// Decode `locationEnc` or `locationV2Enc` fields.
///
/// Both contain a binary plist dict with `lat: f64`, `lng: f64`, `alt: f64`
/// keys. Returns `None` if decoding fails or lat/lng are absent.
pub fn decode_location(fields: &Value, key: &str) -> Option<Location> {
    let entry = fields.get(key)?;
    if entry.is_null() {
        return None;
    }
    let bytes = decoded_bytes(entry)?;
    let value = PlistValue::from_reader(Cursor::new(&bytes)).ok()?;
    let dict = value.into_dictionary()?;
    let latitude = dict.get("lat").and_then(plist_to_f64)?;
    let longitude = dict.get("lng").and_then(plist_to_f64)?;
    let altitude = dict.get("alt").and_then(plist_to_f64);
    Some(Location {
        latitude,
        longitude,
        altitude,
    })
}

fn plist_to_f64(value: &PlistValue) -> Option<f64> {
    match value {
        PlistValue::Real(r) => Some(*r),
        #[allow(
            clippy::cast_precision_loss,
            reason = "plist integers encoding GPS/location values fit well within f64 mantissa"
        )]
        PlistValue::Integer(i) => i.as_signed().map(|v| v as f64),
        _ => None,
    }
}

/// Decode a location with automatic fallback: prefer `locationV2Enc`, then
/// `locationEnc`, then the plain `locationLatitude`/`locationLongitude` pair.
/// Altitude is only available from the plist-encoded variants.
pub fn decode_location_with_fallback(fields: &Value) -> Option<Location> {
    if let Some(loc) = decode_location(fields, "locationV2Enc") {
        return Some(loc);
    }
    if let Some(loc) = decode_location(fields, "locationEnc") {
        return Some(loc);
    }
    let lat = fields.get("locationLatitude")?.get("value")?.as_f64()?;
    let lng = fields.get("locationLongitude")?.get("value")?.as_f64()?;
    Some(Location {
        latitude: lat,
        longitude: lng,
        altitude: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn bplist_from(value: PlistValue) -> Vec<u8> {
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &value).unwrap();
        out
    }

    #[test]
    fn decode_string_string_type() {
        let fields = json!({
            "captionEnc": {"value": "hello", "type": "STRING"}
        });
        assert_eq!(decode_string(&fields, "captionEnc"), Some("hello".into()));
    }

    #[test]
    fn decode_string_encrypted_bytes() {
        let fields = json!({
            "captionEnc": {"value": b64(b"a caption"), "type": "ENCRYPTED_BYTES"}
        });
        assert_eq!(
            decode_string(&fields, "captionEnc"),
            Some("a caption".into())
        );
    }

    #[test]
    fn decode_string_missing_returns_none() {
        let fields = json!({});
        assert_eq!(decode_string(&fields, "captionEnc"), None);
    }

    #[test]
    fn decode_string_null_returns_none() {
        let fields = json!({"captionEnc": null});
        assert_eq!(decode_string(&fields, "captionEnc"), None);
    }

    #[test]
    fn decode_keywords_roundtrip() {
        let bplist = bplist_from(PlistValue::Array(vec![
            PlistValue::String("vacation".into()),
            PlistValue::String("beach".into()),
        ]));
        let fields = json!({
            "keywordsEnc": {"value": b64(&bplist), "type": "ENCRYPTED_BYTES"}
        });
        assert_eq!(
            decode_keywords(&fields),
            Some(vec!["vacation".into(), "beach".into()])
        );
    }

    #[test]
    fn decode_keywords_missing_returns_none() {
        assert_eq!(decode_keywords(&json!({})), None);
    }

    #[test]
    fn decode_keywords_non_array_returns_none() {
        let bplist = bplist_from(PlistValue::String("not an array".into()));
        let fields = json!({
            "keywordsEnc": {"value": b64(&bplist), "type": "ENCRYPTED_BYTES"}
        });
        assert_eq!(decode_keywords(&fields), None);
    }

    #[test]
    fn decode_keywords_malformed_base64_returns_none() {
        let fields = json!({
            "keywordsEnc": {"value": "!!not-base64!!", "type": "ENCRYPTED_BYTES"}
        });
        assert_eq!(decode_keywords(&fields), None);
    }

    #[test]
    fn decode_location_roundtrip_with_altitude() {
        let mut dict = plist::Dictionary::new();
        dict.insert("lat".into(), PlistValue::Real(37.7749));
        dict.insert("lng".into(), PlistValue::Real(-122.4194));
        dict.insert("alt".into(), PlistValue::Real(17.0));
        let bplist = bplist_from(PlistValue::Dictionary(dict));
        let fields = json!({
            "locationEnc": {"value": b64(&bplist), "type": "ENCRYPTED_BYTES"}
        });
        let loc = decode_location(&fields, "locationEnc").unwrap();
        assert!((loc.latitude - 37.7749).abs() < 1e-6);
        assert!((loc.longitude - -122.4194).abs() < 1e-6);
        assert_eq!(loc.altitude, Some(17.0));
    }

    #[test]
    fn decode_location_without_altitude() {
        let mut dict = plist::Dictionary::new();
        dict.insert("lat".into(), PlistValue::Real(1.0));
        dict.insert("lng".into(), PlistValue::Real(2.0));
        let bplist = bplist_from(PlistValue::Dictionary(dict));
        let fields = json!({
            "locationEnc": {"value": b64(&bplist), "type": "ENCRYPTED_BYTES"}
        });
        let loc = decode_location(&fields, "locationEnc").unwrap();
        assert_eq!(loc.altitude, None);
    }

    #[test]
    fn decode_location_missing_returns_none() {
        assert_eq!(decode_location(&json!({}), "locationEnc"), None);
    }

    #[test]
    fn decode_location_missing_lat_returns_none() {
        let mut dict = plist::Dictionary::new();
        dict.insert("lng".into(), PlistValue::Real(2.0));
        let bplist = bplist_from(PlistValue::Dictionary(dict));
        let fields = json!({
            "locationEnc": {"value": b64(&bplist), "type": "ENCRYPTED_BYTES"}
        });
        assert_eq!(decode_location(&fields, "locationEnc"), None);
    }

    #[test]
    fn decode_location_prefers_v2_over_v1() {
        // Build v1 with bad coords, v2 with good coords — fallback must pick v2.
        let mut d1 = plist::Dictionary::new();
        d1.insert("lat".into(), PlistValue::Real(1.0));
        d1.insert("lng".into(), PlistValue::Real(1.0));
        let bp1 = bplist_from(PlistValue::Dictionary(d1));
        let mut d2 = plist::Dictionary::new();
        d2.insert("lat".into(), PlistValue::Real(42.0));
        d2.insert("lng".into(), PlistValue::Real(-7.0));
        let bp2 = bplist_from(PlistValue::Dictionary(d2));
        let fields = json!({
            "locationEnc": {"value": b64(&bp1), "type": "ENCRYPTED_BYTES"},
            "locationV2Enc": {"value": b64(&bp2), "type": "ENCRYPTED_BYTES"},
        });
        let loc = decode_location_with_fallback(&fields).unwrap();
        assert_eq!(loc.latitude, 42.0);
        assert_eq!(loc.longitude, -7.0);
    }

    #[test]
    fn decode_location_falls_back_to_plain_fields() {
        let fields = json!({
            "locationLatitude": {"value": 10.0},
            "locationLongitude": {"value": 20.0},
        });
        let loc = decode_location_with_fallback(&fields).unwrap();
        assert_eq!(loc.latitude, 10.0);
        assert_eq!(loc.longitude, 20.0);
        assert_eq!(loc.altitude, None);
    }

    #[test]
    fn decode_location_fallback_returns_none_when_all_missing() {
        assert_eq!(decode_location_with_fallback(&json!({})), None);
    }
}
