use std::sync::Arc;

pub use crate::types::{AssetItemType, AssetVersionSize, ChangeReason};

/// Information about a downloadable asset version.
///
/// Uses `Box<str>` instead of `String` for url and checksum
/// to save 8 bytes per field (16 vs 24 bytes) since these strings are
/// never mutated after construction.
/// `asset_type` uses `Arc<str>` so repeated UTI values (e.g. "public.jpeg")
/// share a single allocation via the global string interner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetVersion {
    pub size: u64,
    pub url: Box<str>,
    pub asset_type: Arc<str>,
    pub checksum: Box<str>,
}
