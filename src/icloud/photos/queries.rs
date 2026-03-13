use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{json, Value};

use super::types::{AssetItemType, AssetVersionSize};

/// CloudKit field names requested in every query — must include all fields
/// needed for filename resolution, version URLs, checksums, and metadata.
/// Matches the Python `DESIRED_KEYS` list for API compatibility.
pub(crate) const DESIRED_KEYS: &[&str] = &[
    "resJPEGFullWidth",
    "resJPEGFullHeight",
    "resJPEGFullFileType",
    "resJPEGFullFingerprint",
    "resJPEGFullRes",
    "resJPEGLargeWidth",
    "resJPEGLargeHeight",
    "resJPEGLargeFileType",
    "resJPEGLargeFingerprint",
    "resJPEGLargeRes",
    "resJPEGMedWidth",
    "resJPEGMedHeight",
    "resJPEGMedFileType",
    "resJPEGMedFingerprint",
    "resJPEGMedRes",
    "resJPEGThumbWidth",
    "resJPEGThumbHeight",
    "resJPEGThumbFileType",
    "resJPEGThumbFingerprint",
    "resJPEGThumbRes",
    "resVidFullWidth",
    "resVidFullHeight",
    "resVidFullFileType",
    "resVidFullFingerprint",
    "resVidFullRes",
    "resVidMedWidth",
    "resVidMedHeight",
    "resVidMedFileType",
    "resVidMedFingerprint",
    "resVidMedRes",
    "resVidSmallWidth",
    "resVidSmallHeight",
    "resVidSmallFileType",
    "resVidSmallFingerprint",
    "resVidSmallRes",
    "resSidecarWidth",
    "resSidecarHeight",
    "resSidecarFileType",
    "resSidecarFingerprint",
    "resSidecarRes",
    "itemType",
    "dataClassType",
    "filenameEnc",
    "originalOrientation",
    "resOriginalWidth",
    "resOriginalHeight",
    "resOriginalFileType",
    "resOriginalFingerprint",
    "resOriginalRes",
    "resOriginalAltWidth",
    "resOriginalAltHeight",
    "resOriginalAltFileType",
    "resOriginalAltFingerprint",
    "resOriginalAltRes",
    "resOriginalVidComplWidth",
    "resOriginalVidComplHeight",
    "resOriginalVidComplFileType",
    "resOriginalVidComplFingerprint",
    "resOriginalVidComplRes",
    "isDeleted",
    "isExpunged",
    "dateExpunged",
    "remappedRef",
    "recordName",
    "recordType",
    "recordChangeTag",
    "masterRef",
    "adjustmentRenderType",
    "assetDate",
    "addedDate",
    "isFavorite",
    "isHidden",
    "orientation",
    "duration",
    "assetSubtype",
    "assetSubtypeV2",
    "assetHDRType",
    "burstFlags",
    "burstFlagsExt",
    "burstId",
    "captionEnc",
    "locationEnc",
    "locationV2Enc",
    "locationLatitude",
    "locationLongitude",
    "adjustmentType",
    "timeZoneOffset",
    "vidComplDurValue",
    "vidComplDurScale",
    "vidComplDispValue",
    "vidComplDispScale",
    "keywordsEnc",
    "extendedDescEnc",
    "adjustedMediaMetaDataEnc",
    "adjustmentSimpleDataEnc",
    "vidComplVisibilityState",
    "customRenderedValue",
    "containerId",
    "itemId",
    "position",
    "isKeyAsset",
];

pub(crate) static DESIRED_KEYS_VALUES: LazyLock<Vec<Value>> = LazyLock::new(|| {
    DESIRED_KEYS
        .iter()
        .map(|k| Value::String((*k).to_string()))
        .collect()
});

pub(crate) fn item_type_from_str(s: &str) -> Option<AssetItemType> {
    match s {
        "public.heic"
        | "public.heif"
        | "public.jpeg"
        | "public.png"
        | "com.adobe.raw-image"
        | "com.canon.cr2-raw-image"
        | "com.canon.crw-raw-image"
        | "com.sony.arw-raw-image"
        | "com.fuji.raw-image"
        | "com.panasonic.rw2-raw-image"
        | "com.nikon.nrw-raw-image"
        | "com.pentax.raw-image"
        | "com.nikon.raw-image"
        | "com.olympus.raw-image"
        | "com.canon.cr3-raw-image"
        | "com.olympus.or-raw-image"
        | "org.webmproject.webp" => Some(AssetItemType::Image),
        "com.apple.quicktime-movie" => Some(AssetItemType::Movie),
        _ => None,
    }
}

/// Maps logical version sizes to CloudKit field prefixes.
/// The field prefix + "Res" gives the resource field (e.g., "resOriginalRes").
pub(crate) const PHOTO_VERSION_LOOKUP: &[(AssetVersionSize, &str)] = &[
    (AssetVersionSize::Original, "resOriginal"),
    (AssetVersionSize::Alternative, "resOriginalAlt"),
    (AssetVersionSize::Medium, "resJPEGMed"),
    (AssetVersionSize::Thumb, "resJPEGThumb"),
    (AssetVersionSize::Adjusted, "resJPEGFull"),
    (AssetVersionSize::LiveOriginal, "resOriginalVidCompl"),
    (AssetVersionSize::LiveMedium, "resVidMed"),
    (AssetVersionSize::LiveThumb, "resVidSmall"),
];

pub(crate) const VIDEO_VERSION_LOOKUP: &[(AssetVersionSize, &str)] = &[
    (AssetVersionSize::Original, "resOriginal"),
    (AssetVersionSize::Medium, "resVidMed"),
    (AssetVersionSize::Thumb, "resVidSmall"),
];

pub(crate) fn encode_params(params: &HashMap<String, Value>) -> String {
    use std::borrow::Cow;
    let mut pairs: Vec<String> = params
        .iter()
        .map(|(k, v)| {
            let val: Cow<'_, str> = match v {
                Value::String(s) => Cow::Borrowed(s.as_str()),
                Value::Bool(b) => Cow::Owned(b.to_string()),
                Value::Number(n) => Cow::Owned(n.to_string()),
                other => Cow::Owned(other.to_string()),
            };
            format!("{}={}", urlencoding::encode(k), urlencoding::encode(&val))
        })
        .collect();
    pairs.sort();
    pairs.join("&")
}

/// Build the request body for `/changes/database`.
/// If `sync_token` is `None`, requests full zone listing (bootstrap).
/// If `Some`, requests only zones with changes since the token.
pub(crate) fn build_changes_database_request(sync_token: Option<&str>) -> Value {
    match sync_token {
        Some(token) => json!({"syncToken": token}),
        None => json!({}),
    }
}

/// Build the request body for `/changes/zone`.
/// If `sync_token` is `None`, requests full history enumeration.
/// If `Some`, requests changes since the token.
///
/// IMPORTANT: `None` means full history. Empty string means caught-up (returns 0 records).
/// Always use `Option<&str>` with `None`, never empty string, for "no token".
pub(crate) fn build_changes_zone_request(
    zone_id: &Value,
    sync_token: Option<&str>,
    results_limit: u32,
) -> Value {
    let mut zone_entry = json!({
        "zoneID": zone_id,
        "resultsLimit": results_limit,
    });
    if let Some(token) = sync_token {
        zone_entry["syncToken"] = json!(token);
    }
    json!({"zones": [zone_entry]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_item_type_from_str_images() {
        assert_eq!(
            item_type_from_str("public.jpeg"),
            Some(AssetItemType::Image)
        );
        assert_eq!(
            item_type_from_str("public.heic"),
            Some(AssetItemType::Image)
        );
        assert_eq!(item_type_from_str("public.png"), Some(AssetItemType::Image));
        assert_eq!(
            item_type_from_str("com.canon.cr2-raw-image"),
            Some(AssetItemType::Image)
        );
    }

    #[test]
    fn test_item_type_from_str_webp() {
        assert_eq!(
            item_type_from_str("org.webmproject.webp"),
            Some(AssetItemType::Image)
        );
    }

    #[test]
    fn test_item_type_from_str_movie() {
        assert_eq!(
            item_type_from_str("com.apple.quicktime-movie"),
            Some(AssetItemType::Movie)
        );
    }

    #[test]
    fn test_item_type_from_str_unknown() {
        assert_eq!(item_type_from_str("unknown/type"), None);
        assert_eq!(item_type_from_str(""), None);
    }

    #[test]
    fn test_encode_params_basic() {
        let mut params = HashMap::new();
        params.insert("key".to_string(), Value::String("value".to_string()));
        let encoded = encode_params(&params);
        assert_eq!(encoded, "key=value");
    }

    #[test]
    fn test_encode_params_special_chars() {
        let mut params = HashMap::new();
        params.insert("q".to_string(), Value::String("hello world".to_string()));
        let encoded = encode_params(&params);
        assert_eq!(encoded, "q=hello%20world");
    }

    #[test]
    fn test_encode_params_bool() {
        let mut params = HashMap::new();
        params.insert("flag".to_string(), Value::Bool(true));
        let encoded = encode_params(&params);
        assert_eq!(encoded, "flag=true");
    }

    #[test]
    fn test_desired_keys_not_empty() {
        assert!(!DESIRED_KEYS.is_empty());
        assert!(DESIRED_KEYS.contains(&"recordName"));
        assert!(DESIRED_KEYS.contains(&"filenameEnc"));
    }

    #[test]
    fn test_desired_keys_values_matches_keys() {
        let values = &*DESIRED_KEYS_VALUES;
        assert!(!values.is_empty(), "DESIRED_KEYS_VALUES must not be empty");
        assert_eq!(values.len(), DESIRED_KEYS.len());
        for (key, val) in DESIRED_KEYS.iter().zip(values.iter()) {
            assert_eq!(val.as_str().unwrap(), *key);
        }
    }

    #[test]
    fn test_encode_params_number() {
        let mut params = HashMap::new();
        params.insert("count".to_string(), Value::Number(42.into()));
        let encoded = encode_params(&params);
        assert_eq!(encoded, "count=42");
    }

    #[test]
    fn test_encode_params_multiple_sorted() {
        let mut params = HashMap::new();
        params.insert("z".to_string(), Value::String("last".to_string()));
        params.insert("a".to_string(), Value::String("first".to_string()));
        let encoded = encode_params(&params);
        assert_eq!(encoded, "a=first&z=last");
    }

    #[test]
    fn test_encode_params_empty() {
        let params = HashMap::new();
        let encoded = encode_params(&params);
        assert_eq!(encoded, "");
    }

    #[test]
    fn test_item_type_all_raw_images() {
        let raw_types = [
            "com.adobe.raw-image",
            "com.canon.cr2-raw-image",
            "com.canon.crw-raw-image",
            "com.sony.arw-raw-image",
            "com.fuji.raw-image",
            "com.panasonic.rw2-raw-image",
            "com.nikon.nrw-raw-image",
            "com.pentax.raw-image",
            "com.nikon.raw-image",
            "com.olympus.raw-image",
            "com.canon.cr3-raw-image",
            "com.olympus.or-raw-image",
        ];
        for raw in raw_types {
            assert_eq!(
                item_type_from_str(raw),
                Some(AssetItemType::Image),
                "{raw} should be Image"
            );
        }
    }

    #[test]
    fn test_photo_version_lookup_contains_all_sizes() {
        let sizes: Vec<AssetVersionSize> = PHOTO_VERSION_LOOKUP.iter().map(|(s, _)| *s).collect();
        assert!(sizes.contains(&AssetVersionSize::Original));
        assert!(sizes.contains(&AssetVersionSize::Alternative));
        assert!(sizes.contains(&AssetVersionSize::Medium));
        assert!(sizes.contains(&AssetVersionSize::Thumb));
        assert!(sizes.contains(&AssetVersionSize::Adjusted));
        assert!(sizes.contains(&AssetVersionSize::LiveOriginal));
        assert!(sizes.contains(&AssetVersionSize::LiveMedium));
        assert!(sizes.contains(&AssetVersionSize::LiveThumb));
    }

    #[test]
    fn test_video_version_lookup_has_original() {
        let sizes: Vec<AssetVersionSize> = VIDEO_VERSION_LOOKUP.iter().map(|(s, _)| *s).collect();
        assert!(sizes.contains(&AssetVersionSize::Original));
        assert!(sizes.contains(&AssetVersionSize::Medium));
        assert!(sizes.contains(&AssetVersionSize::Thumb));
    }

    #[test]
    fn test_desired_keys_contains_critical_fields() {
        // Fields essential for the download pipeline
        let critical = [
            "resOriginalRes",
            "resOriginalFingerprint",
            "resOriginalFileType",
            "itemType",
            "filenameEnc",
            "assetDate",
            "addedDate",
            "isDeleted",
        ];
        for field in critical {
            assert!(
                DESIRED_KEYS.contains(&field),
                "Missing critical field: {field}"
            );
        }
    }

    #[test]
    fn test_build_changes_database_request_none() {
        let body = build_changes_database_request(None);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn test_build_changes_database_request_some() {
        let body = build_changes_database_request(Some("token123"));
        assert_eq!(body, json!({"syncToken": "token123"}));
    }

    #[test]
    fn test_build_changes_zone_request_without_token() {
        let zone_id = json!({"zoneName": "PrimarySync", "zoneType": "DEFAULT_ZONE"});
        let body = build_changes_zone_request(&zone_id, None, 200);
        let zones = body["zones"].as_array().unwrap();
        assert_eq!(zones.len(), 1);
        let entry = &zones[0];
        assert_eq!(entry["zoneID"], zone_id);
        assert_eq!(entry["resultsLimit"], 200);
        assert!(entry.get("syncToken").is_none());
    }

    #[test]
    fn test_build_changes_zone_request_with_token() {
        let zone_id = json!({"zoneName": "PrimarySync"});
        let body = build_changes_zone_request(&zone_id, Some("tok_abc"), 200);
        let zones = body["zones"].as_array().unwrap();
        assert_eq!(zones.len(), 1);
        let entry = &zones[0];
        assert_eq!(entry["zoneID"], zone_id);
        assert_eq!(entry["resultsLimit"], 200);
        assert_eq!(entry["syncToken"], "tok_abc");
    }

    #[test]
    fn test_build_changes_zone_request_custom_limit() {
        let zone_id = json!({"zoneName": "SharedSync-123"});
        let body = build_changes_zone_request(&zone_id, None, 50);
        let entry = &body["zones"][0];
        assert_eq!(entry["resultsLimit"], 50);
    }
}
