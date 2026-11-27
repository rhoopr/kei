//! Types for the state tracking module.
#![allow(
    dead_code,
    reason = "previous-engine code kept compiled for its remaining callers and tests until that engine is removed (docs/sync-core.md, delivery step 6)"
)]

use std::sync::Arc;

use chrono::{DateTime, FixedOffset, Local, Utc};

use crate::types::AssetVersionSize;

pub(crate) mod embedded_absence {
    pub(crate) const RATING: u8 = 1;
    pub(crate) const DESCRIPTION: u8 = 2;
    pub(crate) const TITLE: u8 = 4;
    pub(crate) const KEYWORDS: u8 = 8;
    pub(crate) const PEOPLE: u8 = 16;
    pub(crate) const GPS: u8 = 32;
}

/// Exact embedded values established by a measured field change. A missing
/// member grants no ownership, including when a recipe completed without writes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddedMetadataFields {
    /// Latitude, longitude, altitude, latitude ref, longitude ref, altitude ref.
    /// Each property has independent write proof; existing companion tags are
    /// never adopted merely because another coordinate component changed.
    pub gps: [Option<String>; 6],
    /// Native companions reconciled by the XMP JPEG/TIFF/PNG handler.
    pub native_gps: [Option<String>; 6],
    /// Native TIFF description is independent of the XMP default-language text.
    pub native_description: Option<String>,
    pub rating: Option<String>,
    pub rating_percent: Option<String>,
    pub description: Option<String>,
    pub title: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub people: Option<Vec<String>>,
}

/// Measured field ownership tied to exact input/output bytes. Rewrites freeze
/// it before publication; initial downloads commit it with downloaded state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddedMetadataReceipt {
    pub published: bool,
    pub input_checksum: String,
    pub output_checksum: String,
    pub previous: EmbeddedMetadataFields,
    pub written: EmbeddedMetadataFields,
}

impl EmbeddedMetadataFields {
    /// Retain previous ownership only while its value survives unchanged, and
    /// acquire ownership only when this writer actually changed the field.
    #[must_use]
    pub(crate) fn after_write(&self, before: &Self, after: &Self) -> Self {
        fn owned<T: Clone + PartialEq>(
            prior: &Option<T>,
            before: &Option<T>,
            after: &Option<T>,
        ) -> Option<T> {
            if before != after || (prior.is_some() && prior == before) {
                after.clone()
            } else {
                None
            }
        }
        let mut gps: [Option<String>; 6] = Default::default();
        let mut native_gps: [Option<String>; 6] = Default::default();
        for (((out, prior), before), after) in native_gps
            .iter_mut()
            .zip(&self.native_gps)
            .zip(&before.native_gps)
            .zip(&after.native_gps)
        {
            *out = owned(prior, before, after);
        }
        for (((out, prior), before), after) in gps
            .iter_mut()
            .zip(&self.gps)
            .zip(&before.gps)
            .zip(&after.gps)
        {
            *out = owned(prior, before, after);
        }
        Self {
            gps,
            native_gps,
            native_description: owned(
                &self.native_description,
                &before.native_description,
                &after.native_description,
            ),
            rating: owned(&self.rating, &before.rating, &after.rating),
            rating_percent: owned(
                &self.rating_percent,
                &before.rating_percent,
                &after.rating_percent,
            ),
            description: owned(&self.description, &before.description, &after.description),
            title: owned(&self.title, &before.title, &after.title),
            keywords: owned(&self.keywords, &before.keywords, &after.keywords),
            people: owned(&self.people, &before.people, &after.people),
        }
    }
}

/// Revision of the catalogue metadata semantics produced by the current
/// binary. Bump this when decoder or capture behavior changes in a way that
/// requires existing catalogue rows to be hydrated again.
pub(crate) const METADATA_CAPTURE_REVISION: i64 = 1;

/// Version size key for state tracking.
///
/// This is a 1-byte enum representing the version size, saving ~23 bytes
/// per `AssetRecord` compared to storing as a String.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// Exact comparison for transient duplicate admission, not the rounded
    /// persisted metadata hash. Non-finite floats are never duplicate evidence.
    pub(crate) fn same_observation(&self, other: &Self) -> bool {
        fn same_float(a: Option<f64>, b: Option<f64>) -> bool {
            match (a, b) {
                (None, None) => true,
                (Some(a), Some(b)) => a.is_finite() && b.is_finite() && a.to_bits() == b.to_bits(),
                _ => false,
            }
        }
        self.source == other.source
            && self.is_favorite == other.is_favorite
            && self.rating == other.rating
            && same_float(self.latitude, other.latitude)
            && same_float(self.longitude, other.longitude)
            && same_float(self.altitude, other.altitude)
            && self.orientation == other.orientation
            && same_float(self.duration_secs, other.duration_secs)
            && self.timezone_offset == other.timezone_offset
            && self.width == other.width
            && self.height == other.height
            && self.title == other.title
            && self.keywords == other.keywords
            && self.description == other.description
            && self.media_subtype == other.media_subtype
            && self.burst_id == other.burst_id
            && self.is_hidden == other.is_hidden
            && self.is_archived == other.is_archived
            && self.modified_at == other.modified_at
            && self.is_deleted == other.is_deleted
            && self.deleted_at == other.deleted_at
            && self.provider_data == other.provider_data
            && self.metadata_hash == other.metadata_hash
    }

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
    fn duplicate_observation_compares_metadata_beyond_rounded_hash() {
        let mut a = AssetMetadata {
            latitude: Some(1.000_000_000_1),
            ..AssetMetadata::default()
        };
        let mut b = AssetMetadata {
            latitude: Some(1.000_000_000_2),
            ..AssetMetadata::default()
        };
        a.refresh_hash();
        b.refresh_hash();
        assert_eq!(a.metadata_hash, b.metadata_hash);
        assert!(!a.same_observation(&b));
        assert!(a.same_observation(&a));
        a.latitude = Some(f64::NAN);
        assert!(!a.same_observation(&a));
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
    fn test_asset_status_is_one_byte() {
        assert_eq!(size_of::<AssetStatus>(), 1);
    }

    #[test]
    fn test_media_type_is_one_byte() {
        assert_eq!(size_of::<MediaType>(), 1);
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
