use std::collections::HashMap;

use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use tracing::warn;

use super::queries::{item_type_from_str, PHOTO_VERSION_LOOKUP, VIDEO_VERSION_LOOKUP};
use super::types::{AssetItemType, AssetVersion, AssetVersionSize};

#[derive(Debug, Clone)]
pub struct PhotoAsset {
    master_record: Value,
    asset_record: Value,
}

impl PhotoAsset {
    pub fn new(master_record: Value, asset_record: Value) -> Self {
        Self {
            master_record,
            asset_record,
        }
    }

    /// The unique record name from the master record.
    pub fn id(&self) -> &str {
        self.master_record["recordName"]
            .as_str()
            .unwrap_or_else(|| {
                tracing::warn!("Missing expected field: recordName");
                ""
            })
    }

    /// Decode the filename from the `filenameEnc` field.
    /// Returns `None` when the field is absent.
    pub fn filename(&self) -> Option<String> {
        let fields = &self.master_record["fields"];
        let enc = &fields["filenameEnc"];
        if enc.is_null() {
            return None;
        }
        let value = enc["value"].as_str()?;
        let enc_type = enc["type"].as_str().unwrap_or("STRING");
        match enc_type {
            "STRING" => Some(value.to_string()),
            "ENCRYPTED_BYTES" => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .ok()?;
                String::from_utf8(decoded).ok()
            }
            other => {
                warn!("Unsupported filenameEnc type: {}", other);
                None
            }
        }
    }

    /// Size in bytes of the original resource.
    #[allow(dead_code)]
    pub fn size(&self) -> u64 {
        self.master_record["fields"]["resOriginalRes"]["value"]["size"]
            .as_u64()
            .unwrap_or(0)
    }

    /// The asset date (when the photo/video was taken), in UTC.
    pub fn asset_date(&self) -> DateTime<Utc> {
        self.asset_record["fields"]["assetDate"]["value"]
            .as_f64()
            .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
            .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
    }

    /// Convenience alias – returns `asset_date` converted to local time.
    pub fn created(&self) -> DateTime<Utc> {
        self.asset_date()
    }

    /// The date the asset was added to the library.
    #[allow(dead_code)]
    pub fn added_date(&self) -> DateTime<Utc> {
        self.asset_record["fields"]["addedDate"]["value"]
            .as_f64()
            .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
            .unwrap_or_else(|| Utc.timestamp_opt(0, 0).unwrap())
    }

    /// Determine the item type from the `itemType` field.
    pub fn item_type(&self) -> Option<AssetItemType> {
        let item_type_str = self.master_record["fields"]["itemType"]["value"].as_str()?;
        if let Some(t) = item_type_from_str(item_type_str) {
            return Some(t);
        }
        // Fallback: guess from filename extension
        if let Some(name) = self.filename() {
            let lower = name.to_lowercase();
            if lower.ends_with(".heic")
                || lower.ends_with(".png")
                || lower.ends_with(".jpg")
                || lower.ends_with(".jpeg")
            {
                return Some(AssetItemType::Image);
            }
        }
        Some(AssetItemType::Movie)
    }

    /// Build the map of available versions for this asset.
    pub fn versions(&self) -> HashMap<AssetVersionSize, AssetVersion> {
        let lookup = if self.item_type() == Some(AssetItemType::Movie) {
            VIDEO_VERSION_LOOKUP
        } else {
            PHOTO_VERSION_LOOKUP
        };

        let mut versions = HashMap::new();
        for (key, prefix) in lookup {
            let res_field = format!("{prefix}Res");
            let type_field = format!("{prefix}FileType");

            // Try asset record first, then master record.
            let fields = if !self.asset_record["fields"][&res_field].is_null() {
                &self.asset_record["fields"]
            } else if !self.master_record["fields"][&res_field].is_null() {
                &self.master_record["fields"]
            } else {
                continue;
            };

            let res_entry = &fields[&res_field]["value"];
            if res_entry.is_null() {
                continue;
            }

            let size = res_entry["size"].as_u64().unwrap_or(0);
            let url = res_entry["downloadURL"]
                .as_str()
                .unwrap_or_else(|| {
                    tracing::warn!("Missing expected field: {prefix}Res.downloadURL");
                    ""
                })
                .to_string();
            let checksum = res_entry["fileChecksum"]
                .as_str()
                .unwrap_or_else(|| {
                    tracing::warn!("Missing expected field: {prefix}Res.fileChecksum");
                    ""
                })
                .to_string();

            let asset_type = fields[&type_field]["value"]
                .as_str()
                .unwrap_or_else(|| {
                    tracing::warn!("Missing expected field: {type_field}");
                    ""
                })
                .to_string();

            versions.insert(
                *key,
                AssetVersion {
                    size,
                    url,
                    asset_type,
                    checksum,
                },
            );
        }
        versions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_asset(master: Value, asset: Value) -> PhotoAsset {
        PhotoAsset::new(master, asset)
    }

    #[test]
    fn test_id_present() {
        let asset = make_asset(
            json!({"recordName": "ABC123"}),
            json!({}),
        );
        assert_eq!(asset.id(), "ABC123");
    }

    #[test]
    fn test_id_missing() {
        let asset = make_asset(json!({}), json!({}));
        assert_eq!(asset.id(), "");
    }

    #[test]
    fn test_filename_string_type() {
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": "photo.jpg", "type": "STRING"}}}),
            json!({}),
        );
        assert_eq!(asset.filename(), Some("photo.jpg".to_string()));
    }

    #[test]
    fn test_filename_encrypted_bytes() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"test.png");
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": encoded, "type": "ENCRYPTED_BYTES"}}}),
            json!({}),
        );
        assert_eq!(asset.filename(), Some("test.png".to_string()));
    }

    #[test]
    fn test_filename_missing() {
        let asset = make_asset(json!({"fields": {}}), json!({}));
        assert_eq!(asset.filename(), None);
    }

    #[test]
    fn test_item_type_image() {
        let asset = make_asset(
            json!({"fields": {"itemType": {"value": "public.jpeg"}}}),
            json!({}),
        );
        assert_eq!(asset.item_type(), Some(AssetItemType::Image));
    }

    #[test]
    fn test_item_type_movie() {
        let asset = make_asset(
            json!({"fields": {"itemType": {"value": "com.apple.quicktime-movie"}}}),
            json!({}),
        );
        assert_eq!(asset.item_type(), Some(AssetItemType::Movie));
    }

    #[test]
    fn test_item_type_fallback_from_extension() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "unknown.type"},
                "filenameEnc": {"value": "photo.heic", "type": "STRING"}
            }}),
            json!({}),
        );
        assert_eq!(asset.item_type(), Some(AssetItemType::Image));
    }

    #[test]
    fn test_asset_date() {
        // 2025-01-15T00:00:00Z = 1736899200000 ms
        let asset = make_asset(
            json!({}),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        let dt = asset.asset_date();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2025-01-15");
    }

    #[test]
    fn test_versions_builds_map() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        let versions = asset.versions();
        assert!(versions.contains_key(&AssetVersionSize::Original));
        let orig = &versions[&AssetVersionSize::Original];
        assert_eq!(orig.url, "https://example.com/orig");
        assert_eq!(orig.checksum, "abc123");
    }

    #[test]
    fn test_display() {
        let asset = make_asset(json!({"recordName": "XYZ"}), json!({}));
        assert_eq!(format!("{}", asset), "<PhotoAsset: id=XYZ>");
    }
}

impl std::fmt::Display for PhotoAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<PhotoAsset: id={}>", self.id())
    }
}
