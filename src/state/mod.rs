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
    OwnedTempFile, ReconciliationCatalogPath, ReconciliationContent, ReconciliationPathKey,
    ReconciliationReservation, ReconciliationStateStore, RetryErrorRetention, ScopedDbSyncToken,
    TempFileOwnershipStore,
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

/// Durable per-zone evidence of asset deltas that cannot yet be hydrated.
/// Cleared atomically with a proven replacement provider checkpoint only.
pub(crate) const UNRESOLVED_IDENTITY_PREFIX: &str = "unresolved_asset_identity:";

pub(crate) fn unresolved_identity_key(zone: &str) -> String {
    format!("{UNRESOLVED_IDENTITY_PREFIX}{zone}")
}
