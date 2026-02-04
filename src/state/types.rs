//! Types for the state tracking module.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::icloud::photos::AssetVersionSize;

/// Version size key for state tracking.
///
/// This is a 1-byte enum representing the version size, saving ~23 bytes
/// per AssetRecord compared to storing as a String.
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
}

impl VersionSizeKey {
    /// Convert to the string stored in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::Medium => "medium",
            Self::Thumb => "thumb",
            Self::Adjusted => "adjusted",
            Self::Alternative => "alternative",
            Self::LiveOriginal => "live_original",
            Self::LiveMedium => "live_medium",
            Self::LiveThumb => "live_thumb",
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
    #[allow(dead_code)] // Symmetric with from_str; used in tests
    pub fn as_str(&self) -> &'static str {
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
    pub fn as_str(&self) -> &'static str {
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

/// A record of an asset's state in the database.
///
/// Fields are ordered for optimal memory layout:
/// - 8-byte aligned heap types first (String, `Option<PathBuf>`, `Option<String>`)
/// - 8-byte primitives (u64)
/// - DateTime fields (12-16 bytes each)
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
    /// Version size key (e.g., Original, Medium, LiveOriginal).
    pub version_size: VersionSizeKey,
    /// Type of media (photo, video, live photo).
    pub media_type: MediaType,
    /// Current status of the asset.
    pub status: AssetStatus,
}

impl AssetRecord {
    /// Create a new pending asset record.
    #[allow(clippy::too_many_arguments)] // Matches SQL table columns
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
        // Verify struct size is reasonable (goal: <= 256 bytes)
        assert!(
            size_of::<AssetRecord>() <= 256,
            "AssetRecord size {} exceeds 256 bytes",
            size_of::<AssetRecord>()
        );
    }
}
