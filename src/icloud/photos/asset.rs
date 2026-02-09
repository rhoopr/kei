use base64::Engine;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use smallvec::SmallVec;
use tracing::warn;

use super::queries::{item_type_from_str, PHOTO_VERSION_LOOKUP, VIDEO_VERSION_LOOKUP};
use super::types::{AssetItemType, AssetVersion, AssetVersionSize};

/// Type alias for the versions map.
///
/// Uses SmallVec with capacity 4 to store versions inline (no heap allocation)
/// for the common case of <=4 versions per asset. Most assets have 1-3 versions
/// (original + optional medium/thumb + optional live photo).
pub type VersionsMap = SmallVec<[(AssetVersionSize, AssetVersion); 4]>;

/// A photo or video asset from iCloud.
///
/// Fields are ordered for optimal memory layout:
/// - Heap types first (String, `Option<String>`)
/// - VersionsMap (SmallVec inline storage)
/// - f64 primitives
/// - Small enums last
#[derive(Debug, Clone)]
pub struct PhotoAsset {
    // Heap types first
    record_name: String,
    filename: Option<String>,
    // SmallVec with inline storage
    versions: VersionsMap,
    // f64 primitives
    asset_date_ms: Option<f64>,
    added_date_ms: Option<f64>,
    // Small enum (1 byte)
    item_type_val: Option<AssetItemType>,
}

/// Decode filename from CloudKit's `filenameEnc` field.
/// Apple uses either plain STRING or base64-encoded ENCRYPTED_BYTES depending
/// on the user's iCloud configuration.
fn decode_filename(fields: &Value) -> Option<String> {
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

/// Determine asset type from the `itemType` CloudKit field, falling back to
/// file extension heuristics. Defaults to Movie for unknown types because
/// videos are more likely to have non-standard UTI strings.
fn resolve_item_type(fields: &Value, filename: &Option<String>) -> Option<AssetItemType> {
    if let Some(s) = fields["itemType"]["value"].as_str() {
        if let Some(t) = item_type_from_str(s) {
            return Some(t);
        }
    }
    if let Some(name) = &filename {
        let lower = name.to_lowercase();
        if lower.ends_with(".heic")
            || lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".webp")
        {
            return Some(AssetItemType::Image);
        }
    }
    Some(AssetItemType::Movie)
}

/// Pre-parse version URLs at construction so `PhotoAsset` carries no raw
/// JSON — reducing per-asset memory and making `versions()` infallible.
/// Incomplete entries (missing URL or checksum) are logged and skipped;
/// the caller sees an empty map rather than a runtime error.
fn extract_versions(
    item_type: Option<AssetItemType>,
    master_fields: &Value,
    asset_fields: &Value,
    record_name: &str,
) -> VersionsMap {
    let lookup = if item_type == Some(AssetItemType::Movie) {
        VIDEO_VERSION_LOOKUP
    } else {
        PHOTO_VERSION_LOOKUP
    };

    let mut versions = VersionsMap::new();
    for (key, prefix) in lookup {
        let res_field = format!("{prefix}Res");
        let type_field = format!("{prefix}FileType");

        // Asset record has adjusted versions; master has originals.
        // Prefer asset record so adjusted/edited versions take priority.
        let fields = if !asset_fields[&res_field].is_null() {
            asset_fields
        } else if !master_fields[&res_field].is_null() {
            master_fields
        } else {
            continue;
        };

        let res_entry = &fields[&res_field]["value"];
        if res_entry.is_null() {
            continue;
        }

        let size = res_entry["size"].as_u64().unwrap_or(0);

        let url: Box<str> = match res_entry["downloadURL"].as_str() {
            Some(u) => u.into(),
            None => {
                warn!(
                    "Asset {}: missing {prefix}Res.downloadURL, skipping version",
                    record_name
                );
                continue;
            }
        };

        let checksum: Box<str> = match res_entry["fileChecksum"].as_str() {
            Some(c) => c.into(),
            None => {
                warn!(
                    "Asset {}: missing {prefix}Res.fileChecksum, skipping version",
                    record_name
                );
                continue;
            }
        };

        let asset_type: Box<str> = fields[&type_field]["value"]
            .as_str()
            .unwrap_or_else(|| {
                tracing::warn!("Missing expected field: {type_field}");
                ""
            })
            .into();

        versions.push((
            *key,
            AssetVersion {
                size,
                url,
                asset_type,
                checksum,
            },
        ));
    }
    versions
}

impl PhotoAsset {
    /// Construct from raw JSON values (used by tests).
    #[cfg(test)]
    pub fn new(master_record: Value, asset_record: Value) -> Self {
        let record_name = master_record["recordName"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let master_fields = master_record.get("fields").cloned().unwrap_or(Value::Null);
        let asset_fields = asset_record.get("fields").cloned().unwrap_or(Value::Null);
        let filename = decode_filename(&master_fields);
        let item_type_val = resolve_item_type(&master_fields, &filename);
        let asset_date_ms = asset_fields["assetDate"]["value"].as_f64();
        let added_date_ms = asset_fields["addedDate"]["value"].as_f64();
        let versions = extract_versions(item_type_val, &master_fields, &asset_fields, &record_name);
        Self {
            record_name,
            filename,
            item_type_val,
            asset_date_ms,
            added_date_ms,
            versions,
        }
    }

    /// Construct from typed `Record` structs (used by album pagination).
    pub fn from_records(master: super::cloudkit::Record, asset: super::cloudkit::Record) -> Self {
        let filename = decode_filename(&master.fields);
        let item_type_val = resolve_item_type(&master.fields, &filename);
        let asset_date_ms = asset.fields["assetDate"]["value"].as_f64();
        let added_date_ms = asset.fields["addedDate"]["value"].as_f64();
        let versions = extract_versions(
            item_type_val,
            &master.fields,
            &asset.fields,
            &master.record_name,
        );
        Self {
            record_name: master.record_name,
            filename,
            item_type_val,
            asset_date_ms,
            added_date_ms,
            versions,
        }
    }

    pub fn id(&self) -> &str {
        &self.record_name
    }

    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    pub fn asset_date(&self) -> DateTime<Utc> {
        self.asset_date_ms
            .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
            .unwrap_or_else(|| {
                warn!(asset_id = %self.record_name, "Missing or invalid assetDate, falling back to epoch");
                DateTime::UNIX_EPOCH
            })
    }

    pub fn created(&self) -> DateTime<Utc> {
        self.asset_date()
    }

    pub fn added_date(&self) -> DateTime<Utc> {
        self.added_date_ms
            .and_then(|ms| Utc.timestamp_millis_opt(ms as i64).single())
            .unwrap_or_else(|| {
                warn!(asset_id = %self.record_name, "Missing or invalid addedDate, falling back to epoch");
                DateTime::UNIX_EPOCH
            })
    }

    pub fn item_type(&self) -> Option<AssetItemType> {
        self.item_type_val
    }

    /// Available download versions, as a list of (size, version) pairs.
    /// Pre-parsed at construction so no JSON traversal happens at download time.
    pub fn versions(&self) -> &VersionsMap {
        &self.versions
    }

    /// Get a specific version by size key.
    pub fn get_version(&self, key: &AssetVersionSize) -> Option<&AssetVersion> {
        self.versions.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Check if a specific version exists.
    pub fn contains_version(&self, key: &AssetVersionSize) -> bool {
        self.versions.iter().any(|(k, _)| k == key)
    }
}

impl std::fmt::Display for PhotoAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<PhotoAsset: id={}>", self.id())
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
        let asset = make_asset(json!({"recordName": "ABC123"}), json!({}));
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
        assert_eq!(asset.filename(), Some("photo.jpg"));
    }

    #[test]
    fn test_filename_encrypted_bytes() {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"test.png");
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": encoded, "type": "ENCRYPTED_BYTES"}}}),
            json!({}),
        );
        assert_eq!(asset.filename(), Some("test.png"));
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
    fn test_item_type_webp_from_uti() {
        let asset = make_asset(
            json!({"fields": {"itemType": {"value": "org.webmproject.webp"}}}),
            json!({}),
        );
        assert_eq!(asset.item_type(), Some(AssetItemType::Image));
    }

    #[test]
    fn test_item_type_webp_from_extension_fallback() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "unknown.type"},
                "filenameEnc": {"value": "photo.webp", "type": "STRING"}
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
        assert!(asset.contains_version(&AssetVersionSize::Original));
        let orig = asset.get_version(&AssetVersionSize::Original).unwrap();
        assert_eq!(&*orig.url, "https://example.com/orig");
        assert_eq!(&*orig.checksum, "abc123");
    }

    #[test]
    fn test_display() {
        let asset = make_asset(json!({"recordName": "XYZ"}), json!({}));
        assert_eq!(format!("{}", asset), "<PhotoAsset: id=XYZ>");
    }

    #[test]
    fn test_versions_missing_download_url() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        // Missing downloadURL now results in empty versions map (logged at construction)
        assert!(asset.versions().is_empty());
    }

    #[test]
    fn test_versions_missing_checksum() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://example.com/orig"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        // Missing checksum now results in empty versions map (logged at construction)
        assert!(asset.versions().is_empty());
    }

    #[test]
    fn test_from_records_extracts_fields() {
        use super::super::cloudkit::Record;

        let master = Record {
            record_name: "MASTER_1".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({
                "filenameEnc": {"value": "vacation.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {"size": 5000, "downloadURL": "https://example.com/dl", "fileChecksum": "ck1"}},
                "resOriginalFileType": {"value": "public.jpeg"}
            }),
        };
        let asset_rec = Record {
            record_name: "ASSET_1".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({
                "assetDate": {"value": 1736899200000.0},
                "addedDate": {"value": 1736899200000.0}
            }),
        };

        let asset = PhotoAsset::from_records(master, asset_rec);
        assert_eq!(asset.id(), "MASTER_1");
        assert_eq!(asset.filename(), Some("vacation.jpg"));
        assert_eq!(asset.item_type(), Some(AssetItemType::Image));
        assert_eq!(
            asset.asset_date().format("%Y-%m-%d").to_string(),
            "2025-01-15"
        );
        assert!(asset.contains_version(&AssetVersionSize::Original));
    }

    #[test]
    fn test_versions_prefers_asset_record_over_master() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://master.example.com/orig",
                    "fileChecksum": "master_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://asset.example.com/adjusted",
                    "fileChecksum": "asset_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
        );
        let orig = asset.get_version(&AssetVersionSize::Original).unwrap();
        assert_eq!(&*orig.url, "https://asset.example.com/adjusted");
        assert_eq!(orig.size, 2000);
    }

    #[test]
    fn test_versions_video_uses_video_lookup() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 50000,
                    "downloadURL": "https://example.com/video",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"},
                "resVidMedRes": {"value": {
                    "size": 10000,
                    "downloadURL": "https://example.com/vid_med",
                    "fileChecksum": "vid_med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {}}),
        );
        assert!(asset.contains_version(&AssetVersionSize::Original));
        assert!(asset.contains_version(&AssetVersionSize::Medium));
        // PHOTO_VERSION_LOOKUP maps Medium to resJPEGMed, but for videos
        // VIDEO_VERSION_LOOKUP maps Medium to resVidMed — verify the right one was used
        let medium = asset.get_version(&AssetVersionSize::Medium).unwrap();
        assert_eq!(&*medium.url, "https://example.com/vid_med");
    }

    #[test]
    fn test_versions_multiple_photo_sizes() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://example.com/orig",
                    "fileChecksum": "ck_orig"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 100,
                    "downloadURL": "https://example.com/thumb",
                    "fileChecksum": "ck_thumb"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert_eq!(asset.versions().len(), 2);
        assert_eq!(
            asset.get_version(&AssetVersionSize::Original).unwrap().size,
            5000
        );
        assert_eq!(
            asset.get_version(&AssetVersionSize::Thumb).unwrap().size,
            100
        );
    }

    #[test]
    fn test_from_records_missing_optional_fields() {
        use super::super::cloudkit::Record;

        let master = Record {
            record_name: "M2".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({}),
        };
        let asset_rec = Record {
            record_name: "A2".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({}),
        };

        let asset = PhotoAsset::from_records(master, asset_rec);
        assert_eq!(asset.id(), "M2");
        assert_eq!(asset.filename(), None);
    }

    #[test]
    fn test_get_version_and_contains_version() {
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
        assert!(asset.contains_version(&AssetVersionSize::Original));
        assert!(!asset.contains_version(&AssetVersionSize::Medium));
        assert!(asset.get_version(&AssetVersionSize::Original).is_some());
        assert!(asset.get_version(&AssetVersionSize::Medium).is_none());
    }

    #[test]
    fn test_struct_sizes() {
        use std::mem::size_of;
        // AssetVersion should be <= 64 bytes
        // With Box<str> fields: size(8) + url(16) + asset_type(16) + checksum(16) = 56 bytes
        assert!(
            size_of::<AssetVersion>() <= 64,
            "AssetVersion size {} exceeds 64 bytes",
            size_of::<AssetVersion>()
        );
        // PhotoAsset with SmallVec<[...; 4]> inline storage is ~360 bytes.
        // This is larger than HashMap but avoids heap allocation for common case (<=4 versions).
        // The trade-off is acceptable since we process assets in streams, not all at once.
        assert!(
            size_of::<PhotoAsset>() <= 400,
            "PhotoAsset size {} exceeds 400 bytes",
            size_of::<PhotoAsset>()
        );
        // AssetVersionSize should be 1 byte (repr(u8))
        assert_eq!(size_of::<AssetVersionSize>(), 1);
    }
}
