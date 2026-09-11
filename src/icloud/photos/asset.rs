use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use rustc_hash::FxHashMap;
use serde_json::Value;

use super::cloudkit::Record;
use super::enc;
use super::metadata;
use super::queries::{PHOTO_VERSION_LOOKUP, VIDEO_VERSION_LOOKUP, item_type_from_str};
use super::types::{AssetItemType, AssetVersion, AssetVersionSize, ChangeReason};
use crate::state::{AssetMetadata, RenditionMetadata, VersionSizeKey};

/// Type alias for the versions map.
///
/// Most assets have 1-3 versions (original + optional medium/thumb +
/// optional live photo), so a compact vector keeps the representation simple.
pub type VersionsMap = Vec<(AssetVersionSize, AssetVersion)>;

/// Malformed resource fields seen while extracting downloadable versions.
///
/// Absence of an optional resource is normal; this records only resources
/// CloudKit advertised with an unusable value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MalformedResource {
    pub(crate) version_size: AssetVersionSize,
    pub(crate) field: Box<str>,
    pub(crate) reason: Box<str>,
}

pub(crate) const MALFORMED_REQUIRED_ASSET_FIELDS_REASON: &str = "malformed_required_asset_fields";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequiredAssetFields {
    Identity,
    Downloadable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum MalformedRequiredAssetField {
    #[error("CPLMaster recordName is missing or blank")]
    MasterRecordName,
    #[error("CPLAsset recordName is missing or blank")]
    AssetRecordName,
    #[error("CPLAsset assetDate is missing or invalid")]
    AssetDate,
}

/// A change event from the `changes/zone` delta API.
#[derive(Debug)]
pub struct ChangeEvent {
    /// The record name (`CloudKit` record ID)
    pub record_name: Box<str>,
    /// The record type, if known (None for hard-deletes)
    pub record_type: Option<Box<str>>,
    /// Master record referenced by an unpaired `CPLAsset`.
    ///
    /// Download state normally uses the master record name, while sibling
    /// `CPLAsset` rows can use their own asset record names. Keep the
    /// reference so state transitions can resolve unpaired asset deltas
    /// against the same key family that normal downloads use.
    pub master_record_name: Option<Box<str>>,
    /// Why this record changed
    pub reason: ChangeReason,
    /// The photo asset, if this is a CPLMaster+CPLAsset pair that was successfully paired.
    /// None for hard-deletes, non-photo record types, or unpaired records.
    pub asset: Option<PhotoAsset>,
    /// Album container metadata delta from `/changes/zone`.
    pub album: Option<AlbumContainerDelta>,
    /// Album relation delta from `/changes/zone`.
    pub relation: Option<AlbumRelationDelta>,
    /// A delta record that was understood well enough to make the cycle
    /// token-unsafe, but not well enough to apply safely.
    pub token_unsafe_reason: Option<&'static str>,
}

impl ChangeEvent {
    fn new(record_name: Box<str>, record_type: Option<Box<str>>, reason: ChangeReason) -> Self {
        Self {
            record_name,
            record_type,
            master_record_name: None,
            reason,
            asset: None,
            album: None,
            relation: None,
            token_unsafe_reason: None,
        }
    }
}

/// Album container metadata carried by `CPLAlbum` change records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumContainerDelta {
    pub container_id: Box<str>,
    pub album_name: Box<str>,
    pub is_deleted: bool,
}

/// Album membership relation carried by `CPLContainerRelation` change records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumRelationDelta {
    pub container_id: Box<str>,
    pub asset_record_name: Box<str>,
    pub is_deleted: bool,
}

/// A photo or video asset from iCloud.
///
/// Fields are ordered for optimal memory layout:
/// - Heap types first (`Arc<str>`, `Option<Box<str>>`)
/// - `VersionsMap`
/// - f64 primitives
/// - Small enums last
///
/// Record IDs are `Arc<str>` so downstream consumers (producer dedup,
/// `DownloadTask`, deferred state writes) can share the same allocation via
/// refcount bumps instead of re-cloning IDs at every stage boundary.
#[derive(Debug, Clone)]
pub struct PhotoAsset {
    // Heap types first
    record_name: Arc<str>,
    asset_record_name: Arc<str>,
    state_record_name: Option<Arc<str>>,
    source_zone: Option<Arc<str>>,
    filename: Option<Box<str>>,
    // Metadata behind Arc so downstream consumers (AssetRecord, the
    // pipeline's metadata-hash comparisons) can share the same
    // allocation via refcount bumps. Immutable after construction.
    asset_metadata: Arc<AssetMetadata>,
    rendition_facts: Arc<[(VersionSizeKey, RenditionMetadata)]>,
    versions: VersionsMap,
    malformed_resources: Arc<[MalformedResource]>,
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

/// Determine asset type from the `itemType` `CloudKit` field, falling back to
/// file extension heuristics. Defaults to Movie for unknown types because
/// videos are more likely to have non-standard UTI strings.
fn resolve_item_type(fields: &Value, filename: Option<&str>) -> AssetItemType {
    if let Some(s) = fields
        .get("itemType")
        .and_then(|f| f.get("value"))
        .and_then(Value::as_str)
        && let Some(t) = item_type_from_str(s)
    {
        return t;
    }
    if let Some(ext) = filename.and_then(|n| std::path::Path::new(n).extension())
        && (ext.eq_ignore_ascii_case("heic")
            || ext.eq_ignore_ascii_case("png")
            || ext.eq_ignore_ascii_case("jpg")
            || ext.eq_ignore_ascii_case("jpeg")
            || ext.eq_ignore_ascii_case("webp"))
    {
        return AssetItemType::Image;
    }
    AssetItemType::Movie
}

/// Pre-parse version URLs at construction so `PhotoAsset` carries no raw
/// JSON, reducing per-asset memory and making `versions()` infallible.
/// Incomplete entries (missing URL or checksum) are logged and skipped;
/// the caller sees an empty map rather than a runtime error.
fn extract_versions(
    item_type: Option<AssetItemType>,
    master_fields: &Value,
    asset_fields: &Value,
    record_name: &str,
) -> (VersionsMap, Vec<MalformedResource>) {
    let lookup = if item_type == Some(AssetItemType::Movie) {
        VIDEO_VERSION_LOOKUP
    } else {
        PHOTO_VERSION_LOOKUP
    };

    let mut versions = VersionsMap::with_capacity(4);
    let mut malformed_resources = Vec::new();
    for (key, res_field, type_field) in lookup {
        // Asset record has adjusted versions; master has originals.
        // Prefer asset record so adjusted/edited versions take priority.
        let fields = if asset_fields.get(res_field).is_some() {
            asset_fields
        } else if master_fields.get(res_field).is_some() {
            master_fields
        } else {
            continue;
        };

        let res_entry = fields
            .get(res_field)
            .and_then(|f| f.get("value"))
            .unwrap_or(&Value::Null);
        if res_entry.is_null() {
            malformed_resources.push(MalformedResource {
                version_size: *key,
                field: (*res_field).into(),
                reason: "resource value is null".into(),
            });
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
            malformed_resources.push(MalformedResource {
                version_size: *key,
                field: format!("{res_field}.size").into_boxed_str(),
                reason: "missing size".into(),
            });
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
                    malformed_resources.push(MalformedResource {
                        version_size: *key,
                        field: format!("{res_field}.downloadURL").into_boxed_str(),
                        reason: reason.into(),
                    });
                    continue;
                }
            },
            None => {
                tracing::warn!(
                    asset = %record_name,
                    field = format_args!("{res_field}.downloadURL"),
                    "Missing downloadURL, skipping version"
                );
                malformed_resources.push(MalformedResource {
                    version_size: *key,
                    field: format!("{res_field}.downloadURL").into_boxed_str(),
                    reason: "missing downloadURL".into(),
                });
                continue;
            }
        };

        let checksum: Box<str> = match res_entry.get("fileChecksum").and_then(Value::as_str) {
            Some(c) if !c.is_empty() => c.into(),
            Some(_) => {
                tracing::warn!(
                    asset = %record_name,
                    field = format_args!("{res_field}.fileChecksum"),
                    "Empty fileChecksum, skipping version"
                );
                malformed_resources.push(MalformedResource {
                    version_size: *key,
                    field: format!("{res_field}.fileChecksum").into_boxed_str(),
                    reason: "empty fileChecksum".into(),
                });
                continue;
            }
            None => {
                tracing::warn!(
                    asset = %record_name,
                    field = format_args!("{res_field}.fileChecksum"),
                    "Missing fileChecksum, skipping version"
                );
                malformed_resources.push(MalformedResource {
                    version_size: *key,
                    field: format!("{res_field}.fileChecksum").into_boxed_str(),
                    reason: "missing fileChecksum".into(),
                });
                continue;
            }
        };

        let asset_type: Arc<str> = match fields
            .get(type_field)
            .and_then(|f| f.get("value"))
            .and_then(Value::as_str)
        {
            Some(s) if !s.is_empty() => Arc::<str>::from(s),
            _ => {
                tracing::warn!(
                    asset = %record_name,
                    field = %type_field,
                    "Missing or empty asset type, skipping version"
                );
                malformed_resources.push(MalformedResource {
                    version_size: *key,
                    field: (*type_field).into(),
                    reason: "missing or empty asset type".into(),
                });
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
    (versions, malformed_resources)
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
    let parsed = url::Url::parse(raw).map_err(|_e| "malformed URL")?;
    #[cfg(test)]
    if parsed.scheme() == "http" && parsed.host_str().is_some_and(is_test_loopback_host) {
        return Ok(());
    }
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

#[cfg(test)]
fn is_test_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "[::1]" | "localhost")
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
        let (versions, malformed_resources) =
            extract_versions(item_type_val, &master_fields, &asset_fields, &record_name);
        let asset_metadata = Arc::new(metadata::extract(&master_fields, &asset_fields));
        let rendition_facts =
            metadata::extract_renditions(item_type_val, &master_fields, &asset_fields);
        let asset_record_name: Arc<str> = asset_record["recordName"]
            .as_str()
            .unwrap_or(record_name.as_ref())
            .into();
        Self {
            record_name,
            asset_record_name,
            state_record_name: None,
            source_zone: None,
            filename,
            asset_metadata,
            rendition_facts,
            item_type_val,
            asset_date_ms,
            added_date_ms,
            versions,
            malformed_resources: Arc::from(malformed_resources.into_boxed_slice()),
        }
    }

    /// Construct from typed `Record` structs (used by album pagination).
    pub fn from_records(master: super::cloudkit::Record, asset: &super::cloudkit::Record) -> Self {
        Self::from_records_in_zone(master, asset, None)
    }

    pub(crate) fn try_from_records(
        master: super::cloudkit::Record,
        asset: &super::cloudkit::Record,
        required: RequiredAssetFields,
    ) -> Result<Self, MalformedRequiredAssetField> {
        // CONTRACT: MALFORMED_REQUIRED_ASSET_FIELDS_BLOCK_CHECKPOINT
        if master.record_name.trim().is_empty() {
            return Err(MalformedRequiredAssetField::MasterRecordName);
        }
        if asset.record_name.trim().is_empty() {
            return Err(MalformedRequiredAssetField::AssetRecordName);
        }
        if required == RequiredAssetFields::Downloadable
            && asset
                .fields
                .get("assetDate")
                .and_then(|field| field.get("value"))
                .and_then(Value::as_f64)
                .and_then(f64_to_millis_datetime)
                .is_none()
        {
            return Err(MalformedRequiredAssetField::AssetDate);
        }
        Ok(Self::from_records(master, asset))
    }

    /// Construct from typed `Record` structs and pin the CloudKit source zone.
    pub(crate) fn from_records_in_zone(
        master: super::cloudkit::Record,
        asset: &super::cloudkit::Record,
        source_zone: Option<Arc<str>>,
    ) -> Self {
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
        let (versions, malformed_resources) = extract_versions(
            item_type_val,
            &master.fields,
            &asset.fields,
            &master.record_name,
        );
        let asset_metadata = Arc::new(metadata::extract(&master.fields, &asset.fields));
        let rendition_facts =
            metadata::extract_renditions(item_type_val, &master.fields, &asset.fields);
        Self {
            record_name: Arc::from(master.record_name),
            asset_record_name: Arc::from(asset.record_name.as_str()),
            state_record_name: None,
            source_zone,
            filename,
            asset_metadata,
            rendition_facts,
            item_type_val,
            asset_date_ms,
            added_date_ms,
            versions,
            malformed_resources: Arc::from(malformed_resources.into_boxed_slice()),
        }
    }

    /// Metadata extracted at construction time.
    pub fn metadata(&self) -> &AssetMetadata {
        &self.asset_metadata
    }

    /// Project the selected provider rendition. Unsupported keys retain
    /// shared fields but have no known dimensions or duration.
    #[must_use]
    pub fn metadata_arc(&self, version: VersionSizeKey) -> Arc<AssetMetadata> {
        let facts = self
            .rendition_facts
            .iter()
            .find(|(key, _)| *key == version)
            .map(|(_, facts)| facts);
        let mut metadata = self.asset_metadata.as_ref().clone();
        metadata.width = facts.and_then(|facts| facts.width);
        metadata.height = facts.and_then(|facts| facts.height);
        metadata.duration_secs = facts.and_then(|facts| facts.duration_secs);
        metadata.refresh_hash();
        Arc::new(metadata)
    }

    /// All supported resource facts, independent of downloadable URLs.
    pub fn rendition_facts(&self) -> &[(VersionSizeKey, RenditionMetadata)] {
        &self.rendition_facts
    }

    /// Shared asset fields; rendition projections replace all measurements.
    #[must_use]
    pub fn shared_metadata_arc(&self) -> Arc<AssetMetadata> {
        Arc::clone(&self.asset_metadata)
    }

    pub fn id(&self) -> &str {
        &self.record_name
    }

    /// Return the CPLAsset record name. Album membership records point at this
    /// value via `CPLContainerRelation.itemId`.
    pub(crate) fn asset_record_name(&self) -> &str {
        &self.asset_record_name
    }

    /// Return the CloudKit zone that produced this asset when known.
    pub(crate) fn source_zone(&self) -> Option<&str> {
        self.source_zone.as_deref()
    }

    pub(crate) fn with_source_zone(mut self, source_zone: Arc<str>) -> Self {
        self.source_zone = Some(source_zone);
        self
    }

    pub(crate) fn with_state_record_name(mut self, state_record_name: Arc<str>) -> Self {
        self.state_record_name = Some(state_record_name);
        self
    }

    /// True when the CloudKit record carried a usable recordName.
    ///
    /// An empty ID cannot safely key state rows, path fingerprints, retry
    /// accounting, or metadata rewrite markers, so production planners must
    /// filter it before deriving any download tasks.
    pub(crate) fn has_valid_id(&self) -> bool {
        !self.record_name.is_empty()
    }

    /// Local state/download identity for this asset.
    ///
    /// The download pipeline normalizes this to `CPLAsset.recordName`, which
    /// stays unique across sibling order and pagination changes. It can
    /// preserve a compatible legacy `CPLMaster.recordName` when durable
    /// mapping history identifies the child that owns that row.
    pub(crate) fn state_id(&self) -> &str {
        self.state_record_name
            .as_deref()
            .unwrap_or(self.record_name.as_ref())
    }

    pub(crate) fn state_id_arc(&self) -> Arc<str> {
        self.state_record_name
            .as_ref()
            .map_or_else(|| Arc::clone(&self.record_name), Arc::clone)
    }

    pub(crate) fn asset_record_name_arc(&self) -> Arc<str> {
        Arc::clone(&self.asset_record_name)
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

    /// Capture-local time reconstructed from Apple's instant and offset.
    ///
    /// Missing or invalid offsets retain host-local rendering.
    pub(crate) fn created_local(&self) -> DateTime<chrono::FixedOffset> {
        self.metadata().capture_local(self.created())
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

    pub(crate) fn malformed_resources(&self) -> &[MalformedResource] {
        &self.malformed_resources
    }

    /// Get a specific version by size key. Test-only convenience.
    #[cfg(test)]
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
    if let Some(is_deleted) = record.fields.get("isDeleted")
        && let Some(val) = is_deleted.get("value")
        && val.as_i64() == Some(1)
    {
        return ChangeReason::SoftDeleted;
    }

    // Hidden: fields.isHidden.value == 1
    if let Some(is_hidden) = record.fields.get("isHidden")
        && let Some(val) = is_hidden.get("value")
        && val.as_i64() == Some(1)
    {
        return ChangeReason::Hidden;
    }

    // Default: Created (new or modified record)
    ChangeReason::Created
}

/// Extract the `masterRef` record name from a `CPLAsset`'s fields.
pub(super) fn extract_master_ref(fields: &Value) -> Option<String> {
    fields
        .get("masterRef")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.get("recordName"))
        .and_then(|n| n.as_str())
        .map(std::string::ToString::to_string)
}

fn field_value_str<'a>(fields: &'a Value, name: &str) -> Option<&'a str> {
    fields
        .get(name)
        .and_then(|field| field.get("value"))
        .and_then(Value::as_str)
}

fn parse_album_delta(record: &Record, reason: ChangeReason) -> ChangeEvent {
    let is_deleted = matches!(
        reason,
        ChangeReason::HardDeleted | ChangeReason::SoftDeleted
    );
    let album_name = enc::decode_string(&record.fields, "albumNameEnc")
        .or_else(|| field_value_str(&record.fields, "albumName").map(ToOwned::to_owned))
        .unwrap_or_else(|| record.record_name.clone());
    let mut event = ChangeEvent::new(
        record.record_name.clone().into_boxed_str(),
        Some(record.record_type.clone().into_boxed_str()),
        reason,
    );
    event.album = Some(AlbumContainerDelta {
        container_id: record.record_name.clone().into_boxed_str(),
        album_name: crate::download::paths::sanitize_path_component(&album_name).into_boxed_str(),
        is_deleted,
    });
    event
}

fn parse_relation_record_name(record_name: &str) -> Option<(&str, &str)> {
    record_name
        .rsplit_once("-IN-")
        .filter(|(asset, container)| !asset.is_empty() && !container.is_empty())
}

fn parse_relation_delta(record: &Record, reason: ChangeReason) -> ChangeEvent {
    let is_deleted = matches!(
        reason,
        ChangeReason::HardDeleted | ChangeReason::SoftDeleted
    );
    let parsed = if is_deleted {
        parse_relation_record_name(&record.record_name)
            .map(|(asset, container)| (container.to_owned(), asset.to_owned()))
    } else {
        field_value_str(&record.fields, "containerId")
            .zip(field_value_str(&record.fields, "itemId"))
            .map(|(container, item)| (container.to_owned(), item.to_owned()))
    };

    match parsed {
        Some((container_id, asset_record_name)) => {
            let mut event = ChangeEvent::new(
                record.record_name.clone().into_boxed_str(),
                Some("CPLContainerRelation".into()),
                reason,
            );
            event.relation = Some(AlbumRelationDelta {
                container_id: container_id.into_boxed_str(),
                asset_record_name: asset_record_name.into_boxed_str(),
                is_deleted,
            });
            event
        }
        None => {
            let mut event = ChangeEvent::new(
                record.record_name.clone().into_boxed_str(),
                Some("CPLContainerRelation".into()),
                reason,
            );
            event.token_unsafe_reason = Some("unparsable_relation_delta");
            event
        }
    }
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
    pending_assets: FxHashMap<String, Vec<Record>>,
    /// Masters that have already paired with at least one asset in this delta
    /// stream. Keep them available so sibling `CPLAsset` records that share the
    /// same `CPLMaster` do not flush as orphaned metadata-only events.
    paired_masters: FxHashMap<String, Record>,
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

            if record.record_name.trim().is_empty() {
                let mut event = ChangeEvent::new(
                    record.record_name.into_boxed_str(),
                    (!record.record_type.is_empty()).then(|| record.record_type.into_boxed_str()),
                    reason,
                );
                event.token_unsafe_reason = Some(MALFORMED_REQUIRED_ASSET_FIELDS_REASON);
                events.push(event);
                continue;
            }

            match reason {
                ChangeReason::HardDeleted => {
                    if record.record_type == "CPLContainerRelation" {
                        events.push(parse_relation_delta(&record, reason));
                        continue;
                    }
                    if record.record_type == "CPLAlbum" {
                        events.push(parse_album_delta(&record, reason));
                        continue;
                    }
                    if parse_relation_record_name(&record.record_name).is_some() {
                        events.push(parse_relation_delta(&record, reason));
                        continue;
                    }
                    // Hard-deleted: no fields, can't tell if master or asset.
                    // Emit immediately as-is.
                    events.push(ChangeEvent::new(
                        record.record_name.into_boxed_str(),
                        None,
                        reason,
                    ));
                }
                _ => match record.record_type.as_str() {
                    "CPLMaster" => {
                        let master_name = record.record_name.clone();
                        if let Some(asset_records) = self.pending_assets.remove(&master_name) {
                            Self::emit_paired_group(
                                record.clone(),
                                asset_records,
                                reason,
                                &mut events,
                            );
                            self.paired_masters.insert(master_name, record);
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
                                Self::emit_paired(
                                    master_record.clone(),
                                    record,
                                    final_reason,
                                    false,
                                    &mut events,
                                );
                                self.paired_masters
                                    .insert(master_name.clone(), master_record);
                            } else if let Some(master_record) = self.paired_masters.get(master_name)
                            {
                                let master_reason = classify_change_reason(master_record);
                                let final_reason = Self::reconcile_reasons(master_reason, reason);
                                Self::emit_paired(
                                    master_record.clone(),
                                    record,
                                    final_reason,
                                    true,
                                    &mut events,
                                );
                            } else {
                                self.pending_assets
                                    .entry(master_name.clone())
                                    .or_default()
                                    .push(record);
                            }
                        } else {
                            // CPLAsset with no masterRef -- metadata-only change
                            events.push(ChangeEvent::new(
                                record.record_name.into_boxed_str(),
                                Some("CPLAsset".into()),
                                reason,
                            ));
                        }
                    }
                    "CPLAlbum" => {
                        events.push(parse_album_delta(&record, reason));
                    }
                    "CPLContainerRelation" => {
                        events.push(parse_relation_delta(&record, reason));
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
            events.push(ChangeEvent::new(
                name.into_boxed_str(),
                Some("CPLMaster".into()),
                reason,
            ));
        }

        for (master_ref, records) in self.pending_assets.drain() {
            for record in records {
                let reason = classify_change_reason(&record);
                let mut event = ChangeEvent::new(
                    record.record_name.into_boxed_str(),
                    Some("CPLAsset".into()),
                    reason,
                );
                event.master_record_name = Some(master_ref.clone().into_boxed_str());
                events.push(event);
            }
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
        if severity(a) >= severity(b) { a } else { b }
    }

    fn emit_paired(
        master_record: Record,
        asset_record: Record,
        reason: ChangeReason,
        use_asset_state_id: bool,
        events: &mut Vec<ChangeEvent>,
    ) {
        // Box::from(&str) copies only the bytes, without the String's Vec
        // capacity slack that .clone().into_boxed_str() would drag along.
        let master_name: Box<str> = Box::from(master_record.record_name.as_str());
        let required = if reason == ChangeReason::Created {
            RequiredAssetFields::Downloadable
        } else {
            RequiredAssetFields::Identity
        };
        let mut event = ChangeEvent::new(master_name, Some("CPLMaster".into()), reason);
        let mut asset = match PhotoAsset::try_from_records(master_record, &asset_record, required) {
            Ok(asset) => asset,
            Err(error) => {
                tracing::warn!(error = %error, "Malformed required CloudKit asset field");
                event.token_unsafe_reason = Some(MALFORMED_REQUIRED_ASSET_FIELDS_REASON);
                events.push(event);
                return;
            }
        };
        if use_asset_state_id {
            asset = asset.with_state_record_name(Arc::from(asset_record.record_name.as_str()));
        }
        event.asset = Some(asset);
        events.push(event);
    }

    fn emit_paired_group(
        master_record: Record,
        asset_records: Vec<Record>,
        master_reason: ChangeReason,
        events: &mut Vec<ChangeEvent>,
    ) {
        let sibling_count = asset_records.len();
        for (index, asset_record) in asset_records.into_iter().enumerate() {
            let reason =
                Self::reconcile_reasons(master_reason, classify_change_reason(&asset_record));
            Self::emit_paired(
                master_record.clone(),
                asset_record,
                reason,
                sibling_count > 1 && index > 0,
                events,
            );
        }
    }
}

impl Drop for DeltaRecordBuffer {
    fn drop(&mut self) {
        if !self.pending_masters.is_empty() || !self.pending_assets.is_empty() {
            tracing::warn!(
                orphaned_masters = self.pending_masters.len(),
                orphaned_assets = self.pending_assets.values().map(Vec::len).sum::<usize>(),
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

    fn rendition_assets(master_fields: Value, asset_fields: Value) -> [PhotoAsset; 2] {
        let master = json!({
            "recordName": "master", "recordType": "CPLMaster", "fields": master_fields,
        });
        let asset = json!({
            "recordName": "asset", "recordType": "CPLAsset", "fields": asset_fields,
        });
        [
            PhotoAsset::new(master.clone(), asset.clone()),
            PhotoAsset::from_records(
                serde_json::from_value(master).unwrap(),
                &serde_json::from_value(asset).unwrap(),
            ),
        ]
    }

    #[test]
    fn rendition_metadata_live_photo_dimensions_and_duration_without_urls() {
        for asset in rendition_assets(
            json!({
                "itemType": {"value": "public.heic"},
                "resOriginalWidth": {"value": 5712},
                "resOriginalHeight": {"value": 4284},
                "resOriginalVidComplWidth": {"value": 1744},
                "resOriginalVidComplHeight": {"value": 1308},
            }),
            json!({
                "duration": {"value": 0},
                "vidComplDurValue": {"type": "INT64", "value": 2300000000_u64},
                "vidComplDurScale": {"type": "INT64", "value": 1000000000},
                "isFavorite": {"value": 1},
                "captionEnc": {"type": "STRING", "value": "Live Photo"},
            }),
        ) {
            assert!(asset.versions().is_empty());
            assert_eq!(asset.rendition_facts().len(), PHOTO_VERSION_LOOKUP.len());
            let cloned = asset.clone();
            assert!(Arc::ptr_eq(&asset.rendition_facts, &cloned.rendition_facts));
            assert!(Arc::ptr_eq(
                &asset.shared_metadata_arc(),
                &cloned.shared_metadata_arc()
            ));
            let original = asset.metadata_arc(VersionSizeKey::Original);
            assert_eq!((original.width, original.height), (Some(5712), Some(4284)));
            assert_eq!(original.duration_secs, Some(0.0));
            assert_eq!(original.metadata_hash, asset.metadata().metadata_hash);
            let live = asset.metadata_arc(VersionSizeKey::LiveOriginal);
            assert_eq!((live.width, live.height), (Some(1744), Some(1308)));
            assert_eq!(live.duration_secs, Some(2.3));
            assert_ne!(original.metadata_hash, live.metadata_hash);
            let raw: Value = serde_json::from_str(live.provider_data.as_ref().unwrap()).unwrap();
            assert_eq!(raw["vidComplDurValue"]["value"], 2300000000_u64);
            assert_eq!(raw["vidComplDurScale"]["value"], 1000000000);
            for (key, _) in asset.rendition_facts() {
                let metadata = asset.metadata_arc(*key);
                assert!(metadata.is_favorite);
                assert_eq!(metadata.title.as_deref(), Some("Live Photo"));
                assert_eq!(metadata.provider_data, original.provider_data);
                assert_eq!(metadata.metadata_hash, Some(metadata.compute_hash()));
                assert!(!Arc::ptr_eq(&metadata, &asset.metadata_arc(*key)));
            }
            for key in [
                VersionSizeKey::LiveMedium,
                VersionSizeKey::LiveThumb,
                VersionSizeKey::LiveAdjusted,
            ] {
                let metadata = asset.metadata_arc(key);
                assert_eq!((metadata.width, metadata.height), (None, None));
                assert_eq!(metadata.duration_secs, Some(2.3));
            }
        }
    }

    #[test]
    fn rendition_metadata_uses_exact_photo_and_video_prefixes_and_resource_provenance() {
        let photo = [
            (VersionSizeKey::Original, "resOriginal"),
            (VersionSizeKey::Alternative, "resOriginalAlt"),
            (VersionSizeKey::Adjusted, "resJPEGFull"),
            (VersionSizeKey::Medium, "resJPEGMed"),
            (VersionSizeKey::Thumb, "resJPEGThumb"),
            (VersionSizeKey::LiveOriginal, "resOriginalVidCompl"),
            (VersionSizeKey::LiveAdjusted, "resVidCompl"),
            (VersionSizeKey::LiveMedium, "resVidMed"),
            (VersionSizeKey::LiveThumb, "resVidSmall"),
        ];
        let video = [
            (VersionSizeKey::Original, "resOriginal"),
            (VersionSizeKey::Adjusted, "resVidFull"),
            (VersionSizeKey::Medium, "resVidMed"),
            (VersionSizeKey::Thumb, "resVidSmall"),
        ];
        for (item_type, mappings) in [
            ("public.heic", photo.as_slice()),
            ("com.apple.quicktime-movie", video.as_slice()),
        ] {
            for (index, (key, prefix)) in mappings.iter().enumerate() {
                let width = u32::try_from(index).unwrap() + 100;
                let height = width + 10;
                let resource = json!({"value": {
                    "size": 1000, "fileChecksum": "checksum",
                    "downloadURL": "https://p01.icloud-content.com/rendition",
                }});
                for source in [
                    "absent",
                    "historical",
                    "master",
                    "asset",
                    "no_url",
                    "malformed",
                    "blank_checksum",
                ] {
                    let mut master = json!({"itemType": {"value": item_type}});
                    let mut asset_fields = json!({"duration": {"value": 12.5}});
                    master[format!("{prefix}Width")] = json!({"value": width});
                    master[format!("{prefix}Height")] = json!({"value": height});
                    if source != "historical" {
                        asset_fields[format!("{prefix}Width")] = json!({"value": width + 1000});
                        asset_fields[format!("{prefix}Height")] = json!({"value": height + 1000});
                    }
                    if !matches!(source, "absent" | "historical") {
                        master[format!("{prefix}Res")] = resource.clone();
                        master[format!("{prefix}FileType")] = json!({"value": item_type});
                    }
                    if matches!(source, "asset" | "no_url" | "malformed" | "blank_checksum") {
                        asset_fields[format!("{prefix}Res")] = match source {
                            "asset" => {
                                let mut selected = resource.clone();
                                selected["value"]["fileChecksum"] = json!("asset-checksum");
                                selected
                            }
                            "no_url" => {
                                json!({"value": {"size": 1000, "fileChecksum": "asset-checksum"}})
                            }
                            "blank_checksum" => json!({"value": {"fileChecksum": " \t"}}),
                            _ => json!({"value": null}),
                        };
                        asset_fields[format!("{prefix}FileType")] = json!({"value": item_type});
                    }
                    for asset in rendition_assets(master, asset_fields) {
                        assert_eq!(asset.rendition_facts().len(), mappings.len());
                        let cloned = asset.clone();
                        assert!(Arc::ptr_eq(&asset.rendition_facts, &cloned.rendition_facts));
                        let facts = &asset
                            .rendition_facts()
                            .iter()
                            .find(|(k, _)| k == key)
                            .unwrap()
                            .1;
                        let cloned_facts = facts.clone();
                        assert_eq!(&cloned_facts, facts);
                        if let Some(checksum) = &facts.checksum {
                            assert!(Arc::ptr_eq(
                                checksum,
                                cloned_facts.checksum.as_ref().unwrap()
                            ));
                        }
                        assert_eq!(
                            facts.checksum.as_deref(),
                            match source {
                                "master" => Some("checksum"),
                                "asset" | "no_url" => Some("asset-checksum"),
                                _ => None,
                            }
                        );
                        let metadata = asset.metadata_arc(*key);
                        let offset = if matches!(source, "master" | "historical") {
                            0
                        } else {
                            1000
                        };
                        assert_eq!(metadata.width, Some(width + offset), "{prefix}: {source}");
                        assert_eq!(metadata.height, Some(height + offset), "{prefix}: {source}");
                        assert_eq!(
                            asset.versions().len(),
                            usize::from(matches!(source, "master" | "asset"))
                        );
                        if item_type == "com.apple.quicktime-movie" {
                            assert_eq!(metadata.duration_secs, Some(12.5));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn rendition_metadata_rejects_missing_and_invalid_dimensions_without_cross_record_fallback() {
        for value in [
            json!(null),
            json!(0),
            json!(-1),
            json!(1.5),
            json!("1744"),
            json!(4294967296_u64),
        ] {
            for asset in rendition_assets(
                json!({
                    "itemType": {"value": "public.heic"},
                    "resOriginalWidth": {"value": 5712},
                    "resOriginalHeight": {"value": 4284},
                    "resOriginalVidComplWidth": {"value": 1744},
                    "resOriginalVidComplHeight": {"value": 1308},
                }),
                json!({
                    "resOriginalVidComplRes": {"value": {}},
                    "resOriginalVidComplWidth": {"value": value},
                }),
            ) {
                let live = asset.metadata_arc(VersionSizeKey::LiveOriginal);
                assert_eq!((live.width, live.height), (None, None));
                assert_eq!(live.duration_secs, None);
            }
        }
    }

    #[test]
    fn rendition_metadata_validates_companion_duration_without_asset_or_master_fallback() {
        for (value, scale, expected) in [
            (json!(null), json!(1000), None),
            (json!(2300), json!(null), None),
            (json!(2300), json!(0), None),
            (json!(2300), json!(-1000), None),
            (json!(-2300), json!(1000), None),
            (json!("2300"), json!(1000), None),
            (json!(2300), json!("1000"), None),
            (json!(true), json!(1000), None),
            (json!(2300), json!({}), None),
            (json!(1e308), json!(1e-308), None),
            (json!(0), json!(1000), Some(0.0)),
            (json!(2300), json!(1000), Some(2.3)),
        ] {
            let mut fields = json!({"duration": {"value": 99}});
            if !value.is_null() {
                fields["vidComplDurValue"] = json!({"value": value});
            }
            if !scale.is_null() {
                fields["vidComplDurScale"] = json!({"value": scale});
            }
            for asset in rendition_assets(
                json!({
                    "itemType": {"value": "public.heic"},
                    "vidComplDurValue": {"value": 2300},
                    "vidComplDurScale": {"value": 1000},
                }),
                fields,
            ) {
                for key in [
                    VersionSizeKey::LiveOriginal,
                    VersionSizeKey::LiveMedium,
                    VersionSizeKey::LiveThumb,
                    VersionSizeKey::LiveAdjusted,
                ] {
                    assert_eq!(asset.metadata_arc(key).duration_secs, expected);
                }
                assert_eq!(asset.metadata().duration_secs, Some(99.0));
                assert_eq!(
                    asset.metadata_arc(VersionSizeKey::Original).duration_secs,
                    Some(99.0)
                );
            }
        }
    }

    #[test]
    fn rendition_metadata_unsupported_key_does_not_inherit_original_measurements() {
        for asset in rendition_assets(
            json!({
                "itemType": {"value": "com.apple.quicktime-movie"},
                "resOriginalWidth": {"value": 1920},
                "resOriginalHeight": {"value": 1080},
            }),
            json!({"duration": {"value": 12.5}, "isFavorite": {"value": 1}}),
        ) {
            let metadata = asset.metadata_arc(VersionSizeKey::LiveOriginal);
            assert_eq!(
                (metadata.width, metadata.height, metadata.duration_secs),
                (None, None, None)
            );
            assert!(metadata.is_favorite);
            assert_eq!(metadata.metadata_hash, Some(metadata.compute_hash()));
            assert_eq!(asset.rendition_facts().len(), VIDEO_VERSION_LOOKUP.len());
        }
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

    /// A `filenameEnc` value that decodes to the empty string must
    /// yield `None`, not `Some("")`. Production routes filename-less assets
    /// through the fingerprint fallback; an empty string would slip past
    /// the `is_some()` check downstream and produce paths like
    /// `2026-04-19/` (a directory) or `.JPG` (a hidden file). Pin both
    /// the STRING and ENCRYPTED_BYTES variants — they hit the same
    /// post-decode early-return.
    #[test]
    fn empty_filename_string_rejected_at_deser() {
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": "", "type": "STRING"}}}),
            json!({}),
        );
        assert_eq!(
            asset.filename(),
            None,
            "empty STRING filename must be treated as missing"
        );
    }

    #[test]
    fn empty_filename_encrypted_bytes_rejected_at_deser() {
        use base64::Engine;
        // Base64 of the empty string is the empty string.
        let encoded = base64::engine::general_purpose::STANDARD.encode(b"");
        assert_eq!(encoded, "");
        let asset = make_asset(
            json!({"fields": {"filenameEnc": {"value": encoded, "type": "ENCRYPTED_BYTES"}}}),
            json!({}),
        );
        assert_eq!(
            asset.filename(),
            None,
            "empty ENCRYPTED_BYTES filename must be treated as missing"
        );
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
    fn test_versions_empty_checksum() {
        let asset = make_asset(
            json!({"fields": {
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "size": 1000,
                    "downloadURL": "https://p01.icloud-content.com/orig",
                    "fileChecksum": ""
                }},
                "resOriginalFileType": {"value": "public.jpeg"}
            }}),
            json!({"fields": {}}),
        );
        assert!(
            asset.versions().is_empty(),
            "empty checksum must not create a downloadable version"
        );
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

        let asset = PhotoAsset::from_records(master, &asset_rec);
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

        let asset = PhotoAsset::from_records(master, &asset_rec);
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
        // PhotoAsset should stay bounded enough for streaming without carrying
        // unnecessary parser payload.
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

    fn make_soft_deleted_asset_record(name: &str, master_ref: &str) -> Record {
        let mut record = make_asset_record(name, master_ref);
        record.fields["isDeleted"] = json!({"value": 1});
        record
    }

    #[test]
    fn required_downloadable_fields_reject_blank_identity_and_invalid_capture_date() {
        let cases = [
            (
                make_master_record(""),
                make_asset_record("A1", ""),
                MalformedRequiredAssetField::MasterRecordName,
            ),
            (
                make_master_record("M1"),
                make_asset_record("", "M1"),
                MalformedRequiredAssetField::AssetRecordName,
            ),
            (
                make_master_record("M1"),
                {
                    let mut asset = make_asset_record("A1", "M1");
                    asset.fields["assetDate"]["value"] = json!("not-a-timestamp");
                    asset
                },
                MalformedRequiredAssetField::AssetDate,
            ),
            (
                make_master_record("M1"),
                {
                    let mut asset = make_asset_record("A1", "M1");
                    asset
                        .fields
                        .as_object_mut()
                        .expect("asset fields object")
                        .remove("assetDate");
                    asset
                },
                MalformedRequiredAssetField::AssetDate,
            ),
            (
                make_master_record("M1"),
                {
                    let mut asset = make_asset_record("A1", "M1");
                    asset.fields["assetDate"]["value"] = json!(i64::MAX);
                    asset
                },
                MalformedRequiredAssetField::AssetDate,
            ),
        ];

        for (master, asset, expected) in cases {
            assert_eq!(
                PhotoAsset::try_from_records(master, &asset, RequiredAssetFields::Downloadable,)
                    .unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn identity_only_records_do_not_require_capture_date() {
        let master = make_master_record("M_DELETED");
        let mut asset = make_asset_record("A_DELETED", "M_DELETED");
        asset
            .fields
            .as_object_mut()
            .expect("asset fields object")
            .remove("assetDate");

        assert!(
            PhotoAsset::try_from_records(master, &asset, RequiredAssetFields::Identity).is_ok()
        );
    }

    #[test]
    fn delta_pair_with_invalid_capture_date_is_token_unsafe_not_downloadable() {
        let mut buffer = DeltaRecordBuffer::new();
        let master = make_master_record("M_BAD_DATE");
        let mut asset = make_asset_record("A_BAD_DATE", "M_BAD_DATE");
        asset.fields["assetDate"]["value"] = Value::Null;

        let events = buffer.process_records(vec![master, asset]);

        assert_eq!(events.len(), 1);
        assert!(events[0].asset.is_none());
        assert_eq!(
            events[0].token_unsafe_reason,
            Some(MALFORMED_REQUIRED_ASSET_FIELDS_REASON)
        );
    }

    #[test]
    fn delta_blank_record_identity_is_token_unsafe() {
        let mut buffer = DeltaRecordBuffer::new();

        let events = buffer.process_records(vec![make_master_record("")]);

        assert_eq!(events.len(), 1);
        assert!(events[0].asset.is_none());
        assert_eq!(
            events[0].token_unsafe_reason,
            Some(MALFORMED_REQUIRED_ASSET_FIELDS_REASON)
        );
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
    fn test_buffer_sibling_assets_share_master_without_clobbering() {
        let mut buffer = DeltaRecordBuffer::new();

        let events = buffer.process_records(vec![
            make_asset_record("A2", "M1"),
            make_asset_record("A1", "M1"),
            make_master_record("M1"),
        ]);

        assert_eq!(events.len(), 2);
        let assets: Vec<_> = events
            .iter()
            .map(|event| event.asset.as_ref().expect("paired asset"))
            .collect();
        assert_eq!(assets[0].asset_record_name(), "A2");
        assert_eq!(assets[0].id(), "M1");
        assert_eq!(assets[0].state_id(), "M1");
        assert_eq!(assets[1].asset_record_name(), "A1");
        assert_eq!(assets[1].id(), "M1");
        assert_eq!(assets[1].state_id(), "A1");
    }

    #[test]
    fn test_buffer_sibling_asset_reasons_stay_per_asset() {
        let mut buffer = DeltaRecordBuffer::new();

        let events = buffer.process_records(vec![
            make_soft_deleted_asset_record("A2", "M1"),
            make_asset_record("A1", "M1"),
            make_master_record("M1"),
        ]);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].asset.as_ref().unwrap().asset_record_name(), "A2");
        assert_eq!(events[0].reason, ChangeReason::SoftDeleted);
        assert_eq!(events[1].asset.as_ref().unwrap().asset_record_name(), "A1");
        assert_eq!(events[1].reason, ChangeReason::Created);
    }

    #[test]
    fn test_buffer_late_sibling_asset_pairs_with_seen_master() {
        let mut buffer = DeltaRecordBuffer::new();

        let first = buffer.process_records(vec![
            make_master_record("M1"),
            make_asset_record("A1", "M1"),
        ]);
        assert_eq!(first.len(), 1);

        let second = buffer.process_records(vec![make_asset_record("A2", "M1")]);
        assert_eq!(second.len(), 1);
        let asset = second[0].asset.as_ref().expect("late sibling asset");
        assert_eq!(asset.id(), "M1");
        assert_eq!(asset.asset_record_name(), "A2");
        assert_eq!(asset.state_id(), "A2");
        assert!(buffer.flush().is_empty());
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
    fn test_buffer_emits_photo_album_and_relation_records() {
        let mut buffer = DeltaRecordBuffer::new();

        let album_record = Record {
            record_name: "ALBUM_1".to_string(),
            record_type: "CPLAlbum".to_string(),
            fields: json!({"albumName": {"value": "Vacation"}}),
            deleted: None,
        };
        let relation_record = Record {
            record_name: "ASSET_1-IN-ALBUM_1".to_string(),
            record_type: "CPLContainerRelation".to_string(),
            fields: json!({
                "containerId": {"value": "ALBUM_1"},
                "itemId": {"value": "ASSET_1"}
            }),
            deleted: None,
        };

        let events = buffer.process_records(vec![
            make_master_record("M1"),
            make_asset_record("A1", "M1"),
            album_record,
            relation_record,
        ]);
        assert_eq!(events.len(), 3);
        assert!(events[0].asset.is_some());
        assert_eq!(
            events[1].album,
            Some(AlbumContainerDelta {
                container_id: "ALBUM_1".into(),
                album_name: "Vacation".into(),
                is_deleted: false,
            })
        );
        assert_eq!(
            events[2].relation,
            Some(AlbumRelationDelta {
                container_id: "ALBUM_1".into(),
                asset_record_name: "ASSET_1".into(),
                is_deleted: false,
            })
        );

        // Flush should still be empty since album/relation records are not buffered.
        let flushed = buffer.flush();
        assert!(flushed.is_empty());
    }

    #[test]
    fn test_buffer_hard_deleted_relation_parses_record_name() {
        let mut buffer = DeltaRecordBuffer::new();
        let relation_delete = Record {
            record_name: "ASSET_1-IN-ALBUM_1".to_string(),
            record_type: "CPLContainerRelation".to_string(),
            fields: json!({}),
            deleted: Some(true),
        };

        let events = buffer.process_records(vec![relation_delete]);
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].relation,
            Some(AlbumRelationDelta {
                container_id: "ALBUM_1".into(),
                asset_record_name: "ASSET_1".into(),
                is_deleted: true,
            })
        );
        assert_eq!(events[0].token_unsafe_reason, None);
    }

    #[test]
    fn test_buffer_unparsable_hard_deleted_relation_is_token_unsafe() {
        let mut buffer = DeltaRecordBuffer::new();
        let relation_delete = Record {
            record_name: "not-a-relation-name".to_string(),
            record_type: "CPLContainerRelation".to_string(),
            fields: json!({}),
            deleted: Some(true),
        };

        let events = buffer.process_records(vec![relation_delete]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].relation, None);
        assert_eq!(
            events[0].token_unsafe_reason,
            Some("unparsable_relation_delta")
        );
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
                    assert_eq!(event.master_record_name.as_deref(), Some("M_MISSING"));
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
        assert_eq!(events[0].master_record_name, None);
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
        assert_eq!(events[0].master_record_name, None);
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
    fn photo_asset_null_record_name_is_not_valid_id() {
        // recordName is JSON null rather than missing entirely
        let asset = make_asset(json!({"recordName": null}), json!({}));
        assert_eq!(asset.id(), "");
        assert!(
            !asset.has_valid_id(),
            "null recordName must not be considered downloadable"
        );
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
    fn asset_empty_record_name_is_not_valid_id() {
        let asset = make_asset(json!({"recordName": ""}), json!({}));
        assert_eq!(
            asset.id(),
            "",
            "empty recordName must be reflected as empty id for diagnostics"
        );
        assert!(
            !asset.has_valid_id(),
            "empty recordName must be filtered before path planning"
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

        let asset = PhotoAsset::from_records(master, &asset_rec);
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
        assert_eq!(asset.malformed_resources().len(), 1);
        assert_eq!(
            asset.malformed_resources()[0].field.as_ref(),
            "resOriginalRes"
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
        assert_eq!(
            asset.malformed_resources()[0].field.as_ref(),
            "resOriginalRes.size"
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
    fn validate_download_url_accepts_loopback_http_for_offline_tests() {
        for u in [
            "http://127.0.0.1:12345/photo.jpg",
            "http://localhost:12345/photo.jpg",
            "http://[::1]:12345/photo.jpg",
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
