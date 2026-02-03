//! Types for the state tracking module.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

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
    #[allow(dead_code)] // Used internally by db.rs for SQL serialization
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
#[derive(Debug, Clone)]
pub struct AssetRecord {
    /// iCloud asset ID (recordName).
    pub id: String,
    /// Version size key (e.g., "original", "medium", "live_original").
    pub version_size: String,
    /// SHA256 checksum of the file.
    pub checksum: String,
    /// Original filename from iCloud.
    pub filename: String,
    /// Asset creation date in iCloud.
    pub created_at: DateTime<Utc>,
    /// Date the asset was added to the iCloud library (optional).
    pub added_at: Option<DateTime<Utc>>,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Type of media (photo, video, live photo).
    pub media_type: MediaType,
    /// Current status of the asset.
    pub status: AssetStatus,
    /// When the asset was downloaded locally (if downloaded).
    pub downloaded_at: Option<DateTime<Utc>>,
    /// Local file path (if downloaded).
    pub local_path: Option<PathBuf>,
    /// When we last saw this asset during a sync.
    pub last_seen_at: DateTime<Utc>,
    /// Number of download attempts made.
    pub download_attempts: u32,
    /// Last error message (if failed).
    pub last_error: Option<String>,
}

impl AssetRecord {
    /// Create a new pending asset record.
    #[allow(clippy::too_many_arguments)] // Matches SQL table columns
    pub fn new_pending(
        id: String,
        version_size: String,
        checksum: String,
        filename: String,
        created_at: DateTime<Utc>,
        added_at: Option<DateTime<Utc>>,
        size_bytes: u64,
        media_type: MediaType,
    ) -> Self {
        Self {
            id,
            version_size,
            checksum,
            filename,
            created_at,
            added_at,
            size_bytes,
            media_type,
            status: AssetStatus::Pending,
            downloaded_at: None,
            local_path: None,
            last_seen_at: Utc::now(),
            download_attempts: 0,
            last_error: None,
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
            "original".to_string(),
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
}
