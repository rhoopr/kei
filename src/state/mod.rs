//! State tracking module for persistent sync state.
//!
//! This module provides SQLite-based state tracking for iCloud photo downloads.
//! It tracks which assets have been seen, downloaded, or failed, enabling:
//! - Skip-by-DB downloads (faster than filesystem checks)
//! - Failure tracking and retry
//! - Status reporting
//! - Verification of downloaded files

pub mod db;
pub mod error;
pub mod schema;
pub mod types;

#[cfg(test)]
pub use db::ImportedRecord;
pub(crate) use db::{
    AssetVerificationState, CheckpointTransition, DownloadContextStateStore, DownloadedFileRecord,
    OwnedTempFile, RetryErrorRetention, ScopedDbSyncToken, TempFileOwnershipStore,
};
pub use db::{
    DownloadStateStore, ImportStateStore, MembershipStore, MetadataRewriteStore, ReportStateStore,
    SqliteStateDb, SyncTokenStore,
};
#[cfg(test)]
pub(crate) use types::MetadataCaptureStatus;
pub use types::{
    AssetMetadata, AssetRecord, AssetStatus, MediaType, MetadataCapture, RenditionMetadata,
    SyncRunStats, VersionSizeKey,
};
pub(crate) use types::{METADATA_CAPTURE_REVISION, MetadataCaptureCandidate};
