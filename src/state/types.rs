//! Types for the state tracking module.

use std::fmt::Write;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

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
/// Boxed inside `AssetRecord` (`Option<Box<AssetMetadata>>`) to avoid
/// inflating the base record on code paths that don't use metadata.
///
/// Fields are ordered for optimal memory layout:
/// - Heap types first (String, `Vec`, `Option<String>`)
/// - f64 fields
/// - i32/i64 fields
/// - bool fields last
#[derive(Debug, Clone, PartialEq)]
pub struct AssetMetadata {
    // Heap types
    /// Provider that created this record ("icloud", "takeout", etc.).
    pub source: String,
    /// Short title / caption (iCloud: `captionEnc`).
    pub title: Option<String>,
    /// Longer description / notes (iCloud: `extendedDescEnc`).
    pub description: Option<String>,
    /// Keyword tags (stored as JSON array in DB).
    pub keywords: Vec<String>,
    /// Groups burst shots.
    pub burst_id: Option<String>,
    /// Asset subtype: "screenshot", "panorama", "hdr", "burst", etc.
    pub media_subtype: Option<String>,
    /// Opaque JSON blob for provider-specific fields.
    pub provider_data: Option<String>,
    /// SHA-256 hash of metadata fields for change detection.
    pub metadata_hash: Option<String>,

    // f64 fields
    /// Decimal degrees, WGS84.
    pub latitude: Option<f64>,
    /// Decimal degrees, WGS84.
    pub longitude: Option<f64>,
    /// Meters above sea level.
    pub altitude: Option<f64>,
    /// Duration in seconds (video, live photo).
    pub duration_secs: Option<f64>,

    // Integer fields
    /// EXIF orientation (1-8).
    pub orientation: Option<i32>,
    /// Seconds from UTC.
    pub timezone_offset: Option<i32>,
    /// Pixel width.
    pub width: Option<i32>,
    /// Pixel height.
    pub height: Option<i32>,
    /// Star rating (1-5).
    pub rating: Option<i32>,
    /// When the photo/metadata was last edited at source (unix timestamp).
    pub modified_at: Option<i64>,
    /// When asset was deleted/expunged at source (unix timestamp).
    pub deleted_at: Option<i64>,

    // Booleans
    /// Provider-native favorite/heart flag.
    pub is_favorite: bool,
    /// Hidden from main library view.
    pub is_hidden: bool,
    /// Hidden from main timeline but retained.
    pub is_archived: bool,
    /// Soft-deleted at source.
    pub is_deleted: bool,
}

impl AssetMetadata {
    /// Compute a deterministic hash of all metadata fields for change detection.
    ///
    /// Excludes `source` (immutable) and `metadata_hash` itself. Uses SHA-256
    /// truncated to 16 hex chars, matching the `hash_download_config()` pattern.
    pub fn compute_hash(&self) -> String {
        let mut hasher = Sha256::new();

        // Booleans
        hasher.update([u8::from(self.is_favorite)]);
        hasher.update([u8::from(self.is_hidden)]);
        hasher.update([u8::from(self.is_archived)]);
        hasher.update([u8::from(self.is_deleted)]);

        // Optional integers
        hash_opt_i32(&mut hasher, self.orientation);
        hash_opt_i32(&mut hasher, self.timezone_offset);
        hash_opt_i32(&mut hasher, self.width);
        hash_opt_i32(&mut hasher, self.height);
        hash_opt_i32(&mut hasher, self.rating);
        hash_opt_i64(&mut hasher, self.modified_at);
        hash_opt_i64(&mut hasher, self.deleted_at);

        // Optional f64
        hash_opt_f64(&mut hasher, self.latitude);
        hash_opt_f64(&mut hasher, self.longitude);
        hash_opt_f64(&mut hasher, self.altitude);
        hash_opt_f64(&mut hasher, self.duration_secs);

        // Optional strings
        hash_opt_str(&mut hasher, self.title.as_deref());
        hash_opt_str(&mut hasher, self.description.as_deref());
        hash_opt_str(&mut hasher, self.burst_id.as_deref());
        hash_opt_str(&mut hasher, self.media_subtype.as_deref());

        // Keywords: sorted for determinism
        let mut sorted_kw = self.keywords.clone();
        sorted_kw.sort_unstable();
        for kw in &sorted_kw {
            hasher.update(kw.as_bytes());
            hasher.update(b"\0");
        }
        hasher.update(b"\x01"); // separator after keywords list

        let hash = hasher.finalize();
        let mut hex = String::with_capacity(16);
        for &b in &hash[..8] {
            let _ = Write::write_fmt(&mut hex, format_args!("{b:02x}"));
        }
        hex
    }
}

impl Default for AssetMetadata {
    fn default() -> Self {
        Self {
            source: "icloud".to_string(),
            title: None,
            description: None,
            keywords: Vec::new(),
            burst_id: None,
            media_subtype: None,
            provider_data: None,
            metadata_hash: None,
            latitude: None,
            longitude: None,
            altitude: None,
            duration_secs: None,
            orientation: None,
            timezone_offset: None,
            width: None,
            height: None,
            rating: None,
            modified_at: None,
            deleted_at: None,
            is_favorite: false,
            is_hidden: false,
            is_archived: false,
            is_deleted: false,
        }
    }
}

fn hash_opt_str(hasher: &mut Sha256, val: Option<&str>) {
    match val {
        Some(s) => {
            hasher.update(s.as_bytes());
            hasher.update(b"\0");
        }
        None => hasher.update(b"\xff"),
    }
}

fn hash_opt_i32(hasher: &mut Sha256, val: Option<i32>) {
    match val {
        Some(v) => hasher.update(v.to_le_bytes()),
        None => hasher.update(b"\xff"),
    }
}

fn hash_opt_i64(hasher: &mut Sha256, val: Option<i64>) {
    match val {
        Some(v) => hasher.update(v.to_le_bytes()),
        None => hasher.update(b"\xff"),
    }
}

fn hash_opt_f64(hasher: &mut Sha256, val: Option<f64>) {
    match val {
        Some(v) => hasher.update(v.to_le_bytes()),
        None => hasher.update(b"\xff"),
    }
}

/// A record of an asset's state in the database.
///
/// Fields are ordered for optimal memory layout:
/// - 8-byte aligned heap types first (String, `Option<PathBuf>`, `Option<String>`)
/// - 8-byte primitives (u64)
/// - `DateTime` fields (12-16 bytes each)
/// - 4-byte primitives (u32)
/// - 1-byte enums grouped at the end
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
    /// Provider-agnostic metadata. Heap-allocated to keep `AssetRecord` small
    /// on code paths that don't use metadata (bulk pre-load, skip decisions).
    pub metadata: Option<Box<AssetMetadata>>,

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
            metadata: None,
            size_bytes,
            created_at,
            added_at,
            downloaded_at: None,
            last_seen_at: Utc::now(),
            download_attempts: 0,
            version_size,
            media_type,
            status: AssetStatus::Pending,
        }
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
        // Verify struct size is reasonable (goal: <= 280 bytes)
        assert!(
            size_of::<AssetRecord>() <= 280,
            "AssetRecord size {} exceeds 280 bytes",
            size_of::<AssetRecord>()
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

    #[test]
    fn test_asset_metadata_default() {
        let meta = AssetMetadata::default();
        assert_eq!(meta.source, "icloud");
        assert!(!meta.is_favorite);
        assert!(!meta.is_hidden);
        assert!(meta.title.is_none());
        assert!(meta.keywords.is_empty());
    }

    #[test]
    fn test_metadata_hash_determinism() {
        let meta = AssetMetadata {
            is_favorite: true,
            latitude: Some(37.7749),
            longitude: Some(-122.4194),
            title: Some("Sunset".to_string()),
            keywords: vec!["beach".to_string(), "sunset".to_string()],
            ..AssetMetadata::default()
        };
        let hash1 = meta.compute_hash();
        let hash2 = meta.compute_hash();
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16);
    }

    #[test]
    fn test_metadata_hash_sensitivity() {
        let base = AssetMetadata {
            is_favorite: true,
            title: Some("Photo".to_string()),
            ..AssetMetadata::default()
        };
        let changed = AssetMetadata {
            is_favorite: false,
            title: Some("Photo".to_string()),
            ..AssetMetadata::default()
        };
        assert_ne!(base.compute_hash(), changed.compute_hash());
    }

    #[test]
    fn test_metadata_hash_keyword_order_independent() {
        let meta1 = AssetMetadata {
            keywords: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            ..AssetMetadata::default()
        };
        let meta2 = AssetMetadata {
            keywords: vec!["c".to_string(), "a".to_string(), "b".to_string()],
            ..AssetMetadata::default()
        };
        assert_eq!(meta1.compute_hash(), meta2.compute_hash());
    }

    #[test]
    fn test_metadata_hash_none_vs_empty_string() {
        let with_none = AssetMetadata {
            title: None,
            ..AssetMetadata::default()
        };
        let with_empty = AssetMetadata {
            title: Some(String::new()),
            ..AssetMetadata::default()
        };
        assert_ne!(with_none.compute_hash(), with_empty.compute_hash());
    }

    #[test]
    fn test_asset_record_with_metadata_stays_under_size_limit() {
        // Box<AssetMetadata> adds only 8 bytes (a pointer)
        assert!(
            size_of::<AssetRecord>() <= 288,
            "AssetRecord size {} exceeds 288 bytes",
            size_of::<AssetRecord>()
        );
    }

    #[test]
    fn test_new_pending_has_no_metadata() {
        let record = AssetRecord::new_pending(
            "test".to_string(),
            VersionSizeKey::Original,
            "ck".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            100,
            MediaType::Photo,
        );
        assert!(record.metadata.is_none());
    }
}
