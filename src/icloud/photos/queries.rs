use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::Value;

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
        | "com.olympus.or-raw-image" => Some(AssetItemType::Image),
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
}
