//! Decoders for iCloud CloudKit `*Enc` fields.
//!
//! Despite the `Enc` suffix, these fields are **not encrypted** for non-ADP
//! accounts. Apple's servers decrypt them before returning them via the web
//! API. What arrives is base64-encoded plaintext (UTF-8 strings) or binary
//! plist data. See `.scratch/metadata-plan.md` "iCloud `*Enc` field decoding".

use base64::Engine;
use serde_json::Value;
use tracing::warn;

/// Decode a CloudKit `ENCRYPTED_BYTES` or `STRING` field to a UTF-8 string.
///
/// Handles both type variants that Apple uses for text fields like
/// `captionEnc`, `extendedDescEnc`, and `filenameEnc`. Returns `None`
/// for null/missing fields or decoding failures.
pub(crate) fn decode_enc_string(field: &Value) -> Option<String> {
    if field.is_null() {
        return None;
    }
    let value = field["value"].as_str()?;
    if value.is_empty() {
        return None;
    }
    let enc_type = field["type"].as_str().unwrap_or("STRING");
    match enc_type {
        "STRING" => Some(value.to_string()),
        "ENCRYPTED_BYTES" => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(value)
                .ok()?;
            let s = String::from_utf8(decoded).ok()?;
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        other => {
            warn!(enc_type = %other, "Unsupported *Enc field type");
            None
        }
    }
}

/// Decode `keywordsEnc` from base64-encoded binary plist to a list of strings.
///
/// The plist contains an array of strings (keyword tags). Returns an empty
/// `Vec` on null/missing fields, decoding errors, or unexpected plist structure.
pub(crate) fn decode_keywords_plist(field: &Value) -> Vec<String> {
    let bytes = match decode_enc_bytes(field) {
        Some(b) => b,
        None => return Vec::new(),
    };

    match plist::from_bytes::<Vec<String>>(&bytes) {
        Ok(keywords) => keywords,
        Err(e) => {
            warn!(error = %e, "Failed to parse keywordsEnc plist");
            Vec::new()
        }
    }
}

/// Decode GPS location from `locationEnc` (binary plist) or fall back to
/// plain `locationLatitude`/`locationLongitude` fields.
///
/// Returns `(latitude, longitude, altitude)`. Any component may be `None`.
pub(crate) fn decode_location(
    master_fields: &Value,
    asset_fields: &Value,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    // Try locationEnc plist first (has altitude)
    if let Some(loc) = decode_location_plist(&asset_fields["locationEnc"]) {
        return loc;
    }
    // Fall back to locationV2Enc
    if let Some(loc) = decode_location_plist(&asset_fields["locationV2Enc"]) {
        return loc;
    }
    // Fall back to plain fields on the master record (no altitude)
    let lat = decode_plain_float(&master_fields["locationLatitude"]);
    let lon = decode_plain_float(&master_fields["locationLongitude"]);
    if lat.is_some() || lon.is_some() {
        return (lat, lon, None);
    }
    // Also check asset fields for plain coordinates
    let lat = decode_plain_float(&asset_fields["locationLatitude"]);
    let lon = decode_plain_float(&asset_fields["locationLongitude"]);
    (lat, lon, None)
}

/// Decode a `locationEnc` or `locationV2Enc` binary plist.
///
/// Expected plist structure: dictionary with `lat`, `lng`, `alt` keys (all f64).
fn decode_location_plist(field: &Value) -> Option<(Option<f64>, Option<f64>, Option<f64>)> {
    let bytes = decode_enc_bytes(field)?;

    let dict: plist::Dictionary = match plist::from_bytes(&bytes) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "Failed to parse locationEnc plist");
            return None;
        }
    };

    let lat = dict.get("lat").and_then(plist::Value::as_real);
    let lon = dict.get("lng").and_then(plist::Value::as_real);
    let alt = dict.get("alt").and_then(plist::Value::as_real);

    // Only return if we got at least latitude and longitude
    if lat.is_some() && lon.is_some() {
        Some((lat, lon, alt))
    } else {
        None
    }
}

/// Extract raw bytes from a CloudKit `ENCRYPTED_BYTES` field (base64 decode).
fn decode_enc_bytes(field: &Value) -> Option<Vec<u8>> {
    if field.is_null() {
        return None;
    }
    let value = field["value"].as_str()?;
    if value.is_empty() {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

/// Read a plain numeric CloudKit field as f64.
fn decode_plain_float(field: &Value) -> Option<f64> {
    field["value"].as_f64()
}

/// Read a plain numeric CloudKit field as i32.
pub(crate) fn decode_plain_i32(field: &Value) -> Option<i32> {
    field["value"].as_i64().and_then(|v| i32::try_from(v).ok())
}

/// Read a plain numeric CloudKit field as i64.
pub(crate) fn decode_plain_i64(field: &Value) -> Option<i64> {
    field["value"].as_i64()
}

/// Read a plain string CloudKit field.
pub(crate) fn decode_plain_string(field: &Value) -> Option<String> {
    field["value"].as_str().map(String::from)
}

/// Read a plain boolean-as-integer CloudKit field (0/1).
pub(crate) fn decode_plain_bool(field: &Value) -> bool {
    field["value"].as_i64() == Some(1)
}

/// Extract all available metadata from iCloud CloudKit master + asset fields.
///
/// Decodes `*Enc` fields, reads plain fields, and computes the metadata hash.
/// Returns an `AssetMetadata` with `source` set to `"icloud"`.
pub(crate) fn extract_metadata(
    master_fields: &Value,
    asset_fields: &Value,
) -> crate::state::types::AssetMetadata {
    let (latitude, longitude, altitude) = decode_location(master_fields, asset_fields);

    let mut meta = crate::state::types::AssetMetadata {
        source: "icloud".to_string(),
        is_favorite: decode_plain_bool(&asset_fields["isFavorite"]),
        is_hidden: decode_plain_bool(&asset_fields["isHidden"]),
        is_deleted: decode_plain_bool(&asset_fields["isDeleted"]),
        is_archived: false, // iCloud doesn't have an archive concept
        orientation: decode_plain_i32(&asset_fields["orientation"])
            .or_else(|| decode_plain_i32(&master_fields["originalOrientation"])),
        duration_secs: asset_fields["duration"]["value"].as_f64(),
        timezone_offset: decode_plain_i32(&asset_fields["timeZoneOffset"]),
        latitude,
        longitude,
        altitude,
        title: decode_enc_string(&asset_fields["captionEnc"]),
        description: decode_enc_string(&asset_fields["extendedDescEnc"]),
        keywords: decode_keywords_plist(&asset_fields["keywordsEnc"]),
        burst_id: decode_plain_string(&asset_fields["burstId"]),
        media_subtype: decode_plain_i32(&asset_fields["assetSubtype"]).map(|v| v.to_string()),
        deleted_at: decode_plain_i64(&asset_fields["dateExpunged"]),
        // Fields not yet populated from iCloud
        rating: None,
        width: None,
        height: None,
        modified_at: None,
        provider_data: None,
        metadata_hash: None,
    };

    meta.metadata_hash = Some(meta.compute_hash());
    meta
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- decode_enc_string ----

    #[test]
    fn string_type_returns_value() {
        let field = json!({"value": "Hello World", "type": "STRING"});
        assert_eq!(decode_enc_string(&field), Some("Hello World".to_string()));
    }

    #[test]
    fn encrypted_bytes_base64_utf8() {
        // "Hello" in base64 = "SGVsbG8="
        let field = json!({"value": "SGVsbG8=", "type": "ENCRYPTED_BYTES"});
        assert_eq!(decode_enc_string(&field), Some("Hello".to_string()));
    }

    #[test]
    fn encrypted_bytes_empty_returns_none() {
        let field = json!({"value": "", "type": "ENCRYPTED_BYTES"});
        assert_eq!(decode_enc_string(&field), None);
    }

    #[test]
    fn null_field_returns_none() {
        assert_eq!(decode_enc_string(&Value::Null), None);
    }

    #[test]
    fn missing_value_returns_none() {
        let field = json!({"type": "STRING"});
        assert_eq!(decode_enc_string(&field), None);
    }

    #[test]
    fn invalid_base64_returns_none() {
        let field = json!({"value": "not-valid-base64!!!", "type": "ENCRYPTED_BYTES"});
        assert_eq!(decode_enc_string(&field), None);
    }

    #[test]
    fn invalid_utf8_returns_none() {
        // \xFF\xFE is not valid UTF-8. Base64 of [0xFF, 0xFE] = "//4="
        let field = json!({"value": "//4=", "type": "ENCRYPTED_BYTES"});
        assert_eq!(decode_enc_string(&field), None);
    }

    #[test]
    fn default_type_is_string() {
        let field = json!({"value": "test caption"});
        assert_eq!(decode_enc_string(&field), Some("test caption".to_string()));
    }

    // ---- decode_keywords_plist ----

    #[test]
    fn keywords_plist_decodes_string_array() {
        let keywords = vec!["sunset", "beach", "vacation"];
        let plist_bytes = plist_to_bytes(&keywords);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&plist_bytes);

        let field = json!({"value": b64, "type": "ENCRYPTED_BYTES"});
        let result = decode_keywords_plist(&field);
        assert_eq!(result, vec!["sunset", "beach", "vacation"]);
    }

    #[test]
    fn keywords_plist_empty_array() {
        let keywords: Vec<&str> = vec![];
        let plist_bytes = plist_to_bytes(&keywords);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&plist_bytes);

        let field = json!({"value": b64, "type": "ENCRYPTED_BYTES"});
        assert!(decode_keywords_plist(&field).is_empty());
    }

    #[test]
    fn keywords_null_returns_empty() {
        assert!(decode_keywords_plist(&Value::Null).is_empty());
    }

    #[test]
    fn keywords_invalid_plist_returns_empty() {
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"not a plist");
        let field = json!({"value": b64, "type": "ENCRYPTED_BYTES"});
        assert!(decode_keywords_plist(&field).is_empty());
    }

    // ---- decode_location ----

    #[test]
    fn location_from_plist() {
        let mut dict = plist::Dictionary::new();
        dict.insert("lat".to_string(), plist::Value::Real(37.7749));
        dict.insert("lng".to_string(), plist::Value::Real(-122.4194));
        dict.insert("alt".to_string(), plist::Value::Real(10.5));

        let plist_bytes = plist_dict_to_bytes(&dict);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&plist_bytes);

        let asset_fields = json!({"locationEnc": {"value": b64, "type": "ENCRYPTED_BYTES"}});
        let master_fields = json!({});

        let (lat, lon, alt) = decode_location(&master_fields, &asset_fields);
        assert!((lat.unwrap() - 37.7749).abs() < 1e-10);
        assert!((lon.unwrap() - (-122.4194)).abs() < 1e-10);
        assert!((alt.unwrap() - 10.5).abs() < 1e-10);
    }

    #[test]
    fn location_falls_back_to_plain_fields() {
        let asset_fields = json!({});
        let master_fields = json!({
            "locationLatitude": {"value": 40.7128},
            "locationLongitude": {"value": -74.0060}
        });

        let (lat, lon, alt) = decode_location(&master_fields, &asset_fields);
        assert!((lat.unwrap() - 40.7128).abs() < 1e-10);
        assert!((lon.unwrap() - (-74.0060)).abs() < 1e-10);
        assert!(alt.is_none());
    }

    #[test]
    fn location_all_missing_returns_none() {
        let (lat, lon, alt) = decode_location(&json!({}), &json!({}));
        assert!(lat.is_none());
        assert!(lon.is_none());
        assert!(alt.is_none());
    }

    #[test]
    fn location_plist_without_altitude() {
        let mut dict = plist::Dictionary::new();
        dict.insert("lat".to_string(), plist::Value::Real(51.5074));
        dict.insert("lng".to_string(), plist::Value::Real(-0.1278));

        let plist_bytes = plist_dict_to_bytes(&dict);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&plist_bytes);

        let asset_fields = json!({"locationEnc": {"value": b64, "type": "ENCRYPTED_BYTES"}});
        let (lat, lon, alt) = decode_location(&json!({}), &asset_fields);
        assert!(lat.is_some());
        assert!(lon.is_some());
        assert!(alt.is_none());
    }

    // ---- plain field helpers ----

    #[test]
    fn plain_i32_valid() {
        assert_eq!(decode_plain_i32(&json!({"value": 6})), Some(6));
    }

    #[test]
    fn plain_i32_null() {
        assert_eq!(decode_plain_i32(&Value::Null), None);
    }

    #[test]
    fn plain_i64_valid() {
        assert_eq!(
            decode_plain_i64(&json!({"value": 1618000000})),
            Some(1618000000)
        );
    }

    #[test]
    fn plain_string_valid() {
        assert_eq!(
            decode_plain_string(&json!({"value": "abc"})),
            Some("abc".to_string())
        );
    }

    #[test]
    fn plain_string_null() {
        assert_eq!(decode_plain_string(&Value::Null), None);
    }

    #[test]
    fn plain_bool_true() {
        assert!(decode_plain_bool(&json!({"value": 1})));
    }

    #[test]
    fn plain_bool_false_on_zero() {
        assert!(!decode_plain_bool(&json!({"value": 0})));
    }

    #[test]
    fn plain_bool_false_on_null() {
        assert!(!decode_plain_bool(&Value::Null));
    }

    // ---- test helpers ----

    fn plist_to_bytes<T: serde::Serialize>(value: &T) -> Vec<u8> {
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, value).unwrap();
        buf
    }

    fn plist_dict_to_bytes(dict: &plist::Dictionary) -> Vec<u8> {
        let mut buf = Vec::new();
        plist::to_writer_binary(&mut buf, dict).unwrap();
        buf
    }
}
