//! Types shared by the provider adapter and the sync core, and ownership
//! checks for previous-engine state files.

pub mod db;
pub mod error;
pub mod types;

pub use types::{AssetMetadata, RenditionMetadata, VersionSizeKey};
