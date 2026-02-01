use crate::types::VersionSize;

#[derive(Debug, Clone)]
#[allow(dead_code)] // fields accessed via pub by download engine
pub struct AssetVersion {
    pub size: u64,
    pub url: String,
    pub asset_type: String,
    pub checksum: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetItemType {
    Image,
    Movie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssetVersionSize {
    Original,
    Alternative,
    Medium,
    Thumb,
    Adjusted,
    LiveOriginal,
    LiveMedium,
    LiveThumb,
}

impl From<VersionSize> for AssetVersionSize {
    fn from(v: VersionSize) -> Self {
        match v {
            VersionSize::Original => AssetVersionSize::Original,
            VersionSize::Medium => AssetVersionSize::Medium,
            VersionSize::Thumb => AssetVersionSize::Thumb,
            VersionSize::Adjusted => AssetVersionSize::Adjusted,
            VersionSize::Alternative => AssetVersionSize::Alternative,
        }
    }
}
