use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
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

/// Sparse-share metadata is evidence, not a resolved master or deletion proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SparseShareEvidence {
    /// A sparse marker and structurally valid share link. The target is unverified.
    Linked(SparseShareLink),
    /// Share metadata was present but its marker, field types, or link was invalid.
    Malformed,
}

#[derive(Clone, PartialEq, Eq)]
struct SparseShareId(Box<str>);

/// An opaque, unverified share link. Debug output never includes provider IDs.
#[derive(Clone, PartialEq, Eq)]
pub struct SparseShareLink {
    record_name: SparseShareId,
    zone_name: SparseShareId,
    owner_record_name: SparseShareId,
}

impl std::fmt::Debug for SparseShareLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SparseShareLink(<redacted>)")
    }
}

impl SparseShareLink {
    /// The linked record's zone and record name.
    pub(crate) fn target(&self) -> (&str, &str) {
        (&self.zone_name.0, &self.record_name.0)
    }
}

impl SparseShareEvidence {
    pub(crate) fn from_fields(fields: &Value) -> Option<Self> {
        let has_link = [
            "linkedShareRecordName",
            "linkedShareZoneName",
            "linkedShareZoneOwner",
        ]
        .iter()
        .any(|key| fields.get(key).is_some());
        let marker = fields.get("isSparsePrivateRecord");
        let marker_value = marker
            .filter(|field| {
                field
                    .get("type")
                    .is_none_or(|kind| matches!(kind.as_str(), Some("INT64" | "NUMBER_INT64")))
            })
            .and_then(|field| field.get("value"))
            .and_then(Value::as_i64);
        if !has_link && (marker.is_none() || marker_value == Some(0)) {
            return None;
        }
        fn field<'a>(fields: &'a Value, key: &str, kind: &str) -> Option<&'a Value> {
            let field = fields.get(key)?;
            if field
                .get("type")
                .is_some_and(|value| value.as_str() != Some(kind))
            {
                return None;
            }
            field.get("value")
        }
        fn identifier(value: Option<&Value>) -> Option<SparseShareId> {
            value
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|s| SparseShareId(s.into()))
        }
        let link = (|| {
            if marker_value != Some(1) {
                return None;
            }
            let record_name = identifier(field(fields, "linkedShareRecordName", "STRING"))?;
            let zone_name = identifier(field(fields, "linkedShareZoneName", "STRING"))?;
            let owner_record_name =
                identifier(field(fields, "linkedShareZoneOwner", "REFERENCE")?.get("recordName"))?;
            if zone_name
                .0
                .strip_prefix("SharedSync-")
                .is_none_or(str::is_empty)
            {
                return None;
            }
            Some(SparseShareLink {
                record_name,
                zone_name,
                owner_record_name,
            })
        })();
        Some(link.map_or(Self::Malformed, Self::Linked))
    }
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
#[allow(
    dead_code,
    reason = "metadata writers wait for the opt-in metadata pass; it removes this allow"
)]
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
    adjusted_still_modified: Option<DateTime<Utc>>,
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
/// Placeholder URL scheme for versions rebuilt from catalog records. It is
/// never fetched: transfers use URLs from a fresh record lookup.
pub(crate) const STORED_URL_PREFIX: &str = "kei-stored:";

/// `require_url` is false only for catalog records, which never retain
/// download URLs. Their versions carry an empty URL and are refreshed by
/// record lookup before any transfer.
fn extract_versions_with(
    item_type: Option<AssetItemType>,
    master_fields: &Value,
    asset_fields: &Value,
    record_name: &str,
    require_url: bool,
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
            None if !require_url => Box::from(""),
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

        // Stored versions stand in for their URL with the resource checksum,
        // so path derivation still tells distinct resources apart.
        let url = if url.is_empty() {
            format!("{STORED_URL_PREFIX}{checksum}").into_boxed_str()
        } else {
            url
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
pub(crate) fn validate_download_url(raw: &str) -> Result<(), &'static str> {
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

#[allow(
    dead_code,
    reason = "metadata writers wait for the opt-in metadata pass; it removes this allow"
)]
impl PhotoAsset {
    /// Only a current adjusted still sourced from CPLAsset has proven edit time.
    /// Original, alternative and video resource clocks remain unknown.
    pub(crate) fn media_modified_at(&self, version: VersionSizeKey) -> Option<DateTime<Utc>> {
        (version == VersionSizeKey::Adjusted
            && self.versions.iter().any(|(key, resource)| {
                *key == AssetVersionSize::Adjusted
                    && item_type_from_str(&resource.asset_type) == Some(AssetItemType::Image)
            }))
        .then_some(self.adjusted_still_modified)
        .flatten()
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

    /// Provider date evidence without the display/path fallback to the epoch.
    pub(crate) fn added_date_evidence(&self) -> Option<DateTime<Utc>> {
        self.added_date_ms.and_then(f64_to_millis_datetime)
    }

    pub fn added_date(&self) -> DateTime<Utc> {
        self.added_date_evidence()
            .unwrap_or_else(|| {
                tracing::warn!(asset_id = %self.record_name, "Missing or invalid addedDate, falling back to epoch");
                DateTime::UNIX_EPOCH
            })
    }

    pub(crate) fn malformed_resources(&self) -> &[MalformedResource] {
        &self.malformed_resources
    }

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
        let adjusted_still_modified = (item_type_val != Some(AssetItemType::Movie)
            && asset_fields.get("resJPEGFullRes").is_some())
        .then(|| enc::decode_adjustment_timestamp(&asset_fields))
        .flatten();
        let (versions, malformed_resources) = extract_versions_with(
            item_type_val,
            &master_fields,
            &asset_fields,
            &record_name,
            true,
        );
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
            adjusted_still_modified,
            versions,
            malformed_resources: Arc::from(malformed_resources.into_boxed_slice()),
        }
    }

    /// Construct from typed `Record` structs (used by album pagination).
    pub fn from_records(master: super::cloudkit::Record, asset: &super::cloudkit::Record) -> Self {
        Self::from_records_in_zone(master, asset, None)
    }

    /// Construct from typed `Record` structs and pin the CloudKit source zone.
    pub(crate) fn from_records_in_zone(
        master: super::cloudkit::Record,
        asset: &super::cloudkit::Record,
        source_zone: Option<Arc<str>>,
    ) -> Self {
        Self::build(master, asset, source_zone, true)
    }

    /// Rebuild an asset from catalog records stored without download URLs.
    /// Its versions carry empty URLs and must not be used for transfer.
    pub(crate) fn from_stored_records(
        master: super::cloudkit::Record,
        asset: &super::cloudkit::Record,
    ) -> Self {
        Self::build(master, asset, None, false)
    }

    fn build(
        master: super::cloudkit::Record,
        asset: &super::cloudkit::Record,
        source_zone: Option<Arc<str>>,
        require_url: bool,
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
        let (versions, malformed_resources) = extract_versions_with(
            item_type_val,
            &master.fields,
            &asset.fields,
            &master.record_name,
            require_url,
        );
        let asset_metadata = Arc::new(metadata::extract(&master.fields, &asset.fields));
        let rendition_facts =
            metadata::extract_renditions(item_type_val, &master.fields, &asset.fields);
        let adjusted_still_modified = (item_type_val != Some(AssetItemType::Movie)
            && asset.fields.get("resJPEGFullRes").is_some())
        .then(|| enc::decode_adjustment_timestamp(&asset.fields))
        .flatten();
        Self {
            record_name: Arc::from(master.record_name),
            asset_record_name: Arc::from(asset.record_name.as_str()),
            state_record_name: None,
            source_zone,
            filename,
            adjusted_still_modified,
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

    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    /// Provider capture-date evidence without the display/path epoch fallback.
    pub(crate) fn asset_date_evidence(&self) -> Option<DateTime<Utc>> {
        self.asset_date_ms.and_then(f64_to_millis_datetime)
    }

    pub fn asset_date(&self) -> DateTime<Utc> {
        self.asset_date_evidence()
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

    pub fn item_type(&self) -> Option<AssetItemType> {
        self.item_type_val
    }

    /// Available download versions, as a list of (size, version) pairs.
    /// Pre-parsed at construction so no JSON traversal happens at download time.
    pub fn versions(&self) -> &VersionsMap {
        &self.versions
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
/// Hard deletion takes precedence over trash, then hidden status. Otherwise
/// return `Created`: distinguishing new, restored, and unhidden records requires
/// the caller's prior state.
pub(crate) fn classify_change_reason(record: &Record) -> ChangeReason {
    // Hard delete: record.deleted == true
    if record.deleted == Some(true) {
        return ChangeReason::HardDeleted;
    }

    if metadata::change_flag_is_set(&record.fields, "isDeleted") {
        return ChangeReason::SoftDeleted;
    }

    if metadata::change_flag_is_set(&record.fields, "isHidden") {
        return ChangeReason::Hidden;
    }

    // Default: Created (new or modified record)
    ChangeReason::Created
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sparse_share_marker_accepts_only_exact_integer_type_aliases() {
        let original = crate::test_helpers::sparse_shared_asset_record();
        let expected = SparseShareEvidence::from_fields(&{
            let mut fields = original["fields"].clone();
            fields["isSparsePrivateRecord"]["type"] = json!("INT64");
            fields
        });
        assert!(matches!(expected, Some(SparseShareEvidence::Linked(_))));
        for kind in [Some("INT64"), Some("NUMBER_INT64"), None] {
            let mut fields = original["fields"].clone();
            let marker = fields["isSparsePrivateRecord"].as_object_mut().unwrap();
            match kind {
                Some(kind) => {
                    marker.insert("type".into(), json!(kind));
                }
                None => {
                    marker.remove("type");
                }
            }
            let mut marker_only = json!({"isSparsePrivateRecord": marker});
            assert_eq!(
                SparseShareEvidence::from_fields(&fields),
                expected,
                "{kind:?}"
            );
            assert_eq!(
                SparseShareEvidence::from_fields(&marker_only),
                Some(SparseShareEvidence::Malformed),
                "marker alone is not a link: {kind:?}"
            );
            marker_only["isSparsePrivateRecord"]["value"] = json!(0);
            assert_eq!(
                SparseShareEvidence::from_fields(&marker_only),
                None,
                "zero without a link: {kind:?}"
            );
        }
    }

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
    fn media_edit_time_is_only_asset_owned_adjusted_still() {
        use base64::Engine as _;
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut bytes = Vec::new();
        plist::Value::Date(time.into())
            .to_writer_binary(&mut bytes)
            .unwrap();
        let resource = json!({"value":{"size":10,"downloadURL":"https://p01.icloud-content.com/edit.jpg","fileChecksum":"edited"}});
        let fields = json!({
            "resJPEGFullRes":resource,
            "resJPEGFullFileType":{"value":"public.jpeg"},
            "adjustmentTimestampEnc":{"type":"ENCRYPTED_BYTES","value":base64::engine::general_purpose::STANDARD.encode(bytes)}
        });
        for asset in rendition_assets(json!({"itemType":{"value":"public.jpeg"}}), fields.clone()) {
            assert_eq!(
                asset.media_modified_at(VersionSizeKey::Adjusted),
                Some(time.into())
            );
            for key in [
                VersionSizeKey::Original,
                VersionSizeKey::Alternative,
                VersionSizeKey::LiveAdjusted,
                VersionSizeKey::LiveOriginal,
            ] {
                assert_eq!(asset.media_modified_at(key), None);
            }
        }
        for asset in rendition_assets(fields.clone(), json!({})) {
            assert_eq!(asset.media_modified_at(VersionSizeKey::Adjusted), None);
        }
        for asset in rendition_assets(json!({"itemType":{"value":"public.mpeg-4"}}), fields) {
            assert_eq!(asset.media_modified_at(VersionSizeKey::Adjusted), None);
        }
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
    fn classify_change_flags_agree_with_asset_metadata() {
        for (value, asserted) in [
            (json!(true), true),
            (json!(1), true),
            (json!(false), false),
            (json!(0), false),
            (json!(2), false),
            (json!("true"), false),
        ] {
            for (field, reason) in [
                ("isDeleted", ChangeReason::SoftDeleted),
                ("isHidden", ChangeReason::Hidden),
            ] {
                let record = make_record("CPLAsset", json!({(field): {"value": value}}), None);
                let meta = metadata::extract(&json!({}), &record.fields);
                assert_eq!(meta.is_deleted, asserted && field == "isDeleted");
                assert_eq!(meta.is_hidden, asserted && field == "isHidden");
                let expected = if asserted {
                    reason
                } else {
                    ChangeReason::Created
                };
                assert_eq!(
                    classify_change_reason(&record),
                    expected,
                    "{field}: {value}"
                );
            }
        }
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

    // --- DeltaRecordBuffer tests ---

    // --- Gap tests: API response handling robustness ---

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
        assert_eq!(asset.added_date_evidence(), None);
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
            "blob:https://p01.icloud-content.com/path",
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
