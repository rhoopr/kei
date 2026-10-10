use serde::{Deserialize, Serialize};

/// Whether an asset is an image or a video.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetItemType {
    Image,
    Movie,
}

/// Version size key for asset versions.
///
/// Uses `#[repr(u8)]` to guarantee 1-byte size for better struct packing.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeReason {
    /// New or modified record (state DB determines which).
    Created,
    /// Soft-deleted (moved to trash / recently deleted).
    SoftDeleted,
    /// Permanently purged from the source.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "lowercase")]
pub enum PhotoResolution {
    None = 0,
    Original = 1,
    Medium = 2,
    Thumb = 3,
}

impl PhotoResolution {
    pub const fn to_asset_version_size(self) -> Option<AssetVersionSize> {
        match self {
            Self::None => None,
            Self::Original => Some(AssetVersionSize::Original),
            Self::Medium => Some(AssetVersionSize::Medium),
            Self::Thumb => Some(AssetVersionSize::Thumb),
        }
    }
}

impl From<VersionSize> for PhotoResolution {
    fn from(value: VersionSize) -> Self {
        match value {
            VersionSize::Original | VersionSize::Adjusted | VersionSize::Alternative => {
                Self::Original
            }
            VersionSize::Medium => Self::Medium,
            VersionSize::Thumb => Self::Thumb,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "lowercase")]
pub enum LivePhotoResolution {
    Original = 0,
    Medium = 1,
    Thumb = 2,
}

impl LivePhotoResolution {
    pub const fn to_asset_version_size(self) -> AssetVersionSize {
        match self {
            Self::Original => AssetVersionSize::LiveOriginal,
            Self::Medium => AssetVersionSize::LiveMedium,
            Self::Thumb => AssetVersionSize::LiveThumb,
        }
    }
}

impl From<LivePhotoSize> for LivePhotoResolution {
    fn from(value: LivePhotoSize) -> Self {
        match value.to_asset_version_size() {
            AssetVersionSize::LiveOriginal | AssetVersionSize::LiveAdjusted => Self::Original,
            AssetVersionSize::LiveMedium => Self::Medium,
            AssetVersionSize::LiveThumb => Self::Thumb,
            _ => Self::Original,
        }
    }
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
#[repr(u8)]
pub enum FileMatchPolicy {
    #[value(name = "name-size-dedup-with-suffix")]
    #[serde(rename = "name-size-dedup-with-suffix")]
    NameSizeDedupWithSuffix = 0,
    #[value(name = "name-id7")]
    #[serde(rename = "name-id7")]
    NameId7 = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[repr(u8)]
pub enum RawTreatmentPolicy {
    #[value(name = "as-is")]
    #[serde(rename = "as-is")]
    Unchanged = 0,
    #[value(name = "original")]
    #[serde(rename = "original")]
    PreferOriginal = 1,
    #[value(name = "alternative")]
    #[serde(rename = "alternative")]
    PreferAlternative = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
#[repr(u8)]
pub enum RawPolicy {
    #[value(name = "as-is")]
    #[serde(rename = "as-is")]
    AsIs = 0,
    #[value(name = "prefer-raw")]
    #[serde(rename = "prefer-raw")]
    PreferRaw = 1,
    #[value(name = "prefer-jpeg")]
    #[serde(rename = "prefer-jpeg")]
    PreferJpeg = 2,
}

impl From<RawTreatmentPolicy> for RawPolicy {
    fn from(value: RawTreatmentPolicy) -> Self {
        match value {
            RawTreatmentPolicy::Unchanged => Self::AsIs,
            RawTreatmentPolicy::PreferOriginal => Self::PreferRaw,
            RawTreatmentPolicy::PreferAlternative => Self::PreferJpeg,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "lowercase")]
pub enum LivePhotoMovFilenamePolicy {
    Suffix = 0,
    Original = 1,
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
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;

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
            let json = serde_json::to_string(&variant).expect("serialize VersionSize");
            assert_eq!(json, expected);
            let parsed: VersionSize = serde_json::from_str(&json).expect("deserialize VersionSize");
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
            let json = serde_json::to_string(&variant).expect("serialize LivePhotoSize");
            assert_eq!(json, expected);
            let parsed: LivePhotoSize =
                serde_json::from_str(&json).expect("deserialize LivePhotoSize");
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn domain_serde_round_trip() {
        for (variant, expected) in [(Domain::Com, "\"com\""), (Domain::Cn, "\"cn\"")] {
            let json = serde_json::to_string(&variant).expect("serialize Domain");
            assert_eq!(json, expected);
            let parsed: Domain = serde_json::from_str(&json).expect("deserialize Domain");
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
            let json = serde_json::to_string(&variant).expect("serialize LogLevel");
            assert_eq!(json, expected);
            let parsed: LogLevel = serde_json::from_str(&json).expect("deserialize LogLevel");
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
            let json = serde_json::to_string(&variant).expect("serialize FileMatchPolicy");
            assert_eq!(json, expected);
            let parsed: FileMatchPolicy =
                serde_json::from_str(&json).expect("deserialize FileMatchPolicy");
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
            let json = serde_json::to_string(&variant).expect("serialize RawTreatmentPolicy");
            assert_eq!(json, expected);
            let parsed: RawTreatmentPolicy =
                serde_json::from_str(&json).expect("deserialize RawTreatmentPolicy");
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
            let json = serde_json::to_string(&variant).expect("serialize LivePhotoMode");
            assert_eq!(json, expected);
            let parsed: LivePhotoMode =
                serde_json::from_str(&json).expect("deserialize LivePhotoMode");
            assert_eq!(parsed, variant);
        }
    }

    #[test]
    fn live_photo_mov_filename_policy_serde_round_trip() {
        for (variant, expected) in [
            (LivePhotoMovFilenamePolicy::Suffix, "\"suffix\""),
            (LivePhotoMovFilenamePolicy::Original, "\"original\""),
        ] {
            let json =
                serde_json::to_string(&variant).expect("serialize LivePhotoMovFilenamePolicy");
            assert_eq!(json, expected);
            let parsed: LivePhotoMovFilenamePolicy =
                serde_json::from_str(&json).expect("deserialize LivePhotoMovFilenamePolicy");
            assert_eq!(parsed, variant);
        }
    }

    fn assert_serde_string_rejected<T>(value: &str)
    where
        T: Debug + DeserializeOwned,
    {
        let json = serde_json::to_string(value).expect("serialize test string");
        serde_json::from_str::<T>(&json).expect_err("invalid enum value should be rejected");
    }

    #[test]
    fn durable_config_enums_reject_unknown_serde_strings() {
        assert_serde_string_rejected::<VersionSize>("full");
        assert_serde_string_rejected::<LivePhotoSize>("live-original");
        assert_serde_string_rejected::<PhotoResolution>("adjusted");
        assert_serde_string_rejected::<LivePhotoResolution>("adjusted");
        assert_serde_string_rejected::<Domain>("icloud.com");
        assert_serde_string_rejected::<LogLevel>("warning");
        assert_serde_string_rejected::<FileMatchPolicy>("name-size");
        assert_serde_string_rejected::<RawTreatmentPolicy>("prefer-raw");
        assert_serde_string_rejected::<RawPolicy>("original");
        assert_serde_string_rejected::<LivePhotoMovFilenamePolicy>("mov");
        assert_serde_string_rejected::<LivePhotoMode>("video_only");
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

/// Placement of selected adjusted renditions. Primary mode opts into
/// preservation-backed management of kei-owned current paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditedNaming {
    #[default]
    Suffix,
    Primary,
}
