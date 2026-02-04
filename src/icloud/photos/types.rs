use crate::types::VersionSize;

/// Information about a downloadable asset version.
///
/// Uses `Box<str>` instead of `String` for url, asset_type, and checksum
/// to save 8 bytes per field (16 vs 24 bytes) since these strings are
/// never mutated after construction.
#[derive(Debug, Clone)]
pub struct AssetVersion {
    pub size: u64,
    pub url: Box<str>,
    pub asset_type: Box<str>,
    pub checksum: Box<str>,
}

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
