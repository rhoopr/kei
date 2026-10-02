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
/// Both contain a binary plist dict with `lat: f64`, `lon: f64`, `alt: f64`
/// keys. Returns `None` if decoding fails or the coordinate pair is absent,
/// nonfinite, or outside latitude [-90, 90] and longitude [-180, 180].
/// Nonfinite optional altitude is omitted without rejecting valid coordinates.
pub fn decode_location(fields: &Value, key: &str) -> Option<Location> {
    let entry = fields.get(key)?;
    if entry.is_null() {
        return None;
    }
    let bytes = decoded_bytes(entry)?;
    let value = PlistValue::from_reader(Cursor::new(&bytes)).ok()?;
    let dict = value.into_dictionary()?;
    let latitude = dict.get("lat").and_then(plist_to_f64)?;
    let longitude = dict.get("lon").and_then(plist_to_f64)?;
    let altitude = dict.get("alt").and_then(plist_to_f64);
    validated_location(latitude, longitude, altitude)
}

fn validated_location(latitude: f64, longitude: f64, altitude: Option<f64>) -> Option<Location> {
    if !latitude.is_finite()
        || !longitude.is_finite()
        || !(-90.0..=90.0).contains(&latitude)
        || !(-180.0..=180.0).contains(&longitude)
    {
        return None;
    }
    Some(Location {
        latitude,
        longitude,
        altitude: altitude.filter(|value| value.is_finite()),
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
/// Selects the first valid coordinate pair, skipping malformed or invalid candidates.
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
    validated_location(lat, lng, None)
}

#[cfg(test)]
#[allow(
    clippy::float_cmp,
    reason = "comparing exact float constants in test fixtures"
)]
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
        dict.insert("lon".into(), PlistValue::Real(-122.4194));
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
        dict.insert("lon".into(), PlistValue::Real(2.0));
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
        dict.insert("lon".into(), PlistValue::Real(2.0));
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
        d1.insert("lon".into(), PlistValue::Real(1.0));
        let bp1 = bplist_from(PlistValue::Dictionary(d1));
        let mut d2 = plist::Dictionary::new();
        d2.insert("lat".into(), PlistValue::Real(42.0));
        d2.insert("lon".into(), PlistValue::Real(-7.0));
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

    fn tactical_location_entry(latitude: f64, longitude: f64, altitude: f64) -> Value {
        let mut dict = plist::Dictionary::new();
        dict.insert("lat".into(), PlistValue::Real(latitude));
        dict.insert("lon".into(), PlistValue::Real(longitude));
        dict.insert("alt".into(), PlistValue::Real(altitude));
        json!({"value": b64(&bplist_from(PlistValue::Dictionary(dict))), "type": "ENCRYPTED_BYTES"})
    }

    #[test]
    fn tactical_gps_invalid_coordinates_do_not_enter_extracted_metadata() {
        let mut accepted = Vec::new();
        for (latitude, longitude) in [
            (91.0, 2.0),
            (-91.0, 2.0),
            (1.0, 181.0),
            (1.0, -181.0),
            (f64::NAN, 2.0),
            (f64::INFINITY, 2.0),
            (f64::NEG_INFINITY, 2.0),
            (1.0, f64::NAN),
            (1.0, f64::INFINITY),
            (1.0, f64::NEG_INFINITY),
        ] {
            for key in ["locationV2Enc", "locationEnc"] {
                let fields = json!({key: tactical_location_entry(latitude, longitude, 3.0)});
                let metadata = super::super::metadata::extract(&json!({}), &fields);
                if metadata.latitude.is_some() || metadata.longitude.is_some() {
                    accepted.push(format!("{key}: {latitude:?}, {longitude:?}"));
                }
            }
            // JSON cannot represent nonfinite numbers; test only representable plain fields.
            if latitude.is_finite() && longitude.is_finite() {
                let fields = json!({"locationLatitude": {"value": latitude}, "locationLongitude": {"value": longitude}});
                let metadata = super::super::metadata::extract(&json!({}), &fields);
                if metadata.latitude.is_some() || metadata.longitude.is_some() {
                    accepted.push(format!("plain: {latitude:?}, {longitude:?}"));
                }
            }
        }
        assert!(
            accepted.is_empty(),
            "invalid coordinates reached canonical metadata: {accepted:?}"
        );
    }

    #[test]
    fn tactical_gps_valid_boundaries_remain_valid() {
        for (latitude, longitude) in [(0.0, 0.0), (90.0, 180.0), (-90.0, -180.0)] {
            for fields in [
                json!({"locationV2Enc": tactical_location_entry(latitude, longitude, 0.0)}),
                json!({"locationEnc": tactical_location_entry(latitude, longitude, 0.0)}),
                json!({"locationLatitude": {"value": latitude}, "locationLongitude": {"value": longitude}}),
            ] {
                let metadata = super::super::metadata::extract(&json!({}), &fields);
                assert_eq!(metadata.latitude, Some(latitude));
                assert_eq!(metadata.longitude, Some(longitude));
            }
        }
    }

    #[test]
    fn tactical_gps_invalid_v2_does_not_override_valid_fallback() {
        let fields = json!({
            "locationV2Enc": tactical_location_entry(91.0, 2.0, 3.0),
            "locationEnc": tactical_location_entry(10.0, 20.0, 30.0),
            "locationLatitude": {"value": 40.0},
            "locationLongitude": {"value": 50.0},
        });
        let metadata = super::super::metadata::extract(&json!({}), &fields);
        // Invalid V2 must fall through to the first valid candidate.
        assert_eq!(
            (metadata.latitude, metadata.longitude, metadata.altitude),
            (Some(10.0), Some(20.0), Some(30.0))
        );
    }

    #[test]
    fn tactical_gps_nonfinite_altitude_does_not_enter_extracted_metadata() {
        for altitude in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            for key in ["locationV2Enc", "locationEnc"] {
                let fields = json!({
                    key: tactical_location_entry(1.0, 2.0, altitude),
                    "locationLatitude": {"value": 40.0},
                    "locationLongitude": {"value": 50.0},
                });
                let metadata = super::super::metadata::extract(&json!({}), &fields);
                assert_eq!(metadata.latitude, Some(1.0));
                assert_eq!(metadata.longitude, Some(2.0));
                assert_eq!(metadata.altitude, None);
            }
        }
    }

    #[test]
    fn gps_extraction_selects_first_valid_coordinate_pair() {
        let malformed = json!({"value": "!!not-base64!!", "type": "ENCRYPTED_BYTES"});
        for (v2, v1, expected) in [
            (
                tactical_location_entry(1.0, 2.0, -3.0),
                tactical_location_entry(10.0, 20.0, 30.0),
                (Some(1.0), Some(2.0), Some(-3.0)),
            ),
            (
                tactical_location_entry(1.0, 2.0, f64::NAN),
                tactical_location_entry(10.0, 20.0, 30.0),
                (Some(1.0), Some(2.0), None),
            ),
            (
                tactical_location_entry(1.0, 181.0, 3.0),
                tactical_location_entry(-91.0, 20.0, 30.0),
                (Some(40.0), Some(50.0), None),
            ),
            (
                malformed.clone(),
                tactical_location_entry(10.0, 20.0, 30.0),
                (Some(10.0), Some(20.0), Some(30.0)),
            ),
            (malformed.clone(), malformed, (Some(40.0), Some(50.0), None)),
            (
                Value::Null,
                tactical_location_entry(10.0, f64::NAN, 30.0),
                (Some(40.0), Some(50.0), None),
            ),
        ] {
            let fields = json!({
                "locationV2Enc": v2,
                "locationEnc": v1,
                "locationLatitude": {"value": 40.0},
                "locationLongitude": {"value": 50.0},
            });
            let metadata = super::super::metadata::extract(&json!({}), &fields);
            assert_eq!(
                (metadata.latitude, metadata.longitude, metadata.altitude),
                expected,
                "fields: {fields}"
            );
        }
    }

    #[test]
    fn gps_extraction_omits_location_when_no_valid_pair_exists() {
        for plain in [
            json!({"locationLatitude": {"value": 91.0}, "locationLongitude": {"value": 50.0}}),
            json!({"locationLatitude": {"value": 40.0}}),
            json!({"locationLongitude": {"value": 50.0}}),
            json!({"locationLatitude": {"value": "40"}, "locationLongitude": {"value": 50.0}}),
        ] {
            let mut fields = plain;
            fields["locationV2Enc"] = tactical_location_entry(f64::INFINITY, 2.0, 3.0);
            fields["locationEnc"] = tactical_location_entry(10.0, -181.0, 30.0);
            let metadata = super::super::metadata::extract(&json!({}), &fields);
            assert_eq!(
                (metadata.latitude, metadata.longitude, metadata.altitude),
                (None, None, None)
            );
        }
    }
}
