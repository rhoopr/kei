use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum, Serialize, Deserialize)]
pub enum VersionSize {
    Original,
    Medium,
    Thumb,
    Adjusted,
    Alternative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum LivePhotoSize {
    Original,
    Medium,
    Thumb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Domain {
    Com,
    Cn,
}

impl Domain {
    pub fn as_str(&self) -> &str {
        match self {
            Domain::Com => "com",
            Domain::Cn => "cn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FileMatchPolicy {
    #[value(name = "name-size-dedup-with-suffix")]
    NameSizeDedupWithSuffix,
    #[value(name = "name-id7")]
    NameId7,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RawTreatmentPolicy {
    #[value(name = "as-is")]
    Unchanged,
    #[value(name = "original")]
    PreferOriginal,
    #[value(name = "alternative")]
    PreferAlternative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LivePhotoMovFilenamePolicy {
    Suffix,
    Original,
}

impl LivePhotoSize {
    pub fn to_asset_version_size(self) -> crate::icloud::photos::AssetVersionSize {
        use crate::icloud::photos::AssetVersionSize;
        match self {
            LivePhotoSize::Original => AssetVersionSize::LiveOriginal,
            LivePhotoSize::Medium => AssetVersionSize::LiveMedium,
            LivePhotoSize::Thumb => AssetVersionSize::LiveThumb,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icloud::photos::AssetVersionSize;

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
    }

    #[test]
    fn test_domain_as_str() {
        assert_eq!(Domain::Com.as_str(), "com");
        assert_eq!(Domain::Cn.as_str(), "cn");
    }
}
