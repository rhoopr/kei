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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::VersionSize;

    #[test]
    fn from_version_size_original() {
        assert_eq!(
            AssetVersionSize::from(VersionSize::Original),
            AssetVersionSize::Original
        );
    }

    #[test]
    fn from_version_size_medium() {
        assert_eq!(
            AssetVersionSize::from(VersionSize::Medium),
            AssetVersionSize::Medium
        );
    }

    #[test]
    fn from_version_size_thumb() {
        assert_eq!(
            AssetVersionSize::from(VersionSize::Thumb),
            AssetVersionSize::Thumb
        );
    }

    #[test]
    fn from_version_size_adjusted() {
        assert_eq!(
            AssetVersionSize::from(VersionSize::Adjusted),
            AssetVersionSize::Adjusted
        );
    }

    #[test]
    fn from_version_size_alternative() {
        assert_eq!(
            AssetVersionSize::from(VersionSize::Alternative),
            AssetVersionSize::Alternative
        );
    }

    #[test]
    fn asset_version_size_is_one_byte() {
        assert_eq!(std::mem::size_of::<AssetVersionSize>(), 1);
    }

    #[test]
    fn asset_item_type_debug_output() {
        assert_eq!(format!("{:?}", AssetItemType::Image), "Image");
        assert_eq!(format!("{:?}", AssetItemType::Movie), "Movie");
    }

    #[test]
    fn asset_version_construction_and_field_access() {
        let version = AssetVersion {
            size: 1024,
            url: "https://example.com/photo.jpg".into(),
            asset_type: "public.jpeg".into(),
            checksum: "abc123".into(),
        };

        assert_eq!(version.size, 1024);
        assert_eq!(&*version.url, "https://example.com/photo.jpg");
        assert_eq!(&*version.asset_type, "public.jpeg");
        assert_eq!(&*version.checksum, "abc123");
    }

    #[test]
    fn asset_version_clone() {
        let version = AssetVersion {
            size: 2048,
            url: "https://example.com/video.mov".into(),
            asset_type: "public.mpeg-4".into(),
            checksum: "def456".into(),
        };

        let cloned = version.clone();
        assert_eq!(cloned.size, version.size);
        assert_eq!(&*cloned.url, &*version.url);
        assert_eq!(&*cloned.asset_type, &*version.asset_type);
        assert_eq!(&*cloned.checksum, &*version.checksum);
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
        ];

        // Check all 8 variants have unique values
        let unique: std::collections::HashSet<u8> = variants.iter().copied().collect();
        assert_eq!(
            unique.len(),
            variants.len(),
            "all repr(u8) values must be distinct"
        );
    }
}
