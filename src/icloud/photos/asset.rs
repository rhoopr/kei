use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use rustc_hash::FxHashMap;
use serde_json::Value;
use smallvec::SmallVec;

use super::cloudkit::Record;
use super::enc;
use super::metadata;
use super::queries::{item_type_from_str, PHOTO_VERSION_LOOKUP, VIDEO_VERSION_LOOKUP};
use super::types::{AssetItemType, AssetVersion, AssetVersionSize, ChangeReason};
use crate::state::AssetMetadata;

/// Type alias for the versions map.
///
/// Uses `SmallVec` with capacity 4 to store versions inline (no heap allocation)
/// for the common case of <=4 versions per asset. Most assets have 1-3 versions
/// (original + optional medium/thumb + optional live photo).
pub type VersionsMap = SmallVec<[(AssetVersionSize, AssetVersion); 4]>;

/// A change event from the `changes/zone` delta API.
#[derive(Debug)]
pub struct ChangeEvent {
    /// The record name (`CloudKit` record ID)
    pub record_name: Box<str>,
    /// The record type, if known (None for hard-deletes)
    pub record_type: Option<Box<str>>,
    /// Why this record changed
    pub reason: ChangeReason,
    /// The photo asset, if this is a CPLMaster+CPLAsset pair that was successfully paired.
    /// None for hard-deletes, non-photo record types, or unpaired records.
    pub asset: Option<PhotoAsset>,
}

/// A photo or video asset from iCloud.
///
/// Fields are ordered for optimal memory layout:
/// - Heap types first (`Arc<str>`, `Option<Box<str>>`)
/// - `VersionsMap` (`SmallVec` inline storage)
/// - f64 primitives
/// - Small enums last
///
/// `record_name` is `Arc<str>` so downstream consumers (producer
/// dedup set, `DownloadTask`, deferred state writes) can share the
/// same allocation via refcount bumps instead of re-cloning the
/// record ID at every stage boundary.
#[derive(Debug, Clone)]
pub struct PhotoAsset {
    // Heap types first
    record_name: Arc<str>,
    filename: Option<Box<str>>,
    // Metadata boxed to keep PhotoAsset compact when metadata is large
    // (keywords, provider_data JSON can be several hundred bytes).
    asset_metadata: Box<AssetMetadata>,
    // SmallVec with inline storage
    versions: VersionsMap,
    // f64 primitives
    asset_date_ms: Option<f64>,
    added_date_ms: Option<f64>,
    // Small enum (1 byte)
    item_type_val: Option<AssetItemType>,
}

/// Decode filename from `CloudKit`'s `filenameEnc` field.
/// Apple uses either plain STRING or base64-encoded `ENCRYPTED_BYTES` depending
/// on the user's iCloud configuration.
///
/// An empty string is treated as missing so downstream path construction
/// routes through the fingerprint fallback rather than producing a
/// directory-only path like `2026-04-19/` or an extension-only hidden file
/// like `.JPG`.
fn decode_filename(fields: &Value) -> Option<String> {
    let name = enc::decode_string(fields, "filenameEnc")?;
    if name.is_empty() {
        return None;
    }
    Some(name)
}

/// Convert an `f64` millisecond timestamp to a `DateTime<Utc>`, returning
/// `None` if the value is out of `i64` range.
pub(crate) fn f64_to_millis_datetime(ms: f64) -> Option<DateTime<Utc>> {
    #[allow(
        clippy::cast_precision_loss,
        reason = "range check is conservative; exact i64 precision loss at the extremes is fine since we only need a membership test"
    )]
    let in_range = (i64::MIN as f64..=i64::MAX as f64).contains(&ms);
    if in_range {
        #[allow(clippy::cast_possible_truncation, reason = "bounds checked above")]
        let ms_i64 = ms as i64;
        Utc.timestamp_millis_opt(ms_i64).single()
    } else {
        None
    }
}

/// True when `fields.key` is a non-null `Value`.
fn field_present(fields: &Value, key: &str) -> bool {
    fields.get(key).is_some_and(|v| !v.is_null())
}

/// Determine asset type from the `itemType` `CloudKit` field, falling back to
/// file extension heuristics. Defaults to Movie for unknown types because
/// videos are more likely to have non-standard UTI strings.
fn resolve_item_type(fields: &Value, filename: Option<&str>) -> AssetItemType {
    if let Some(s) = fields
        .get("itemType")
        .and_then(|f| f.get("value"))
        .and_then(Value::as_str)
    {
        if let Some(t) = item_type_from_str(s) {
            return t;
        }
    }
    if let Some(ext) = filename.and_then(|n| std::path::Path::new(n).extension()) {
        if ext.eq_ignore_ascii_case("heic")
            || ext.eq_ignore_ascii_case("png")
            || ext.eq_ignore_ascii_case("jpg")
            || ext.eq_ignore_ascii_case("jpeg")
            || ext.eq_ignore_ascii_case("webp")
        {
            return AssetItemType::Image;
        }
    }
    AssetItemType::Movie
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
    for (key, res_field, type_field) in lookup {
        // Asset record has adjusted versions; master has originals.
        // Prefer asset record so adjusted/edited versions take priority.
        let fields = if field_present(asset_fields, res_field) {
            asset_fields
        } else if field_present(master_fields, res_field) {
            master_fields
        } else {
            continue;
        };

        let res_entry = fields
            .get(res_field)
            .and_then(|f| f.get("value"))
            .unwrap_or(&Value::Null);
        if res_entry.is_null() {
            continue;
        }

        let size = if let Some(s) = res_entry.get("size").and_then(Value::as_u64) {
            s
        } else {
            tracing::warn!(
                asset = %record_name,
                field = format_args!("{res_field}.size"),
                "Missing size, skipping version"
            );
            continue;
        };

        let url: Box<str> = match res_entry.get("downloadURL").and_then(Value::as_str) {
            Some(u) => match validate_download_url(u) {
                Ok(()) => u.into(),
                Err(reason) => {
                    tracing::warn!(
                        asset = %record_name,
                        field = format_args!("{res_field}.downloadURL"),
                        url = u,
                        reason,
                        "Rejected downloadURL, skipping version"
                    );
                    continue;
                }
            },
            None => {
                tracing::warn!(
                    asset = %record_name,
                    field = format_args!("{res_field}.downloadURL"),
                    "Missing downloadURL, skipping version"
                );
                continue;
            }
        };

        let checksum: Box<str> =
            if let Some(c) = res_entry.get("fileChecksum").and_then(Value::as_str) {
                c.into()
            } else {
                tracing::warn!(
                    asset = %record_name,
                    field = format_args!("{res_field}.fileChecksum"),
                    "Missing fileChecksum, skipping version"
                );
                continue;
            };

        let asset_type: Box<str> = match fields
            .get(type_field)
            .and_then(|f| f.get("value"))
            .and_then(Value::as_str)
        {
            Some(s) if !s.is_empty() => s.into(),
            _ => {
                tracing::warn!(
                    asset = %record_name,
                    field = %type_field,
                    "Missing or empty asset type, skipping version"
                );
                continue;
            }
        };

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

/// Host suffixes kei will download from. Narrow to content-delivery hosts
/// so a compromised or malformed CloudKit response can't point kei at an
/// auth/gateway/marketing subdomain. Add new suffixes explicitly when
/// Apple introduces one; a loud skip is preferable to a silent
/// cross-origin fetch.
const ALLOWED_DOWNLOAD_HOST_SUFFIXES: &[&str] = &[
    ".icloud-content.com",
    ".icloud-content.com.cn",
    ".cdn-apple.com",
];

/// Validate that a CloudKit-provided download URL is safe to fetch from.
/// Rejects empty strings, non-https schemes, and hosts outside the Apple
/// CDN allowlist. On reject, returns a short reason suitable for a log line.
fn validate_download_url(raw: &str) -> Result<(), &'static str> {
    if raw.is_empty() {
        return Err("empty URL");
    }
    let parsed = url::Url::parse(raw).map_err(|_| "malformed URL")?;
    if parsed.scheme() != "https" {
        return Err("non-https scheme");
    }
    let host = parsed.host_str().ok_or("missing host")?;
    let host_lc = host.to_ascii_lowercase();
    let host_allowed = ALLOWED_DOWNLOAD_HOST_SUFFIXES
        .iter()
        .any(|suffix| host_lc.ends_with(suffix));
    if !host_allowed {
        return Err("host not in Apple CDN allowlist");
    }
    Ok(())
}

impl PhotoAsset {
    /// Construct from raw JSON values (used by tests).
    #[cfg(test)]
    pub fn new(master_record: Value, asset_record: Value) -> Self {
        let record_name: Arc<str> = master_record["recordName"].as_str().unwrap_or("").into();
        let master_fields = master_record.get("fields").cloned().unwrap_or(Value::Null);
        let asset_fields = asset_record.get("fields").cloned().unwrap_or(Value::Null);
        let filename = decode_filename(&master_fields).map(String::into_boxed_str);
        let item_type_val = Some(resolve_item_type(&master_fields, filename.as_deref()));
        let asset_date_ms = asset_fields["assetDate"]["value"].as_f64();
        let added_date_ms = asset_fields["addedDate"]["value"].as_f64();
        let versions = extract_versions(item_type_val, &master_fields, &asset_fields, &record_name);
        let asset_metadata = Box::new(metadata::extract(&master_fields, &asset_fields));
        Self {
            record_name,
            filename,
            asset_metadata,
            item_type_val,
            asset_date_ms,
            added_date_ms,
            versions,
        }
    }

    /// Construct from typed `Record` structs (used by album pagination).
    pub fn from_records(master: super::cloudkit::Record, asset: super::cloudkit::Record) -> Self {
        let filename = decode_filename(&master.fields).map(String::into_boxed_str);
        let item_type_val = Some(resolve_item_type(&master.fields, filename.as_deref()));
        let asset_date_ms = asset
            .fields
            .get("assetDate")
            .and_then(|f| f.get("value"))
            .and_then(Value::as_f64);
        let added_date_ms = asset
            .fields
            .get("addedDate")
            .and_then(|f| f.get("value"))
            .and_then(Value::as_f64);
        let versions = extract_versions(
            item_type_val,
            &master.fields,
            &asset.fields,
            &master.record_name,
        );
        let asset_metadata = Box::new(metadata::extract(&master.fields, &asset.fields));
        Self {
            record_name: Arc::from(master.record_name),
            filename,
            asset_metadata,
            item_type_val,
            asset_date_ms,
            added_date_ms,
            versions,
        }
    }

    /// Provider-agnostic metadata extracted at construction time.
    pub fn metadata(&self) -> &AssetMetadata {
        &self.asset_metadata
    }

    pub fn id(&self) -> &str {
        &self.record_name
    }

    /// Shared handle on the record ID. Consumers that want to store the ID
    /// (producer dedup set, `DownloadTask`, deferred state writes) clone the
    /// `Arc<str>` instead of allocating a fresh owned copy.
    #[must_use]
    pub fn id_arc(&self) -> Arc<str> {
        Arc::clone(&self.record_name)
    }

    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    pub fn asset_date(&self) -> DateTime<Utc> {
        self.asset_date_ms
            .and_then(f64_to_millis_datetime)
            .unwrap_or_else(|| {
                tracing::warn!(asset_id = %self.record_name, "Missing or invalid assetDate, falling back to epoch");
                DateTime::UNIX_EPOCH
            })
    }

    pub fn created(&self) -> DateTime<Utc> {
        self.asset_date()
    }

    pub fn added_date(&self) -> DateTime<Utc> {
        self.added_date_ms
            .and_then(f64_to_millis_datetime)
            .unwrap_or_else(|| {
                tracing::warn!(asset_id = %self.record_name, "Missing or invalid addedDate, falling back to epoch");
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
    pub fn get_version(&self, key: AssetVersionSize) -> Option<&AssetVersion> {
        self.versions
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// Check if a specific version exists.
    pub(crate) fn contains_version(&self, key: AssetVersionSize) -> bool {
        self.versions.iter().any(|(k, _)| *k == key)
    }

    /// Whether this asset is a live photo (image with a companion video).
    pub fn is_live_photo(&self) -> bool {
        self.item_type() == Some(AssetItemType::Image)
            && (self.contains_version(AssetVersionSize::LiveOriginal)
                || self.contains_version(AssetVersionSize::LiveMedium)
                || self.contains_version(AssetVersionSize::LiveThumb)
                || self.contains_version(AssetVersionSize::LiveAdjusted))
    }
}

impl std::fmt::Display for PhotoAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<PhotoAsset: id={}>", self.id())
    }
}

/// Classify a `CloudKit` record from `changes/zone` into a `ChangeReason`.
///
/// Detection logic (from empirical API testing):
/// - `record.deleted == Some(true)` --> `HardDeleted` (purged, recordType unknown)
/// - `fields.isDeleted.value == 1` --> `SoftDeleted` (trashed, recoverable)
/// - `fields.isHidden.value == 1` --> Hidden
/// - Otherwise --> Modified (caller checks state DB for Created/Restored/Unhidden)
///
/// Note: "Restored" and "Unhidden" require knowledge of previous state, which this
/// function does NOT have. Those cases return `Modified`; the caller (download pipeline)
/// should check the state DB to distinguish them.
pub(crate) fn classify_change_reason(record: &Record) -> ChangeReason {
    // Hard delete: record.deleted == true
    if record.deleted == Some(true) {
        return ChangeReason::HardDeleted;
    }

    // Soft delete: fields.isDeleted.value == 1
    if let Some(is_deleted) = record.fields.get("isDeleted") {
        if let Some(val) = is_deleted.get("value") {
            if val.as_i64() == Some(1) {
                return ChangeReason::SoftDeleted;
            }
        }
    }

    // Hidden: fields.isHidden.value == 1
    if let Some(is_hidden) = record.fields.get("isHidden") {
        if let Some(val) = is_hidden.get("value") {
            if val.as_i64() == Some(1) {
                return ChangeReason::Hidden;
            }
        }
    }

    // Default: Created (new or modified record)
    ChangeReason::Created
}

/// Extract the `masterRef` record name from a `CPLAsset`'s fields.
fn extract_master_ref(fields: &Value) -> Option<String> {
    fields
        .get("masterRef")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("recordName"))
        .and_then(|n| n.as_str())
        .map(std::string::ToString::to_string)
}

/// Buffers `CPLMaster` and `CPLAsset` records from `changes/zone` responses
/// and pairs them into `ChangeEvent`s when both halves are available.
///
/// In `changes/zone`, records arrive in change-log order (not paired like `records/query`).
/// A `CPLAsset` references its `CPLMaster` via the `masterRef` field.
#[derive(Debug, Default)]
pub(crate) struct DeltaRecordBuffer {
    /// Unpaired `CPLMaster` records, keyed by recordName
    pending_masters: FxHashMap<String, Record>,
    /// Unpaired `CPLAsset` records, keyed by masterRef recordName
    pending_assets: FxHashMap<String, Record>,
}

impl DeltaRecordBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process a batch of records from a `changes/zone` page.
    /// Returns any `ChangeEvent`s that could be assembled (paired master+asset,
    /// hard-deletes, soft-deletes, etc.).
    pub fn process_records(&mut self, records: Vec<Record>) -> Vec<ChangeEvent> {
        let mut events = Vec::new();

        for record in records {
            let reason = classify_change_reason(&record);

            match reason {
                ChangeReason::HardDeleted => {
                    // Hard-deleted: no fields, can't tell if master or asset.
                    // Emit immediately as-is.
                    events.push(ChangeEvent {
                        record_name: record.record_name.into_boxed_str(),
                        record_type: None,
                        reason,
                        asset: None,
                    });
                }
                _ => match record.record_type.as_str() {
                    "CPLMaster" => {
                        let master_name = record.record_name.clone();
                        if let Some(asset_record) = self.pending_assets.remove(&master_name) {
                            let asset_reason = classify_change_reason(&asset_record);
                            let final_reason = Self::reconcile_reasons(reason, asset_reason);
                            Self::emit_paired(record, asset_record, final_reason, &mut events);
                        } else {
                            self.pending_masters.insert(master_name, record);
                        }
                    }
                    "CPLAsset" => {
                        let master_ref = extract_master_ref(&record.fields);
                        if let Some(master_name) = &master_ref {
                            if let Some(master_record) = self.pending_masters.remove(master_name) {
                                let master_reason = classify_change_reason(&master_record);
                                let final_reason = Self::reconcile_reasons(master_reason, reason);
                                Self::emit_paired(master_record, record, final_reason, &mut events);
                            } else {
                                self.pending_assets.insert(master_name.clone(), record);
                            }
                        } else {
                            // CPLAsset with no masterRef -- metadata-only change
                            events.push(ChangeEvent {
                                record_name: record.record_name.into_boxed_str(),
                                record_type: Some("CPLAsset".into()),
                                reason,
                                asset: None,
                            });
                        }
                    }
                    _ => {
                        // Non-photo record types (CPLAlbum, CPLContainerRelation, etc.)
                        // Skip silently -- we only care about CPLMaster + CPLAsset
                    }
                },
            }
        }

        events
    }

    /// Flush any remaining unpaired records as standalone events.
    /// Call this after all pages have been processed (`moreComing: false`).
    pub fn flush(&mut self) -> Vec<ChangeEvent> {
        let mut events = Vec::new();

        for (name, record) in self.pending_masters.drain() {
            let reason = classify_change_reason(&record);
            events.push(ChangeEvent {
                record_name: name.into_boxed_str(),
                record_type: Some("CPLMaster".into()),
                reason,
                asset: None,
            });
        }

        for (_master_ref, record) in self.pending_assets.drain() {
            let reason = classify_change_reason(&record);
            events.push(ChangeEvent {
                record_name: record.record_name.into_boxed_str(),
                record_type: Some("CPLAsset".into()),
                reason,
                asset: None,
            });
        }

        events
    }

    /// Pick the more severe reason from a pair of records.
    ///
    /// When `CPLMaster` and `CPLAsset` arrive on different pages, we classify
    /// each independently. A soft-deleted master paired with a non-deleted
    /// asset should emit `SoftDeleted`, not `Created`. Severity order:
    /// `HardDeleted` > `SoftDeleted` > Hidden > Created.
    fn reconcile_reasons(a: ChangeReason, b: ChangeReason) -> ChangeReason {
        fn severity(r: ChangeReason) -> u8 {
            match r {
                ChangeReason::HardDeleted => 3,
                ChangeReason::SoftDeleted => 2,
                ChangeReason::Hidden => 1,
                ChangeReason::Created => 0,
            }
        }
        if severity(a) >= severity(b) {
            a
        } else {
            b
        }
    }

    fn emit_paired(
        master_record: Record,
        asset_record: Record,
        reason: ChangeReason,
        events: &mut Vec<ChangeEvent>,
    ) {
        let master_name: Box<str> = master_record.record_name.clone().into_boxed_str();
        let asset = PhotoAsset::from_records(master_record, asset_record);
        events.push(ChangeEvent {
            record_name: master_name,
            record_type: Some("CPLMaster".into()),
            reason,
            asset: Some(asset),
        });
    }
}

impl Drop for DeltaRecordBuffer {
    fn drop(&mut self) {
        if !self.pending_masters.is_empty() || !self.pending_assets.is_empty() {
            tracing::warn!(
                orphaned_masters = self.pending_masters.len(),
                orphaned_assets = self.pending_assets.len(),
                "DeltaRecordBuffer dropped with orphaned records"
            );
        }
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
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert!(asset.contains_version(AssetVersionSize::Original));
        let orig = asset.get_version(AssetVersionSize::Original).unwrap();
        assert_eq!(&*orig.url, "https://p01.icloud-content.com/orig");
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
                    "downloadURL": "https://p01.icloud-content.com/orig"
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
                "resOriginalRes": {"value": {"size": 5000, "downloadURL": "https://p01.icloud-content.com/dl", "fileChecksum": "ck1"}},
                "resOriginalFileType": {"value": "public.jpeg"}
            }),
            deleted: None,
        };
        let asset_rec = Record {
            record_name: "ASSET_1".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({
                "assetDate": {"value": 1736899200000.0},
                "addedDate": {"value": 1736899200000.0}
            }),
            deleted: None,
        };

        let asset = PhotoAsset::from_records(master, asset_rec);
        assert_eq!(asset.id(), "MASTER_1");
        assert_eq!(asset.filename(), Some("vacation.jpg"));
        assert_eq!(asset.item_type(), Some(AssetItemType::Image));
        assert_eq!(
            asset.asset_date().format("%Y-%m-%d").to_string(),
            "2025-01-15"
        );
        assert!(asset.contains_version(AssetVersionSize::Original));
    }

    #[test]
    fn test_versions_prefers_asset_record_over_master() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://p01.icloud-content.com/master-orig",
                    "fileChecksum": "master_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {
                "resOriginalRes": {"value": {
                    "size": 2000,
                    "downloadURL": "https://p01.icloud-content.com/asset-adjusted",
                    "fileChecksum": "asset_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
        );
        let orig = asset.get_version(AssetVersionSize::Original).unwrap();
        assert_eq!(&*orig.url, "https://p01.icloud-content.com/asset-adjusted");
        assert_eq!(orig.size, 2000);
    }

    #[test]
    fn test_versions_video_uses_video_lookup() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 50000,
                    "downloadURL": "https://p01.icloud-content.com/video",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"},
                "resVidMedRes": {"value": {
                    "size": 10000,
                    "downloadURL": "https://p01.icloud-content.com/vid_med",
                    "fileChecksum": "vid_med_ck"
                }},
                "resVidMedFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {}}),
        );
        assert!(asset.contains_version(AssetVersionSize::Original));
        assert!(asset.contains_version(AssetVersionSize::Medium));
        // PHOTO_VERSION_LOOKUP maps Medium to resJPEGMed, but for videos
        // VIDEO_VERSION_LOOKUP maps Medium to resVidMed — verify the right one was used
        let medium = asset.get_version(AssetVersionSize::Medium).unwrap();
        assert_eq!(&*medium.url, "https://p01.icloud-content.com/vid_med");
    }

    #[test]
    fn test_versions_multiple_photo_sizes() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "ck_orig"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 100,
                    "downloadURL": "https://p01.icloud-content.com/thumb",
                    "fileChecksum": "ck_thumb"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert_eq!(asset.versions().len(), 2);
        assert_eq!(
            asset.get_version(AssetVersionSize::Original).unwrap().size,
            5000
        );
        assert_eq!(
            asset.get_version(AssetVersionSize::Thumb).unwrap().size,
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
            deleted: None,
        };
        let asset_rec = Record {
            record_name: "A2".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({}),
            deleted: None,
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
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert!(asset.contains_version(AssetVersionSize::Original));
        assert!(!asset.contains_version(AssetVersionSize::Medium));
        assert!(asset.get_version(AssetVersionSize::Original).is_some());
        assert!(asset.get_version(AssetVersionSize::Medium).is_none());
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
        // PhotoAsset with SmallVec<[...; 4]> inline storage and Box<str> fields is ~344 bytes.
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

    // --- classify_change_reason tests ---

    fn make_record(record_type: &str, fields: Value, deleted: Option<bool>) -> Record {
        Record {
            record_name: "test-record".to_string(),
            record_type: record_type.to_string(),
            fields,
            deleted,
        }
    }

    #[test]
    fn test_classify_hard_deleted() {
        let record = make_record("", json!({}), Some(true));
        assert_eq!(classify_change_reason(&record), ChangeReason::HardDeleted);
    }

    #[test]
    fn test_classify_soft_deleted() {
        let record = make_record("CPLAsset", json!({"isDeleted": {"value": 1}}), Some(false));
        assert_eq!(classify_change_reason(&record), ChangeReason::SoftDeleted);
    }

    #[test]
    fn test_classify_hidden() {
        let record = make_record("CPLAsset", json!({"isHidden": {"value": 1}}), Some(false));
        assert_eq!(classify_change_reason(&record), ChangeReason::Hidden);
    }

    #[test]
    fn test_classify_normal_record() {
        let record = make_record("CPLAsset", json!({}), Some(false));
        assert_eq!(classify_change_reason(&record), ChangeReason::Created);
    }

    #[test]
    fn test_classify_is_deleted_null_value() {
        // isDeleted field present but value is null -- should NOT be SoftDeleted
        let record = make_record(
            "CPLAsset",
            json!({"isDeleted": {"value": null}}),
            Some(false),
        );
        assert_eq!(classify_change_reason(&record), ChangeReason::Created);
    }

    #[test]
    fn test_classify_is_deleted_zero() {
        // isDeleted == 0 means restored, but we return Created (caller checks state DB)
        let record = make_record("CPLAsset", json!({"isDeleted": {"value": 0}}), Some(false));
        assert_eq!(classify_change_reason(&record), ChangeReason::Created);
    }

    #[test]
    fn test_classify_is_hidden_zero() {
        // isHidden == 0 means unhidden, but we return Created (caller checks state DB)
        let record = make_record("CPLAsset", json!({"isHidden": {"value": 0}}), Some(false));
        assert_eq!(classify_change_reason(&record), ChangeReason::Created);
    }

    #[test]
    fn test_classify_deleted_none() {
        // deleted field absent (None) with no special flags
        let record = make_record("CPLMaster", json!({}), None);
        assert_eq!(classify_change_reason(&record), ChangeReason::Created);
    }

    #[test]
    fn test_classify_soft_deleted_takes_priority_over_hidden() {
        // Both isDeleted and isHidden set -- soft delete should win
        let record = make_record(
            "CPLAsset",
            json!({"isDeleted": {"value": 1}, "isHidden": {"value": 1}}),
            Some(false),
        );
        assert_eq!(classify_change_reason(&record), ChangeReason::SoftDeleted);
    }

    // --- extract_master_ref tests ---

    #[test]
    fn test_extract_master_ref_valid() {
        let fields = json!({
            "masterRef": {
                "value": {
                    "recordName": "MASTER_ABC",
                    "zoneID": {"zoneName": "PrimarySync"}
                }
            }
        });
        assert_eq!(extract_master_ref(&fields), Some("MASTER_ABC".to_string()));
    }

    #[test]
    fn test_extract_master_ref_missing() {
        let fields = json!({});
        assert_eq!(extract_master_ref(&fields), None);
    }

    #[test]
    fn test_extract_master_ref_malformed_no_value() {
        let fields = json!({"masterRef": {}});
        assert_eq!(extract_master_ref(&fields), None);
    }

    #[test]
    fn test_extract_master_ref_malformed_no_record_name() {
        let fields = json!({"masterRef": {"value": {"zoneID": "PrimarySync"}}});
        assert_eq!(extract_master_ref(&fields), None);
    }

    #[test]
    fn test_extract_master_ref_record_name_not_string() {
        let fields = json!({"masterRef": {"value": {"recordName": 12345}}});
        assert_eq!(extract_master_ref(&fields), None);
    }

    // --- DeltaRecordBuffer tests ---

    fn make_master_record(name: &str) -> Record {
        Record {
            record_name: name.to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"}
            }),
            deleted: None,
        }
    }

    fn make_asset_record(name: &str, master_ref: &str) -> Record {
        Record {
            record_name: name.to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({
                "masterRef": {
                    "value": {
                        "recordName": master_ref,
                        "zoneID": {"zoneName": "PrimarySync"}
                    }
                },
                "assetDate": {"value": 1736899200000.0},
                "addedDate": {"value": 1736899200000.0}
            }),
            deleted: None,
        }
    }

    #[test]
    fn test_buffer_master_then_asset_pairs() {
        let mut buffer = DeltaRecordBuffer::new();

        // Page 1: master arrives
        let events = buffer.process_records(vec![make_master_record("M1")]);
        assert!(events.is_empty(), "master alone should not emit an event");

        // Page 2: asset arrives, referencing M1
        let events = buffer.process_records(vec![make_asset_record("A1", "M1")]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "M1");
        assert_eq!(events[0].record_type.as_deref(), Some("CPLMaster"));
        assert_eq!(events[0].reason, ChangeReason::Created);
        assert!(events[0].asset.is_some());
        assert_eq!(events[0].asset.as_ref().unwrap().id(), "M1");
    }

    #[test]
    fn test_buffer_asset_then_master_pairs() {
        let mut buffer = DeltaRecordBuffer::new();

        // Page 1: asset arrives first
        let events = buffer.process_records(vec![make_asset_record("A1", "M1")]);
        assert!(events.is_empty(), "asset alone should not emit an event");

        // Page 2: master arrives
        let events = buffer.process_records(vec![make_master_record("M1")]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "M1");
        assert!(events[0].asset.is_some());
    }

    #[test]
    fn test_buffer_same_page_pairing() {
        let mut buffer = DeltaRecordBuffer::new();

        // Both on same page, master first
        let events = buffer.process_records(vec![
            make_master_record("M1"),
            make_asset_record("A1", "M1"),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "M1");
        assert!(events[0].asset.is_some());
    }

    #[test]
    fn test_buffer_same_page_asset_before_master() {
        let mut buffer = DeltaRecordBuffer::new();

        // Both on same page, asset first
        let events = buffer.process_records(vec![
            make_asset_record("A1", "M1"),
            make_master_record("M1"),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "M1");
        assert!(events[0].asset.is_some());
    }

    #[test]
    fn test_buffer_hard_delete_emitted_immediately() {
        let mut buffer = DeltaRecordBuffer::new();

        let hard_deleted = Record {
            record_name: "DELETED_1".to_string(),
            record_type: String::new(),
            fields: json!({}),
            deleted: Some(true),
        };

        let events = buffer.process_records(vec![hard_deleted]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "DELETED_1");
        assert_eq!(events[0].record_type, None);
        assert_eq!(events[0].reason, ChangeReason::HardDeleted);
        assert!(events[0].asset.is_none());
    }

    #[test]
    fn test_buffer_non_photo_records_skipped() {
        let mut buffer = DeltaRecordBuffer::new();

        let album_record = Record {
            record_name: "ALBUM_1".to_string(),
            record_type: "CPLAlbum".to_string(),
            fields: json!({"albumName": {"value": "Vacation"}}),
            deleted: None,
        };
        let container_record = Record {
            record_name: "CR_1".to_string(),
            record_type: "CPLContainerRelation".to_string(),
            fields: json!({}),
            deleted: None,
        };

        let events = buffer.process_records(vec![album_record, container_record]);
        assert!(events.is_empty(), "non-photo records should be skipped");

        // Flush should also be empty since non-photo records are not buffered
        let flushed = buffer.flush();
        assert!(flushed.is_empty());
    }

    #[test]
    fn test_buffer_flush_unpaired_records() {
        let mut buffer = DeltaRecordBuffer::new();

        // Add unpaired master and asset (referencing different masters)
        let events = buffer.process_records(vec![
            make_master_record("M_ORPHAN"),
            make_asset_record("A_ORPHAN", "M_MISSING"),
        ]);
        assert!(events.is_empty());

        let flushed = buffer.flush();
        assert_eq!(flushed.len(), 2);

        // Check that both orphans appear (order not guaranteed due to HashMap)
        let names: Vec<&str> = flushed.iter().map(|e| &*e.record_name).collect();
        assert!(names.contains(&"M_ORPHAN"));
        assert!(names.contains(&"A_ORPHAN"));

        // Verify record types
        for event in &flushed {
            assert!(event.asset.is_none());
            match &*event.record_name {
                "M_ORPHAN" => {
                    assert_eq!(event.record_type.as_deref(), Some("CPLMaster"));
                }
                "A_ORPHAN" => {
                    assert_eq!(event.record_type.as_deref(), Some("CPLAsset"));
                }
                _ => panic!("unexpected record name: {}", event.record_name),
            }
        }
    }

    #[test]
    fn test_buffer_multiple_pairs_across_pages() {
        let mut buffer = DeltaRecordBuffer::new();

        // Page 1: two masters
        let events =
            buffer.process_records(vec![make_master_record("M1"), make_master_record("M2")]);
        assert!(events.is_empty());

        // Page 2: one asset for M2, plus a new master M3
        let events = buffer.process_records(vec![
            make_asset_record("A2", "M2"),
            make_master_record("M3"),
        ]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "M2");

        // Page 3: assets for M1 and M3
        let events = buffer.process_records(vec![
            make_asset_record("A1", "M1"),
            make_asset_record("A3", "M3"),
        ]);
        assert_eq!(events.len(), 2);
        let names: Vec<&str> = events.iter().map(|e| &*e.record_name).collect();
        assert!(names.contains(&"M1"));
        assert!(names.contains(&"M3"));

        // All paired, flush should be empty
        let flushed = buffer.flush();
        assert!(flushed.is_empty());
    }

    #[test]
    fn test_buffer_asset_without_master_ref() {
        let mut buffer = DeltaRecordBuffer::new();

        // CPLAsset with no masterRef field -- metadata-only change
        let asset_no_ref = Record {
            record_name: "A_NO_REF".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({"assetDate": {"value": 1736899200000.0}}),
            deleted: None,
        };

        let events = buffer.process_records(vec![asset_no_ref]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "A_NO_REF");
        assert_eq!(events[0].record_type.as_deref(), Some("CPLAsset"));
        assert!(events[0].asset.is_none());
    }

    #[test]
    fn test_buffer_soft_deleted_asset_emitted_with_reason() {
        let mut buffer = DeltaRecordBuffer::new();

        let soft_deleted_master = Record {
            record_name: "M_SD".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({"isDeleted": {"value": 1}}),
            deleted: Some(false),
        };

        // Master is soft-deleted but still has record_type and fields
        let events = buffer.process_records(vec![soft_deleted_master]);
        assert!(events.is_empty()); // buffered, waiting for asset

        let flushed = buffer.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].reason, ChangeReason::SoftDeleted);
    }

    #[test]
    fn test_buffer_new_returns_empty() {
        let buffer = DeltaRecordBuffer::new();
        assert!(
            format!("{:?}", buffer).contains("DeltaRecordBuffer"),
            "should implement Debug"
        );
    }

    #[test]
    fn test_buffer_default_returns_empty() {
        let buffer = DeltaRecordBuffer::default();
        let mut buffer = buffer;
        let flushed = buffer.flush();
        assert!(flushed.is_empty());
    }

    #[test]
    fn test_buffer_multiple_pages_accumulation() {
        let mut buffer = DeltaRecordBuffer::new();

        // Page 1: master M1 only
        let events = buffer.process_records(vec![make_master_record("M1")]);
        assert!(events.is_empty(), "page 1 should emit nothing");

        // Page 2: master M2 + asset A1 referencing M1
        let events = buffer.process_records(vec![
            make_master_record("M2"),
            make_asset_record("A1", "M1"),
        ]);
        assert_eq!(events.len(), 1, "page 2 should pair M1+A1");
        assert_eq!(&*events[0].record_name, "M1");
        assert!(events[0].asset.is_some());

        // Page 3: asset A2 referencing M2
        let events = buffer.process_records(vec![make_asset_record("A2", "M2")]);
        assert_eq!(events.len(), 1, "page 3 should pair M2+A2");
        assert_eq!(&*events[0].record_name, "M2");
        assert!(events[0].asset.is_some());

        // Everything paired, flush empty
        let flushed = buffer.flush();
        assert!(flushed.is_empty());
    }

    #[test]
    fn test_buffer_soft_deleted_asset_with_master() {
        let mut buffer = DeltaRecordBuffer::new();

        let master = make_master_record("M_DEL");
        let soft_deleted_asset = Record {
            record_name: "A_DEL".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({
                "isDeleted": {"value": 1},
                "masterRef": {
                    "value": {
                        "recordName": "M_DEL",
                        "zoneID": {"zoneName": "PrimarySync"}
                    }
                },
                "assetDate": {"value": 1736899200000.0}
            }),
            deleted: Some(false),
        };

        let events = buffer.process_records(vec![master, soft_deleted_asset]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "M_DEL");
        assert_eq!(events[0].reason, ChangeReason::SoftDeleted);
        assert!(events[0].asset.is_some());
    }

    #[test]
    fn test_buffer_soft_deleted_master_with_normal_asset_reconciles() {
        // Bug fix: when CPLMaster has isDeleted=1 but CPLAsset does not,
        // the pair should be SoftDeleted (most severe reason wins).
        let mut buffer = DeltaRecordBuffer::new();

        let soft_deleted_master = Record {
            record_name: "M_SD".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({
                "isDeleted": {"value": 1},
                "filenameEnc": {"value": "deleted.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"}
            }),
            deleted: Some(false),
        };
        let normal_asset = make_asset_record("A_SD", "M_SD");

        // Master arrives first (soft-deleted), asset arrives second (normal)
        let events = buffer.process_records(vec![soft_deleted_master, normal_asset]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, ChangeReason::SoftDeleted);
        assert!(events[0].asset.is_some());
    }

    #[test]
    fn test_buffer_normal_master_with_soft_deleted_asset_reconciles() {
        // Reverse order: normal master, soft-deleted asset
        let mut buffer = DeltaRecordBuffer::new();

        let normal_master = make_master_record("M_SD2");
        let soft_deleted_asset = Record {
            record_name: "A_SD2".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({
                "isDeleted": {"value": 1},
                "masterRef": {
                    "value": {
                        "recordName": "M_SD2",
                        "zoneID": {"zoneName": "PrimarySync"}
                    }
                },
                "assetDate": {"value": 1736899200000.0}
            }),
            deleted: Some(false),
        };

        let events = buffer.process_records(vec![normal_master, soft_deleted_asset]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason, ChangeReason::SoftDeleted);
        assert!(events[0].asset.is_some());
    }

    #[test]
    fn test_buffer_cross_page_soft_deleted_master_reconciles() {
        // Master arrives on page 1, asset on page 2 — tests the pending path
        let mut buffer = DeltaRecordBuffer::new();

        let soft_deleted_master = Record {
            record_name: "M_XP".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({
                "isDeleted": {"value": 1},
                "filenameEnc": {"value": "cross_page.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"}
            }),
            deleted: Some(false),
        };

        // Page 1: only the soft-deleted master
        let events1 = buffer.process_records(vec![soft_deleted_master]);
        assert!(events1.is_empty()); // buffered, waiting for asset

        // Page 2: normal asset arrives
        let normal_asset = make_asset_record("A_XP", "M_XP");
        let events2 = buffer.process_records(vec![normal_asset]);
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].reason, ChangeReason::SoftDeleted);
        assert!(events2[0].asset.is_some());
    }

    #[test]
    fn test_classify_is_deleted_string_not_integer() {
        // isDeleted with value as string "1" instead of integer 1
        // as_i64() returns None for strings, so this should NOT be SoftDeleted
        let record = make_record(
            "CPLAsset",
            json!({"isDeleted": {"value": "1"}}),
            Some(false),
        );
        assert_eq!(classify_change_reason(&record), ChangeReason::Created);
    }

    #[test]
    fn test_buffer_asset_no_master_ref_emits_standalone() {
        let mut buffer = DeltaRecordBuffer::new();

        let asset_no_ref = Record {
            record_name: "A_STANDALONE".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({"assetDate": {"value": 1736899200000.0}}),
            deleted: None,
        };

        let events = buffer.process_records(vec![asset_no_ref]);
        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "A_STANDALONE");
        assert_eq!(events[0].record_type.as_deref(), Some("CPLAsset"));
        assert_eq!(events[0].reason, ChangeReason::Created);
        assert!(
            events[0].asset.is_none(),
            "standalone asset should have no PhotoAsset"
        );
    }

    #[tracing_test::traced_test]
    #[test]
    fn test_buffer_drop_logs_orphaned_records() {
        {
            let mut buffer = DeltaRecordBuffer::new();
            buffer.process_records(vec![make_master_record("M_ORPHAN")]);
            // Drop without calling flush()
        }

        assert!(logs_contain(
            "DeltaRecordBuffer dropped with orphaned records"
        ));
        assert!(logs_contain("orphaned_masters=1"));
        assert!(logs_contain("orphaned_assets=0"));
    }

    #[tracing_test::traced_test]
    #[test]
    fn test_buffer_drop_no_log_when_empty() {
        {
            let _buffer = DeltaRecordBuffer::new();
        }

        assert!(!logs_contain(
            "DeltaRecordBuffer dropped with orphaned records"
        ));
    }

    // --- Gap tests: API response handling robustness ---

    #[test]
    fn photo_asset_null_record_name_returns_empty_id() {
        // recordName is JSON null rather than missing entirely
        let asset = make_asset(json!({"recordName": null}), json!({}));
        assert_eq!(asset.id(), "");
    }

    #[test]
    fn decode_filename_invalid_base64_encrypted_bytes_returns_none() {
        // "!!not-valid-base64!!" is not decodable
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": "!!not-valid-base64!!", "type": "ENCRYPTED_BYTES"}}}),
            json!({}),
        );
        assert_eq!(asset.filename(), None);
    }

    #[test]
    fn decode_filename_invalid_utf8_after_base64_decode_returns_none() {
        use base64::Engine;
        // 0xFF 0xFE is not valid UTF-8
        let encoded = base64::engine::general_purpose::STANDARD.encode([0xFF, 0xFE, 0x80, 0x81]);
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": encoded, "type": "ENCRYPTED_BYTES"}}}),
            json!({}),
        );
        assert_eq!(asset.filename(), None);
    }

    #[test]
    fn decode_filename_unsupported_type_returns_none() {
        // "BINARY" is not a recognized filenameEnc type
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": "photo.jpg", "type": "BINARY"}}}),
            json!({}),
        );
        assert_eq!(asset.filename(), None);
    }

    #[test]
    fn resolve_item_type_no_item_type_no_filename_defaults_to_movie() {
        // No itemType field and no filenameEnc -> extension heuristic cannot fire -> Movie
        let asset = make_asset(json!({"fields": {}}), json!({}));
        assert_eq!(asset.item_type(), Some(AssetItemType::Movie));
    }

    #[test]
    fn asset_date_negative_timestamp_pre_epoch() {
        // 1969-07-20T20:17:00Z (Apollo 11 landing) = -14182980000 ms
        let asset = make_asset(
            json!({}),
            json!({"fields": {"assetDate": {"value": -14_182_980_000.0}}}),
        );
        let dt = asset.asset_date();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "1969-07-20");
    }

    #[test]
    fn asset_date_zero_timestamp_returns_epoch() {
        let asset = make_asset(json!({}), json!({"fields": {"assetDate": {"value": 0.0}}}));
        let dt = asset.asset_date();
        assert_eq!(dt, DateTime::UNIX_EPOCH);
    }

    #[test]
    fn asset_date_f64_infinity_falls_back_to_epoch() {
        let asset = make_asset(
            json!({"recordName": "BAD_DATE"}),
            json!({"fields": {"assetDate": {"value": f64::INFINITY}}}),
        );
        let dt = asset.asset_date();
        assert_eq!(
            dt,
            DateTime::UNIX_EPOCH,
            "out-of-range f64 assetDate must not produce a bogus date; \
             must fall back to epoch so the user sees 1970-01-01, not silent garbage"
        );
    }

    #[test]
    fn asset_date_f64_nan_falls_back_to_epoch() {
        let asset = make_asset(
            json!({"recordName": "NAN_DATE"}),
            json!({"fields": {"assetDate": {"value": f64::NAN}}}),
        );
        let dt = asset.asset_date();
        assert_eq!(
            dt,
            DateTime::UNIX_EPOCH,
            "NaN assetDate must fall back to epoch — (i64::MIN..=i64::MAX).contains(&NaN) is false"
        );
    }

    #[test]
    fn asset_empty_record_name_still_exposes_empty_id() {
        let asset = make_asset(json!({"recordName": ""}), json!({}));
        assert_eq!(
            asset.id(),
            "",
            "empty recordName must be reflected as empty id so upstream producers \
             can detect and reject; no silent defaulting to a placeholder"
        );
    }

    #[test]
    fn versions_completely_empty_fields_returns_empty_map() {
        // Both master and asset have fields: {} — no version resources at all
        let asset = make_asset(json!({"fields": {}}), json!({"fields": {}}));
        assert!(asset.versions().is_empty());
    }

    #[test]
    fn photo_asset_very_large_size_values() {
        let large_size: u64 = u64::MAX;
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": large_size,
                    "downloadURL": "https://p01.icloud-content.com/huge",
                    "fileChecksum": "ck_huge"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        let orig = asset.get_version(AssetVersionSize::Original).unwrap();
        // serde_json may not round-trip u64::MAX exactly through f64,
        // but the version should still be present and have a non-zero size
        assert!(orig.size > 0);
        assert_eq!(&*orig.url, "https://p01.icloud-content.com/huge");
    }

    #[test]
    fn from_records_null_master_fields_partial_data() {
        use super::super::cloudkit::Record;

        // Master record where fields has null values for everything except recordName
        let master = Record {
            record_name: "M_PARTIAL".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({
                "filenameEnc": null,
                "itemType": null,
                "resOriginalRes": null
            }),
            deleted: None,
        };
        let asset_rec = Record {
            record_name: "A_PARTIAL".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({
                "assetDate": null,
                "addedDate": null
            }),
            deleted: None,
        };

        let asset = PhotoAsset::from_records(master, asset_rec);
        assert_eq!(asset.id(), "M_PARTIAL");
        assert_eq!(asset.filename(), None);
        assert!(asset.versions().is_empty());
        // Missing assetDate falls back to epoch
        assert_eq!(asset.asset_date(), DateTime::UNIX_EPOCH);
        assert_eq!(asset.added_date(), DateTime::UNIX_EPOCH);
    }

    /// T-10: CPLMaster on page 1 with no matching CPLAsset; CPLAsset arrives
    /// on page 2. The buffer must pair them and emit a PhotoAsset.
    #[test]
    fn test_unpaired_master_buffered_across_pages() {
        let mut buffer = DeltaRecordBuffer::new();

        // Page 1: CPLMaster only — no matching CPLAsset yet
        let events = buffer.process_records(vec![Record {
            record_name: "M_SPLIT".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({
                "filenameEnc": {"value": "split.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 4096,
                    "downloadURL": "https://p01.icloud-content.com/split",
                    "fileChecksum": "split_ck"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }),
            deleted: None,
        }]);
        assert!(
            events.is_empty(),
            "page 1 should buffer the unpaired master"
        );

        // Page 2: matching CPLAsset arrives
        let events = buffer.process_records(vec![Record {
            record_name: "A_SPLIT".to_string(),
            record_type: "CPLAsset".to_string(),
            fields: json!({
                "masterRef": {
                    "value": {
                        "recordName": "M_SPLIT",
                        "zoneID": {"zoneName": "PrimarySync"}
                    }
                },
                "assetDate": {"value": 1736899200000.0},
                "addedDate": {"value": 1736899200000.0}
            }),
            deleted: None,
        }]);
        assert_eq!(events.len(), 1, "page 2 should pair M_SPLIT + A_SPLIT");
        assert_eq!(&*events[0].record_name, "M_SPLIT");
        assert_eq!(events[0].reason, ChangeReason::Created);

        let asset = events[0]
            .asset
            .as_ref()
            .expect("paired event should have a PhotoAsset");
        assert_eq!(asset.id(), "M_SPLIT");
        assert_eq!(asset.filename(), Some("split.jpg"));
        assert!(
            asset.contains_version(AssetVersionSize::Original),
            "paired asset should have the Original version from the master"
        );

        // Flush should be empty — everything was paired
        let flushed = buffer.flush();
        assert!(flushed.is_empty());
    }

    #[test]
    fn test_is_live_photo_true() {
        let asset = make_asset(
            json!({
                "recordName": "LIVE_1",
                "fields": {
                    "filenameEnc": {"value": "IMG_0001.HEIC", "type": "STRING"},
                    "itemType": {"value": "public.heic"},
                    "resOriginalRes": {"value": {
                        "size": 2000, "downloadURL": "https://p01.icloud-content.com/heic",
                        "fileChecksum": "heic_ck"
                    }},
                    "resOriginalFileType": {"value": "public.heic"},
                    "resOriginalVidComplRes": {"value": {
                        "size": 3000, "downloadURL": "https://p01.icloud-content.com/mov",
                        "fileChecksum": "mov_ck"
                    }},
                    "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
                }
            }),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        assert!(asset.is_live_photo());
    }

    #[test]
    fn test_is_live_photo_false_no_companion() {
        let asset = make_asset(
            json!({
                "recordName": "PHOTO_1",
                "fields": {
                    "itemType": {"value": "public.jpeg"},
                    "resOriginalRes": {"value": {
                        "size": 1000, "downloadURL": "https://p01.icloud-content.com/jpg",
                        "fileChecksum": "jpg_ck"
                    }}
                }
            }),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        assert!(!asset.is_live_photo());
    }

    #[test]
    fn test_is_live_photo_false_for_movie() {
        let asset = make_asset(
            json!({
                "recordName": "VID_1",
                "fields": {
                    "itemType": {"value": "com.apple.quicktime-movie"},
                    "resOriginalRes": {"value": {
                        "size": 5000, "downloadURL": "https://p01.icloud-content.com/mov",
                        "fileChecksum": "vid_ck"
                    }},
                    "resOriginalVidComplRes": {"value": {
                        "size": 3000, "downloadURL": "https://p01.icloud-content.com/mov2",
                        "fileChecksum": "mov2_ck"
                    }}
                }
            }),
            json!({"fields": {"assetDate": {"value": 1736899200000.0}}}),
        );
        assert!(
            !asset.is_live_photo(),
            "Movies with video companion are not live photos"
        );
    }

    #[test]
    fn test_versions_skips_empty_asset_type() {
        // When resOriginalFileType is missing, the version should be excluded.
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "ck_orig"
                }}
                // resOriginalFileType intentionally omitted
            }}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "Version with missing asset type should be skipped"
        );
    }

    #[test]
    fn test_versions_skips_null_asset_type() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "ck_orig"
                }},
                "resOriginalFileType": {"value": null}
            }}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "Version with null asset type should be skipped"
        );
    }

    // ── Gap: asset with completely empty fields produces no versions ──

    #[test]
    fn extract_versions_empty_fields_produces_empty_map() {
        let asset = make_asset(
            json!({"recordName": "EMPTY_FIELDS", "fields": {}}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "asset with no version fields should have empty versions"
        );
    }

    // ── Gap: asset with null resOriginalRes value ────────────────────

    #[test]
    fn extract_versions_null_res_value_produces_empty_map() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": null},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "null resOriginalRes value should produce empty versions"
        );
    }

    // ── Gap: asset with missing size skips the version ───────────────

    #[test]
    fn extract_versions_missing_size_skips_version() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "abc123"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        // A size-less version cannot be reliably downloaded or verified.
        // Skip it so a 0-byte placeholder doesn't poison downstream decisions.
        assert!(
            asset.versions().is_empty(),
            "version with missing size should be skipped"
        );
    }

    // ── Gap: asset with empty string downloadURL ─────────────────────

    #[test]
    fn extract_versions_empty_download_url_is_rejected() {
        // An empty-but-present URL is a CloudKit shape we cannot safely
        // download from; the version is skipped.
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 100,
                    "downloadURL": "",
                    "fileChecksum": "ck_empty_url"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "empty URL should cause the version to be skipped"
        );
    }

    #[test]
    fn extract_versions_non_https_url_is_rejected() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 100,
                    "downloadURL": "http://p01.icloud-content.com/foo",
                    "fileChecksum": "ck_http"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "http URL should cause the version to be skipped"
        );
    }

    #[test]
    fn extract_versions_foreign_host_is_rejected() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 100,
                    "downloadURL": "https://attacker.example.com/pwned.jpg",
                    "fileChecksum": "ck_bad"
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "non-Apple host should cause the version to be skipped"
        );
    }

    #[test]
    fn validate_download_url_accepts_allowlisted_hosts() {
        for u in [
            "https://p01.icloud-content.com/path",
            "https://P99.ICLOUD-CONTENT.COM/path",
            "https://cvws.icloud-content.com/B/foo",
            "https://cvws.icloud-content.com.cn/B/foo",
            "https://something.cdn-apple.com/resource",
        ] {
            assert!(super::validate_download_url(u).is_ok(), "expected ok: {u}");
        }
    }

    #[test]
    fn validate_download_url_rejects_bad_inputs() {
        for u in [
            "",
            "ftp://p01.icloud-content.com/path",
            "http://p01.icloud-content.com/path",
            "https://evil.example.com/x",
            "not-a-url",
            "https://",
        ] {
            assert!(
                super::validate_download_url(u).is_err(),
                "expected err: {u}"
            );
        }
    }

    /// Auth, gateway, and marketing subdomains of icloud.com / apple.com must
    /// not be reachable as download origins even over HTTPS.
    #[test]
    fn validate_download_url_rejects_icloud_and_apple_subdomains() {
        for u in [
            "https://www.icloud.com/photos/asset",
            "https://gateway.icloud.com/foo",
            "https://appleid.icloud.com/token",
            "https://www.apple.com/resource",
            "https://configuration.apple.com/cfg",
            "https://developer.apple.com/sdk",
            "https://www.icloud.com.cn/x",
            "https://www.apple.com.cn/x",
        ] {
            assert!(
                super::validate_download_url(u).is_err(),
                "expected err (not a CDN host): {u}"
            );
        }
    }

    // ── Gap: multiple versions with partial failures ─────────────────

    #[test]
    fn extract_versions_partial_valid_versions() {
        // Original is valid, Thumb has missing checksum -- only Original
        // should appear in versions.
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 5000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": "ck_orig"
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resJPEGThumbRes": {"value": {
                    "size": 100,
                    "downloadURL": "https://p01.icloud-content.com/thumb"
                }},
                "resJPEGThumbFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert_eq!(
            asset.versions().len(),
            1,
            "only the valid Original version should be extracted"
        );
        assert!(asset.contains_version(AssetVersionSize::Original));
        assert!(
            !asset.contains_version(AssetVersionSize::Thumb),
            "Thumb with missing checksum should be skipped"
        );
    }

    // ── Gap: is_live_photo false for video with LiveOriginal ─────────

    #[test]
    fn is_live_photo_false_for_video_with_live_version() {
        // A Movie-type asset with LiveOriginal should NOT be considered a
        // live photo (live photo = Image + companion video).
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalRes": {"value": {
                    "size": 50000,
                    "downloadURL": "https://p01.icloud-content.com/vid",
                    "fileChecksum": "vid_ck"
                }},
                "resOriginalFileType": {"value": "com.apple.quicktime-movie"},
                "resOriginalVidComplRes": {"value": {
                    "size": 3000,
                    "downloadURL": "https://p01.icloud-content.com/live",
                    "fileChecksum": "live_ck"
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"}
            }}),
            json!({"fields": {}}),
        );
        assert!(
            !asset.is_live_photo(),
            "Movie with LiveOriginal should NOT be is_live_photo"
        );
    }

    // ── Gap: asset_date falls back to epoch for missing field ────────

    #[test]
    fn asset_date_missing_returns_epoch() {
        let asset = make_asset(json!({"recordName": "NO_DATE"}), json!({"fields": {}}));
        let dt = asset.asset_date();
        assert_eq!(dt, DateTime::UNIX_EPOCH);
    }

    // ── Gap: classify_change_reason for various field combinations ───

    #[test]
    fn classify_change_reason_soft_deleted() {
        use super::super::cloudkit::Record;
        let record = Record {
            record_name: "DEL".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({"isDeleted": {"value": 1}}),
            deleted: None,
        };
        assert_eq!(classify_change_reason(&record), ChangeReason::SoftDeleted);
    }

    #[test]
    fn classify_change_reason_hidden() {
        use super::super::cloudkit::Record;
        let record = Record {
            record_name: "HID".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({"isHidden": {"value": 1}}),
            deleted: None,
        };
        assert_eq!(classify_change_reason(&record), ChangeReason::Hidden);
    }

    #[test]
    fn classify_change_reason_hard_deleted_overrides_fields() {
        // record.deleted == true should take precedence over fields
        use super::super::cloudkit::Record;
        let record = Record {
            record_name: "HARD".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({"isDeleted": {"value": 0}, "isHidden": {"value": 0}}),
            deleted: Some(true),
        };
        assert_eq!(classify_change_reason(&record), ChangeReason::HardDeleted);
    }

    #[test]
    fn classify_change_reason_default_is_created() {
        use super::super::cloudkit::Record;
        let record = Record {
            record_name: "NEW".to_string(),
            record_type: "CPLMaster".to_string(),
            fields: json!({}),
            deleted: None,
        };
        assert_eq!(classify_change_reason(&record), ChangeReason::Created);
    }
}
