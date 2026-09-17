//! State-store roles and the records exchanged with their callers.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::state::error::StateError;
use crate::state::types::{
    AssetRecord, MetadataCapture, MetadataCaptureCandidate, MetadataCaptureStatus, SyncRunStats,
    SyncSummary, VersionSizeKey,
};

fn unsupported_album_membership_api(operation: &'static str) -> StateError {
    StateError::Invariant {
        operation,
        detail: "album membership snapshots are not implemented by this state store".into(),
    }
}

/// Snapshot of an already-imported asset, returned by
/// [`ImportStateStore::get_all_imported_records`].
///
/// `import-existing` consults this on every match candidate to decide whether
/// the on-disk file can be trusted as unchanged since the last adopt. If
/// `local_path`, `imported_size`, and `imported_mtime` all match what the
/// filesystem reports right now, the SHA-256 re-read is skipped. Pre-v11
/// rows have `imported_size`/`imported_mtime` of `None`, which forces a real
/// hash on the first post-upgrade pass.
#[derive(Debug, Clone)]
pub struct ImportedRecord {
    pub local_path: PathBuf,
    pub local_checksum: String,
    pub imported_size: Option<u64>,
    pub imported_mtime: Option<i64>,
}

/// Compact downloaded-state projection used to preload sync decisions.
#[derive(Debug)]
pub(crate) struct DownloadedFileRecord {
    pub(crate) library: String,
    pub(crate) id: String,
    pub(crate) version_size: VersionSizeKey,
    pub(crate) checksum: String,
    pub(crate) local_path: Option<PathBuf>,
    pub(crate) local_checksum: Option<String>,
    pub(crate) download_checksum: Option<String>,
}

/// Metadata-rewrite debt selected for one bounded drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataRewriteQueue {
    Ordinary,
    CaptureRepair,
}

/// Durable capture-repair state attached to a queued rewrite without adding
/// repair-only fields to [`AssetRecord`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureRepairReceipt {
    Pending {
        metadata_hash: String,
    },
    Prepared {
        metadata_hash: String,
        output_checksum: String,
        output_size: u64,
    },
}

impl CaptureRepairReceipt {
    pub(super) fn metadata_hash(&self) -> &str {
        match self {
            Self::Pending { metadata_hash } | Self::Prepared { metadata_hash, .. } => metadata_hash,
        }
    }
}

/// One queued metadata rewrite and its independently persisted capture debt.
#[derive(Debug, Clone)]
pub struct PendingMetadataRewrite {
    pub asset: AssetRecord,
    pub capture_repair_receipt: Option<CaptureRepairReceipt>,
    /// SHA-256 captured from a verified download before embedding, for this
    /// exact path and provider rendition. Never inferred from local checksums.
    #[cfg_attr(
        not(feature = "xmp"),
        allow(
            dead_code,
            reason = "native-only builds retain provenance for later sidecar writes"
        )
    )]
    pub source_checksum: Option<String>,
}

/// Debt retired by one atomic metadata-rewrite completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataRewriteCompletion {
    None,
    Ordinary,
    CaptureRepair,
    Both,
}

impl MetadataRewriteCompletion {
    pub(super) const fn clears(self, queue: MetadataRewriteQueue) -> bool {
        matches!(
            (self, queue),
            (Self::Ordinary | Self::Both, MetadataRewriteQueue::Ordinary)
                | (
                    Self::CaptureRepair | Self::Both,
                    MetadataRewriteQueue::CaptureRepair
                )
        )
    }
}

/// Durable evidence that kei claimed one exact temporary download path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnedTempFile {
    pub(crate) path: PathBuf,
    pub(crate) claimed_at: i64,
}

/// Checked, absolute, platform-normalized key supplied by the path planner.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReconciliationPathKey(pub(crate) String);

/// Provider identity of one rendition's bytes, independent of URLs and metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ReconciliationContent {
    pub(crate) checksum: Box<str>,
    pub(crate) size: u64,
}

/// One immutable destination choice, retained across restarts and config drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconciliationReservation {
    /// None preserves a pre-generation reservation without guessing its content.
    pub(crate) content: Option<ReconciliationContent>,
    pub(crate) library: Arc<str>,
    pub(crate) asset_id: Box<str>,
    pub(crate) version_size: VersionSizeKey,
    pub(crate) requested_path_key: ReconciliationPathKey,
    pub(crate) destination_path_key: ReconciliationPathKey,
    pub(crate) destination_path: PathBuf,
}

/// Current or historical catalog ownership, including pending and deleted rows.
#[derive(Debug)]
pub(crate) struct ReconciliationCatalogPath {
    pub(crate) library: Arc<str>,
    pub(crate) asset_id: Box<str>,
    pub(crate) version_size: VersionSizeKey,
    pub(crate) path: PathBuf,
}

/// Durable destination ownership for local catalog reconciliation.
#[async_trait]
pub(crate) trait ReconciliationStateStore: Send + Sync {
    async fn get_reconciliation_catalog_paths(
        &self,
    ) -> Result<Vec<ReconciliationCatalogPath>, StateError>;

    async fn get_reconciliation_reservations(
        &self,
    ) -> Result<Vec<ReconciliationReservation>, StateError>;

    /// Commit every choice before any copy. Conflicting ownership or changed
    /// choices fail the whole transaction; callers must not publish on error.
    async fn reserve_reconciliation_paths(
        &self,
        reservations: &[ReconciliationReservation],
    ) -> Result<(), StateError>;
}

/// State operation used only to preload the download context.
#[async_trait]
pub(crate) trait DownloadContextStateStore: Send + Sync {
    async fn get_downloaded_file_records(&self) -> Result<Vec<DownloadedFileRecord>, StateError>;
}

/// State operations for the temporary-file ownership ledger.
#[async_trait]
pub(crate) trait TempFileOwnershipStore: Send + Sync {
    async fn claim_temp_file(&self, path: &Path) -> Result<(), StateError>;
    async fn get_owned_temp_files_before(
        &self,
        claimed_before: i64,
    ) -> Result<Vec<OwnedTempFile>, StateError>;
    async fn retire_temp_files(&self, paths: &[PathBuf]) -> Result<u64, StateError>;
}

/// Live album-membership row keyed by CloudKit asset record name.
///
/// Album relation records refer to `PhotoAsset::asset_record_name()`, while
/// downloaded files and legacy `assets` rows are keyed by the master record
/// name returned by `PhotoAsset::id()`. Keep both identifiers when available
/// so later routing can bridge relation deltas to existing download state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumMembershipRecord {
    pub library: String,
    pub asset_record_name: String,
    pub master_record_name: Option<String>,
    pub container_id: String,
    pub generation: i64,
    pub source: String,
}

/// Album and people rows for a bounded set of asset IDs in one library.
#[derive(Debug, Default)]
pub(crate) struct AssetGroupingRows {
    pub(crate) albums: Vec<(String, String)>,
    pub(crate) people: Vec<(String, String)>,
}

/// Scoped database-level `/changes/database` pre-check token.
///
/// This is not a per-zone coverage token. The canonical JSON fields are
/// stored alongside the hash so a hash match alone never proves that a
/// watch-mode no-change skip is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopedDbSyncToken {
    pub(crate) provider: String,
    pub(crate) account: String,
    pub(crate) shape_version: i64,
    pub(crate) scope_hash: String,
    pub(crate) selected_zones_json: String,
    pub(crate) scope_json: String,
    pub(crate) token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckpointTransition {
    pub(crate) metadata_updates: Vec<(String, String)>,
    pub(crate) metadata_deletes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetVerificationState {
    Unknown,
    TransientFailure,
}

impl AssetVerificationState {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::TransientFailure => "transient_failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryErrorRetention {
    Clear,
    Preserve(&'static str),
}

/// State operations used by the download producer and finalizer.
#[allow(
    dead_code,
    reason = "role traits expose test and command slices that are not all used in the main binary"
)]
#[async_trait]
pub trait DownloadStateStore: Send + Sync {
    #[cfg(test)]
    async fn should_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError>;

    async fn upsert_seen(&self, record: &AssetRecord) -> Result<(), StateError>;
    /// Persist the result of a landed local file.
    ///
    /// `mark_downloaded` and `mark_soft_deleted` may target the same
    /// `(library, id, version_size)` row during incremental sync. Keep this
    /// method limited to download-result columns so provider tombstones
    /// (`is_deleted`, `deleted_at`) survive regardless of writer ordering.
    async fn mark_downloaded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), StateError>;
    async fn mark_downloaded_with_capture_repair(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        if mark_capture_repair {
            return Err(StateError::Invariant {
                operation: "mark_downloaded_with_capture_repair",
                detail: "capture-repair download finalization is not implemented by this store"
                    .into(),
            });
        }
        self.mark_downloaded(
            library,
            id,
            version_size,
            local_path,
            local_checksum,
            download_checksum,
        )
        .await
    }
    /// Finalize bytes received by the verified download pipeline.
    ///
    /// `download_checksum`, when present, must hash the received bytes before
    /// any kei metadata write. Adoption and local reconciliation must instead
    /// use `mark_downloaded` or `mark_downloaded_with_capture_repair`.
    /// Stores without source-provenance support retain unknown evidence.
    ///
    /// # Errors
    /// Returns a state error if downloaded-state finalization fails.
    async fn mark_verified_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        self.mark_downloaded_with_capture_repair(
            library,
            id,
            version_size,
            local_path,
            local_checksum,
            download_checksum,
            mark_capture_repair,
        )
        .await
    }
    async fn mark_failed(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError>;
    async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError>;
    /// Return live policy-excluded identities for targeted deletion revalidation.
    ///
    /// These identities are not actionable retry work. Callers must not plan
    /// downloads from this projection.
    async fn get_policy_excluded_ids_for_revalidation(
        &self,
        library: &str,
    ) -> Result<Vec<String>, StateError>;
    async fn reset_failed(&self) -> Result<u64, StateError>;
    async fn prepare_for_retry(
        &self,
        library: Option<&str>,
        error_retention: RetryErrorRetention,
    ) -> Result<(u64, u64, u64), StateError>;
    async fn prune_source_deleted_retries(
        &self,
        _library: Option<&str>,
    ) -> Result<u64, StateError> {
        Ok(0)
    }
    async fn promote_pending_to_failed(&self, seen_since: i64) -> Result<u64, StateError>;
    async fn prune_stale_pending_not_seen_since(
        &self,
        _library: &str,
        _seen_since: i64,
    ) -> Result<u64, StateError> {
        Ok(0)
    }
    async fn prune_pending_asset_versions(
        &self,
        _library: &str,
        _asset_versions: &[(String, String)],
    ) -> Result<u64, StateError> {
        Ok(0)
    }
    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String, String)>, StateError>;
    /// Assets whose downloaded rows survive a provider deletion. Staleness
    /// checks skip these, so an implementor must answer deliberately rather
    /// than inherit an empty default.
    async fn get_soft_deleted_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String)>, StateError>;
    async fn get_all_known_ids(&self) -> Result<HashSet<(String, String)>, StateError>;
    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError>;
    async fn get_downloaded_local_paths(
        &self,
    ) -> Result<HashMap<(String, String, String), PathBuf>, StateError> {
        Ok(HashMap::new())
    }
    async fn get_attempt_counts(&self) -> Result<HashMap<(String, String), u32>, StateError>;
    async fn touch_last_seen_many(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<(), StateError>;
    async fn upsert_asset_master_mapping(
        &self,
        _library: &str,
        _asset_record_name: &str,
        _master_record_name: &str,
    ) -> Result<(), StateError> {
        Ok(())
    }
    async fn get_master_record_name_for_asset(
        &self,
        _library: &str,
        _asset_record_name: &str,
    ) -> Result<Option<String>, StateError> {
        Ok(None)
    }
    async fn get_asset_record_names_for_master(
        &self,
        _library: &str,
        _master_record_name: &str,
    ) -> Result<Vec<String>, StateError> {
        Ok(Vec::new())
    }
    async fn get_asset_master_mappings(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        Ok(HashSet::new())
    }
    async fn get_legacy_master_state_owners(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        Ok(HashSet::new())
    }
    async fn claim_legacy_master_state_owner(
        &self,
        _library: &str,
        _master_record_name: &str,
        _asset_record_name: &str,
    ) -> Result<bool, StateError> {
        Ok(false)
    }
    async fn set_asset_verification(
        &self,
        _library: &str,
        _id: &str,
        _version_size: &str,
        _state: AssetVerificationState,
        _reason: &str,
    ) -> Result<(), StateError> {
        Ok(())
    }
    async fn clear_asset_verification(
        &self,
        _library: &str,
        _id: &str,
        _version_size: &str,
    ) -> Result<(), StateError> {
        Ok(())
    }
    /// Mark a live asset as intentionally outside the active download policy.
    ///
    /// Returns `true` when a pending row was transitioned. A later
    /// producer-dispatched `upsert_seen` reactivates the row to pending.
    async fn mark_policy_excluded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<bool, StateError>;
    async fn backfill_asset_master_mappings_from_album_memberships(
        &self,
    ) -> Result<u64, StateError> {
        Ok(0)
    }
    /// Persist a provider tombstone without changing local download state.
    ///
    /// A row can legitimately be both `status = 'downloaded'` and
    /// `is_deleted = 1`: the local file landed, and the provider later reported
    /// the source asset deleted. Do not clear download status, local paths,
    /// checksums, or error state here.
    async fn mark_soft_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<(), StateError>;
    async fn mark_soft_deleted_affected(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        self.mark_soft_deleted(library, asset_id, deleted_at)
            .await?;
        Ok(1)
    }
    /// Resolve a provider delete while respecting local download state.
    ///
    /// Every row stays in the catalog with a source tombstone so kei retains
    /// provider history and local-file evidence. Actionable retry and status
    /// readers exclude tombstoned pending/failed rows.
    async fn resolve_source_deleted_affected(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        self.mark_soft_deleted_affected(library, asset_id, deleted_at)
            .await
    }
    async fn mark_master_family_soft_deleted_affected(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        self.mark_soft_deleted_affected(library, master_record_name, deleted_at)
            .await
    }
    async fn resolve_master_family_source_deleted_affected(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        self.mark_master_family_soft_deleted_affected(library, master_record_name, deleted_at)
            .await
    }
    async fn mark_hidden_at_source(&self, library: &str, asset_id: &str) -> Result<(), StateError>;
    async fn mark_hidden_at_source_affected(
        &self,
        library: &str,
        asset_id: &str,
    ) -> Result<usize, StateError> {
        self.mark_hidden_at_source(library, asset_id).await?;
        Ok(1)
    }
}

/// Import-time adoption and imported-file snapshot reads.
#[async_trait]
pub trait ImportStateStore: Send + Sync {
    async fn import_adopt(
        &self,
        record: &AssetRecord,
        local_path: &Path,
        local_checksum: &str,
        imported_size: u64,
        imported_mtime: Option<i64>,
    ) -> Result<(), StateError>;

    async fn get_all_imported_records(
        &self,
        library: &str,
    ) -> Result<HashMap<(String, String), ImportedRecord>, StateError>;
}

/// Summary, status-page, failed-sample, and sync-run ledger reads/writes.
#[allow(
    dead_code,
    reason = "status and test-only readers are part of the report role even when the main binary does not call every method"
)]
#[async_trait]
pub trait ReportStateStore: Send + Sync {
    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError>;
    async fn get_failed_sample(&self, limit: u32) -> Result<(Vec<AssetRecord>, u64), StateError>;
    async fn get_failed_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError>;
    async fn get_pending_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError>;
    async fn get_summary(&self) -> Result<SyncSummary, StateError>;
    async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError>;
    async fn start_sync_run_at(&self, started_at: DateTime<Utc>) -> Result<i64, StateError>;
    async fn start_sync_run(&self) -> Result<i64, StateError>;
    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError>;
    async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError>;
}

/// Metadata key-value operations used for sync tokens and state markers.
#[allow(
    dead_code,
    reason = "startup diagnostics and tests use only part of the sync-token role in some build targets"
)]
#[async_trait]
pub trait SyncTokenStore: Send + Sync {
    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError>;
    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError>;
    async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError>;
    async fn commit_checkpoint_transition(
        &self,
        _transition: CheckpointTransition,
    ) -> Result<(), StateError> {
        Err(StateError::Invariant {
            operation: "commit_checkpoint_transition",
            detail: "atomic checkpoint transitions are not implemented by this state store".into(),
        })
    }
    async fn get_scoped_db_sync_token(
        &self,
        _provider: &str,
        _account: &str,
        _shape_version: i64,
        _scope_hash: &str,
    ) -> Result<Option<ScopedDbSyncToken>, StateError> {
        Ok(None)
    }
    async fn upsert_scoped_db_sync_token(
        &self,
        _token: ScopedDbSyncToken,
    ) -> Result<(), StateError> {
        Err(StateError::Invariant {
            operation: "upsert_scoped_db_sync_token",
            detail: "scoped db sync tokens are not implemented by this state store".into(),
        })
    }
    async fn delete_scoped_db_sync_tokens(&self) -> Result<u64, StateError> {
        Ok(0)
    }
    async fn begin_enum_progress(&self, zone: &str) -> Result<(), StateError>;
    async fn end_enum_progress(&self, zone: &str) -> Result<(), StateError>;
    async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError>;
}

/// Album and people membership reads/writes.
#[async_trait]
pub trait MembershipStore: Send + Sync {
    async fn add_asset_album(
        &self,
        library: &str,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError>;
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    async fn get_all_asset_albums(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError>;
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    async fn get_all_asset_people(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError>;

    async fn get_asset_groupings(
        &self,
        _library: &str,
        _asset_ids: &[&str],
    ) -> Result<AssetGroupingRows, StateError> {
        Err(StateError::Invariant {
            operation: "get_asset_groupings",
            detail: "bounded asset grouping reads are not implemented by this state store".into(),
        })
    }

    async fn upsert_album_container(
        &self,
        _library: &str,
        _container_id: &str,
        _album_name: &str,
        _pass_kind: &str,
    ) -> Result<(), StateError> {
        Err(unsupported_album_membership_api("upsert_album_container"))
    }
    async fn mark_album_container_deleted(
        &self,
        _library: &str,
        _container_id: &str,
    ) -> Result<(), StateError> {
        Err(unsupported_album_membership_api(
            "mark_album_container_deleted",
        ))
    }
    async fn start_album_membership_snapshot(
        &self,
        _library: &str,
        _container_id: &str,
        _enum_config_hash: Option<&str>,
    ) -> Result<i64, StateError> {
        Err(unsupported_album_membership_api(
            "start_album_membership_snapshot",
        ))
    }
    async fn add_album_membership_to_snapshot(
        &self,
        _library: &str,
        _container_id: &str,
        _generation: i64,
        _asset_record_name: &str,
        _master_record_name: Option<&str>,
        _source: &str,
    ) -> Result<(), StateError> {
        Err(unsupported_album_membership_api(
            "add_album_membership_to_snapshot",
        ))
    }

    /// Add or refresh an album relation learned from `/changes/zone`.
    ///
    /// Returns whether the relation's album container was already known when
    /// the row was applied.
    async fn upsert_album_membership_delta(
        &self,
        _library: &str,
        _container_id: &str,
        _asset_record_name: &str,
        _master_record_name: Option<&str>,
        _source: &str,
    ) -> Result<bool, StateError> {
        Err(unsupported_album_membership_api(
            "upsert_album_membership_delta",
        ))
    }

    /// Mark an album relation deleted from `/changes/zone`.
    ///
    /// Returns whether the relation's album container was already known when
    /// the tombstone was applied.
    async fn mark_album_membership_deleted(
        &self,
        _library: &str,
        _container_id: &str,
        _asset_record_name: &str,
    ) -> Result<bool, StateError> {
        Err(unsupported_album_membership_api(
            "mark_album_membership_deleted",
        ))
    }
    async fn complete_album_membership_snapshot(
        &self,
        _library: &str,
        _container_id: &str,
        _generation: i64,
    ) -> Result<(), StateError> {
        Err(unsupported_album_membership_api(
            "complete_album_membership_snapshot",
        ))
    }
    async fn invalidate_album_membership_snapshot(
        &self,
        _library: &str,
        _container_id: &str,
    ) -> Result<(), StateError> {
        Err(unsupported_album_membership_api(
            "invalidate_album_membership_snapshot",
        ))
    }
    async fn selected_album_containers_have_complete_snapshots(
        &self,
        _library: &str,
        _container_ids: &[&str],
    ) -> Result<bool, StateError> {
        Err(unsupported_album_membership_api(
            "selected_album_containers_have_complete_snapshots",
        ))
    }
    async fn get_live_selected_album_memberships_for_asset(
        &self,
        _library: &str,
        _asset_record_name: &str,
        _selected_container_ids: &[&str],
    ) -> Result<Vec<AlbumMembershipRecord>, StateError> {
        Err(unsupported_album_membership_api(
            "get_live_selected_album_memberships_for_asset",
        ))
    }
}

/// Metadata rewrite markers and hashes.
#[async_trait]
pub trait MetadataRewriteStore: Send + Sync {
    async fn record_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError>;
    async fn refresh_downloaded_asset_metadata(
        &self,
        library: &str,
        asset_id: &str,
        capture: (&MetadataCapture, DateTime<Utc>, Option<DateTime<Utc>>),
        mark_for_rewrite: bool,
        mark_capture_repair: bool,
        capture_revision: i64,
    ) -> Result<usize, StateError>;
    async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError>;
    async fn get_metadata_retry_markers(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError>;
    async fn get_pending_metadata_rewrites_page(
        &self,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError>;
    async fn get_pending_metadata_rewrites_page_for_queue(
        &self,
        queue: MetadataRewriteQueue,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PendingMetadataRewrite>, StateError>;
    async fn record_capture_repair_prepared(
        &self,
        pending: &PendingMetadataRewrite,
        output_checksum: &str,
        output_size: u64,
    ) -> Result<Option<CaptureRepairReceipt>, StateError>;
    async fn finish_metadata_rewrite(
        &self,
        pending: &PendingMetadataRewrite,
        selected_queue: MetadataRewriteQueue,
        local_checksum: Option<&str>,
        pre_rewrite_checksum: Option<&str>,
        completion: MetadataRewriteCompletion,
    ) -> Result<bool, StateError>;
    async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError>;
    async fn begin_metadata_capture_revision(
        &self,
        _library: &str,
        _target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        Err(StateError::Invariant {
            operation: "begin_metadata_capture_revision",
            detail: "metadata-capture revision state is not implemented by this store".into(),
        })
    }
    async fn get_metadata_capture_candidates(
        &self,
        _library: &str,
        _target_revision: i64,
        _limit: usize,
    ) -> Result<Vec<MetadataCaptureCandidate>, StateError> {
        Err(StateError::Invariant {
            operation: "get_metadata_capture_candidates",
            detail: "metadata-capture candidate reads are not implemented by this store".into(),
        })
    }
    async fn record_metadata_capture_failure(
        &self,
        _library: &str,
        _target_revision: i64,
        _error: &str,
    ) -> Result<(), StateError> {
        Err(StateError::Invariant {
            operation: "record_metadata_capture_failure",
            detail: "metadata-capture failure state is not implemented by this store".into(),
        })
    }
    async fn complete_metadata_capture_revision(
        &self,
        _library: &str,
        _target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        Err(StateError::Invariant {
            operation: "complete_metadata_capture_revision",
            detail: "metadata-capture completion is not implemented by this store".into(),
        })
    }
    async fn has_metadata_capture_work(
        &self,
        _libraries: &[&str],
        _target_revision: i64,
    ) -> Result<bool, StateError> {
        Err(StateError::Invariant {
            operation: "has_metadata_capture_work",
            detail: "metadata-capture work checks are not implemented by this store".into(),
        })
    }
}

/// One row from the read-only local manifest export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestAssetRow {
    pub(crate) library: String,
    pub(crate) asset_id: String,
    pub(crate) version: String,
    pub(crate) filename: String,
    pub(crate) local_path: Option<PathBuf>,
    pub(crate) checksum: String,
    pub(crate) local_checksum: Option<String>,
    pub(crate) download_checksum: Option<String>,
    pub(crate) size_bytes: u64,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) added_at: Option<DateTime<Utc>>,
    pub(crate) downloaded_at: Option<DateTime<Utc>>,
    pub(crate) last_seen_at: DateTime<Utc>,
    pub(crate) media_type: String,
    pub(crate) status: String,
    pub(crate) albums: Vec<String>,
}
