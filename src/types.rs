use serde::{Deserialize, Serialize};

/// Whether an asset is an image or a video.
///
/// Provider-agnostic — each provider adapter maps its native classification
/// (e.g., iCloud UTI strings) into this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetItemType {
    Image,
    Movie,
}

/// Version size key for asset versions.
///
/// Uses `#[repr(u8)]` to guarantee 1-byte size for better struct packing.
/// Provider-agnostic — maps to every provider's concept of resolution tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AssetVersionSize {
    Original = 0,
    Alternative = 1,
    Medium = 2,
    Thumb = 3,
    Adjusted = 4,
    LiveOriginal = 5,
    LiveMedium = 6,
    LiveThumb = 7,
    LiveAdjusted = 8,
}

impl From<VersionSize> for AssetVersionSize {
    fn from(v: VersionSize) -> Self {
        match v {
            VersionSize::Original => Self::Original,
            VersionSize::Medium => Self::Medium,
            VersionSize::Thumb => Self::Thumb,
            VersionSize::Adjusted => Self::Adjusted,
            VersionSize::Alternative => Self::Alternative,
        }
    }
}

/// Reason for a record change in a delta/incremental sync response.
///
/// Provider-agnostic — each provider maps its native change types into
/// these categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeReason {
    /// New or modified record (state DB determines which).
    Created,
    /// Soft-deleted (moved to trash / recently deleted).
    SoftDeleted,
    /// Permanently purged from the provider.
    HardDeleted,
    /// Hidden from the main library view.
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VersionSize {
    Original,
    Medium,
    Thumb,
    Adjusted,
    Alternative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LivePhotoSize {
    Original,
    Medium,
    Thumb,
    Adjusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Domain {
    Com,
    Cn,
}

impl Domain {
    pub const fn as_str(&self) -> &str {
        match self {
            Self::Com => "com",
            Self::Cn => "cn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
pub enum FileMatchPolicy {
    #[value(name = "name-size-dedup-with-suffix")]
    #[serde(rename = "name-size-dedup-with-suffix")]
    NameSizeDedupWithSuffix,
    #[value(name = "name-id7")]
    #[serde(rename = "name-id7")]
    NameId7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
pub enum RawTreatmentPolicy {
    #[value(name = "as-is")]
    #[serde(rename = "as-is")]
    Unchanged,
    #[value(name = "original")]
    #[serde(rename = "original")]
    PreferOriginal,
    #[value(name = "alternative")]
    #[serde(rename = "alternative")]
    PreferAlternative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LivePhotoMovFilenamePolicy {
    Suffix,
    Original,
}

/// Controls which components of live photos are downloaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "kebab-case")]
pub enum LivePhotoMode {
    /// Download both the still image and the MOV video
    #[default]
    Both,
    /// Download only the still image, skip the MOV
    #[value(name = "image-only")]
    ImageOnly,
    /// Download only the MOV video, skip the still image
    #[value(name = "video-only")]
    VideoOnly,
    /// Skip live photos entirely (both image and MOV)
    Skip,
}

impl LivePhotoSize {
    pub fn to_asset_version_size(self) -> AssetVersionSize {
        match self {
            Self::Original => AssetVersionSize::LiveOriginal,
            Self::Medium => AssetVersionSize::LiveMedium,
            Self::Thumb => AssetVersionSize::LiveThumb,
            Self::Adjusted => AssetVersionSize::LiveAdjusted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_live_photo_size_to_asset_version_size() {
        assert_eq!(
            LivePhotoSize::Original.to_asset_version_size(),
            AssetVersionSize::LiveOriginal
        );
        assert_eq!(
            LivePhotoSize::Medium.to_asset_version_size(),
            AssetVersionSize::LiveMedium
        );
        assert_eq!(
            LivePhotoSize::Thumb.to_asset_version_size(),
            AssetVersionSize::LiveThumb
        );
        assert_eq!(
            LivePhotoSize::Adjusted.to_asset_version_size(),
            AssetVersionSize::LiveAdjusted
        );
    }

    #[test]
    fn test_domain_as_str() {
        assert_eq!(Domain::Com.as_str(), "com");
        assert_eq!(Domain::Cn.as_str(), "cn");
    }

    #[test]
    fn version_size_serde_round_trip() {
        for (variant, expected) in [
            (VersionSize::Original, "\"original\""),
            (VersionSize::Medium, "\"medium\""),
            (VersionSize::Thumb, "\"thumb\""),
            (VersionSize::Adjusted, "\"adjusted\""),
            (VersionSize::Alternative, "\"alternative\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: VersionSize = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn live_photo_size_serde_round_trip() {
        for (variant, expected) in [
            (LivePhotoSize::Original, "\"original\""),
            (LivePhotoSize::Medium, "\"medium\""),
            (LivePhotoSize::Thumb, "\"thumb\""),
            (LivePhotoSize::Adjusted, "\"adjusted\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: LivePhotoSize = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn domain_serde_round_trip() {
        for (variant, expected) in [(Domain::Com, "\"com\""), (Domain::Cn, "\"cn\"")] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: Domain = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn log_level_serde_round_trip() {
        for (variant, expected) in [
            (LogLevel::Debug, "\"debug\""),
            (LogLevel::Info, "\"info\""),
            (LogLevel::Warn, "\"warn\""),
            (LogLevel::Error, "\"error\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: LogLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn file_match_policy_serde_round_trip() {
        for (variant, expected) in [
            (
                FileMatchPolicy::NameSizeDedupWithSuffix,
                "\"name-size-dedup-with-suffix\"",
            ),
            (FileMatchPolicy::NameId7, "\"name-id7\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: FileMatchPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn raw_treatment_policy_serde_round_trip() {
        for (variant, expected) in [
            (RawTreatmentPolicy::Unchanged, "\"as-is\""),
            (RawTreatmentPolicy::PreferOriginal, "\"original\""),
            (RawTreatmentPolicy::PreferAlternative, "\"alternative\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: RawTreatmentPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn live_photo_mode_serde_round_trip() {
        for (variant, expected) in [
            (LivePhotoMode::Both, "\"both\""),
            (LivePhotoMode::ImageOnly, "\"image-only\""),
            (LivePhotoMode::VideoOnly, "\"video-only\""),
            (LivePhotoMode::Skip, "\"skip\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: LivePhotoMode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn live_photo_mov_filename_policy_serde_round_trip() {
        for (variant, expected) in [
            (LivePhotoMovFilenamePolicy::Suffix, "\"suffix\""),
            (LivePhotoMovFilenamePolicy::Original, "\"original\""),
        ] {
            let json = serde_json::to_string(&variant).unwrap();
            assert_eq!(json, expected);
            let parsed: LivePhotoMovFilenamePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn from_version_size_all_variants() {
        for (input, expected) in [
            (VersionSize::Original, AssetVersionSize::Original),
            (VersionSize::Medium, AssetVersionSize::Medium),
            (VersionSize::Thumb, AssetVersionSize::Thumb),
            (VersionSize::Adjusted, AssetVersionSize::Adjusted),
            (VersionSize::Alternative, AssetVersionSize::Alternative),
        ] {
            assert_eq!(AssetVersionSize::from(input), expected, "from {input:?}");
        }
    }

    #[test]
    fn asset_version_size_is_one_byte() {
        assert_eq!(std::mem::size_of::<AssetVersionSize>(), 1);
    }

    #[test]
    fn asset_version_size_variants_have_distinct_repr_values() {
        let variants = [
            AssetVersionSize::Original as u8,
            AssetVersionSize::Alternative as u8,
            AssetVersionSize::Medium as u8,
            AssetVersionSize::Thumb as u8,
            AssetVersionSize::Adjusted as u8,
            AssetVersionSize::LiveOriginal as u8,
            AssetVersionSize::LiveMedium as u8,
            AssetVersionSize::LiveThumb as u8,
            AssetVersionSize::LiveAdjusted as u8,
        ];
        let unique: std::collections::HashSet<u8> = variants.iter().copied().collect();
        assert_eq!(
            unique.len(),
            variants.len(),
            "all repr(u8) values must be distinct"
        );
    }
}
