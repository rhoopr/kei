//! Extract provider-agnostic metadata from iCloud CloudKit records.
//!
//! Maps iCloud-specific fields (e.g. `isFavorite`, `captionEnc`, `locationEnc`,
//! `assetSubtype`) into the canonical `AssetMetadata` schema. Provider-specific
//! fields that don't fit the canonical schema are preserved verbatim in
//! `provider_data` so that invariant 4 (capture everything available) holds.

use serde_json::{json, Value};

use crate::state::AssetMetadata;

use super::asset::f64_to_millis_datetime;
use super::enc;

/// Provider identifier stored on every iCloud-sourced asset record.
pub const SOURCE: &str = "icloud";

/// Fields whose raw values we preserve in `provider_data` for fidelity to the
/// iCloud record even when they don't map cleanly into the canonical schema.
const PROVIDER_DATA_FIELDS: &[&str] = &[
    "remappedRef",
    "recordChangeTag",
    "dataClassType",
    "burstFlags",
    "burstFlagsExt",
    "assetHDRType",
    "adjustmentRenderType",
    "adjustmentType",
    "adjustmentSimpleDataEnc",
    "adjustedMediaMetaDataEnc",
    "vidComplDurValue",
    "vidComplDurScale",
    "vidComplDispValue",
    "vidComplDispScale",
    "vidComplVisibilityState",
    "customRenderedValue",
    "containerId",
    "itemId",
    "position",
    "isKeyAsset",
    "assetSubtype",
    "assetSubtypeV2",
];

/// Extract `AssetMetadata` from an iCloud CPLMaster + CPLAsset record pair.
///
/// `master_fields` is the `fields` object of the CPLMaster record; `asset_fields`
/// is the `fields` object of the CPLAsset record. Most metadata lives on the
/// asset record; the master record is consulted as fallback.
pub fn extract(master_fields: &Value, asset_fields: &Value) -> AssetMetadata {
    let mut meta = AssetMetadata {
        source: Some(crate::string_interner::intern(SOURCE)),
        ..AssetMetadata::default()
    };

    meta.is_favorite = bool_field(asset_fields, "isFavorite").unwrap_or(false);
    if meta.is_favorite {
        // Apple uses a boolean favorite flag. Mapping to a 5-star rating lets
        // downstream consumers (Immich, XMP sidecars) render favorites
        // consistently without special-casing iCloud.
        meta.rating = Some(5);
    }
    meta.is_hidden = bool_field(asset_fields, "isHidden").unwrap_or(false);
    meta.is_deleted = bool_field(asset_fields, "isDeleted").unwrap_or(false)
        || bool_field(asset_fields, "isExpunged").unwrap_or(false);
    meta.deleted_at = f64_field(asset_fields, "dateExpunged").and_then(f64_to_millis_datetime);

    if let Some(loc) = enc::decode_location_with_fallback(asset_fields) {
        meta.latitude = Some(loc.latitude);
        meta.longitude = Some(loc.longitude);
        meta.altitude = loc.altitude;
    }

    meta.orientation = u64_field(asset_fields, "orientation")
        .or_else(|| u64_field(master_fields, "originalOrientation"))
        .and_then(|v| u8::try_from(v).ok());

    meta.duration_secs = f64_field(asset_fields, "duration");
    meta.timezone_offset =
        i64_field(asset_fields, "timeZoneOffset").and_then(|v| i32::try_from(v).ok());

    meta.width = u64_field(asset_fields, "resOriginalWidth")
        .or_else(|| u64_field(master_fields, "resOriginalWidth"))
        .and_then(|v| u32::try_from(v).ok());
    meta.height = u64_field(asset_fields, "resOriginalHeight")
        .or_else(|| u64_field(master_fields, "resOriginalHeight"))
        .and_then(|v| u32::try_from(v).ok());

    meta.title = enc::decode_string(asset_fields, "captionEnc");
    meta.description = enc::decode_string(asset_fields, "extendedDescEnc");

    if let Some(keywords) = enc::decode_keywords(asset_fields) {
        if !keywords.is_empty() {
            meta.keywords = serde_json::to_string(&keywords).ok();
        }
    }

    meta.media_subtype = map_media_subtype(asset_fields);
    meta.burst_id = string_field(asset_fields, "burstId");

    meta.provider_data = collect_provider_data(master_fields, asset_fields);
    meta.refresh_hash();
    meta
}

/// PHAssetMediaSubtype bit flags. Values match Apple's PhotoKit enum as
/// observed in CloudKit responses.
mod subtype {
    pub const PANORAMA: u64 = 1;
    pub const HDR: u64 = 1 << 1;
    pub const SCREENSHOT: u64 = 1 << 2;
    pub const LIVE_PHOTO: u64 = 1 << 3;
    pub const PORTRAIT: u64 = 1 << 4;
    pub const VIDEO_HIGH_FRAME_RATE: u64 = 1 << 17;
    pub const VIDEO_TIMELAPSE: u64 = 1 << 18;
}

/// Priority-ordered mapping: specific subtypes resolve first so a photo tagged
/// as both Portrait and HDR reports as Portrait.
const MEDIA_SUBTYPE_MAP: &[(u64, &str)] = &[
    (subtype::PORTRAIT, "portrait"),
    (subtype::LIVE_PHOTO, "live_photo"),
    (subtype::SCREENSHOT, "screenshot"),
    (subtype::HDR, "hdr"),
    (subtype::PANORAMA, "panorama"),
    (subtype::VIDEO_HIGH_FRAME_RATE, "slo_mo"),
    (subtype::VIDEO_TIMELAPSE, "timelapse"),
];

/// Apple's `assetSubtypeV2` / `assetSubtype` integer bit-flag enum mapped to
/// canonical string values. Unknown values leave `media_subtype` as `None`;
/// the raw integer is preserved in `provider_data` for downstream use.
fn map_media_subtype(fields: &Value) -> Option<String> {
    let raw = u64_field(fields, "assetSubtypeV2").or_else(|| u64_field(fields, "assetSubtype"))?;
    MEDIA_SUBTYPE_MAP
        .iter()
        .find(|(flag, _)| raw & flag != 0)
        .map(|(_, name)| (*name).into())
}

fn collect_provider_data(master_fields: &Value, asset_fields: &Value) -> Option<String> {
    let mut obj = serde_json::Map::new();
    for field in PROVIDER_DATA_FIELDS {
        if let Some(value) = extract_raw_value(asset_fields, field)
            .or_else(|| extract_raw_value(master_fields, field))
        {
            obj.insert((*field).to_string(), value);
        }
    }
    if obj.is_empty() {
        None
    } else {
        serde_json::to_string(&Value::Object(obj)).ok()
    }
}

fn extract_raw_value(fields: &Value, key: &str) -> Option<Value> {
    let entry = fields.get(key)?;
    if entry.is_null() {
        return None;
    }
    entry.get("value").cloned().map(|v| {
        // Keep value with type info when present, so consumers can tell a
        // STRING/ENCRYPTED_BYTES pair apart.
        if let Some(t) = entry.get("type") {
            json!({"value": v, "type": t})
        } else {
            v
        }
    })
}

fn bool_field(fields: &Value, key: &str) -> Option<bool> {
    let entry = fields.get(key)?.get("value")?;
    match entry {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => Some(n.as_i64().map(|v| v != 0).unwrap_or(false)),
        _ => None,
    }
}

fn f64_field(fields: &Value, key: &str) -> Option<f64> {
    fields.get(key)?.get("value")?.as_f64()
}

fn u64_field(fields: &Value, key: &str) -> Option<u64> {
    fields.get(key)?.get("value")?.as_u64()
}

fn i64_field(fields: &Value, key: &str) -> Option<i64> {
    fields.get(key)?.get("value")?.as_i64()
}

fn string_field(fields: &Value, key: &str) -> Option<String> {
    fields
        .get(key)?
        .get("value")?
        .as_str()
        .map(std::string::ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use plist::Value as PlistValue;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn bplist(value: PlistValue) -> Vec<u8> {
        let mut out = Vec::new();
        plist::to_writer_binary(&mut out, &value).unwrap();
        out
    }

    #[test]
    fn extract_sets_source_icloud() {
        let m = extract(&json!({}), &json!({}));
        assert_eq!(m.source.as_deref(), Some("icloud"));
    }

    #[test]
    fn extract_maps_favorite_to_rating_5() {
        let m = extract(&json!({}), &json!({"isFavorite": {"value": 1}}));
        assert!(m.is_favorite);
        assert_eq!(m.rating, Some(5));
    }

    #[test]
    fn extract_not_favorite_leaves_rating_none() {
        let m = extract(&json!({}), &json!({"isFavorite": {"value": 0}}));
        assert!(!m.is_favorite);
        assert_eq!(m.rating, None);
    }

    #[test]
    fn extract_hidden_and_deleted_flags() {
        let m = extract(
            &json!({}),
            &json!({
                "isHidden": {"value": 1},
                "isDeleted": {"value": 1},
                "dateExpunged": {"value": 1_700_000_000_000_f64},
            }),
        );
        assert!(m.is_hidden);
        assert!(m.is_deleted);
        assert!(m.deleted_at.is_some());
    }

    #[test]
    fn extract_is_expunged_sets_is_deleted() {
        let m = extract(&json!({}), &json!({"isExpunged": {"value": 1}}));
        assert!(m.is_deleted);
    }

    #[test]
    fn extract_location_from_plist() {
        let mut dict = plist::Dictionary::new();
        dict.insert("lat".into(), PlistValue::Real(10.0));
        dict.insert("lng".into(), PlistValue::Real(-20.0));
        dict.insert("alt".into(), PlistValue::Real(5.0));
        let bp = bplist(PlistValue::Dictionary(dict));
        let m = extract(
            &json!({}),
            &json!({"locationEnc": {"value": b64(&bp), "type": "ENCRYPTED_BYTES"}}),
        );
        assert_eq!(m.latitude, Some(10.0));
        assert_eq!(m.longitude, Some(-20.0));
        assert_eq!(m.altitude, Some(5.0));
    }

    #[test]
    fn extract_location_from_plain_fallback() {
        let m = extract(
            &json!({}),
            &json!({
                "locationLatitude": {"value": 1.0},
                "locationLongitude": {"value": 2.0},
            }),
        );
        assert_eq!(m.latitude, Some(1.0));
        assert_eq!(m.longitude, Some(2.0));
        assert_eq!(m.altitude, None);
    }

    #[test]
    fn extract_caption_and_description() {
        let m = extract(
            &json!({}),
            &json!({
                "captionEnc": {"value": "title", "type": "STRING"},
                "extendedDescEnc": {"value": "longer", "type": "STRING"},
            }),
        );
        assert_eq!(m.title.as_deref(), Some("title"));
        assert_eq!(m.description.as_deref(), Some("longer"));
    }

    #[test]
    fn extract_keywords_serializes_as_json_array() {
        let bp = bplist(PlistValue::Array(vec![
            PlistValue::String("a".into()),
            PlistValue::String("b".into()),
        ]));
        let m = extract(
            &json!({}),
            &json!({"keywordsEnc": {"value": b64(&bp), "type": "ENCRYPTED_BYTES"}}),
        );
        assert_eq!(m.keywords.as_deref(), Some(r#"["a","b"]"#));
    }

    #[test]
    fn extract_keywords_empty_stays_none() {
        let bp = bplist(PlistValue::Array(vec![]));
        let m = extract(
            &json!({}),
            &json!({"keywordsEnc": {"value": b64(&bp), "type": "ENCRYPTED_BYTES"}}),
        );
        assert_eq!(m.keywords, None);
    }

    #[test]
    fn extract_orientation_and_dimensions() {
        let m = extract(
            &json!({}),
            &json!({
                "orientation": {"value": 6},
                "resOriginalWidth": {"value": 4032},
                "resOriginalHeight": {"value": 3024},
            }),
        );
        assert_eq!(m.orientation, Some(6));
        assert_eq!(m.width, Some(4032));
        assert_eq!(m.height, Some(3024));
    }

    #[test]
    fn extract_duration_and_timezone() {
        let m = extract(
            &json!({}),
            &json!({
                "duration": {"value": 12.5},
                "timeZoneOffset": {"value": -28800},
            }),
        );
        assert_eq!(m.duration_secs, Some(12.5));
        assert_eq!(m.timezone_offset, Some(-28800));
    }

    #[test]
    fn extract_media_subtype_portrait() {
        let m = extract(&json!({}), &json!({"assetSubtypeV2": {"value": 16}}));
        assert_eq!(m.media_subtype.as_deref(), Some("portrait"));
    }

    #[test]
    fn extract_media_subtype_live_photo() {
        let m = extract(&json!({}), &json!({"assetSubtypeV2": {"value": 8}}));
        assert_eq!(m.media_subtype.as_deref(), Some("live_photo"));
    }

    #[test]
    fn extract_media_subtype_falls_back_to_v1() {
        let m = extract(&json!({}), &json!({"assetSubtype": {"value": 4}}));
        assert_eq!(m.media_subtype.as_deref(), Some("screenshot"));
    }

    #[test]
    fn extract_media_subtype_unknown_bit_is_none() {
        // Bit 5 (value 32) is not one of the canonical subtype flags.
        let m = extract(&json!({}), &json!({"assetSubtype": {"value": 32}}));
        assert_eq!(m.media_subtype, None);
    }

    #[test]
    fn extract_media_subtype_none_flags_is_none() {
        let m = extract(&json!({}), &json!({"assetSubtype": {"value": 0}}));
        assert_eq!(m.media_subtype, None);
    }

    #[test]
    fn extract_burst_id_plain() {
        let m = extract(&json!({}), &json!({"burstId": {"value": "burst_abc"}}));
        assert_eq!(m.burst_id.as_deref(), Some("burst_abc"));
    }

    #[test]
    fn extract_provider_data_contains_unmapped_fields() {
        let m = extract(
            &json!({}),
            &json!({
                "recordChangeTag": {"value": "tag42"},
                "containerId": {"value": "c1"},
                "isKeyAsset": {"value": 1},
            }),
        );
        let pd = m.provider_data.expect("provider_data should be populated");
        let parsed: Value = serde_json::from_str(&pd).unwrap();
        assert_eq!(parsed["recordChangeTag"], json!("tag42"));
        assert_eq!(parsed["containerId"], json!("c1"));
        assert_eq!(parsed["isKeyAsset"], json!(1));
    }

    #[test]
    fn extract_provider_data_none_when_no_unmapped_fields() {
        let m = extract(&json!({}), &json!({}));
        assert_eq!(m.provider_data, None);
    }

    #[test]
    fn extract_populates_metadata_hash() {
        let m = extract(&json!({}), &json!({"isFavorite": {"value": 1}}));
        assert!(m.metadata_hash.is_some());
    }
}
