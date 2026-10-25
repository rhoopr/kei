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
pub(crate) use db::{
    SparseAttemptOutcome, SparseDeletionCheckpoint, SparseEvidence, SparseIdentity,
    SparseIdentityProof, SparseIdentityStore, SparseSourceId,
};

#[cfg(test)]
pub use db::ImportedRecord;
pub(crate) use db::{
    AssetVerificationState, CheckpointTransition, DownloadContextStateStore, DownloadedFileRecord,
    LegacyFileEvidence, OwnedTempFile, PendingPublicationRecord, ReconciliationCatalogPath,
    ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    ReconciliationStateStore, RetryErrorRetention, ScopedDbSyncToken, TempFileOwnershipStore,
};
pub use db::{
    DownloadStateStore, ImportStateStore, MembershipStore, MetadataRewriteStore, ReportStateStore,
    SqliteStateDb, SyncTokenStore,
};
pub(crate) use types::METADATA_CAPTURE_REVISION;
pub use types::{
    AssetMetadata, AssetRecord, AssetStatus, MediaType, MetadataCapture, RenditionMetadata,
    SyncRunStats, VersionSizeKey,
};

/// Durable per-zone evidence of asset deltas that cannot yet be hydrated.
/// Cleared atomically with a proven replacement provider checkpoint only.
pub(crate) const UNRESOLVED_IDENTITY_PREFIX: &str = "unresolved_asset_identity:";
/// Current provider input lacked a usable source key; unrelated rows cannot
/// certify that obligation. Keep its scoped cursor until explicit proof exists.
pub(crate) const UNACCOUNTED_CURRENT_IDENTITY: &str = "unaccounted_current_v1";

pub(crate) fn unresolved_identity_key(zone: &str) -> String {
    format!("{UNRESOLVED_IDENTITY_PREFIX}{zone}")
}
