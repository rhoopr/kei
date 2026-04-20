//! Types for the state tracking module.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::types::AssetVersionSize;

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
            Self::Downloaded => "downloaded",
            Self::Failed => "failed",
        }
    }

    /// Parse from the string stored in the database.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
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
}

/// Provider-agnostic metadata for an asset.
///
/// Every field is optional or has a safe default: providers populate what they
/// can, consumers handle missing values. Mapping from provider-specific fields
/// (iCloud `isFavorite`, Takeout `favorited`, etc.) lives in provider adapters.
///
/// `metadata_hash` is computed from the metadata fields and stored alongside
/// them so that incremental sync can detect metadata-only changes in O(1).
#[derive(Debug, Clone, Default)]
pub struct AssetMetadata {
    /// Provider that created this record ("icloud", "takeout", etc.).
    pub source: Option<String>,
    /// Provider-native favorite/heart flag.
    pub is_favorite: bool,
    /// 1-5 star rating (providers with boolean favorites set 5).
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
    /// When metadata was last edited at source (provider-supplied only).
    pub modified_at: Option<DateTime<Utc>>,
    /// Soft-deleted at source.
    pub is_deleted: bool,
    /// When the asset was deleted/expunged at source.
    pub deleted_at: Option<DateTime<Utc>>,
    /// Opaque JSON blob for provider-specific fields that don't fit the
    /// canonical schema (invariant 4: capture everything available).
    pub provider_data: Option<String>,
    /// SHA-256 of the metadata fields above, for change detection.
    pub metadata_hash: Option<String>,
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
        hash_opt(h, "rat", self.rating.map(|v| v.to_string()));
        hash_opt(h, "lat", self.latitude.map(format_f64));
        hash_opt(h, "lng", self.longitude.map(format_f64));
        hash_opt(h, "alt", self.altitude.map(format_f64));
        hash_opt(h, "ori", self.orientation.map(|v| v.to_string()));
        hash_opt(h, "dur", self.duration_secs.map(format_f64));
        hash_opt(h, "tzo", self.timezone_offset.map(|v| v.to_string()));
        hash_opt(h, "w", self.width.map(|v| v.to_string()));
        hash_opt(h, "hh", self.height.map(|v| v.to_string()));
        hash_opt(h, "tit", self.title.clone());
        hash_opt(h, "kw", self.keywords.clone());
        hash_opt(h, "desc", self.description.clone());
        hash_opt(h, "sub", self.media_subtype.clone());
        hash_opt(h, "bur", self.burst_id.clone());
        hash_bool(h, "hid", self.is_hidden);
        hash_bool(h, "arc", self.is_archived);
        hash_opt(
            h,
            "mod",
            self.modified_at.map(|dt| dt.timestamp().to_string()),
        );
        hash_bool(h, "del", self.is_deleted);
        hash_opt(
            h,
            "delat",
            self.deleted_at.map(|dt| dt.timestamp().to_string()),
        );
        hash_opt(h, "pd", self.provider_data.clone());
        let digest = hasher.finalize();
        data_encoding::HEXLOWER.encode(&digest)
    }

    /// Populate `metadata_hash` from the current field values.
    pub fn refresh_hash(&mut self) {
        self.metadata_hash = Some(self.compute_hash());
    }
}

fn hash_opt(hasher: &mut sha2::Sha256, tag: &str, value: Option<String>) {
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
    /// iCloud asset ID (recordName).
    pub id: String,
    /// SHA256 checksum of the file.
    pub checksum: String,
    /// Original filename from iCloud.
    pub filename: String,
    /// Local file path (if downloaded).
    pub local_path: Option<PathBuf>,
    /// Last error message (if failed).
    pub last_error: Option<String>,
    /// Locally-computed SHA-256 hash of the downloaded file (hex-encoded).
    /// None for assets downloaded before schema v3.
    pub local_checksum: Option<String>,

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

    /// Provider-agnostic metadata captured from the source (v5+).
    pub metadata: AssetMetadata,
}

impl AssetRecord {
    /// Create a new pending asset record.
    pub fn new_pending(
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
            id,
            checksum,
            filename,
            local_path: None,
            last_error: None,
            local_checksum: None,
            size_bytes,
            created_at,
            added_at,
            downloaded_at: None,
            last_seen_at: Utc::now(),
            download_attempts: 0,
            version_size,
            media_type,
            status: AssetStatus::Pending,
            metadata: AssetMetadata::default(),
        }
    }

    /// Attach metadata, populating `metadata_hash` if unset.
    #[must_use]
    pub fn with_metadata(mut self, mut metadata: AssetMetadata) -> Self {
        if metadata.metadata_hash.is_none() {
            metadata.refresh_hash();
        }
        self.metadata = metadata;
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
    /// Whether the sync was interrupted (shutdown, re-auth, etc.).
    pub interrupted: bool,
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
    /// Number of assets that failed to download.
    pub failed: u64,
    /// Total size in bytes of downloaded assets.
    pub downloaded_bytes: u64,
    /// Time of the last completed sync run (if any).
    pub last_sync_completed: Option<DateTime<Utc>>,
    /// Time of the last sync run start (if any).
    pub last_sync_started: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // V5 added AssetMetadata (22 optional fields + 4 bools). Cap lifted
        // accordingly. The hot-path skip decisions still use the pre-metadata
        // fields; metadata is loaded separately.
        assert!(
            size_of::<AssetRecord>() <= 720,
            "AssetRecord size {} exceeds 720 bytes",
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
    fn test_asset_metadata_hash_ignores_source_and_hash() {
        let a = AssetMetadata {
            source: Some("icloud".into()),
            metadata_hash: Some("ignored".into()),
            is_favorite: true,
            ..AssetMetadata::default()
        }
        .compute_hash();
        let b = AssetMetadata {
            source: Some("takeout".into()),
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
