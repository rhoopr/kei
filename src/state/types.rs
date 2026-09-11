//! Types for the state tracking module.

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, FixedOffset, Local, Utc};

use crate::types::AssetVersionSize;

/// Revision of the catalogue metadata semantics produced by the current
/// binary. Bump this when decoder or capture behavior changes in a way that
/// requires existing catalogue rows to be hydrated again.
pub(crate) const METADATA_CAPTURE_REVISION: i64 = 1;

/// Version size key for state tracking.
///
/// This is a 1-byte enum representing the version size, saving ~23 bytes
/// per `AssetRecord` compared to storing as a String.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VersionSizeKey {
    Original = 0,
    Medium = 1,
    Thumb = 2,
    Adjusted = 3,
    Alternative = 4,
    LiveOriginal = 5,
    LiveMedium = 6,
    LiveThumb = 7,
    LiveAdjusted = 8,
}

impl VersionSizeKey {
    /// Convert to the string stored in the database.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Medium => "medium",
            Self::Thumb => "thumb",
            Self::Adjusted => "adjusted",
            Self::Alternative => "alternative",
            Self::LiveOriginal => "live_original",
            Self::LiveMedium => "live_medium",
            Self::LiveThumb => "live_thumb",
            Self::LiveAdjusted => "live_adjusted",
        }
    }

    /// Return true for the still/video version that owns the local filename stem.
    pub(crate) const fn is_primary_media(self) -> bool {
        matches!(self, Self::Original | Self::Medium | Self::Thumb)
    }

    /// Return true for Live Photo motion versions derived from the primary stem.
    pub(crate) const fn is_live_photo_motion(self) -> bool {
        matches!(
            self,
            Self::LiveOriginal | Self::LiveMedium | Self::LiveThumb
        )
    }

    /// Parse from the string stored in the database.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "original" => Some(Self::Original),
            "medium" => Some(Self::Medium),
            "thumb" => Some(Self::Thumb),
            "adjusted" => Some(Self::Adjusted),
            "alternative" => Some(Self::Alternative),
            "live_original" | "liveoriginal" => Some(Self::LiveOriginal),
            "live_medium" | "livemedium" => Some(Self::LiveMedium),
            "live_thumb" | "livethumb" => Some(Self::LiveThumb),
            "live_adjusted" | "liveadjusted" => Some(Self::LiveAdjusted),
            _ => None,
        }
    }
}

impl From<AssetVersionSize> for VersionSizeKey {
    fn from(v: AssetVersionSize) -> Self {
        match v {
            AssetVersionSize::Original => Self::Original,
            AssetVersionSize::Medium => Self::Medium,
            AssetVersionSize::Thumb => Self::Thumb,
            AssetVersionSize::Adjusted => Self::Adjusted,
            AssetVersionSize::Alternative => Self::Alternative,
            AssetVersionSize::LiveOriginal => Self::LiveOriginal,
            AssetVersionSize::LiveMedium => Self::LiveMedium,
            AssetVersionSize::LiveThumb => Self::LiveThumb,
            AssetVersionSize::LiveAdjusted => Self::LiveAdjusted,
        }
    }
}

/// Status of an asset in the state database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetStatus {
    /// Asset has been seen but not yet downloaded.
    Pending,
    /// Asset is live at the provider but excluded by the active download policy.
    PolicyExcluded,
    /// Asset has been successfully downloaded.
    Downloaded,
    /// Asset download failed (will be retried).
    Failed,
}

impl AssetStatus {
    /// Convert to the string stored in the database.
    #[cfg(test)]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::PolicyExcluded => "policy_excluded",
            Self::Downloaded => "downloaded",
            Self::Failed => "failed",
        }
    }

    /// Parse from the string stored in the database.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "policy_excluded" => Some(Self::PolicyExcluded),
            "downloaded" => Some(Self::Downloaded),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Media type of an asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Photo,
    Video,
    LivePhotoImage,
    LivePhotoVideo,
}

impl MediaType {
    /// Convert to the string stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::LivePhotoImage => "live_photo_image",
            Self::LivePhotoVideo => "live_photo_video",
        }
    }

    /// Parse from the string stored in the database.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "photo" => Some(Self::Photo),
            "video" => Some(Self::Video),
            "live_photo_image" => Some(Self::LivePhotoImage),
            "live_photo_video" => Some(Self::LivePhotoVideo),
            _ => None,
        }
    }

    /// True for `Photo` and the still-image side of a Live Photo. Used by
    /// sync stats to bucket the cycle's downloads as "photos" vs "videos"
    /// without each call site re-spelling the match.
    pub fn is_photo_like(self) -> bool {
        matches!(self, Self::Photo | Self::LivePhotoImage)
    }

    /// True for `Video` and the moving side of a Live Photo. Mirrors
    /// `is_photo_like`; the two are exhaustive over `MediaType`.
    pub fn is_video_like(self) -> bool {
        matches!(self, Self::Video | Self::LivePhotoVideo)
    }
}

/// Metadata captured for an asset.
///
/// Every field is optional or has a safe default: CloudKit parsing populates
/// what Apple returns, and consumers handle missing values.
///
/// `metadata_hash` is computed from the metadata fields and stored alongside
/// them so that incremental sync can detect metadata-only changes in O(1).
#[derive(Debug, Clone, Default)]
pub struct AssetMetadata {
    /// Source label for this record. Currently "icloud".
    /// Uses `Arc<str>` so cloned records can share string storage without
    /// carrying `String` capacity.
    pub source: Option<Arc<str>>,
    /// Favorite/heart flag from iCloud Photos.
    pub is_favorite: bool,
    /// 1-5 star rating. Boolean favorites set 5.
    pub rating: Option<u8>,
    /// Latitude in decimal degrees, WGS84.
    pub latitude: Option<f64>,
    /// Longitude in decimal degrees, WGS84.
    pub longitude: Option<f64>,
    /// Altitude in meters above sea level.
    pub altitude: Option<f64>,
    /// EXIF orientation (1-8).
    pub orientation: Option<u8>,
    /// Duration in seconds (video / live photo).
    pub duration_secs: Option<f64>,
    /// Timezone offset in seconds from UTC.
    pub timezone_offset: Option<i32>,
    /// Width in pixels.
    pub width: Option<u32>,
    /// Height in pixels.
    pub height: Option<u32>,
    /// Short title / caption.
    pub title: Option<String>,
    /// JSON array of keyword strings.
    pub keywords: Option<String>,
    /// Longer description / notes.
    pub description: Option<String>,
    /// Subtype enum: screenshot, panorama, hdr, burst, timelapse, slo_mo, etc.
    pub media_subtype: Option<String>,
    /// Groups burst shots together.
    pub burst_id: Option<String>,
    /// Hidden from main timeline.
    pub is_hidden: bool,
    /// Archived (hidden from main timeline but retained).
    pub is_archived: bool,
    /// When metadata was last edited at source.
    pub modified_at: Option<DateTime<Utc>>,
    /// Soft-deleted at source.
    pub is_deleted: bool,
    /// When the asset was deleted/expunged at source.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Opaque JSON blob for source-native fields that don't fit the
    /// canonical schema (invariant 4: capture everything available).
    pub provider_data: Option<String>,
    /// SHA-256 of the metadata fields above, for change detection.
    pub metadata_hash: Option<String>,
}

/// Provider resource identity and measurements, without shared asset metadata.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenditionMetadata {
    pub checksum: Option<Arc<str>>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_secs: Option<f64>,
}

/// Shared provider metadata and candidates already mapped to logical versions.
#[derive(Clone, Debug)]
pub struct MetadataCapture {
    pub shared: Arc<AssetMetadata>,
    pub renditions: Arc<[(VersionSizeKey, RenditionMetadata)]>,
}

impl MetadataCapture {
    /// Resolve only exact, nonblank resource identities with unanimous measurements.
    #[must_use]
    pub(crate) fn resolve(&self, version: VersionSizeKey, checksum: &str) -> AssetMetadata {
        let mut matching = self.renditions.iter().filter_map(|(key, facts)| {
            (*key == version
                && !checksum.trim().is_empty()
                && facts.checksum.as_deref() == Some(checksum))
            .then_some(facts)
        });
        let facts = matching
            .next()
            .filter(|first| matching.all(|other| *first == other));
        let mut metadata = self.shared.as_ref().clone();
        metadata.width = facts.and_then(|facts| facts.width);
        metadata.height = facts.and_then(|facts| facts.height);
        metadata.duration_secs = facts.and_then(|facts| facts.duration_secs);
        metadata.refresh_hash();
        metadata
    }
}

impl AssetMetadata {
    /// Compute a stable SHA-256 hash of metadata fields for change detection.
    ///
    /// Does not include `source` (immutable per record) or `metadata_hash`
    /// itself (the output). Uses a pipe-delimited, tagged encoding so that
    /// adding None fields or empty strings cannot collide.
    pub fn compute_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        let h = &mut hasher;
        hash_bool(h, "fav", self.is_favorite);
        // Bind formatted values so their `&str` lives long enough to pass
        // through `hash_opt`. The String-typed fields below go in directly
        // as `&str` — no clone.
        let rat = self.rating.map(|v| v.to_string());
        let lat = self.latitude.map(format_f64);
        let lng = self.longitude.map(format_f64);
        let alt = self.altitude.map(format_f64);
        let ori = self.orientation.map(|v| v.to_string());
        let dur = self.duration_secs.map(format_f64);
        let tzo = self.timezone_offset.map(|v| v.to_string());
        let w = self.width.map(|v| v.to_string());
        let hh = self.height.map(|v| v.to_string());
        let m_mod = self.modified_at.map(|dt| dt.timestamp().to_string());
        let delat = self.deleted_at.map(|dt| dt.timestamp().to_string());
        hash_opt(h, "rat", rat.as_deref());
        hash_opt(h, "lat", lat.as_deref());
        hash_opt(h, "lng", lng.as_deref());
        hash_opt(h, "alt", alt.as_deref());
        hash_opt(h, "ori", ori.as_deref());
        hash_opt(h, "dur", dur.as_deref());
        hash_opt(h, "tzo", tzo.as_deref());
        hash_opt(h, "w", w.as_deref());
        hash_opt(h, "hh", hh.as_deref());
        hash_opt(h, "tit", self.title.as_deref());
        hash_opt(h, "kw", self.keywords.as_deref());
        hash_opt(h, "desc", self.description.as_deref());
        hash_opt(h, "sub", self.media_subtype.as_deref());
        hash_opt(h, "bur", self.burst_id.as_deref());
        hash_bool(h, "hid", self.is_hidden);
        hash_bool(h, "arc", self.is_archived);
        hash_opt(h, "mod", m_mod.as_deref());
        hash_bool(h, "del", self.is_deleted);
        hash_opt(h, "delat", delat.as_deref());
        hash_opt(h, "pd", self.provider_data.as_deref());
        let digest = hasher.finalize();
        data_encoding::HEXLOWER.encode(&digest)
    }

    /// Populate `metadata_hash` from the current field values.
    pub fn refresh_hash(&mut self) {
        self.metadata_hash = Some(self.compute_hash());
    }

    /// Return Apple's offset when it can be represented by chrono.
    pub(crate) fn valid_timezone_offset(&self) -> Option<FixedOffset> {
        self.timezone_offset.and_then(FixedOffset::east_opt)
    }

    /// Resolve an asset instant to its capture-local wall clock.
    ///
    /// Missing or invalid offsets retain the host-local rendering used when
    /// Apple supplies no capture timezone.
    pub(crate) fn capture_local(&self, created_at: DateTime<Utc>) -> DateTime<FixedOffset> {
        self.valid_timezone_offset().map_or_else(
            || created_at.with_timezone(&Local).fixed_offset(),
            |offset| created_at.with_timezone(&offset),
        )
    }
}

fn hash_opt(hasher: &mut sha2::Sha256, tag: &str, value: Option<&str>) {
    use sha2::Digest;
    hasher.update(tag.as_bytes());
    hasher.update(b"|");
    match value {
        Some(v) => {
            hasher.update(b"S|");
            hasher.update(v.as_bytes());
        }
        None => hasher.update(b"N"),
    }
    hasher.update(b"\x1f");
}

fn hash_bool(hasher: &mut sha2::Sha256, tag: &str, value: bool) {
    use sha2::Digest;
    hasher.update(tag.as_bytes());
    hasher.update(b"|");
    hasher.update(if value { b"1" } else { b"0" });
    hasher.update(b"\x1f");
}

fn format_f64(v: f64) -> String {
    // Fixed-precision formatting keeps hash stable across runs even when
    // floats round-trip through SQLite REAL storage.
    format!("{v:.9}")
}

/// A record of an asset's state in the database.
///
/// Fields are ordered for optimal memory layout:
/// - 8-byte aligned heap types first (String, `Option<PathBuf>`, `Option<String>`)
/// - 8-byte primitives (u64)
/// - `DateTime` fields (12-16 bytes each)
/// - 4-byte primitives (u32)
/// - 1-byte enums grouped at the end
/// - `metadata` carried last (variable-size nullable fields, not part of the
///   memory-hot path for skip decisions)
#[derive(Debug, Clone)]
pub struct AssetRecord {
    // 8-byte aligned heap types
    /// CloudKit zone name (e.g. "PrimarySync", "SharedSync-A1B2C3D4-...")
    /// scoping this asset. Part of the v8+ primary key so the same asset ID
    /// across multiple shared zones cannot collide. `Arc<str>` so the
    /// per-pass `DownloadConfig.library` can be refcount-cloned into every
    /// AssetRecord instead of allocating a fresh `Box<str>` per asset.
    pub library: Arc<str>,
    /// iCloud asset ID (recordName).
    pub id: Box<str>,
    /// SHA256 checksum of the file.
    pub checksum: Box<str>,
    /// Original filename from iCloud.
    pub filename: Box<str>,
    /// Local file path (if downloaded).
    pub local_path: Option<PathBuf>,
    /// Last error message (if failed).
    pub last_error: Option<String>,
    /// Locally-computed SHA-256 hash of the downloaded file (hex-encoded).
    /// None for assets downloaded before schema v3.
    pub local_checksum: Option<String>,
    /// SHA-256 hash captured before kei applies configured metadata writes.
    /// Differs from `local_checksum` when kei changed the downloaded bytes.
    pub download_checksum: Option<String>,

    // 8-byte primitives
    /// File size in bytes.
    pub size_bytes: u64,

    // DateTime fields (12-16 bytes each)
    /// Asset creation date in iCloud.
    pub created_at: DateTime<Utc>,
    /// Date the asset was added to the iCloud library (optional).
    pub added_at: Option<DateTime<Utc>>,
    /// When the asset was downloaded locally (if downloaded).
    pub downloaded_at: Option<DateTime<Utc>>,
    /// When we last saw this asset during a sync.
    pub last_seen_at: DateTime<Utc>,

    // 4-byte primitives
    /// Number of download attempts made.
    pub download_attempts: u32,

    // 1-byte enums grouped together
    /// Version size key (e.g., Original, Medium, `LiveOriginal`).
    pub version_size: VersionSizeKey,
    /// Type of media (photo, video, live photo).
    pub media_type: MediaType,
    /// Current status of the asset.
    pub status: AssetStatus,

    /// Metadata captured from the source (v5+).
    ///
    /// Behind `Arc` so `PhotoAsset` → `AssetRecord` (the producer hot
    /// loop) shares the same allocation instead of deep-cloning every
    /// `Option<String>` field per asset.
    pub metadata: Arc<AssetMetadata>,
}

impl AssetRecord {
    /// Create a new pending asset record.
    #[allow(
        clippy::too_many_arguments,
        reason = "this is the canonical constructor; every field is load-bearing"
    )]
    pub fn new_pending(
        library: Arc<str>,
        id: String,
        version_size: VersionSizeKey,
        checksum: String,
        filename: String,
        created_at: DateTime<Utc>,
        added_at: Option<DateTime<Utc>>,
        size_bytes: u64,
        media_type: MediaType,
    ) -> Self {
        Self {
            library,
            id: id.into_boxed_str(),
            checksum: checksum.into_boxed_str(),
            filename: filename.into_boxed_str(),
            local_path: None,
            last_error: None,
            local_checksum: None,
            download_checksum: None,
            size_bytes,
            created_at,
            added_at,
            downloaded_at: None,
            last_seen_at: Utc::now(),
            download_attempts: 0,
            version_size,
            media_type,
            status: AssetStatus::Pending,
            metadata: Arc::new(AssetMetadata::default()),
        }
    }

    /// Attach metadata, populating `metadata_hash` if unset.
    ///
    /// Wraps the passed `AssetMetadata` in a fresh `Arc`. Prod code
    /// uses [`Self::with_metadata_arc`] directly to reuse the
    /// allocation that `PhotoAsset` already owns; this convenience
    /// is only referenced from tests. Hash refresh happens inside
    /// `with_metadata_arc` when the hash is absent.
    #[cfg(test)]
    #[must_use]
    pub fn with_metadata(self, metadata: AssetMetadata) -> Self {
        self.with_metadata_arc(Arc::new(metadata))
    }

    /// Attach shared metadata, populating `metadata_hash` if unset.
    ///
    /// If the passed `Arc` has its hash already populated (the normal
    /// case — `metadata::extract()` refreshes before returning), this
    /// is a refcount bump. Missing hash triggers a single deep clone
    /// via `Arc::unwrap_or_clone` + `refresh_hash`.
    #[must_use]
    pub fn with_metadata_arc(mut self, metadata: Arc<AssetMetadata>) -> Self {
        self.metadata = if metadata.metadata_hash.is_some() {
            metadata
        } else {
            let mut m = Arc::unwrap_or_clone(metadata);
            m.refresh_hash();
            Arc::new(m)
        };
        self
    }
}

/// Statistics for a single sync run.
#[derive(Debug, Clone, Default)]
pub struct SyncRunStats {
    /// Number of assets seen during the sync.
    pub assets_seen: u64,
    /// Number of assets successfully downloaded.
    pub assets_downloaded: u64,
    /// Number of assets that failed to download.
    pub assets_failed: u64,
    /// Number of records the producer could not enumerate (CloudKit error
    /// per-record, transient API failures past retry budget). Drives
    /// `PartialFailure` in the zero-download branch even when
    /// `assets_failed == 0`; recorded in `sync_runs` so `kei status` can
    /// surface it without grepping logs.
    pub enumeration_errors: u64,
    /// Whether the sync was interrupted (shutdown, re-auth, etc.).
    pub interrupted: bool,
    /// Count-only CloudKit total observed before a reliable full enumeration.
    /// `None` for incremental, dry-run, recent-limited, or count-error runs.
    pub api_total_at_start: Option<u64>,
    /// True when the run aggregated at least one reliable library total and
    /// at least one library without a comparable total.
    pub api_total_at_start_partial: bool,
    /// Number of cross-cycle inventory-drop warnings detected this run.
    pub inventory_drop_warnings: u64,
    /// Previous API total for the largest inventory drop.
    pub inventory_drop_previous_total: Option<u64>,
    /// Current API total for the largest inventory drop.
    pub inventory_drop_current_total: Option<u64>,
    /// Library where the largest inventory drop was observed.
    pub inventory_drop_library: Option<String>,
}

/// One provider version used to identify the correct child of a legacy
/// master-keyed catalogue row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetadataCaptureVersionEvidence {
    pub(crate) version_size: VersionSizeKey,
    pub(crate) checksum: String,
    pub(crate) size_bytes: u64,
}

/// One stale catalogue identity selected for bounded provider hydration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MetadataCaptureCandidate {
    pub(crate) library: String,
    pub(crate) asset_id: String,
    pub(crate) master_record_name: String,
    pub(crate) asset_record_name: Option<String>,
    pub(crate) versions: Vec<MetadataCaptureVersionEvidence>,
}

/// Durable metadata-capture state for one provider library.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MetadataCaptureStatus {
    pub library: String,
    pub active_revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_revision: Option<i64>,
    pub processed_assets: u64,
    pub failed_assets: u64,
    pub remaining_assets: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Summary of the current state database.
#[derive(Debug, Clone)]
pub struct SyncSummary {
    /// Total number of assets tracked.
    pub total_assets: u64,
    /// Number of assets successfully downloaded.
    pub downloaded: u64,
    /// Number of assets pending download.
    pub pending: u64,
    /// Number of live assets intentionally excluded by the active download policy.
    pub policy_excluded: u64,
    /// Number of assets that failed to download.
    pub failed: u64,
    /// Pending or failed rows whose targeted provider lookup was inconclusive.
    pub awaiting_provider_verification: u64,
    /// Provider-confirmed deletions retained as catalog history.
    pub source_deleted: u64,
    /// Oldest unresolved targeted provider verification.
    pub oldest_provider_verification_at: Option<DateTime<Utc>>,
    /// Last provider checkpoint decision recorded by the cycle owner.
    pub provider_checkpoint_status: Option<String>,
    /// Last bounded recovery action selected by the cycle owner.
    pub last_recovery_action: Option<String>,
    /// Last reason that selected full provider enumeration.
    pub last_full_enumeration_reason: Option<String>,
    /// Total size in bytes of downloaded assets.
    pub downloaded_bytes: u64,
    /// Time of the newest sync run that is still marked running.
    pub active_sync_started: Option<DateTime<Utc>>,
    /// Zones with a live or uncleared full-enumeration progress marker.
    pub active_enumeration_zones: Vec<String>,
    /// Time of the last completed sync run (if any).
    pub last_sync_completed: Option<DateTime<Utc>>,
    /// Time of the last sync run start (if any).
    pub last_sync_started: Option<DateTime<Utc>>,
    /// Raw status of the latest sync run.
    pub last_sync_status: Option<String>,
    /// Asset failures recorded by the latest sync run.
    pub last_sync_assets_failed: u64,
    /// Enumeration errors recorded by the latest sync run.
    pub last_sync_enumeration_errors: u64,
    /// Whether the latest sync run was interrupted.
    pub last_sync_interrupted: bool,
    /// Latest persisted count-only CloudKit inventory total, if the last run
    /// had a reliable full-enumeration snapshot.
    pub last_api_total_at_start: Option<u64>,
    /// True when `last_api_total_at_start` covers only the libraries that had
    /// reliable full-enumeration totals.
    pub last_api_total_at_start_partial: bool,
    /// Whether the latest run detected a cross-cycle inventory drop.
    pub last_inventory_drop_detected: bool,
    /// Previous API total for the latest inventory warning.
    pub last_inventory_drop_previous_total: Option<u64>,
    /// Current API total for the latest inventory warning.
    pub last_inventory_drop_current_total: Option<u64>,
    /// Library where the latest inventory warning was observed.
    pub last_inventory_drop_library: Option<String>,
    /// Per-library metadata-capture revision and repair progress.
    pub metadata_capture: Vec<MetadataCaptureStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::mem::size_of;

    #[test]
    fn test_version_size_key_round_trip() {
        for key in [
            VersionSizeKey::Original,
            VersionSizeKey::Medium,
            VersionSizeKey::Thumb,
            VersionSizeKey::Adjusted,
            VersionSizeKey::Alternative,
            VersionSizeKey::LiveOriginal,
            VersionSizeKey::LiveMedium,
            VersionSizeKey::LiveThumb,
            VersionSizeKey::LiveAdjusted,
        ] {
            assert_eq!(VersionSizeKey::from_str(key.as_str()), Some(key));
        }
    }

    #[test]
    fn test_version_size_key_from_str_aliases() {
        // Test alternate spellings (without underscore)
        assert_eq!(
            VersionSizeKey::from_str("liveoriginal"),
            Some(VersionSizeKey::LiveOriginal)
        );
        assert_eq!(
            VersionSizeKey::from_str("livemedium"),
            Some(VersionSizeKey::LiveMedium)
        );
        assert_eq!(
            VersionSizeKey::from_str("livethumb"),
            Some(VersionSizeKey::LiveThumb)
        );
        assert_eq!(
            VersionSizeKey::from_str("liveadjusted"),
            Some(VersionSizeKey::LiveAdjusted)
        );
    }

    #[test]
    fn test_version_size_key_from_invalid() {
        assert_eq!(VersionSizeKey::from_str("invalid"), None);
    }

    #[test]
    fn metadata_capture_requires_exact_unanimous_resource_identity() {
        let facts = RenditionMetadata {
            checksum: Some(Arc::from("resource")),
            width: Some(1920),
            height: None,
            duration_secs: Some(2.3),
        };
        let shared = Arc::new(AssetMetadata {
            width: Some(4000),
            height: Some(3000),
            duration_secs: Some(99.0),
            title: Some("shared edit".into()),
            metadata_hash: Some("stale hash".into()),
            ..AssetMetadata::default()
        });
        let original = VersionSizeKey::Original;
        let mut missing_checksum = facts.clone();
        missing_checksum.checksum = None;
        let mut blank_checksum = facts.clone();
        blank_checksum.checksum = Some(Arc::from(" \t"));
        let mut other_checksum = facts.clone();
        other_checksum.checksum = Some(Arc::from("other"));
        other_checksum.width = None;
        let mut missing_width = facts.clone();
        missing_width.width = None;
        let mut different_height = facts.clone();
        different_height.height = Some(1080);
        let mut missing_duration = facts.clone();
        missing_duration.duration_secs = None;
        for (candidates, checksum, known) in [
            (vec![(original, facts.clone())], "resource", true),
            (vec![(original, facts.clone()); 2], "resource", true),
            (vec![(original, facts.clone())], "other", false),
            (vec![(original, facts.clone())], " resource", false),
            (vec![(original, facts.clone())], "", false),
            (vec![], "resource", false),
            (
                vec![(VersionSizeKey::Medium, facts.clone())],
                "resource",
                false,
            ),
            (vec![(original, missing_checksum)], "resource", false),
            (vec![(original, blank_checksum)], " \t", false),
            (
                vec![(original, facts.clone()), (original, missing_width)],
                "resource",
                false,
            ),
            (
                vec![(original, facts.clone()), (original, different_height)],
                "resource",
                false,
            ),
            (
                vec![(original, facts.clone()), (original, missing_duration)],
                "resource",
                false,
            ),
            (
                vec![(original, facts.clone()), (original, other_checksum)],
                "resource",
                true,
            ),
        ] {
            let capture = MetadataCapture {
                shared: Arc::clone(&shared),
                renditions: candidates.into(),
            };
            let resolved = capture.resolve(original, checksum);
            let expected = if known {
                (facts.width, facts.height, facts.duration_secs)
            } else {
                (None, None, None)
            };
            assert_eq!(
                (resolved.width, resolved.height, resolved.duration_secs),
                expected
            );
            assert_eq!(resolved.title, shared.title);
            assert_eq!(resolved.metadata_hash, Some(resolved.compute_hash()));
        }
    }

    #[test]
    fn test_version_size_key_from_asset_version_size() {
        assert_eq!(
            VersionSizeKey::from(AssetVersionSize::Original),
            VersionSizeKey::Original
        );
        assert_eq!(
            VersionSizeKey::from(AssetVersionSize::LiveOriginal),
            VersionSizeKey::LiveOriginal
        );
    }

    #[test]
    fn test_version_size_key_size() {
        assert_eq!(size_of::<VersionSizeKey>(), 1);
    }

    #[test]
    fn test_asset_status_round_trip() {
        for status in [
            AssetStatus::Pending,
            AssetStatus::PolicyExcluded,
            AssetStatus::Downloaded,
            AssetStatus::Failed,
        ] {
            assert_eq!(AssetStatus::from_str(status.as_str()), Some(status));
        }
    }

    #[test]
    fn test_asset_status_from_invalid() {
        assert_eq!(AssetStatus::from_str("invalid"), None);
    }

    #[test]
    fn test_media_type_round_trip() {
        for media_type in [
            MediaType::Photo,
            MediaType::Video,
            MediaType::LivePhotoImage,
            MediaType::LivePhotoVideo,
        ] {
            assert_eq!(MediaType::from_str(media_type.as_str()), Some(media_type));
        }
    }

    #[test]
    fn test_media_type_from_invalid() {
        assert_eq!(MediaType::from_str("invalid"), None);
    }

    #[test]
    fn test_asset_record_new_pending() {
        let now = Utc::now();
        let record = AssetRecord::new_pending(
            Arc::from("PrimarySync"),
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "checksum123".to_string(),
            "photo.jpg".to_string(),
            now,
            None,
            12345,
            MediaType::Photo,
        );
        assert_eq!(record.status, AssetStatus::Pending);
        assert_eq!(record.download_attempts, 0);
        assert!(record.downloaded_at.is_none());
        assert!(record.local_path.is_none());
        // Verify last_seen_at is set to a recent time (within 1 second of now)
        assert!((record.last_seen_at - now).num_seconds().abs() <= 1);
    }

    #[test]
    fn test_asset_record_size() {
        // V5 added AssetMetadata (22 optional fields + 4 bools); v8 added
        // a `library: Arc<str>` field (16 bytes). Cap lifted accordingly.
        // The hot-path skip decisions still use the pre-metadata fields;
        // metadata is loaded separately.
        assert!(
            size_of::<AssetRecord>() <= 736,
            "AssetRecord size {} exceeds 736 bytes",
            size_of::<AssetRecord>()
        );
    }

    #[test]
    fn test_asset_metadata_hash_stable_for_empty() {
        let a = AssetMetadata::default().compute_hash();
        let b = AssetMetadata::default().compute_hash();
        assert_eq!(a, b);
    }

    #[test]
    fn test_asset_metadata_hash_changes_with_favorite() {
        let before = AssetMetadata::default().compute_hash();
        let after = AssetMetadata {
            is_favorite: true,
            ..AssetMetadata::default()
        }
        .compute_hash();
        assert_ne!(before, after);
    }

    #[test]
    fn test_asset_metadata_hash_changes_with_location() {
        let base = AssetMetadata::default().compute_hash();
        let with_gps = AssetMetadata {
            latitude: Some(37.7749),
            longitude: Some(-122.4194),
            ..AssetMetadata::default()
        }
        .compute_hash();
        assert_ne!(base, with_gps);
    }

    #[test]
    fn test_asset_metadata_hash_changes_with_timezone_offset() {
        let base = AssetMetadata::default().compute_hash();
        let with_offset = AssetMetadata {
            timezone_offset: Some(39_600),
            ..AssetMetadata::default()
        }
        .compute_hash();
        assert_ne!(base, with_offset);
    }

    #[test]
    fn capture_local_uses_asset_offset_or_host_local_fallback() {
        let created_at = Utc.with_ymd_and_hms(2025, 2, 1, 2, 0, 0).unwrap();
        let expected_host_local = created_at.with_timezone(&Local).fixed_offset();

        for metadata in [
            AssetMetadata::default(),
            AssetMetadata {
                timezone_offset: Some(i32::MAX),
                ..AssetMetadata::default()
            },
        ] {
            let resolved = metadata.capture_local(created_at);
            assert_eq!(resolved.naive_local(), expected_host_local.naive_local());
            assert_eq!(resolved.offset(), expected_host_local.offset());
        }

        let offset = FixedOffset::east_opt(50_400).unwrap();
        let metadata = AssetMetadata {
            timezone_offset: Some(offset.local_minus_utc()),
            ..AssetMetadata::default()
        };
        let resolved = metadata.capture_local(created_at);
        let expected = created_at.with_timezone(&offset);
        assert_eq!(resolved.naive_local(), expected.naive_local());
        assert_eq!(resolved.offset(), expected.offset());
    }

    #[test]
    fn test_asset_metadata_hash_ignores_source_and_hash() {
        let a = AssetMetadata {
            source: Some("icloud".into()),
            metadata_hash: Some("ignored".into()),
            is_favorite: true,
            ..AssetMetadata::default()
        }
        .compute_hash();
        let b = AssetMetadata {
            source: Some("external-import".into()),
            metadata_hash: Some("also_ignored".into()),
            is_favorite: true,
            ..AssetMetadata::default()
        }
        .compute_hash();
        assert_eq!(a, b);
    }

    #[test]
    fn test_asset_metadata_hash_distinguishes_none_vs_empty_string() {
        let none_title = AssetMetadata::default().compute_hash();
        let empty_title = AssetMetadata {
            title: Some(String::new()),
            ..AssetMetadata::default()
        }
        .compute_hash();
        assert_ne!(none_title, empty_title);
    }

    #[test]
    fn test_with_metadata_refreshes_hash_if_missing() {
        let now = Utc::now();
        let record = AssetRecord::new_pending(
            Arc::from("PrimarySync"),
            "A".into(),
            VersionSizeKey::Original,
            "ck".into(),
            "p.jpg".into(),
            now,
            None,
            1,
            MediaType::Photo,
        )
        .with_metadata(AssetMetadata {
            is_favorite: true,
            ..AssetMetadata::default()
        });
        assert!(record.metadata.metadata_hash.is_some());
    }

    #[test]
    fn test_with_metadata_preserves_existing_hash() {
        let now = Utc::now();
        let record = AssetRecord::new_pending(
            Arc::from("PrimarySync"),
            "A".into(),
            VersionSizeKey::Original,
            "ck".into(),
            "p.jpg".into(),
            now,
            None,
            1,
            MediaType::Photo,
        )
        .with_metadata(AssetMetadata {
            metadata_hash: Some("precomputed".into()),
            ..AssetMetadata::default()
        });
        assert_eq!(
            record.metadata.metadata_hash.as_deref(),
            Some("precomputed")
        );
    }

    #[test]
    fn test_asset_status_is_one_byte() {
        assert_eq!(size_of::<AssetStatus>(), 1);
    }

    #[test]
    fn test_media_type_is_one_byte() {
        assert_eq!(size_of::<MediaType>(), 1);
    }

    #[test]
    fn test_sync_run_stats_default() {
        let stats = SyncRunStats::default();
        assert_eq!(stats.assets_seen, 0);
        assert_eq!(stats.assets_downloaded, 0);
        assert_eq!(stats.assets_failed, 0);
        assert!(!stats.interrupted);
    }

    #[test]
    fn test_asset_record_new_pending_with_added_at() {
        let now = Utc::now();
        let added = now - chrono::Duration::hours(1);
        let record = AssetRecord::new_pending(
            Arc::from("PrimarySync"),
            "XYZ".to_string(),
            VersionSizeKey::LiveOriginal,
            "ck".to_string(),
            "video.mov".to_string(),
            now,
            Some(added),
            99999,
            MediaType::LivePhotoVideo,
        );
        assert_eq!(record.added_at, Some(added));
        assert_eq!(record.media_type, MediaType::LivePhotoVideo);
        assert_eq!(record.version_size, VersionSizeKey::LiveOriginal);
    }

    #[test]
    fn test_version_size_key_all_from_asset_version_size() {
        let conversions = [
            (AssetVersionSize::Original, VersionSizeKey::Original),
            (AssetVersionSize::Medium, VersionSizeKey::Medium),
            (AssetVersionSize::Thumb, VersionSizeKey::Thumb),
            (AssetVersionSize::Adjusted, VersionSizeKey::Adjusted),
            (AssetVersionSize::Alternative, VersionSizeKey::Alternative),
            (AssetVersionSize::LiveOriginal, VersionSizeKey::LiveOriginal),
            (AssetVersionSize::LiveMedium, VersionSizeKey::LiveMedium),
            (AssetVersionSize::LiveThumb, VersionSizeKey::LiveThumb),
            (AssetVersionSize::LiveAdjusted, VersionSizeKey::LiveAdjusted),
        ];
        for (avs, expected) in conversions {
            assert_eq!(VersionSizeKey::from(avs), expected, "{:?}", avs);
        }
    }
}
