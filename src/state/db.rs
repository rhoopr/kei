//! State database trait and `SQLite` implementation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction};

use super::error::StateError;
use super::schema;
use super::types::{
    AssetMetadata, AssetRecord, AssetStatus, METADATA_CAPTURE_REVISION, MediaType,
    MetadataCaptureCandidate, MetadataCaptureStatus, MetadataCaptureVersionEvidence, SyncRunStats,
    SyncSummary, VersionSizeKey,
};

/// Fallback source identifier when `AssetMetadata::source` is unset.
///
/// The `assets.source` column is NOT NULL (v5 migration defaults pre-existing
/// rows to "icloud"). Test fixtures and legacy call sites that don't populate
/// metadata get the same value written here so that inserts always succeed.
/// CloudKit parsing sets `source` explicitly; this fallback is a safety net,
/// not the intended write path.
const DEFAULT_SOURCE: &str = "icloud";

/// Project provider relations into the compatibility grouping table. Only
/// explicit relation/container tombstones remove names; an unfinished snapshot
/// retains its prior live rows. External grouping sources are never removed.
fn refresh_asset_album_groupings_tx(
    tx: &Transaction<'_>,
    library: &str,
    asset_id: &str,
    previous_name: Option<&str>,
) -> Result<(), StateError> {
    let operation = "refresh_asset_album_groupings";
    let mut stmt = tx
        .prepare_cached(
            "SELECT c.album_name, m.is_deleted OR c.is_deleted \
         FROM asset_album_memberships m JOIN album_containers c \
           ON c.library = m.library AND c.container_id = m.container_id \
         WHERE m.library = ?1 AND m.asset_record_name IN ( \
             SELECT ?2 UNION SELECT asset_record_name FROM legacy_master_state_owners \
             WHERE library = ?1 AND master_record_name = ?2)",
        )
        .map_err(|e| StateError::query(operation, e))?;
    let rows = stmt
        .query_map(rusqlite::params![library, asset_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })
        .map_err(|e| StateError::query(operation, e))?;
    let mut known = BTreeSet::new();
    let mut live = BTreeSet::new();
    for row in rows {
        let (name, deleted) = row.map_err(|e| StateError::query(operation, e))?;
        known.insert(name.clone());
        if !deleted {
            live.insert(name);
        }
    }
    if let Some(name) = previous_name {
        known.insert(name.to_owned());
    }
    let mut changed = 0;
    for name in known.difference(&live) {
        changed += tx
            .execute(
                "DELETE FROM asset_albums WHERE library = ?1 AND asset_id = ?2 \
             AND album_name = ?3 AND source = 'icloud'",
                rusqlite::params![library, asset_id, name],
            )
            .map_err(|e| StateError::query(operation, e))?;
    }
    for name in live {
        changed += tx
            .execute(
                "INSERT INTO asset_albums (library, asset_id, album_name, source) \
             SELECT ?1, ?2, ?3, 'icloud' WHERE NOT EXISTS ( \
                 SELECT 1 FROM asset_albums WHERE library = ?1 AND asset_id = ?2 \
                 AND album_name = ?3 AND source = 'icloud')",
                rusqlite::params![library, asset_id, name],
            )
            .map_err(|e| StateError::query(operation, e))?;
    }
    if changed > 0 {
        mark_album_groupings_dirty_tx(tx, library, asset_id)?;
    }
    Ok(())
}

fn mark_album_groupings_dirty_tx(
    tx: &Transaction<'_>,
    library: &str,
    asset_id: &str,
) -> Result<(), StateError> {
    tx.execute(
        "UPDATE assets SET metadata_write_failed_at = COALESCE(metadata_write_failed_at, ?3) \
         WHERE library = ?1 AND id = ?2 AND is_deleted = 0",
        rusqlite::params![library, asset_id, Utc::now().timestamp()],
    )
    .map_err(|e| StateError::query("mark_album_groupings_dirty", e))?;
    tx.execute(
        "UPDATE asset_metadata_paths SET metadata_write_failed_at = COALESCE(metadata_write_failed_at, ?3) \
         WHERE library = ?1 AND id = ?2",
        rusqlite::params![library, asset_id, Utc::now().timestamp()],
    ).map_err(|e| StateError::query("mark_album_metadata_paths_dirty", e))?;
    Ok(())
}

// Keep the optional member filter in separate branches so both scopes use
// indexed lookups. IN deduplicates child and legacy-owner identities.
const ALBUM_GROUPING_STATE_IDS_SQL: &str = "WITH members AS MATERIALIZED ( \
    SELECT asset_record_name FROM asset_album_memberships \
    WHERE library = ?1 AND container_id = ?2 AND asset_record_name = ?3 \
    UNION ALL \
    SELECT asset_record_name FROM asset_album_memberships \
    INDEXED BY idx_asset_album_memberships_container \
    WHERE library = ?1 AND container_id = ?2 AND ?3 IS NULL \
) SELECT DISTINCT id FROM assets WHERE library = ?1 AND id IN ( \
    SELECT asset_record_name FROM members \
    UNION ALL \
    SELECT master_record_name FROM legacy_master_state_owners \
    WHERE library = ?1 AND asset_record_name IN (SELECT asset_record_name FROM members))";

fn refresh_container_groupings_tx(
    tx: &Transaction<'_>,
    library: &str,
    container_id: &str,
    asset_record_name: Option<&str>,
    previous_name: Option<&str>,
) -> Result<(), StateError> {
    let operation = "refresh_container_groupings";
    let mut stmt = tx
        .prepare_cached(ALBUM_GROUPING_STATE_IDS_SQL)
        .map_err(|e| StateError::query(operation, e))?;
    let ids = stmt
        .query_map(
            rusqlite::params![library, container_id, asset_record_name],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| StateError::query(operation, e))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| StateError::query(operation, e))?;
    for id in ids {
        refresh_asset_album_groupings_tx(tx, library, &id, previous_name)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct OwnedTempPathKey(Vec<u8>);

impl OwnedTempPathKey {
    fn from_path(path: &Path) -> Result<Self, StateError> {
        let absolute =
            crate::fs_util::absolute_lexical(path).map_err(|source| StateError::TempPath {
                path: path.to_path_buf(),
                source,
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Ok(Self(absolute.as_os_str().as_bytes().to_vec()))
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            Ok(Self(
                absolute
                    .as_os_str()
                    .encode_wide()
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ))
        }
    }

    fn into_path(self) -> Result<PathBuf, StateError> {
        #[cfg(unix)]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            Ok(PathBuf::from(OsString::from_vec(self.0)))
        }
        #[cfg(windows)]
        {
            use std::ffi::OsString;
            use std::os::windows::ffi::OsStringExt;

            let chunks = self.0.chunks_exact(2);
            if !chunks.remainder().is_empty() {
                return Err(StateError::Invariant {
                    operation: "decode_owned_temp_path",
                    detail: "stored Windows temporary path has an odd byte length".into(),
                });
            }
            let wide: Vec<u16> = chunks
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            Ok(PathBuf::from(OsString::from_wide(&wide)))
        }
    }
}

fn unsupported_album_membership_api(operation: &'static str) -> StateError {
    StateError::Invariant {
        operation,
        detail: "album membership snapshots are not implemented by this state store".into(),
    }
}

fn album_container_known_tx(
    tx: &Transaction<'_>,
    library: &str,
    container_id: &str,
    operation: &'static str,
) -> Result<bool, StateError> {
    tx.query_row(
        "SELECT 1 FROM album_containers \
         WHERE library = ?1 AND container_id = ?2",
        rusqlite::params![library, container_id],
        |_| Ok(()),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(|e| StateError::query(operation, e))
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
    fn metadata_hash(&self) -> &str {
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
    const fn clears(self, queue: MetadataRewriteQueue) -> bool {
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
    const fn as_str(self) -> &'static str {
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
        metadata: (&AssetMetadata, DateTime<Utc>, Option<DateTime<Utc>>),
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

pub struct SqliteStateDb {
    /// Wrapped in `Arc<Mutex<...>>` because `rusqlite::Connection` is
    /// not `Sync` and every async method runs its body on the blocking
    /// pool (see [`Self::with_conn`] / [`Self::with_conn_mut`]). The
    /// `Arc` lets those closures own a handle to the shared connection
    /// without borrowing `&self`.
    ///
    /// WAL mode keeps reader/writer contention low; the mutex only
    /// serializes at the Rust level, not the `SQLite` file level.
    conn: Arc<Mutex<Connection>>,
    /// Path to the database file (for error messages).
    path: PathBuf,
    #[cfg(test)]
    legacy_owner_claim_failures: std::sync::atomic::AtomicUsize,
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

impl std::fmt::Debug for SqliteStateDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteStateDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SqliteStateDb {
    /// Open or create a database at the given path.
    ///
    /// Creates the parent directory if it doesn't exist; see
    /// [`StateError::ParentDir`].
    pub async fn open(path: &Path) -> Result<Self, StateError> {
        let path = path.to_path_buf();

        // create_dir_all is idempotent on an existing directory, so concurrent
        // opens on the same path don't race here. SQLite's own file locking
        // handles the open() race that follows.
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|source| StateError::ParentDir {
                    path: parent.to_path_buf(),
                    source,
                })?;
        }

        let path_clone = path.clone();

        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open(&path_clone).map_err(|e| StateError::Open {
                path: path_clone.clone(),
                source: e,
            })?;

            // Enable WAL mode for better concurrent read/write performance
            conn.pragma_update(None, "journal_mode", "WAL")
                .map_err(StateError::Migration)?;

            // Use NORMAL synchronous mode for better performance
            // (still safe with WAL mode)
            conn.pragma_update(None, "synchronous", "NORMAL")
                .map_err(StateError::Migration)?;

            // Run migrations
            schema::migrate(&conn)?;

            Ok::<_, StateError>(conn)
        })
        .await??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
            #[cfg(test)]
            legacy_owner_claim_failures: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Open an existing database for read-only inspection.
    ///
    /// This intentionally skips migration and WAL setup so diagnostic export
    /// commands can prove they do not mutate the state DB. Callers should
    /// check that the file exists before opening so a typo doesn't create a
    /// fresh empty database.
    pub(crate) async fn open_read_only(path: &Path) -> Result<Self, StateError> {
        let path = path.to_path_buf();
        let path_clone = path.clone();
        let conn = tokio::task::spawn_blocking(move || {
            let conn = Connection::open_with_flags(
                &path_clone,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(|e| StateError::Open {
                path: path_clone.clone(),
                source: e,
            })?;

            let version = schema::get_schema_version(&conn)?;
            if version > schema::SCHEMA_VERSION {
                return Err(StateError::UnsupportedSchemaVersion {
                    found: version,
                    expected: schema::SCHEMA_VERSION,
                });
            }
            if version < schema::SCHEMA_VERSION {
                return Err(StateError::ReadOnlySchemaTooOld {
                    found: version,
                    expected: schema::SCHEMA_VERSION,
                });
            }

            Ok::<_, StateError>(conn)
        })
        .await??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path,
            #[cfg(test)]
            legacy_owner_claim_failures: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Open an in-memory database (for testing).
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory().map_err(|e| StateError::Open {
            path: PathBuf::from(":memory:"),
            source: e,
        })?;
        schema::migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            path: PathBuf::from(":memory:"),
            legacy_owner_claim_failures: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_legacy_master_state_owner_claims(&self, count: usize) {
        self.legacy_owner_claim_failures
            .store(count, std::sync::atomic::Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn remaining_legacy_master_state_owner_claim_failures(&self) -> usize {
        self.legacy_owner_claim_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Get the path to the database file.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Acquire the database lock, adding the operation name to any error.
    ///
    /// Used only from tests that need to poke the connection directly;
    /// production code goes through [`Self::with_conn`] /
    /// [`Self::with_conn_mut`] so the sync rusqlite call runs on the
    /// blocking pool.
    #[cfg(test)]
    pub(crate) fn acquire_lock(
        &self,
        operation: &str,
    ) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>, StateError> {
        self.conn
            .lock()
            .map_err(|e| StateError::LockPoisoned(format!("{operation}: {e}")))
    }

    /// Run a synchronous rusqlite closure on the blocking pool with
    /// `&Connection` access. This is the correct entry point for every
    /// read-path state role method.
    async fn with_conn<F, T>(&self, operation: &'static str, f: F) -> Result<T, StateError>
    where
        F: FnOnce(&Connection) -> Result<T, StateError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .map_err(|e| StateError::LockPoisoned(format!("{operation}: {e}")))?;
            f(&guard)
        })
        .await?
    }

    /// Variant of [`Self::with_conn`] that hands the closure `&mut
    /// Connection`. Required for methods that open a `Transaction`.
    async fn with_conn_mut<F, T>(&self, operation: &'static str, f: F) -> Result<T, StateError>
    where
        F: FnOnce(&mut Connection) -> Result<T, StateError> + Send + 'static,
        T: Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|e| StateError::LockPoisoned(format!("{operation}: {e}")))?;
            f(&mut guard)
        })
        .await?
    }
}

/// Execute the asset UPSERT on `conn` (works against either a `Connection`
/// or a `Transaction`, since `Transaction: Deref<Target = Connection>`).
/// Shared by `upsert_seen` and `import_adopt` so both write the same column
/// set and conflict resolution.
fn upsert_asset_row(
    conn: &Connection,
    record: &AssetRecord,
    last_seen_at: i64,
) -> Result<(), StateError> {
    let meta = &record.metadata;
    // Lazily compute metadata_hash if caller supplied metadata without one.
    // Storing the hash alongside the metadata is what lets feature 5 detect
    // metadata-only changes in O(1) during incremental sync. Computed only
    // when missing (rare — extract() normally pre-populates it).
    let computed_hash: Option<String> = if meta.metadata_hash.is_none() {
        Some(meta.compute_hash())
    } else {
        None
    };
    let metadata_hash: Option<&str> = meta.metadata_hash.as_deref().or(computed_hash.as_deref());
    let source = metadata_rewrite_source_sql();
    let blocked_capture_repair: i64 = conn
        .query_row(
            &format!(
                "SELECT EXISTS( \
                SELECT 1 FROM ({source}) \
                WHERE library = ?1 AND id = ?2 \
                  AND status = 'downloaded' AND is_deleted = 0 \
                  AND capture_repair_metadata_hash IS NOT NULL \
                  AND capture_repair_metadata_hash <> '' \
                  AND capture_repair_output_checksum IS NOT NULL \
                  AND capture_repair_output_checksum <> '' \
                  AND capture_repair_output_size IS NOT NULL \
                  AND capture_repair_output_size >= 0 \
                  AND (created_at IS NOT ?6 OR capture_repair_metadata_hash IS NOT ?3) \
                  AND (version_size <> ?4 OR checksum = ?5) \
            )"
            ),
            rusqlite::params![
                &record.library,
                &record.id,
                metadata_hash,
                record.version_size.as_str(),
                &record.checksum,
                encode_asset_date(record.created_at)
            ],
            |row| row.get(0),
        )
        .map_err(|e| StateError::query("upsert_seen::capture_repair_guard", e))?;
    if blocked_capture_repair != 0 {
        return Err(StateError::Invariant {
            operation: "upsert_seen",
            detail:
                "provider metadata changed while capture-repair publication awaits finalization"
                    .into(),
        });
    }

    let mut stmt = conn
        .prepare_cached(
            r"
                INSERT INTO assets (
                    library, id, version_size, checksum, filename, created_at, added_at,
                    size_bytes, media_type, status, last_seen_at,
                    source, is_favorite, rating, latitude, longitude, altitude,
                    orientation, duration_secs, timezone_offset, width, height,
                    title, keywords, description, media_subtype, burst_id,
                    is_hidden, is_archived, modified_at, is_deleted, deleted_at,
                    provider_data, metadata_hash
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10,
                        ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
                        ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)
                ON CONFLICT(library, id, version_size) DO UPDATE SET
                    status = CASE
                        WHEN assets.status = 'policy_excluded'
                          OR assets.checksum <> excluded.checksum THEN 'pending'
                        ELSE assets.status
                    END,
                    downloaded_at = CASE
                        WHEN assets.checksum <> excluded.checksum THEN NULL
                        ELSE assets.downloaded_at
                    END,
                    checksum = excluded.checksum,
                    filename = excluded.filename,
                    created_at = excluded.created_at,
                    added_at = excluded.added_at,
                    size_bytes = excluded.size_bytes,
                    media_type = excluded.media_type,
                    last_seen_at = excluded.last_seen_at,
                    source = COALESCE(excluded.source, assets.source),
                    is_favorite = excluded.is_favorite,
                    rating = excluded.rating,
                    latitude = excluded.latitude,
                    longitude = excluded.longitude,
                    altitude = excluded.altitude,
                    orientation = excluded.orientation,
                    duration_secs = excluded.duration_secs,
                    timezone_offset = excluded.timezone_offset,
                    width = excluded.width,
                    height = excluded.height,
                    title = excluded.title,
                    keywords = excluded.keywords,
                    description = excluded.description,
                    media_subtype = excluded.media_subtype,
                    burst_id = excluded.burst_id,
                    is_hidden = excluded.is_hidden,
                    is_archived = excluded.is_archived,
                    modified_at = excluded.modified_at,
                    is_deleted = excluded.is_deleted,
                    deleted_at = excluded.deleted_at,
                    provider_data = excluded.provider_data,
                    metadata_hash = excluded.metadata_hash,
                    capture_repair_metadata_hash =
                        CASE
                            WHEN assets.checksum <> excluded.checksum THEN NULL
                            WHEN assets.capture_repair_metadata_hash IS NULL
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN excluded.metadata_hash
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum <> ''
                                 AND assets.capture_repair_output_size >= 0
                                THEN excluded.metadata_hash
                            ELSE assets.capture_repair_metadata_hash
                        END,
                    capture_repair_output_checksum =
                        CASE
                            WHEN assets.checksum <> excluded.checksum THEN NULL
                            WHEN assets.capture_repair_metadata_hash IS NULL
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum <> ''
                                 AND assets.capture_repair_output_size >= 0
                                THEN CASE
                                    WHEN assets.capture_repair_metadata_hash IS excluded.metadata_hash
                                        THEN assets.capture_repair_output_checksum
                                    ELSE NULL
                                END
                            ELSE assets.capture_repair_output_checksum
                        END,
                    capture_repair_output_size =
                        CASE
                            WHEN assets.checksum <> excluded.checksum THEN NULL
                            WHEN assets.capture_repair_metadata_hash IS NULL
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum IS NULL
                                 AND assets.capture_repair_output_size IS NULL
                                THEN NULL
                            WHEN assets.capture_repair_metadata_hash <> ''
                                 AND assets.capture_repair_output_checksum <> ''
                                 AND assets.capture_repair_output_size >= 0
                                THEN CASE
                                    WHEN assets.capture_repair_metadata_hash IS excluded.metadata_hash
                                        THEN assets.capture_repair_output_size
                                    ELSE NULL
                                END
                            ELSE assets.capture_repair_output_size
                        END
                WHERE NOT (
                    assets.capture_repair_metadata_hash IS NOT NULL
                    AND assets.capture_repair_metadata_hash <> ''
                    AND assets.capture_repair_output_checksum IS NOT NULL
                    AND assets.capture_repair_output_checksum <> ''
                    AND assets.capture_repair_output_size IS NOT NULL
                    AND assets.capture_repair_output_size >= 0
                    AND assets.checksum = excluded.checksum
                    AND (assets.created_at IS NOT excluded.created_at
                         OR assets.capture_repair_metadata_hash IS NOT excluded.metadata_hash)
                )
                ",
        )
        .map_err(|e| StateError::query("upsert_seen::prepare", e))?;
    let changed = stmt
        .execute(rusqlite::params![
            &record.library,
            &record.id,
            record.version_size.as_str(),
            &record.checksum,
            &record.filename,
            encode_asset_date(record.created_at),
            record.added_at.map(encode_asset_date),
            i64::try_from(record.size_bytes).unwrap_or(i64::MAX),
            record.media_type.as_str(),
            last_seen_at,
            meta.source.as_deref().unwrap_or(DEFAULT_SOURCE),
            i64::from(meta.is_favorite),
            meta.rating.map(i64::from),
            meta.latitude,
            meta.longitude,
            meta.altitude,
            meta.orientation.map(i64::from),
            meta.duration_secs,
            meta.timezone_offset.map(i64::from),
            meta.width.map(i64::from),
            meta.height.map(i64::from),
            meta.title.as_deref(),
            meta.keywords.as_deref(),
            meta.description.as_deref(),
            meta.media_subtype.as_deref(),
            meta.burst_id.as_deref(),
            i64::from(meta.is_hidden),
            i64::from(meta.is_archived),
            meta.modified_at.map(|dt| dt.timestamp()),
            i64::from(meta.is_deleted),
            meta.deleted_at.map(|dt| dt.timestamp()),
            meta.provider_data.as_deref(),
            metadata_hash,
        ])
        .map_err(|e| StateError::query("upsert_seen", e))?;
    if changed == 0 {
        return Err(StateError::Invariant {
            operation: "upsert_seen",
            detail:
                "provider metadata changed while capture-repair publication awaits finalization"
                    .into(),
        });
    }

    Ok(())
}

fn record_metadata_capture_revision(
    conn: &Connection,
    library: &str,
    asset_id: &str,
    revision: i64,
    updated_at: i64,
) -> Result<(), StateError> {
    let changed = conn
        .execute(
            "INSERT INTO asset_metadata_capture_revisions \
                (library, asset_id, revision, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(library, asset_id) DO UPDATE SET \
                revision = excluded.revision, updated_at = excluded.updated_at \
             WHERE asset_metadata_capture_revisions.revision < excluded.revision",
            rusqlite::params![library, asset_id, revision, updated_at],
        )
        .map_err(|e| StateError::query("record_metadata_capture_revision", e))?;
    if changed > 0 {
        conn.execute(
            "UPDATE metadata_capture_state SET \
                processed_assets = processed_assets + 1, updated_at = ?1 \
             WHERE library = ?2 AND pending_revision = ?3",
            rusqlite::params![updated_at, library, revision],
        )
        .map_err(|e| StateError::query("record_metadata_capture_revision::progress", e))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum MetadataPathWrite {
    Published,
    Rewritten,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MetadataRewriteTarget {
    Catalogue,
    AdditionalPath,
}

impl MetadataRewriteTarget {
    fn sql(self) -> (&'static str, &'static str) {
        match self {
            Self::Catalogue => (
                "assets",
                "status = 'downloaded' AND is_deleted = 0 AND metadata_hash IS ?4",
            ),
            Self::AdditionalPath => (
                "asset_metadata_paths",
                "EXISTS (SELECT 1 FROM assets a \
                WHERE a.library = asset_metadata_paths.library AND a.id = asset_metadata_paths.id \
                  AND a.version_size = asset_metadata_paths.version_size \
                  AND a.checksum = asset_metadata_paths.provider_checksum \
                  AND a.status = 'downloaded' AND a.is_deleted = 0 AND a.metadata_hash IS ?4)",
            ),
        }
    }
}

/// Select a path-specific compare-and-swap target without changing sync ownership.
fn metadata_rewrite_target(
    conn: &Connection,
    library: &str,
    id: &str,
    version_size: &str,
    path: &Path,
) -> Result<MetadataRewriteTarget, StateError> {
    let primary = conn.query_row(
        "SELECT local_path IS ?4 FROM assets WHERE library = ?1 AND id = ?2 AND version_size = ?3",
        rusqlite::params![library, id, version_size, path.to_string_lossy()], |row| row.get::<_, bool>(0),
    ).optional().map_err(|e| StateError::query("metadata_rewrite_target", e))?.unwrap_or(false);
    Ok(if primary {
        MetadataRewriteTarget::Catalogue
    } else {
        MetadataRewriteTarget::AdditionalPath
    })
}

/// Checksum roles supplied by one file-finalization route.
struct DownloadChecksums<'a> {
    local: &'a str,
    downloaded: Option<&'a str>,
    source: Option<&'a str>,
}

/// Copy the catalogue path's exact publication evidence in the same transaction.
/// Other paths retain their own fingerprints and independent retry receipts.
fn record_metadata_path(
    conn: &Connection,
    library: &str,
    id: &str,
    version_size: &str,
    write: MetadataPathWrite,
) -> Result<(), StateError> {
    let preserve_debt = i64::from(matches!(write, MetadataPathWrite::Published));
    conn.execute(
        "INSERT INTO asset_metadata_paths \
            (library, id, version_size, local_path, provider_checksum, local_checksum, \
             download_checksum, metadata_write_failed_at, capture_repair_metadata_hash, \
             capture_repair_output_checksum, capture_repair_output_size) \
         SELECT library, id, version_size, local_path, checksum, local_checksum, \
                download_checksum, metadata_write_failed_at, capture_repair_metadata_hash, \
                capture_repair_output_checksum, capture_repair_output_size \
         FROM assets WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
             AND status = 'downloaded' AND local_path IS NOT NULL \
         ON CONFLICT(library, id, version_size, local_path) DO UPDATE SET \
             provider_checksum = excluded.provider_checksum, \
             local_checksum = excluded.local_checksum, \
             download_checksum = excluded.download_checksum, \
             source_checksum = CASE WHEN asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 THEN asset_metadata_paths.source_checksum ELSE NULL END, \
             metadata_write_failed_at = CASE WHEN ?4 = 1 THEN \
                 COALESCE(excluded.metadata_write_failed_at, asset_metadata_paths.metadata_write_failed_at) \
                 ELSE excluded.metadata_write_failed_at END, \
             capture_repair_metadata_hash = CASE WHEN ?4 = 1 \
                 AND asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 AND asset_metadata_paths.local_checksum IS excluded.local_checksum \
                 THEN COALESCE(excluded.capture_repair_metadata_hash, asset_metadata_paths.capture_repair_metadata_hash) \
                 ELSE excluded.capture_repair_metadata_hash END, \
             capture_repair_output_checksum = CASE WHEN ?4 = 1 \
                 AND asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 AND asset_metadata_paths.local_checksum IS excluded.local_checksum \
                 THEN COALESCE(excluded.capture_repair_output_checksum, asset_metadata_paths.capture_repair_output_checksum) \
                 ELSE excluded.capture_repair_output_checksum END, \
             capture_repair_output_size = CASE WHEN ?4 = 1 \
                 AND asset_metadata_paths.provider_checksum = excluded.provider_checksum \
                 AND asset_metadata_paths.local_checksum IS excluded.local_checksum \
                 THEN COALESCE(excluded.capture_repair_output_size, asset_metadata_paths.capture_repair_output_size) \
                 ELSE excluded.capture_repair_output_size END",
        rusqlite::params![library, id, version_size, preserve_debt],
    )
    .map_err(|e| StateError::query("record_metadata_path", e))?;
    if matches!(write, MetadataPathWrite::Published) {
        // A previously additional path can become the catalogue path again.
        // Keep its own preserved debt visible in the catalogue projection.
        conn.execute(
            "UPDATE assets SET (metadata_write_failed_at, capture_repair_metadata_hash, \
                 capture_repair_output_checksum, capture_repair_output_size) = ( \
                 SELECT p.metadata_write_failed_at, p.capture_repair_metadata_hash, \
                        p.capture_repair_output_checksum, p.capture_repair_output_size \
                 FROM asset_metadata_paths p WHERE p.library = assets.library AND p.id = assets.id \
                   AND p.version_size = assets.version_size AND p.local_path = assets.local_path) \
             WHERE library = ?1 AND id = ?2 AND version_size = ?3 AND EXISTS ( \
                 SELECT 1 FROM asset_metadata_paths p WHERE p.library = assets.library AND p.id = assets.id \
                   AND p.version_size = assets.version_size AND p.local_path = assets.local_path \
                   AND (p.metadata_write_failed_at IS NOT assets.metadata_write_failed_at \
                     OR p.capture_repair_metadata_hash IS NOT assets.capture_repair_metadata_hash \
                     OR p.capture_repair_output_checksum IS NOT assets.capture_repair_output_checksum \
                     OR p.capture_repair_output_size IS NOT assets.capture_repair_output_size))",
            rusqlite::params![library, id, version_size],
        ).map_err(|e| StateError::query("record_metadata_path::restore_debt", e))?;
    }
    Ok(())
}

/// Execute the `mark_downloaded` UPDATE on `conn`. Returns rows affected;
/// callers decide what zero rows means in their context.
#[expect(
    clippy::too_many_arguments,
    reason = "the state owner receives the exact downloaded-file fields and explicit repair intent for one atomic SQL update"
)]
fn update_status_to_downloaded(
    conn: &Connection,
    library: &str,
    id: &str,
    version_size: &str,
    local_path: &Path,
    local_checksum: &str,
    download_checksum: Option<&str>,
    mark_capture_repair: bool,
    downloaded_at: i64,
) -> Result<usize, StateError> {
    let mut stmt = conn
        .prepare_cached(
            "UPDATE assets SET status = 'downloaded', downloaded_at = ?1, local_path = ?2, \
             local_checksum = ?3, download_checksum = COALESCE(?4, download_checksum), last_error = NULL, \
             capture_repair_metadata_hash = CASE \
                 WHEN ?5 = 1 THEN metadata_hash \
                 WHEN local_checksum IS ?3 AND local_path IS ?2 THEN capture_repair_metadata_hash ELSE NULL END, \
             capture_repair_output_checksum = CASE \
                 WHEN ?5 = 1 THEN NULL \
                 WHEN local_checksum IS ?3 AND local_path IS ?2 THEN capture_repair_output_checksum ELSE NULL END, \
             capture_repair_output_size = CASE \
                 WHEN ?5 = 1 THEN NULL \
                 WHEN local_checksum IS ?3 AND local_path IS ?2 THEN capture_repair_output_size ELSE NULL END \
             WHERE library = ?6 AND id = ?7 AND version_size = ?8",
        )
        .map_err(|e| StateError::query("mark_downloaded::prepare", e))?;
    let updated = stmt
        .execute(rusqlite::params![
            downloaded_at,
            local_path.to_string_lossy(),
            local_checksum,
            download_checksum,
            i64::from(mark_capture_repair),
            library,
            id,
            version_size
        ])
        .map_err(|e| StateError::query("mark_downloaded", e))?;
    record_metadata_path(
        conn,
        library,
        id,
        version_size,
        MetadataPathWrite::Published,
    )?;
    Ok(updated)
}

fn ensure_asset_has_no_prepared_capture_repair(
    conn: &Connection,
    library: &str,
    asset_id: &str,
    operation: &'static str,
) -> Result<(), StateError> {
    let source = metadata_rewrite_source_sql();
    let prepared: i64 = conn
        .query_row(
            &format!(
                "SELECT EXISTS( \
                SELECT 1 FROM ({source}) \
                WHERE library = ?1 AND id = ?2 \
                  AND capture_repair_metadata_hash IS NOT NULL \
                  AND capture_repair_metadata_hash <> '' \
                  AND capture_repair_output_checksum IS NOT NULL \
                  AND capture_repair_output_checksum <> '' \
                  AND capture_repair_output_size IS NOT NULL \
                  AND capture_repair_output_size >= 0 \
            )"
            ),
            rusqlite::params![library, asset_id],
            |row| row.get(0),
        )
        .map_err(|error| StateError::query(operation, error))?;
    if prepared != 0 {
        return Err(StateError::Invariant {
            operation,
            detail: "source deletion cannot hide a prepared capture-repair receipt".into(),
        });
    }
    Ok(())
}

fn ensure_master_family_has_no_prepared_capture_repair(
    conn: &Connection,
    library: &str,
    master_record_name: &str,
    operation: &'static str,
) -> Result<(), StateError> {
    let source = metadata_rewrite_source_sql();
    let prepared: i64 = conn
        .query_row(
            &format!(
                "SELECT EXISTS( \
                SELECT 1 FROM ({source}) \
                WHERE library = ?1 \
                  AND (id = ?2 OR id IN ( \
                      SELECT asset_record_name FROM asset_master_mappings \
                      WHERE library = ?1 AND master_record_name = ?2 \
                  )) \
                  AND capture_repair_metadata_hash IS NOT NULL \
                  AND capture_repair_metadata_hash <> '' \
                  AND capture_repair_output_checksum IS NOT NULL \
                  AND capture_repair_output_checksum <> '' \
                  AND capture_repair_output_size IS NOT NULL \
                  AND capture_repair_output_size >= 0 \
            )"
            ),
            rusqlite::params![library, master_record_name],
            |row| row.get(0),
        )
        .map_err(|error| StateError::query(operation, error))?;
    if prepared != 0 {
        return Err(StateError::Invariant {
            operation,
            detail: "source deletion cannot hide a prepared capture-repair receipt".into(),
        });
    }
    Ok(())
}

/// Drain a rusqlite row iterator into `Vec<T>`, dropping parse failures but
/// logging each at `debug!` and summarising the drop count at `warn!` so a
/// corrupted row never silently disappears from a bulk loader.
fn collect_rows_with_warn<T, I>(rows: I, label: &'static str) -> Vec<T>
where
    I: Iterator<Item = rusqlite::Result<T>>,
{
    let mut out = Vec::new();
    let mut dropped = 0usize;
    for r in rows {
        match r {
            Ok(v) => out.push(v),
            Err(e) => {
                dropped += 1;
                tracing::debug!(error = %e, "{label}: row parse error");
            }
        }
    }
    if dropped > 0 {
        tracing::warn!(dropped, "{label}: dropped rows with parse errors");
    }
    out
}

fn unique_sorted_strings(values: &[&str]) -> Vec<String> {
    let mut values: Vec<String> = values.iter().map(|value| (*value).to_owned()).collect();
    values.sort();
    values.dedup();
    values
}

fn sqlite_placeholders(len: usize) -> String {
    std::iter::repeat_n("?", len).collect::<Vec<_>>().join(", ")
}

fn album_membership_record_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<AlbumMembershipRecord> {
    Ok(AlbumMembershipRecord {
        library: row.get(0)?,
        asset_record_name: row.get(1)?,
        master_record_name: row.get(2)?,
        container_id: row.get(3)?,
        generation: row.get(4)?,
        source: row.get(5)?,
    })
}

impl SqliteStateDb {
    pub(crate) async fn claim_temp_file(&self, path: &Path) -> Result<(), StateError> {
        let path_key = OwnedTempPathKey::from_path(path)?.0;
        let claimed_at = Utc::now().timestamp();
        self.with_conn("claim_temp_file", move |conn| {
            conn.execute(
                "INSERT INTO owned_temp_files (path, claimed_at) VALUES (?1, ?2) \
                 ON CONFLICT(path) DO UPDATE SET claimed_at = excluded.claimed_at",
                rusqlite::params![path_key, claimed_at],
            )
            .map_err(|e| StateError::query("claim_temp_file", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_owned_temp_files_before(
        &self,
        claimed_before: i64,
    ) -> Result<Vec<OwnedTempFile>, StateError> {
        self.with_conn("get_owned_temp_files_before", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT path, claimed_at FROM owned_temp_files \
                     WHERE claimed_at < ?1 ORDER BY path",
                )
                .map_err(|e| StateError::query("get_owned_temp_files_before", e))?;
            let rows = stmt
                .query_map([claimed_before], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|e| StateError::query("get_owned_temp_files_before", e))?;

            let mut owned = Vec::new();
            for row in rows {
                let (path_key, claimed_at) =
                    row.map_err(|e| StateError::query("get_owned_temp_files_before", e))?;
                owned.push(OwnedTempFile {
                    path: OwnedTempPathKey(path_key).into_path()?,
                    claimed_at,
                });
            }
            Ok(owned)
        })
        .await
    }

    pub(crate) async fn retire_temp_files(&self, paths: &[PathBuf]) -> Result<u64, StateError> {
        if paths.is_empty() {
            return Ok(0);
        }
        let path_keys = paths
            .iter()
            .map(|path| OwnedTempPathKey::from_path(path).map(|key| key.0))
            .collect::<Result<Vec<_>, _>>()?;
        self.with_conn_mut("retire_temp_files", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("retire_temp_files", e))?;
            let mut retired = 0u64;
            for path_key in path_keys {
                retired = retired.saturating_add(
                    tx.execute("DELETE FROM owned_temp_files WHERE path = ?1", [path_key])
                        .map_err(|e| StateError::query("retire_temp_files", e))?
                        as u64,
                );
            }
            tx.commit()
                .map_err(|e| StateError::query("retire_temp_files", e))?;
            Ok(retired)
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn should_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError> {
        if checksum.is_empty() {
            tracing::warn!(
                id,
                version_size,
                "Empty remote checksum cannot be trusted for state skip decisions"
            );
            return Ok(true);
        }

        let library_owned = library.to_owned();
        let id_owned = id.to_owned();
        let version_size_owned = version_size.to_owned();
        let result: Option<(String, String, Option<String>)> = self
            .with_conn("should_download", move |conn| {
                conn.query_row(
                    "SELECT status, checksum, local_path FROM assets \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                    [&library_owned, &id_owned, &version_size_owned],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|e| StateError::query("should_download", e))
            })
            .await?;

        match result {
            None => {
                // Not in database — should download
                Ok(true)
            }
            Some((status_str, stored_checksum, stored_path_opt)) => {
                let status = AssetStatus::from_str(&status_str).unwrap_or(AssetStatus::Pending);

                // Checksum changed — re-download
                if stored_checksum != checksum {
                    tracing::debug!(
                        id = %id,
                        "Asset checksum changed, will re-download"
                    );
                    return Ok(true);
                }

                match status {
                    AssetStatus::Downloaded => {
                        // Check if file still exists (async to avoid blocking)
                        let path_to_check: PathBuf = stored_path_opt
                            .map(PathBuf::from)
                            .unwrap_or_else(|| local_path.to_path_buf());
                        match tokio::fs::try_exists(&path_to_check).await {
                            Ok(true) => Ok(false),
                            Ok(false) => {
                                tracing::debug!(
                                    id = %id,
                                    path = %path_to_check.display(),
                                    "Downloaded file missing, will re-download"
                                );
                                Ok(true)
                            }
                            Err(e) => {
                                tracing::warn!(
                                    id = %id,
                                    path = %path_to_check.display(),
                                    error = %e,
                                    "Failed to check file existence, assuming missing"
                                );
                                Ok(true)
                            }
                        }
                    }
                    AssetStatus::Pending | AssetStatus::PolicyExcluded | AssetStatus::Failed => {
                        Ok(true)
                    }
                }
            }
        }
    }

    pub(crate) async fn upsert_seen(&self, record: &AssetRecord) -> Result<(), StateError> {
        let record = record.clone();
        self.with_conn_mut("upsert_seen", move |conn| {
            let tx = conn.transaction().map_err(|e| StateError::query("upsert_seen::begin", e))?;
            upsert_asset_row(&tx, &record, Utc::now().timestamp())?;
            refresh_asset_album_groupings_tx(&tx, &record.library, &record.id, None)?;
            // Membership may precede the first state row. Keep a retry marker
            // until the end-of-cycle writer sees all selected album producers.
            tx.execute(
                "UPDATE assets SET metadata_write_failed_at = COALESCE(metadata_write_failed_at, ?3) \
                 WHERE library = ?1 AND id = ?2 AND status = 'pending' AND EXISTS ( \
                     SELECT 1 FROM asset_albums WHERE library = ?1 AND asset_id = ?2)",
                rusqlite::params![&record.library, &record.id, Utc::now().timestamp()],
            ).map_err(|e| StateError::query("upsert_seen::album_marker", e))?;
            tx.commit().map_err(|e| StateError::query("upsert_seen::commit", e))
        })
        .await
    }

    pub(crate) async fn mark_downloaded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), StateError> {
        self.mark_downloaded_with_capture_repair(
            library,
            id,
            version_size,
            local_path,
            local_checksum,
            download_checksum,
            false,
        )
        .await
    }

    pub(crate) async fn mark_downloaded_with_capture_repair(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        self.mark_downloaded_with_checksums(
            library,
            id,
            version_size,
            local_path,
            DownloadChecksums {
                local: local_checksum,
                downloaded: download_checksum,
                source: None,
            },
            mark_capture_repair,
        )
        .await
    }

    async fn mark_downloaded_with_checksums(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        checksums: DownloadChecksums<'_>,
        mark_capture_repair: bool,
    ) -> Result<(), StateError> {
        let downloaded_at = Utc::now().timestamp();
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let local_path = local_path.to_path_buf();
        let local_checksum = checksums.local.to_owned();
        let download_checksum = checksums.downloaded.map(str::to_owned);
        let source_checksum = checksums.source.map(str::to_owned);

        self.with_conn_mut("mark_downloaded", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_downloaded::begin", e))?;
            let rows = update_status_to_downloaded(
                &tx,
                &library,
                &id,
                &version_size,
                &local_path,
                &local_checksum,
                download_checksum.as_deref(),
                mark_capture_repair,
                downloaded_at,
            )?;

            if rows == 0 {
                crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.inc();
                return Err(StateError::AssetRowMissing {
                    asset_id: id,
                    version_size,
                });
            }

            if let Some(source_checksum) = &source_checksum {
                // CONTRACT: XMP_GPS_ACCURACY_REQUIRES_MATCHING_LOCATION
                // Source provenance and downloaded state commit together.
                tx.execute(
                    "UPDATE asset_metadata_paths SET source_checksum = ?5 \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 AND local_path = ?4",
                    rusqlite::params![
                        library,
                        id,
                        version_size,
                        local_path.to_string_lossy(),
                        source_checksum
                    ],
                )
                .map_err(|e| StateError::query("mark_downloaded::source_checksum", e))?;
            }

            record_metadata_capture_revision(
                &tx,
                &library,
                &id,
                METADATA_CAPTURE_REVISION,
                downloaded_at,
            )?;

            tx.commit()
                .map_err(|e| StateError::query("mark_downloaded::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn import_adopt(
        &self,
        record: &AssetRecord,
        local_path: &Path,
        local_checksum: &str,
        imported_size: u64,
        imported_mtime: Option<i64>,
    ) -> Result<(), StateError> {
        let record = record.clone();
        let local_path = local_path.to_path_buf();
        let local_checksum = local_checksum.to_owned();

        self.with_conn_mut("import_adopt", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("import_adopt::begin", e))?;

            let now = Utc::now().timestamp();
            upsert_asset_row(&tx, &record, now)?;
            let rows = update_status_to_downloaded(
                &tx,
                &record.library,
                &record.id,
                record.version_size.as_str(),
                &local_path,
                &local_checksum,
                None,
                false,
                now,
            )?;
            debug_assert_eq!(
                rows, 1,
                "import_adopt UPDATE missed the row inserted by UPSERT in the same tx — SQL bug"
            );
            record_metadata_capture_revision(
                &tx,
                &record.library,
                &record.id,
                METADATA_CAPTURE_REVISION,
                now,
            )?;

            // Snapshot on-disk size + mtime so the next import-existing run
            // can short-circuit the SHA-256 re-read when the file is
            // unchanged. Done as a separate UPDATE (rather than rolled into
            // `update_status_to_downloaded`) because the production download
            // path doesn't have these values and shouldn't carry them.
            tx.execute(
                "UPDATE assets SET imported_size = ?1, imported_mtime = ?2 \
                 WHERE library = ?3 AND id = ?4 AND version_size = ?5",
                rusqlite::params![
                    i64::try_from(imported_size).unwrap_or(i64::MAX),
                    imported_mtime,
                    &record.library,
                    &record.id,
                    record.version_size.as_str(),
                ],
            )
            .map_err(|e| StateError::query("import_adopt::imported_meta", e))?;

            tx.commit()
                .map_err(|e| StateError::query("import_adopt::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_all_imported_records(
        &self,
        library: &str,
    ) -> Result<HashMap<(String, String), ImportedRecord>, StateError> {
        let library = library.to_owned();

        self.with_conn("get_all_imported_records", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, version_size, local_path, local_checksum, \
                            imported_size, imported_mtime \
                     FROM assets \
                     WHERE library = ?1 AND status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_all_imported_records::prepare", e))?;
            let rows = stmt
                .query_map([&library], |row| {
                    let id: String = row.get(0)?;
                    let version_size: String = row.get(1)?;
                    let local_path: String = row.get(2)?;
                    let local_checksum: String = row.get(3)?;
                    let imported_size: Option<i64> = row.get(4)?;
                    let imported_mtime: Option<i64> = row.get(5)?;
                    Ok((
                        (id, version_size),
                        ImportedRecord {
                            local_path: PathBuf::from(local_path),
                            local_checksum,
                            imported_size: imported_size.and_then(|v| u64::try_from(v).ok()),
                            imported_mtime,
                        },
                    ))
                })
                .map_err(|e| StateError::query("get_all_imported_records::query", e))?;
            let mut out = HashMap::new();
            for r in rows {
                let (k, v) =
                    r.map_err(|e| StateError::query("get_all_imported_records::row", e))?;
                out.insert(k, v);
            }
            Ok(out)
        })
        .await
    }

    pub(crate) async fn mark_failed(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let error = error.to_owned();

        self.with_conn("mark_failed", move |conn| {
            let rows = conn
                .execute(
                    "UPDATE assets SET status = 'failed', download_attempts = download_attempts + 1, \
                     last_error = ?1 WHERE library = ?2 AND id = ?3 AND version_size = ?4",
                    rusqlite::params![&error, &library, &id, &version_size],
                )
                .map_err(|e| StateError::query("mark_failed", e))?;

            if rows == 0 {
                tracing::error!(
                    id = %id,
                    version_size = %version_size,
                    "mark_failed matched 0 rows; caller must upsert_seen before mark_failed \
                     (producer-dispatch invariant). Failure not persisted"
                );
                crate::metrics::MARK_FAILED_ZERO_ROWS.inc();
                return Err(StateError::Invariant {
                    operation: "mark_failed",
                    detail: format!(
                        "library={library} id={id} version_size={version_size} \
                         not present; upsert_seen must run before mark_failed"
                    ),
                });
            }

            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_failed", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_failed", e))?;

            let records = stmt
                .query_map([], row_to_asset_record)
                .map_err(|e| StateError::query("get_failed", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_failed", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_failed_sample(
        &self,
        limit: u32,
    ) -> Result<(Vec<AssetRecord>, u64), StateError> {
        self.with_conn("get_failed_sample", move |conn| {
            let total: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'failed' AND is_deleted = 0",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_failed_sample", e))?;

            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC LIMIT ?1",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_failed_sample", e))?;

            let records = stmt
                .query_map([i64::from(limit)], row_to_asset_record)
                .map_err(|e| StateError::query("get_failed_sample", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_failed_sample", e))?;

            #[allow(
                clippy::cast_sign_loss,
                reason = ".max(0) clamps any negative COUNT(*) result to 0 before the cast"
            )]
            let total_u64 = total.max(0) as u64;
            Ok((records, total_u64))
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_pending", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'pending' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_pending", e))?;

            let records = stmt
                .query_map([], row_to_asset_record)
                .map_err(|e| StateError::query("get_pending", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_pending", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_policy_excluded_ids_for_revalidation(
        &self,
        library: &str,
    ) -> Result<Vec<String>, StateError> {
        let library = library.to_owned();
        self.with_conn("get_policy_excluded_ids_for_revalidation", move |conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT DISTINCT id FROM assets \
                     WHERE status = 'policy_excluded' AND is_deleted = 0 AND library = ?1 \
                     ORDER BY id",
                )
                .map_err(|e| StateError::query("get_policy_excluded_ids_for_revalidation", e))?;

            let records = stmt
                .query_map(rusqlite::params![library], |row| row.get(0))
                .map_err(|e| StateError::query("get_policy_excluded_ids_for_revalidation", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_policy_excluded_ids_for_revalidation", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_failed_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_failed_page", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC LIMIT ?1 OFFSET ?2",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_failed_page", e))?;

            #[allow(
                clippy::cast_possible_wrap,
                reason = "offset is bounded by the failed-row count and well below i64::MAX"
            )]
            let offset_i = offset as i64;
            let records = stmt
                .query_map(
                    rusqlite::params![i64::from(limit), offset_i],
                    row_to_asset_record,
                )
                .map_err(|e| StateError::query("get_failed_page", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_failed_page", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_pending_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_pending_page", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'pending' AND is_deleted = 0 \
                 ORDER BY last_seen_at DESC LIMIT ?1 OFFSET ?2",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_pending_page", e))?;

            #[allow(
                clippy::cast_possible_wrap,
                reason = "offset is bounded by the pending-row count and well below i64::MAX"
            )]
            let offset_i = offset as i64;
            let records = stmt
                .query_map(
                    rusqlite::params![i64::from(limit), offset_i],
                    row_to_asset_record,
                )
                .map_err(|e| StateError::query("get_pending_page", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_pending_page", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        self.with_conn("get_summary", move |conn| {
            let (
                total_assets,
                downloaded,
                pending,
                policy_excluded,
                failed,
                source_deleted,
                downloaded_bytes,
            ) = conn.query_row(
                    "SELECT \
                         COUNT(*), \
                         COUNT(CASE WHEN status = 'downloaded' THEN 1 END), \
                         COUNT(CASE WHEN status = 'pending' AND is_deleted = 0 THEN 1 END), \
                         COUNT(CASE WHEN status = 'policy_excluded' AND is_deleted = 0 THEN 1 END), \
                         COUNT(CASE WHEN status = 'failed' AND is_deleted = 0 THEN 1 END), \
                         COUNT(CASE WHEN is_deleted = 1 THEN 1 END), \
                         COALESCE(SUM(CASE WHEN status = 'downloaded' THEN size_bytes ELSE 0 END), 0) \
                     FROM assets",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, i64>(6)?,
                        ))
                    },
                )
                .map(|(t, d, p, e, f, s, b)| {
                    (
                        u64::try_from(t).unwrap_or(0),
                        u64::try_from(d).unwrap_or(0),
                        u64::try_from(p).unwrap_or(0),
                        u64::try_from(e).unwrap_or(0),
                        u64::try_from(f).unwrap_or(0),
                        u64::try_from(s).unwrap_or(0),
                        u64::try_from(b).unwrap_or(0),
                    )
                })
                .map_err(|e| StateError::query("get_summary", e))?;
            let awaiting_provider_verification: u64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM asset_verifications AS verification \
                     JOIN assets \
                       ON assets.library = verification.library \
                      AND assets.id = verification.id \
                      AND assets.version_size = verification.version_size \
                     WHERE assets.status IN ('pending', 'failed') AND assets.is_deleted = 0",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|count| u64::try_from(count).unwrap_or(0))
                .map_err(|e| StateError::query("get_summary::provider_verification", e))?;
            let oldest_provider_verification_at = conn
                .query_row(
                    "SELECT MIN(verification.checked_at) FROM asset_verifications AS verification \
                     JOIN assets \
                       ON assets.library = verification.library \
                      AND assets.id = verification.id \
                      AND assets.version_size = verification.version_size \
                     WHERE assets.status IN ('pending', 'failed') AND assets.is_deleted = 0",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .map_err(|e| StateError::query("get_summary::oldest_provider_verification", e))?
                .and_then(|timestamp| Utc.timestamp_opt(timestamp, 0).single());
            let metadata_value = |key: &str| {
                conn.query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
            };
            let mut provider_checkpoint_status = metadata_value("last_checkpoint_status")
                .map_err(|e| StateError::query("get_summary::checkpoint_status", e))?;
            if provider_checkpoint_status.is_none() {
                let token_exists = conn
                    .prepare("SELECT 1 FROM metadata WHERE key LIKE 'sync_token:%' LIMIT 1")
                    .and_then(|mut stmt| stmt.exists([]))
                    .map_err(|e| StateError::query("get_summary::checkpoint_exists", e))?;
                provider_checkpoint_status = token_exists.then(|| "current".to_owned());
            }
            let last_recovery_action = metadata_value("last_recovery_action")
                .map_err(|e| StateError::query("get_summary::recovery_action", e))?;
            let last_full_enumeration_reason = metadata_value("last_full_enumeration_reason")
                .map_err(|e| StateError::query("get_summary::full_enumeration_reason", e))?;

            type LastSyncRow = (
                Option<i64>,
                Option<i64>,
                Option<String>,
                i64,
                i64,
                i32,
                Option<i64>,
                i32,
                i32,
                Option<i64>,
                Option<i64>,
                Option<String>,
            );
            let last_sync: Option<LastSyncRow> = conn
                .query_row(
                    "SELECT started_at, completed_at, \
                            status, assets_failed, enumeration_errors, interrupted, \
                            api_total_at_start, api_total_at_start_partial, inventory_drop_detected, \
                            inventory_drop_previous_total, inventory_drop_current_total, \
                            inventory_drop_library \
                     FROM sync_runs ORDER BY id DESC LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                            row.get(11)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| StateError::query("get_summary", e))?;

            let (
                last_sync_started,
                last_sync_completed,
                last_sync_status,
                last_sync_assets_failed,
                last_sync_enumeration_errors,
                last_sync_interrupted,
                last_api_total_at_start,
                last_api_total_at_start_partial,
                last_inventory_drop_detected,
                last_inventory_drop_previous_total,
                last_inventory_drop_current_total,
                last_inventory_drop_library,
            ) = match last_sync {
                Some((
                    started,
                    completed,
                    status,
                    assets_failed,
                    enumeration_errors,
                    interrupted,
                    api_total,
                    api_total_partial,
                    drop_detected,
                    drop_previous,
                    drop_current,
                    drop_library,
                )) => (
                    started.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                    completed.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                    status,
                    u64::try_from(assets_failed).unwrap_or(0),
                    u64::try_from(enumeration_errors).unwrap_or(0),
                    interrupted != 0,
                    api_total.and_then(|n| u64::try_from(n).ok()),
                    api_total_partial != 0,
                    drop_detected != 0,
                    drop_previous.and_then(|n| u64::try_from(n).ok()),
                    drop_current.and_then(|n| u64::try_from(n).ok()),
                    drop_library,
                ),
                None => (None, None, None, 0, 0, false, None, false, false, None, None, None),
            };

            let active_sync_started: Option<DateTime<Utc>> = conn
                .query_row(
                    "SELECT started_at FROM sync_runs \
                     WHERE status = 'running' ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|e| StateError::query("get_summary", e))?
                .and_then(|ts| Utc.timestamp_opt(ts, 0).single());

            let mut enum_stmt = conn
                .prepare(
                    "SELECT key FROM metadata \
                     WHERE key LIKE 'enum_in_progress:%' ORDER BY key",
                )
                .map_err(|e| StateError::query("get_summary", e))?;
            let enum_rows = enum_stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| StateError::query("get_summary", e))?;
            let mut active_enumeration_zones = Vec::new();
            for row in enum_rows {
                let key = row.map_err(|e| StateError::query("get_summary", e))?;
                if let Some(zone) = key.strip_prefix("enum_in_progress:") {
                    active_enumeration_zones.push(zone.to_string());
                }
            }

            let mut capture_stmt = conn
                .prepare(
                    "SELECT library FROM metadata_capture_state \
                     UNION SELECT DISTINCT library FROM assets \
                     ORDER BY library",
                )
                .map_err(|e| StateError::query("get_summary::metadata_capture", e))?;
            let capture_libraries = capture_stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| StateError::query("get_summary::metadata_capture", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_summary::metadata_capture", e))?;
            let mut metadata_capture = Vec::with_capacity(capture_libraries.len());
            for library in capture_libraries {
                let remaining =
                    metadata_capture_remaining(conn, &library, METADATA_CAPTURE_REVISION)?;
                metadata_capture.push(metadata_capture_status(
                    conn,
                    &library,
                    METADATA_CAPTURE_REVISION,
                    remaining,
                )?);
            }

            Ok(SyncSummary {
                total_assets,
                downloaded,
                pending,
                policy_excluded,
                failed,
                awaiting_provider_verification,
                source_deleted,
                oldest_provider_verification_at,
                provider_checkpoint_status,
                last_recovery_action,
                last_full_enumeration_reason,
                downloaded_bytes,
                active_sync_started,
                active_enumeration_zones,
                last_sync_completed,
                last_sync_started,
                last_sync_status,
                last_sync_assets_failed,
                last_sync_enumeration_errors,
                last_sync_interrupted,
                last_api_total_at_start,
                last_api_total_at_start_partial,
                last_inventory_drop_detected,
                last_inventory_drop_previous_total,
                last_inventory_drop_current_total,
                last_inventory_drop_library,
                metadata_capture,
            })
        })
        .await
    }

    pub(crate) async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_downloaded_page", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'downloaded' \
                 ORDER BY rowid LIMIT ?1 OFFSET ?2",
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_downloaded_page", e))?;

            let records = stmt
                .query_map(
                    rusqlite::params![i64::from(limit), offset as i64],
                    row_to_asset_record,
                )
                .map_err(|e| StateError::query("get_downloaded_page", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_downloaded_page", e))?;

            Ok(records)
        })
        .await
    }

    pub(crate) async fn get_manifest_assets(&self) -> Result<Vec<ManifestAssetRow>, StateError> {
        self.with_conn("get_manifest_assets", move |conn| {
            let mut stmt = conn
                .prepare(
                    r"
                    SELECT
                        a.library,
                        a.id,
                        a.version_size,
                        a.filename,
                        a.local_path,
                        a.checksum,
                        a.local_checksum,
                        a.download_checksum,
                        a.size_bytes,
                        a.created_at,
                        a.added_at,
                        a.downloaded_at,
                        a.last_seen_at,
                        a.media_type,
                        a.status,
                        aa.album_name
                    FROM assets a
                    LEFT JOIN asset_albums aa
                        ON aa.library = a.library
                       AND aa.asset_id = a.id
                    ORDER BY a.library, a.id, a.version_size, aa.album_name
                    ",
                )
                .map_err(|e| StateError::query("get_manifest_assets::prepare", e))?;

            let rows = stmt
                .query_map([], manifest_joined_row_from_row)
                .map_err(|e| StateError::query("get_manifest_assets::query", e))?;

            let mut assets: BTreeMap<(String, String, String), ManifestAssetRow> = BTreeMap::new();
            for row in rows {
                let joined = row.map_err(|e| StateError::query("get_manifest_assets::row", e))?;
                let key = (
                    joined.asset.library.clone(),
                    joined.asset.asset_id.clone(),
                    joined.asset.version.clone(),
                );
                let asset = assets.entry(key).or_insert(joined.asset);
                if let Some(album) = joined.album_name {
                    asset.albums.push(album);
                }
            }

            Ok(assets.into_values().collect())
        })
        .await
    }

    pub(crate) async fn start_sync_run(&self) -> Result<i64, StateError> {
        self.start_sync_run_at(Utc::now()).await
    }

    pub(crate) async fn start_sync_run_at(
        &self,
        started_at: DateTime<Utc>,
    ) -> Result<i64, StateError> {
        let started_at = started_at.timestamp();
        self.with_conn("start_sync_run", move |conn| {
            conn.execute(
                "INSERT INTO sync_runs (started_at, status) VALUES (?1, 'running')",
                [started_at],
            )
            .map_err(|e| StateError::query("start_sync_run", e))?;

            Ok(conn.last_insert_rowid())
        })
        .await
    }

    pub(crate) async fn complete_sync_run(
        &self,
        run_id: i64,
        stats: &SyncRunStats,
    ) -> Result<(), StateError> {
        let completed_at = Utc::now().timestamp();
        let assets_seen = i64::try_from(stats.assets_seen).unwrap_or(i64::MAX);
        let assets_downloaded = i64::try_from(stats.assets_downloaded).unwrap_or(i64::MAX);
        let assets_failed = i64::try_from(stats.assets_failed).unwrap_or(i64::MAX);
        let enumeration_errors = i64::try_from(stats.enumeration_errors).unwrap_or(i64::MAX);
        let api_total_at_start = stats
            .api_total_at_start
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX));
        let api_total_at_start_partial = i32::from(stats.api_total_at_start_partial);
        let inventory_drop_detected = i32::from(stats.inventory_drop_warnings > 0);
        let inventory_drop_previous_total = stats
            .inventory_drop_previous_total
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX));
        let inventory_drop_current_total = stats
            .inventory_drop_current_total
            .map(|n| i64::try_from(n).unwrap_or(i64::MAX));
        let inventory_drop_library = stats.inventory_drop_library.clone();
        let interrupted_i32 = i32::from(stats.interrupted);
        let status = if stats.interrupted {
            "interrupted"
        } else {
            "complete"
        };

        self.with_conn("complete_sync_run", move |conn| {
            let rows = conn.execute(
                "UPDATE sync_runs SET completed_at = ?1, assets_seen = ?2, assets_downloaded = ?3, \
                 assets_failed = ?4, interrupted = ?5, status = ?6, enumeration_errors = ?7, \
                 api_total_at_start = ?8, api_total_at_start_partial = ?9, \
                 inventory_drop_detected = ?10, inventory_drop_previous_total = ?11, \
                 inventory_drop_current_total = ?12, inventory_drop_library = ?13 \
                 WHERE id = ?14",
                rusqlite::params![
                    completed_at,
                    assets_seen,
                    assets_downloaded,
                    assets_failed,
                    interrupted_i32,
                    status,
                    enumeration_errors,
                    api_total_at_start,
                    api_total_at_start_partial,
                    inventory_drop_detected,
                    inventory_drop_previous_total,
                    inventory_drop_current_total,
                    inventory_drop_library,
                    run_id
                ],
            )
            .map_err(|e| StateError::query("complete_sync_run", e))?;
            if rows == 0 {
                return Err(StateError::Invariant {
                    operation: "complete_sync_run",
                    detail: format!("no sync_runs row for id {run_id}"),
                });
            }

            Ok(())
        })
        .await
    }

    #[cfg(test)]
    pub(crate) fn sync_run_snapshot_for_test(
        &self,
        run_id: i64,
    ) -> Result<(String, i64, i64, i64, i32), StateError> {
        let conn = self.acquire_lock("sync_run_snapshot_for_test")?;
        conn.query_row(
            "SELECT status, assets_seen, assets_failed, enumeration_errors, interrupted \
             FROM sync_runs WHERE id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i32>(4)?,
                ))
            },
        )
        .map_err(|e| StateError::query("sync_run_snapshot_for_test", e))
    }

    pub(crate) async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
        self.with_conn("promote_orphaned_sync_runs", move |conn| {
            let rows = conn
                .execute(
                    "UPDATE sync_runs SET status = 'interrupted', interrupted = 1 \
                     WHERE status = 'running'",
                    [],
                )
                .map_err(|e| StateError::query("promote_orphaned_sync_runs", e))?;
            Ok(rows as u64)
        })
        .await
    }

    pub(crate) async fn begin_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        let key = format!("enum_in_progress:{zone}");
        let now = Utc::now().timestamp().to_string();
        self.with_conn("begin_enum_progress", move |conn| {
            // INSERT OR IGNORE so re-entry doesn't reset the age operators use to
            // judge stuck zones; `end_enum_progress` is the only path that clears.
            conn.execute(
                "INSERT OR IGNORE INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![key, now],
            )
            .map_err(|e| StateError::query("begin_enum_progress", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn end_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        let key = format!("enum_in_progress:{zone}");
        self.with_conn("end_enum_progress", move |conn| {
            conn.execute("DELETE FROM metadata WHERE key = ?1", [key])
                .map_err(|e| StateError::query("end_enum_progress", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError> {
        self.with_conn("list_interrupted_enumerations", move |conn| {
            let mut stmt = conn
                .prepare("SELECT key FROM metadata WHERE key LIKE 'enum_in_progress:%'")
                .map_err(|e| StateError::query("list_interrupted_enumerations", e))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| StateError::query("list_interrupted_enumerations", e))?;
            let mut zones = Vec::new();
            for row in rows {
                let key = row.map_err(|e| StateError::query("list_interrupted_enumerations", e))?;
                if let Some(zone) = key.strip_prefix("enum_in_progress:") {
                    zones.push(zone.to_string());
                }
            }
            Ok(zones)
        })
        .await
    }

    pub(crate) async fn reset_failed(&self) -> Result<u64, StateError> {
        let (failed, _, _) = self
            .prepare_for_retry(None, RetryErrorRetention::Clear)
            .await?;
        Ok(failed)
    }

    pub(crate) async fn prepare_for_retry(
        &self,
        library: Option<&str>,
        error_retention: RetryErrorRetention,
    ) -> Result<(u64, u64, u64), StateError> {
        let library = library.map(ToOwned::to_owned);
        self.with_conn("prepare_for_retry", move |conn| {
            let failed = match (library.as_deref(), error_retention) {
                (Some(library), RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, last_error = NULL \
                     WHERE status = 'failed' AND is_deleted = 0 AND library = ?1",
                    rusqlite::params![library],
                ),
                (None, RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, last_error = NULL \
                     WHERE status = 'failed' AND is_deleted = 0",
                    [],
                ),
                (Some(library), RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?2 THEN last_error ELSE NULL END \
                     WHERE status = 'failed' AND is_deleted = 0 AND library = ?1",
                    rusqlite::params![library, reason],
                ),
                (None, RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?1 THEN last_error ELSE NULL END \
                     WHERE status = 'failed' AND is_deleted = 0",
                    rusqlite::params![reason],
                ),
            }
            .map_err(|e| StateError::query("prepare_for_retry", e))?
                as u64;

            let pending = match (library.as_deref(), error_retention) {
                (Some(library), RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, last_error = NULL \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0 AND library = ?1",
                    rusqlite::params![library],
                ),
                (None, RetryErrorRetention::Clear) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, last_error = NULL \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0",
                    [],
                ),
                (Some(library), RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?2 THEN last_error ELSE NULL END \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0 AND library = ?1",
                    rusqlite::params![library, reason],
                ),
                (None, RetryErrorRetention::Preserve(reason)) => conn.execute(
                    "UPDATE assets SET download_attempts = 0, \
                     last_error = CASE WHEN last_error = ?1 THEN last_error ELSE NULL END \
                     WHERE status = 'pending' AND is_deleted = 0 AND download_attempts > 0",
                    rusqlite::params![reason],
                ),
            }
            .map_err(|e| StateError::query("prepare_for_retry", e))?
                as u64;

            let total_pending: i64 = if let Some(library) = library.as_deref() {
                conn.query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'pending' AND is_deleted = 0 AND library = ?1",
                    rusqlite::params![library],
                    |row| row.get(0),
                )
            } else {
                conn.query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'pending' AND is_deleted = 0",
                    [],
                    |row| row.get(0),
                )
            }
            .map_err(|e| StateError::query("prepare_for_retry", e))?;
            #[allow(clippy::cast_sign_loss, reason = "SQL COUNT(*) is always non-negative")]
            let total_pending = total_pending as u64;

            Ok((failed, pending, total_pending))
        })
        .await
    }

    pub(crate) async fn prune_source_deleted_retries(
        &self,
        _library: Option<&str>,
    ) -> Result<u64, StateError> {
        Ok(0)
    }

    pub(crate) async fn promote_pending_to_failed(
        &self,
        seen_since: i64,
    ) -> Result<u64, StateError> {
        self.with_conn("promote_pending_to_failed", move |conn| {
            // Only promote assets the producer dispatched this sync (last_seen_at
            // was bumped by upsert_seen at or after sync_started_at) that never
            // reached mark_downloaded or mark_failed. See the trait doc comment
            // and issue #211 for the rationale.
            let promoted = conn
                .execute(
                    "UPDATE assets SET status = 'failed', last_error = 'Not resolved during sync' \
                     WHERE status = 'pending' AND last_seen_at >= ?1 \
                       AND NOT EXISTS ( \
                         SELECT 1 FROM asset_verifications AS verification \
                         WHERE verification.library = assets.library \
                           AND verification.id = assets.id \
                           AND verification.version_size = assets.version_size \
                       )",
                    rusqlite::params![seen_since],
                )
                .map_err(|e| StateError::query("promote_pending_to_failed", e))?
                as u64;

            Ok(promoted)
        })
        .await
    }

    pub(crate) async fn prune_stale_pending_not_seen_since(
        &self,
        library: &str,
        seen_since: i64,
    ) -> Result<u64, StateError> {
        let library = library.to_string();
        self.with_conn("prune_stale_pending_not_seen_since", move |conn| {
            let pruned = conn
                .execute(
                    "DELETE FROM assets \
                     WHERE library = ?1 AND status = 'pending' AND last_seen_at < ?2",
                    rusqlite::params![library, seen_since],
                )
                .map_err(|e| StateError::query("prune_stale_pending_not_seen_since", e))?
                as u64;

            Ok(pruned)
        })
        .await
    }

    pub(crate) async fn prune_pending_asset_versions(
        &self,
        library: &str,
        asset_versions: &[(String, String)],
    ) -> Result<u64, StateError> {
        if asset_versions.is_empty() {
            return Ok(0);
        }
        let library = library.to_string();
        let asset_versions = asset_versions.to_vec();
        self.with_conn_mut("prune_pending_asset_versions", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("prune_pending_asset_versions::begin", e))?;
            let pruned = {
                let mut stmt = tx
                    .prepare_cached(
                        "DELETE FROM assets \
                         WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                           AND status = 'pending'",
                    )
                    .map_err(|e| StateError::query("prune_pending_asset_versions::prepare", e))?;
                let mut pruned = 0u64;
                for (asset_id, version_size) in &asset_versions {
                    pruned += stmt
                        .execute(rusqlite::params![&library, asset_id, version_size])
                        .map_err(|e| {
                            StateError::query("prune_pending_asset_versions::execute", e)
                        })? as u64;
                }
                pruned
            };
            tx.commit()
                .map_err(|e| StateError::query("prune_pending_asset_versions::commit", e))?;
            Ok(pruned)
        })
        .await
    }

    pub(crate) async fn get_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_downloaded_ids", move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'downloaded'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;
            let count = usize::try_from(count).unwrap_or(0);

            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size FROM assets WHERE status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;

            let mut ids = HashSet::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;
            for row in rows {
                ids.insert(row.map_err(|e| StateError::query("get_downloaded_ids", e))?);
            }

            Ok(ids)
        })
        .await
    }

    pub(crate) async fn get_downloaded_file_records(
        &self,
    ) -> Result<Vec<DownloadedFileRecord>, StateError> {
        self.with_conn("get_downloaded_file_records", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, checksum, local_path, \
                            local_checksum, download_checksum \
                     FROM assets WHERE status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_downloaded_file_records", e))?;

            stmt.query_map([], |row| {
                let version_size: String = row.get(2)?;
                let local_path: Option<String> = row.get(4)?;
                Ok(DownloadedFileRecord {
                    library: row.get(0)?,
                    id: row.get(1)?,
                    version_size: VersionSizeKey::from_str(&version_size)
                        .unwrap_or(VersionSizeKey::Original),
                    checksum: row.get(3)?,
                    local_path: local_path.map(PathBuf::from),
                    local_checksum: row.get(5)?,
                    download_checksum: row.get(6)?,
                })
            })
            .map_err(|e| StateError::query("get_downloaded_file_records", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StateError::query("get_downloaded_file_records", e))
        })
        .await
    }

    /// Assets that still hold `status = 'downloaded'` rows after the provider
    /// reported them deleted. Soft deletion flips every version of an asset
    /// together, so this is keyed by asset rather than by version.
    pub(crate) async fn get_soft_deleted_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String)>, StateError> {
        self.with_conn("get_soft_deleted_downloaded_ids", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT DISTINCT library, id FROM assets \
                     WHERE status = 'downloaded' AND is_deleted = 1",
                )
                .map_err(|e| StateError::query("get_soft_deleted_downloaded_ids", e))?;

            let mut ids = HashSet::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_soft_deleted_downloaded_ids", e))?;
            for row in rows {
                ids.insert(
                    row.map_err(|e| StateError::query("get_soft_deleted_downloaded_ids", e))?,
                );
            }

            Ok(ids)
        })
        .await
    }

    pub(crate) async fn get_all_known_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
        self.with_conn("get_all_known_ids", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT DISTINCT library, id FROM assets")
                .map_err(|e| StateError::query("get_all_known_ids", e))?;

            let ids = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_known_ids", e))?
                .collect::<Result<HashSet<_>, _>>()
                .map_err(|e| StateError::query("get_all_known_ids", e))?;

            Ok(ids)
        })
        .await
    }

    pub(crate) async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        self.with_conn("get_downloaded_checksums", move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'downloaded'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_downloaded_checksums", e))?;
            let count = usize::try_from(count).unwrap_or(0);

            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, checksum FROM assets \
                     WHERE status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_downloaded_checksums", e))?;

            let mut checksums = HashMap::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        (
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ),
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| StateError::query("get_downloaded_checksums", e))?;
            for row in rows {
                let (key, val) =
                    row.map_err(|e| StateError::query("get_downloaded_checksums", e))?;
                checksums.insert(key, val);
            }

            Ok(checksums)
        })
        .await
    }

    pub(crate) async fn get_downloaded_local_paths(
        &self,
    ) -> Result<HashMap<(String, String, String), PathBuf>, StateError> {
        self.with_conn("get_downloaded_local_paths", move |conn| {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets \
                     WHERE status = 'downloaded' AND local_path IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_downloaded_local_paths", e))?;
            let count = usize::try_from(count).unwrap_or(0);

            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, local_path FROM assets \
                     WHERE status = 'downloaded' AND local_path IS NOT NULL",
                )
                .map_err(|e| StateError::query("get_downloaded_local_paths", e))?;

            let mut paths = HashMap::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    let local_path: String = row.get(3)?;
                    Ok((
                        (
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ),
                        PathBuf::from(local_path),
                    ))
                })
                .map_err(|e| StateError::query("get_downloaded_local_paths", e))?;
            for row in rows {
                let (key, val) =
                    row.map_err(|e| StateError::query("get_downloaded_local_paths", e))?;
                paths.insert(key, val);
            }

            Ok(paths)
        })
        .await
    }

    pub(crate) async fn get_attempt_counts(
        &self,
    ) -> Result<HashMap<(String, String), u32>, StateError> {
        self.with_conn("get_attempt_counts", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, MAX(download_attempts) FROM assets \
                     WHERE download_attempts > 0 GROUP BY library, id",
                )
                .map_err(|e| StateError::query("get_attempt_counts", e))?;

            let counts = stmt
                .query_map([], |row| {
                    let library: String = row.get(0)?;
                    let id: String = row.get(1)?;
                    let count: i64 = row.get(2)?;
                    Ok(((library, id), u32::try_from(count).unwrap_or(u32::MAX)))
                })
                .map_err(|e| StateError::query("get_attempt_counts", e))?
                .collect::<Result<HashMap<_, _>, _>>()
                .map_err(|e| StateError::query("get_attempt_counts", e))?;

            Ok(counts)
        })
        .await
    }

    pub(crate) async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError> {
        let key = key.to_owned();
        self.with_conn("get_metadata", move |conn| {
            let value = conn
                .query_row("SELECT value FROM metadata WHERE key = ?1", [&key], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
                .map_err(|e| StateError::query("get_metadata", e))?;

            Ok(value)
        })
        .await
    }

    pub(crate) async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
        let key = key.to_owned();
        let value = value.to_owned();
        self.with_conn("set_metadata", move |conn| {
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )
            .map_err(|e| StateError::query("set_metadata", e))?;

            Ok(())
        })
        .await
    }

    pub(crate) async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError> {
        let prefix = prefix.to_owned();
        self.with_conn("delete_metadata_by_prefix", move |conn| {
            let mut stmt = conn
                .prepare_cached("DELETE FROM metadata WHERE key LIKE ?1")
                .map_err(|e| StateError::query("delete_metadata_by_prefix::prepare", e))?;
            let deleted = stmt
                .execute([format!("{prefix}%")])
                .map_err(|e| StateError::query("delete_metadata_by_prefix", e))?;

            Ok(deleted as u64)
        })
        .await
    }

    pub(crate) async fn commit_checkpoint_transition(
        &self,
        transition: CheckpointTransition,
    ) -> Result<(), StateError> {
        self.with_conn_mut("commit_checkpoint_transition", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("commit_checkpoint_transition::begin", e))?;
            for (key, value) in transition.metadata_updates {
                tx.execute(
                    "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![key, value],
                )
                .map_err(|e| StateError::query("commit_checkpoint_transition::update", e))?;
            }
            for key in transition.metadata_deletes {
                tx.execute("DELETE FROM metadata WHERE key = ?1", [key])
                    .map_err(|e| StateError::query("commit_checkpoint_transition::delete", e))?;
            }
            tx.commit()
                .map_err(|e| StateError::query("commit_checkpoint_transition::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_scoped_db_sync_token(
        &self,
        provider: &str,
        account: &str,
        shape_version: i64,
        scope_hash: &str,
    ) -> Result<Option<ScopedDbSyncToken>, StateError> {
        let provider = provider.to_owned();
        let account = account.to_owned();
        let scope_hash = scope_hash.to_owned();
        self.with_conn("get_scoped_db_sync_token", move |conn| {
            conn.query_row(
                "SELECT selected_zones_json, scope_json, token \
                 FROM scoped_db_sync_tokens \
                 WHERE provider = ?1 AND account = ?2 AND shape_version = ?3 AND scope_hash = ?4",
                rusqlite::params![provider, account, shape_version, scope_hash],
                |row| {
                    Ok(ScopedDbSyncToken {
                        provider: provider.clone(),
                        account: account.clone(),
                        shape_version,
                        scope_hash: scope_hash.clone(),
                        selected_zones_json: row.get(0)?,
                        scope_json: row.get(1)?,
                        token: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| StateError::query("get_scoped_db_sync_token", e))
        })
        .await
    }

    pub(crate) async fn upsert_scoped_db_sync_token(
        &self,
        token: ScopedDbSyncToken,
    ) -> Result<(), StateError> {
        self.with_conn("upsert_scoped_db_sync_token", move |conn| {
            let now = Utc::now().timestamp();
            conn.execute(
                "INSERT INTO scoped_db_sync_tokens \
                    (provider, account, shape_version, scope_hash, selected_zones_json, scope_json, token, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8) \
                 ON CONFLICT(provider, account, shape_version, scope_hash) DO UPDATE SET \
                    selected_zones_json = excluded.selected_zones_json, \
                    scope_json = excluded.scope_json, \
                    token = excluded.token, \
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    token.provider,
                    token.account,
                    token.shape_version,
                    token.scope_hash,
                    token.selected_zones_json,
                    token.scope_json,
                    token.token,
                    now,
                ],
            )
            .map_err(|e| StateError::query("upsert_scoped_db_sync_token", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn delete_scoped_db_sync_tokens(&self) -> Result<u64, StateError> {
        self.with_conn("delete_scoped_db_sync_tokens", move |conn| {
            let deleted = conn
                .execute("DELETE FROM scoped_db_sync_tokens", [])
                .map_err(|e| StateError::query("delete_scoped_db_sync_tokens", e))?;
            Ok(deleted as u64)
        })
        .await
    }

    pub(crate) async fn touch_last_seen_many(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<(), StateError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let library = library.to_owned();
        let ids: Vec<String> = asset_ids.iter().map(|s| (*s).to_owned()).collect();
        self.with_conn_mut("touch_last_seen_many", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("touch_last_seen_many::begin", e))?;
            {
                let mut stmt = tx
                    .prepare_cached(
                        "UPDATE assets SET last_seen_at = ?1 WHERE library = ?2 AND id = ?3",
                    )
                    .map_err(|e| StateError::query("touch_last_seen_many::prepare", e))?;
                for id in &ids {
                    stmt.execute(rusqlite::params![now, &library, id])
                        .map_err(|e| StateError::query("touch_last_seen_many::execute", e))?;
                }
            }
            tx.commit()
                .map_err(|e| StateError::query("touch_last_seen_many::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn add_asset_album(
        &self,
        library: &str,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let album_name = album_name.to_owned();
        let source = source.to_owned();
        self.with_conn_mut("add_asset_album", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("add_asset_album::begin", e))?;
            let changed = tx
                .execute(
                    "INSERT OR IGNORE INTO asset_albums (library, asset_id, album_name, source) \
                 VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![&library, &asset_id, album_name, source],
                )
                .map_err(|e| StateError::query("add_asset_album", e))?;
            if changed > 0 {
                mark_album_groupings_dirty_tx(&tx, &library, &asset_id)?;
            }
            tx.commit()
                .map_err(|e| StateError::query("add_asset_album::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_all_asset_albums(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        let library = library.to_owned();
        self.with_conn("get_all_asset_albums", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT asset_id, album_name FROM asset_albums WHERE library = ?1")
                .map_err(|e| StateError::query("get_all_asset_albums", e))?;
            let rows = stmt
                .query_map([&library], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_asset_albums", e))?;
            Ok(collect_rows_with_warn(rows, "get_all_asset_albums"))
        })
        .await
    }

    pub(crate) async fn get_all_asset_people(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        let library = library.to_owned();
        self.with_conn("get_all_asset_people", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT asset_id, person_name FROM asset_people WHERE library = ?1")
                .map_err(|e| StateError::query("get_all_asset_people", e))?;
            let rows = stmt
                .query_map([&library], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_asset_people", e))?;
            Ok(collect_rows_with_warn(rows, "get_all_asset_people"))
        })
        .await
    }

    pub(crate) async fn get_asset_groupings(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<AssetGroupingRows, StateError> {
        let library = library.to_owned();
        let asset_ids = unique_sorted_strings(asset_ids);
        if asset_ids.is_empty() {
            return Ok(AssetGroupingRows::default());
        }
        self.with_conn("get_asset_groupings", move |conn| {
            let placeholders = sqlite_placeholders(asset_ids.len());
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(asset_ids.len() + 1);
            params.push(&library);
            for asset_id in &asset_ids {
                params.push(asset_id);
            }

            let album_sql = format!(
                "SELECT asset_id, album_name FROM asset_albums \
                 WHERE library = ? AND asset_id IN ({placeholders}) \
                 ORDER BY asset_id, album_name"
            );
            let mut album_stmt = conn
                .prepare(&album_sql)
                .map_err(|e| StateError::query("get_asset_groupings", e))?;
            let albums = album_stmt
                .query_map(rusqlite::params_from_iter(params.iter().copied()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_asset_groupings", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_asset_groupings", e))?;

            let people_sql = format!(
                "SELECT asset_id, person_name FROM asset_people \
                 WHERE library = ? AND asset_id IN ({placeholders}) \
                 ORDER BY asset_id, person_name"
            );
            let mut people_stmt = conn
                .prepare(&people_sql)
                .map_err(|e| StateError::query("get_asset_groupings", e))?;
            let people = people_stmt
                .query_map(rusqlite::params_from_iter(params), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_asset_groupings", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_asset_groupings", e))?;

            Ok(AssetGroupingRows { albums, people })
        })
        .await
    }

    pub(crate) async fn upsert_album_container(
        &self,
        library: &str,
        container_id: &str,
        album_name: &str,
        pass_kind: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let album_name = album_name.to_owned();
        let pass_kind = pass_kind.to_owned();
        self.with_conn_mut("upsert_album_container", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("upsert_album_container::begin", e))?;
            let previous_name: Option<String> = tx.query_row(
                "SELECT album_name FROM album_containers WHERE library = ?1 AND container_id = ?2",
                rusqlite::params![&library, &container_id], |row| row.get(0),
            ).optional().map_err(|e| StateError::query("upsert_album_container::name", e))?;
            tx.execute(
                "INSERT INTO album_containers \
                    (library, container_id, album_name, pass_kind, is_deleted, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 0, ?5) \
                 ON CONFLICT(library, container_id) DO UPDATE SET \
                    album_name = excluded.album_name, \
                    pass_kind = excluded.pass_kind, \
                    is_deleted = 0, \
                    updated_at = excluded.updated_at",
                rusqlite::params![&library, &container_id, album_name, pass_kind, now],
            )
            .map_err(|e| StateError::query("upsert_album_container", e))?;
            refresh_container_groupings_tx(
                &tx,
                &library,
                &container_id,
                None,
                previous_name.as_deref(),
            )?;
            tx.commit()
                .map_err(|e| StateError::query("upsert_album_container::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_album_container_deleted(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        self.with_conn_mut("mark_album_container_deleted", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_album_container_deleted::begin", e))?;
            let previous_name: Option<String> = tx.query_row(
                "SELECT album_name FROM album_containers WHERE library = ?1 AND container_id = ?2",
                rusqlite::params![&library, &container_id], |row| row.get(0),
            ).optional().map_err(|e| StateError::query("mark_album_container_deleted::name", e))?;
            tx.execute(
                "UPDATE album_containers \
                 SET is_deleted = 1, updated_at = ?1 \
                 WHERE library = ?2 AND container_id = ?3",
                rusqlite::params![now, &library, &container_id],
            )
            .map_err(|e| StateError::query("mark_album_container_deleted", e))?;
            refresh_container_groupings_tx(
                &tx,
                &library,
                &container_id,
                None,
                previous_name.as_deref(),
            )?;
            tx.commit()
                .map_err(|e| StateError::query("mark_album_container_deleted::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn start_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        enum_config_hash: Option<&str>,
    ) -> Result<i64, StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let enum_config_hash = enum_config_hash.map(ToOwned::to_owned);
        self.with_conn_mut("start_album_membership_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("start_album_membership_snapshot::begin", e))?;
            let generation: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(generation), 0) + 1 \
                     FROM album_membership_snapshots \
                     WHERE library = ?1 AND container_id = ?2",
                    rusqlite::params![&library, &container_id],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("start_album_membership_snapshot::generation", e))?;
            tx.execute(
                "INSERT INTO album_membership_snapshots \
                    (library, container_id, generation, status, enum_config_hash, started_at) \
                 VALUES (?1, ?2, ?3, 'running', ?4, ?5)",
                rusqlite::params![
                    &library,
                    &container_id,
                    generation,
                    enum_config_hash.as_deref(),
                    now
                ],
            )
            .map_err(|e| StateError::query("start_album_membership_snapshot::insert", e))?;
            tx.commit()
                .map_err(|e| StateError::query("start_album_membership_snapshot::commit", e))?;
            Ok(generation)
        })
        .await
    }

    pub(crate) async fn add_album_membership_to_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let master_record_name = master_record_name.map(ToOwned::to_owned);
        let source = source.to_owned();
        self.with_conn_mut("add_album_membership_to_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("add_album_membership_to_snapshot::begin", e))?;
            let snapshot_exists: bool = tx
                .query_row(
                    "SELECT 1 FROM album_membership_snapshots \
                     WHERE library = ?1 AND container_id = ?2 \
                       AND generation = ?3 AND status = 'running'",
                    rusqlite::params![&library, &container_id, generation],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|e| {
                    StateError::query("add_album_membership_to_snapshot::snapshot", e)
                })?
                .is_some();
            if !snapshot_exists {
                return Err(StateError::Invariant {
                    operation: "add_album_membership_to_snapshot",
                    detail: format!(
                        "no running snapshot for library {library} container {container_id} generation {generation}"
                    ),
                });
            }
            tx.execute(
                "INSERT INTO asset_album_memberships \
                    (library, asset_record_name, master_record_name, container_id, generation, \
                     is_deleted, source, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7) \
                 ON CONFLICT(library, asset_record_name, container_id) DO UPDATE SET \
                    master_record_name = COALESCE(excluded.master_record_name, asset_album_memberships.master_record_name), \
                    generation = excluded.generation, \
                    is_deleted = 0, \
                    source = excluded.source, \
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    &library,
                    &asset_record_name,
                    master_record_name.as_deref(),
                    &container_id,
                    generation,
                    &source,
                    now
                ],
            )
            .map_err(|e| StateError::query("add_album_membership_to_snapshot::upsert", e))?;
            refresh_container_groupings_tx(&tx, &library, &container_id, Some(&asset_record_name), None)?;
            tx.commit()
                .map_err(|e| StateError::query("add_album_membership_to_snapshot::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn upsert_album_membership_delta(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<bool, StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let master_record_name = master_record_name.map(ToOwned::to_owned);
        let source = source.to_owned();
        self.with_conn_mut("upsert_album_membership_delta", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("upsert_album_membership_delta::begin", e))?;
            let container_known = album_container_known_tx(
                &tx,
                &library,
                &container_id,
                "upsert_album_membership_delta::container",
            )?;
            tx.execute(
                "INSERT INTO asset_album_memberships \
                    (library, asset_record_name, master_record_name, container_id, generation, \
                     is_deleted, source, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5, ?6) \
                 ON CONFLICT(library, asset_record_name, container_id) DO UPDATE SET \
                    master_record_name = COALESCE(excluded.master_record_name, asset_album_memberships.master_record_name), \
                    is_deleted = 0, \
                    source = excluded.source, \
                    updated_at = excluded.updated_at",
                rusqlite::params![
                    &library,
                    &asset_record_name,
                    master_record_name.as_deref(),
                    &container_id,
                    &source,
                    now
                ],
            )
            .map_err(|e| StateError::query("upsert_album_membership_delta::upsert", e))?;
            refresh_container_groupings_tx(&tx, &library, &container_id, Some(&asset_record_name), None)?;
            tx.commit()
                .map_err(|e| StateError::query("upsert_album_membership_delta::commit", e))?;
            Ok(container_known)
        })
        .await
    }

    pub(crate) async fn mark_album_membership_deleted(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        self.with_conn_mut("mark_album_membership_deleted", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_album_membership_deleted::begin", e))?;
            let container_known = album_container_known_tx(
                &tx,
                &library,
                &container_id,
                "mark_album_membership_deleted::container",
            )?;
            tx.execute(
                "UPDATE asset_album_memberships \
                 SET is_deleted = 1, updated_at = ?1 \
                 WHERE library = ?2 AND container_id = ?3 AND asset_record_name = ?4",
                rusqlite::params![now, &library, &container_id, &asset_record_name],
            )
            .map_err(|e| StateError::query("mark_album_membership_deleted::update", e))?;
            refresh_container_groupings_tx(
                &tx,
                &library,
                &container_id,
                Some(&asset_record_name),
                None,
            )?;
            tx.commit()
                .map_err(|e| StateError::query("mark_album_membership_deleted::commit", e))?;
            Ok(container_known)
        })
        .await
    }

    pub(crate) async fn complete_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        self.with_conn_mut("complete_album_membership_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("complete_album_membership_snapshot::begin", e))?;
            let updated = tx
                .execute(
                    "UPDATE album_membership_snapshots \
                     SET status = 'complete', completed_at = ?1 \
                     WHERE library = ?2 AND container_id = ?3 \
                       AND generation = ?4 AND status = 'running'",
                    rusqlite::params![now, &library, &container_id, generation],
                )
                .map_err(|e| {
                    StateError::query("complete_album_membership_snapshot::snapshot", e)
                })?;
            if updated == 0 {
                return Err(StateError::Invariant {
                    operation: "complete_album_membership_snapshot",
                    detail: format!(
                        "no running snapshot for library {library} container {container_id} generation {generation}"
                    ),
                });
            }
            tx.execute(
                "UPDATE asset_album_memberships \
                 SET is_deleted = 1, updated_at = ?1 \
                 WHERE library = ?2 AND container_id = ?3 \
                   AND generation <> ?4 AND is_deleted = 0",
                rusqlite::params![now, &library, &container_id, generation],
            )
            .map_err(|e| StateError::query("complete_album_membership_snapshot::prune", e))?;
            refresh_container_groupings_tx(&tx, &library, &container_id, None, None)?;
            tx.commit()
                .map_err(|e| StateError::query("complete_album_membership_snapshot::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn invalidate_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let container_id = container_id.to_owned();
        self.with_conn("invalidate_album_membership_snapshot", move |conn| {
            let now = Utc::now().timestamp();
            conn.execute(
                "UPDATE album_membership_snapshots \
                 SET status = 'invalidated', completed_at = COALESCE(completed_at, ?1) \
                 WHERE library = ?2 AND container_id = ?3 \
                   AND status IN ('running', 'complete')",
                rusqlite::params![now, library, container_id],
            )
            .map_err(|e| StateError::query("invalidate_album_membership_snapshot", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn selected_album_containers_have_complete_snapshots(
        &self,
        library: &str,
        container_ids: &[&str],
    ) -> Result<bool, StateError> {
        let container_ids = unique_sorted_strings(container_ids);
        if container_ids.is_empty() {
            return Ok(true);
        }
        let library = library.to_owned();
        let placeholders = sqlite_placeholders(container_ids.len());
        self.with_conn(
            "selected_album_containers_have_complete_snapshots",
            move |conn| {
                let sql = format!(
                    "SELECT COUNT(DISTINCT s.container_id) \
                     FROM album_membership_snapshots s \
                     JOIN album_containers c \
                       ON c.library = s.library AND c.container_id = s.container_id \
                     WHERE s.library = ? AND s.container_id IN ({placeholders}) \
                       AND s.status = 'complete' AND c.is_deleted = 0"
                );
                let mut params: Vec<&dyn rusqlite::ToSql> =
                    Vec::with_capacity(1 + container_ids.len());
                params.push(&library);
                for container_id in &container_ids {
                    params.push(container_id);
                }
                let complete_count: i64 = conn
                    .query_row(&sql, rusqlite::params_from_iter(params), |row| row.get(0))
                    .map_err(|e| {
                        StateError::query(
                            "selected_album_containers_have_complete_snapshots::query",
                            e,
                        )
                    })?;
                Ok(complete_count == container_ids.len() as i64)
            },
        )
        .await
    }

    pub(crate) async fn get_live_selected_album_memberships_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
        selected_container_ids: &[&str],
    ) -> Result<Vec<AlbumMembershipRecord>, StateError> {
        let selected_container_ids = unique_sorted_strings(selected_container_ids);
        if selected_container_ids.is_empty() {
            return Ok(Vec::new());
        }
        let library = library.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let placeholders = sqlite_placeholders(selected_container_ids.len());
        self.with_conn(
            "get_live_selected_album_memberships_for_asset",
            move |conn| {
                let sql = format!(
                    "SELECT library, asset_record_name, master_record_name, container_id, \
                            generation, source \
                     FROM asset_album_memberships \
                     WHERE library = ? AND asset_record_name = ? AND is_deleted = 0 \
                       AND container_id IN ({placeholders}) \
                     ORDER BY container_id",
                );
                let mut params: Vec<&dyn rusqlite::ToSql> =
                    Vec::with_capacity(2 + selected_container_ids.len());
                params.push(&library);
                params.push(&asset_record_name);
                for container_id in &selected_container_ids {
                    params.push(container_id);
                }
                let mut stmt = conn.prepare(&sql).map_err(|e| {
                    StateError::query("get_live_selected_album_memberships_for_asset", e)
                })?;
                let rows = stmt
                    .query_map(rusqlite::params_from_iter(params), |row| {
                        album_membership_record_from_row(row)
                    })
                    .map_err(|e| {
                        StateError::query("get_live_selected_album_memberships_for_asset", e)
                    })?;
                Ok(collect_rows_with_warn(
                    rows,
                    "get_live_selected_album_memberships_for_asset",
                ))
            },
        )
        .await
    }

    pub(crate) async fn upsert_asset_master_mapping(
        &self,
        library: &str,
        asset_record_name: &str,
        master_record_name: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn("upsert_asset_master_mapping", move |conn| {
            let now = Utc::now().timestamp();
            conn.execute(
                "INSERT INTO asset_master_mappings \
                    (library, asset_record_name, master_record_name, updated_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(library, asset_record_name) DO UPDATE SET \
                    master_record_name = excluded.master_record_name, \
                    updated_at = excluded.updated_at",
                rusqlite::params![library, asset_record_name, master_record_name, now],
            )
            .map_err(|e| StateError::query("upsert_asset_master_mapping", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_master_record_name_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
    ) -> Result<Option<String>, StateError> {
        let library = library.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        self.with_conn("get_master_record_name_for_asset", move |conn| {
            conn.query_row(
                "SELECT master_record_name FROM asset_master_mappings \
                 WHERE library = ?1 AND asset_record_name = ?2",
                rusqlite::params![library, asset_record_name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| StateError::query("get_master_record_name_for_asset", e))
        })
        .await
    }

    pub(crate) async fn get_asset_record_names_for_master(
        &self,
        library: &str,
        master_record_name: &str,
    ) -> Result<Vec<String>, StateError> {
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn("get_asset_record_names_for_master", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT asset_record_name FROM asset_master_mappings \
                     WHERE library = ?1 AND master_record_name = ?2 \
                     ORDER BY asset_record_name",
                )
                .map_err(|e| StateError::query("get_asset_record_names_for_master::prepare", e))?;
            let rows = stmt
                .query_map(rusqlite::params![library, master_record_name], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|e| StateError::query("get_asset_record_names_for_master::query", e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_asset_record_names_for_master::row", e))?;
            Ok(rows)
        })
        .await
    }

    pub(crate) async fn get_asset_master_mappings(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_asset_master_mappings", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, asset_record_name, master_record_name \
                     FROM asset_master_mappings",
                )
                .map_err(|e| StateError::query("get_asset_master_mappings::prepare", e))?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| StateError::query("get_asset_master_mappings::query", e))?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|e| StateError::query("get_asset_master_mappings::row", e))
        })
        .await
    }

    pub(crate) async fn get_legacy_master_state_owners(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_legacy_master_state_owners", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, master_record_name, asset_record_name \
                     FROM legacy_master_state_owners",
                )
                .map_err(|e| StateError::query("get_legacy_master_state_owners::prepare", e))?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| StateError::query("get_legacy_master_state_owners::query", e))?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|e| StateError::query("get_legacy_master_state_owners::row", e))
        })
        .await
    }

    pub(crate) async fn claim_legacy_master_state_owner(
        &self,
        library: &str,
        master_record_name: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        #[cfg(test)]
        {
            let mut remaining = self
                .legacy_owner_claim_failures
                .load(std::sync::atomic::Ordering::Relaxed);
            let inject_failure = loop {
                if remaining == 0 {
                    break false;
                }
                match self.legacy_owner_claim_failures.compare_exchange_weak(
                    remaining,
                    remaining - 1,
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                ) {
                    Ok(_) => break true,
                    Err(actual) => remaining = actual,
                }
            };
            if inject_failure {
                return Err(StateError::LockPoisoned(
                    "injected legacy owner claim failure".into(),
                ));
            }
        }
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        let asset_record_name = asset_record_name.to_owned();
        self.with_conn_mut("claim_legacy_master_state_owner", move |conn| {
            let tx = conn.transaction().map_err(|e| {
                StateError::query("claim_legacy_master_state_owner::transaction", e)
            })?;
            tx.execute(
                "INSERT OR IGNORE INTO legacy_master_state_owners \
                    (library, master_record_name, asset_record_name, claimed_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    library,
                    master_record_name,
                    asset_record_name,
                    Utc::now().timestamp()
                ],
            )
            .map_err(|e| StateError::query("claim_legacy_master_state_owner::insert", e))?;
            let owner: String = tx
                .query_row(
                    "SELECT asset_record_name FROM legacy_master_state_owners \
                     WHERE library = ?1 AND master_record_name = ?2",
                    rusqlite::params![library, master_record_name],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("claim_legacy_master_state_owner::query", e))?;
            tx.commit()
                .map_err(|e| StateError::query("claim_legacy_master_state_owner::commit", e))?;
            Ok(owner == asset_record_name)
        })
        .await
    }

    pub(crate) async fn set_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        state: AssetVerificationState,
        reason: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let reason = reason.to_owned();
        self.with_conn("set_asset_verification", move |conn| {
            conn.execute(
                "INSERT INTO asset_verifications \
                    (library, id, version_size, state, reason, checked_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(library, id, version_size) DO UPDATE SET \
                    state = excluded.state, reason = excluded.reason, \
                    checked_at = excluded.checked_at",
                rusqlite::params![
                    library,
                    id,
                    version_size,
                    state.as_str(),
                    reason,
                    Utc::now().timestamp()
                ],
            )
            .map_err(|e| StateError::query("set_asset_verification", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn clear_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn("clear_asset_verification", move |conn| {
            conn.execute(
                "DELETE FROM asset_verifications \
                 WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                rusqlite::params![library, id, version_size],
            )
            .map_err(|e| StateError::query("clear_asset_verification", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn mark_policy_excluded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<bool, StateError> {
        let library = library.to_owned();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn_mut("mark_policy_excluded", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("mark_policy_excluded::begin", e))?;
            let affected = tx
                .execute(
                    "UPDATE assets SET status = 'policy_excluded', last_error = NULL \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                       AND status = 'pending' AND is_deleted = 0",
                    rusqlite::params![library, id, version_size],
                )
                .map_err(|e| StateError::query("mark_policy_excluded::update", e))?;
            if affected > 0 {
                tx.execute(
                    "DELETE FROM asset_verifications \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                    rusqlite::params![library, id, version_size],
                )
                .map_err(|e| StateError::query("mark_policy_excluded::verification", e))?;
            }
            tx.commit()
                .map_err(|e| StateError::query("mark_policy_excluded::commit", e))?;
            Ok(affected > 0)
        })
        .await
    }

    pub(crate) async fn backfill_asset_master_mappings_from_album_memberships(
        &self,
    ) -> Result<u64, StateError> {
        self.with_conn(
            "backfill_asset_master_mappings_from_album_memberships",
            move |conn| {
                let now = Utc::now().timestamp();
                let inserted = conn
                    .execute(
                        "INSERT OR IGNORE INTO asset_master_mappings \
                        (library, asset_record_name, master_record_name, updated_at) \
                     SELECT \
                        membership.library, \
                        membership.asset_record_name, \
                        MIN(membership.master_record_name), \
                        ?1 \
                     FROM asset_album_memberships AS membership \
                     WHERE membership.asset_record_name <> '' \
                       AND membership.master_record_name IS NOT NULL \
                       AND membership.master_record_name <> '' \
                       AND NOT EXISTS ( \
                           SELECT 1 \
                           FROM asset_master_mappings AS mapping \
                           WHERE mapping.library = membership.library \
                             AND mapping.asset_record_name = membership.asset_record_name \
                       ) \
                     GROUP BY membership.library, membership.asset_record_name \
                     HAVING COUNT(DISTINCT membership.master_record_name) = 1",
                        rusqlite::params![now],
                    )
                    .map_err(|e| {
                        StateError::query(
                            "backfill_asset_master_mappings_from_album_memberships",
                            e,
                        )
                    })?;
                Ok(inserted as u64)
            },
        )
        .await
    }

    pub(crate) async fn mark_soft_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        self.with_conn("mark_soft_deleted", move |conn| {
            ensure_asset_has_no_prepared_capture_repair(
                conn,
                &library,
                &asset_id,
                "mark_soft_deleted",
            )?;
            let updated = conn
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                 WHERE library = ?2 AND id = ?3",
                    rusqlite::params![deleted_at.map(|dt| dt.timestamp()), &library, &asset_id],
                )
                .map_err(|e| StateError::query("mark_soft_deleted", e))?;
            Ok(updated)
        })
        .await
    }

    pub(crate) async fn resolve_source_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        self.with_conn_mut("resolve_source_deleted", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("resolve_source_deleted::begin", e))?;
            ensure_asset_has_no_prepared_capture_repair(
                &tx,
                &library,
                &asset_id,
                "resolve_source_deleted",
            )?;
            let marked = tx
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                     WHERE library = ?2 AND id = ?3",
                    rusqlite::params![deleted_at.map(|dt| dt.timestamp()), library, asset_id],
                )
                .map_err(|e| StateError::query("resolve_source_deleted::mark", e))?;
            tx.execute(
                "DELETE FROM asset_verifications WHERE library = ?1 AND id = ?2",
                rusqlite::params![&library, &asset_id],
            )
            .map_err(|e| StateError::query("resolve_source_deleted::clear_verification", e))?;
            tx.commit()
                .map_err(|e| StateError::query("resolve_source_deleted::commit", e))?;
            Ok(marked)
        })
        .await
    }

    pub(crate) async fn mark_master_family_soft_deleted(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn("mark_master_family_soft_deleted", move |conn| {
            ensure_master_family_has_no_prepared_capture_repair(
                conn,
                &library,
                &master_record_name,
                "mark_master_family_soft_deleted",
            )?;
            let updated = conn
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                     WHERE library = ?2 AND (id = ?3 OR id IN ( \
                        SELECT asset_record_name FROM asset_master_mappings \
                        WHERE library = ?2 AND master_record_name = ?3 \
                     ))",
                    rusqlite::params![
                        deleted_at.map(|dt| dt.timestamp()),
                        &library,
                        &master_record_name
                    ],
                )
                .map_err(|e| StateError::query("mark_master_family_soft_deleted", e))?;
            Ok(updated)
        })
        .await
    }

    pub(crate) async fn resolve_master_family_source_deleted(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let master_record_name = master_record_name.to_owned();
        self.with_conn_mut("resolve_master_family_source_deleted", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("resolve_master_family_source_deleted::begin", e))?;
            ensure_master_family_has_no_prepared_capture_repair(
                &tx,
                &library,
                &master_record_name,
                "resolve_master_family_source_deleted",
            )?;
            let marked = tx
                .execute(
                    "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                     WHERE library = ?2 AND (id = ?3 OR id IN ( \
                        SELECT asset_record_name FROM asset_master_mappings \
                        WHERE library = ?2 AND master_record_name = ?3 \
                     ))",
                    rusqlite::params![
                        deleted_at.map(|dt| dt.timestamp()),
                        library,
                        master_record_name
                    ],
                )
                .map_err(|e| StateError::query("resolve_master_family_source_deleted::mark", e))?;
            tx.execute(
                "DELETE FROM asset_verifications \
                 WHERE library = ?1 AND (id = ?2 OR id IN ( \
                    SELECT asset_record_name FROM asset_master_mappings \
                    WHERE library = ?1 AND master_record_name = ?2 \
                 ))",
                rusqlite::params![&library, &master_record_name],
            )
            .map_err(|e| {
                StateError::query(
                    "resolve_master_family_source_deleted::clear_verification",
                    e,
                )
            })?;
            tx.commit().map_err(|e| {
                StateError::query("resolve_master_family_source_deleted::commit", e)
            })?;
            Ok(marked)
        })
        .await
    }

    pub(crate) async fn mark_hidden_at_source(
        &self,
        library: &str,
        asset_id: &str,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        self.with_conn("mark_hidden_at_source", move |conn| {
            let updated = conn
                .execute(
                    "UPDATE assets SET is_hidden = 1 WHERE library = ?1 AND id = ?2",
                    rusqlite::params![library, asset_id],
                )
                .map_err(|e| StateError::query("mark_hidden_at_source", e))?;
            Ok(updated)
        })
        .await
    }

    pub(crate) async fn record_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let ts = Utc::now().timestamp();
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn_mut("record_metadata_write_failure", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("record_metadata_write_failure::begin", e))?;
            tx.execute(
                "UPDATE assets SET metadata_write_failed_at = ?1 \
                 WHERE library = ?2 AND id = ?3 AND version_size = ?4",
                rusqlite::params![ts, &library, &asset_id, &version_size],
            )
            .map_err(|e| StateError::query("record_metadata_write_failure", e))?;
            tx.execute(
                "UPDATE asset_metadata_paths SET metadata_write_failed_at = ?1 \
                 WHERE library = ?2 AND id = ?3 AND version_size = ?4",
                rusqlite::params![ts, library, asset_id, version_size],
            )
            .map_err(|e| StateError::query("record_metadata_write_failure::paths", e))?;
            tx.commit()
                .map_err(|e| StateError::query("record_metadata_write_failure::commit", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn record_capture_repair_prepared(
        &self,
        pending: &PendingMetadataRewrite,
        output_checksum: &str,
        output_size: u64,
    ) -> Result<Option<CaptureRepairReceipt>, StateError> {
        if output_checksum.is_empty() {
            return Err(StateError::Invariant {
                operation: "record_capture_repair_prepared",
                detail: "capture-repair output checksum is empty".into(),
            });
        }
        let receipt =
            pending
                .capture_repair_receipt
                .as_ref()
                .ok_or_else(|| StateError::Invariant {
                    operation: "record_capture_repair_prepared",
                    detail: "the selected row has no capture-repair receipt".into(),
                });
        let receipt = receipt?;
        let metadata_hash = receipt.metadata_hash();
        let (selected_output_checksum, selected_output_size) = match receipt {
            CaptureRepairReceipt::Pending { .. } => (None, None),
            CaptureRepairReceipt::Prepared {
                output_checksum,
                output_size,
                ..
            } => (
                Some(output_checksum.to_owned()),
                Some(
                    i64::try_from(*output_size).map_err(|_error| StateError::Invariant {
                        operation: "record_capture_repair_prepared",
                        detail: "selected capture-repair output size exceeds SQLite INTEGER".into(),
                    })?,
                ),
            ),
        };
        let Some(input_checksum) = pending.asset.local_checksum.as_deref() else {
            return Ok(None);
        };
        let library = pending.asset.library.to_string();
        let asset_id = pending.asset.id.to_string();
        let version_size = pending.asset.version_size.as_str().to_owned();
        let metadata_hash = metadata_hash.to_owned();
        let input_checksum = input_checksum.to_owned();
        let output_checksum = output_checksum.to_owned();
        let output_size_sql =
            i64::try_from(output_size).map_err(|_error| StateError::Invariant {
                operation: "record_capture_repair_prepared",
                detail: "capture-repair output size exceeds SQLite INTEGER".into(),
            })?;
        let Some(path) = pending.asset.local_path.clone() else {
            return Ok(None);
        };
        self.with_conn_mut("record_capture_repair_prepared", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("record_capture_repair_prepared::begin", e))?;
            let target = metadata_rewrite_target(&tx, &library, &asset_id, &version_size, &path)?;
            let (table, evidence) = target.sql();

            let updated = tx
                .execute(
                    &format!(
                        "UPDATE {table} SET capture_repair_output_checksum = ?8, \
                        capture_repair_output_size = ?9 \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                       AND {evidence} AND local_checksum IS ?5 \
                       AND capture_repair_metadata_hash IS ?4 \
                       AND capture_repair_output_checksum IS ?6 \
                       AND capture_repair_output_size IS ?7 AND local_path IS ?10"
                    ),
                    rusqlite::params![
                        &library,
                        &asset_id,
                        &version_size,
                        metadata_hash,
                        input_checksum,
                        selected_output_checksum,
                        selected_output_size,
                        output_checksum,
                        output_size_sql,
                        path.to_string_lossy()
                    ],
                )
                .map_err(|e| StateError::query("record_capture_repair_prepared", e))?;
            if updated > 0 && target == MetadataRewriteTarget::Catalogue {
                record_metadata_path(
                    &tx,
                    &library,
                    &asset_id,
                    &version_size,
                    MetadataPathWrite::Rewritten,
                )?;
            }
            tx.commit()
                .map_err(|e| StateError::query("record_capture_repair_prepared::commit", e))?;
            Ok((updated > 0).then_some(CaptureRepairReceipt::Prepared {
                metadata_hash,
                output_checksum,
                output_size,
            }))
        })
        .await
    }

    pub(crate) async fn refresh_downloaded_asset_metadata(
        &self,
        library: &str,
        asset_id: &str,
        metadata: (&AssetMetadata, DateTime<Utc>, Option<DateTime<Utc>>),
        mark_for_rewrite: bool,
        mark_capture_repair: bool,
        capture_revision: i64,
    ) -> Result<usize, StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let (metadata, created_at, added_at) = metadata;
        let created_at = encode_asset_date(created_at);
        let added_at = added_at.map(encode_asset_date);
        let mut metadata = metadata.clone();
        if metadata.metadata_hash.is_none() {
            metadata.refresh_hash();
        }
        let rewrite_at = Utc::now().timestamp();
        self.with_conn_mut("refresh_downloaded_asset_metadata", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("refresh_downloaded_asset_metadata::begin", e))?;
            let source = metadata_rewrite_source_sql();
            let updated = tx
                .execute(
                    &format!(r"
                    UPDATE assets SET
                        source = COALESCE(?1, source),
                        is_favorite = ?2,
                        rating = ?3,
                        latitude = ?4,
                        longitude = ?5,
                        altitude = ?6,
                        orientation = ?7,
                        duration_secs = ?8,
                        timezone_offset = ?9,
                        width = ?10,
                        height = ?11,
                        title = ?12,
                        keywords = ?13,
                        description = ?14,
                        media_subtype = ?15,
                        burst_id = ?16,
                        is_hidden = ?17,
                        is_archived = ?18,
                        modified_at = ?19,
                        is_deleted = ?20,
                        deleted_at = ?21,
                        provider_data = ?22,
                        metadata_hash = ?23,
                        created_at = ?29,
                        added_at = ?30,
                        capture_repair_output_checksum =
                            CASE
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum <> ''
                                     AND capture_repair_output_size >= 0
                                    THEN capture_repair_output_checksum
                                WHEN capture_repair_metadata_hash IS NULL
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                ELSE capture_repair_output_checksum
                            END,
                        capture_repair_output_size =
                            CASE
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum <> ''
                                     AND capture_repair_output_size >= 0
                                    THEN capture_repair_output_size
                                WHEN capture_repair_metadata_hash IS NULL
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN NULL
                                ELSE capture_repair_output_size
                            END,
                        capture_repair_metadata_hash =
                            CASE
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum <> ''
                                     AND capture_repair_output_size >= 0
                                    THEN capture_repair_metadata_hash
                                WHEN capture_repair_metadata_hash <> ''
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN ?23
                                WHEN capture_repair_metadata_hash IS NULL
                                     AND capture_repair_output_checksum IS NULL
                                     AND capture_repair_output_size IS NULL
                                    THEN CASE WHEN ?25 = 1 THEN ?23 ELSE NULL END
                                ELSE capture_repair_metadata_hash
                            END,
                        metadata_write_failed_at =
                            CASE WHEN ?24 = 1 THEN ?26 ELSE metadata_write_failed_at END
                    WHERE library = ?27 AND id = ?28
                      AND status = 'downloaded' AND is_deleted = 0
                      AND NOT EXISTS (
                          SELECT 1 FROM ({source}) AS blocked
                          WHERE blocked.library = ?27 AND blocked.id = ?28
                            AND blocked.status = 'downloaded' AND blocked.is_deleted = 0
                            AND blocked.capture_repair_metadata_hash IS NOT NULL
                            AND blocked.capture_repair_metadata_hash <> ''
                            AND blocked.capture_repair_output_checksum IS NOT NULL
                            AND blocked.capture_repair_output_checksum <> ''
                            AND blocked.capture_repair_output_size IS NOT NULL
                            AND blocked.capture_repair_output_size >= 0
                            AND (blocked.capture_repair_metadata_hash IS NOT ?23
                                 OR blocked.created_at IS NOT ?29)
                      )
                    "),
                    rusqlite::params![
                        metadata.source.as_deref(),
                        i64::from(metadata.is_favorite),
                        metadata.rating.map(i64::from),
                        metadata.latitude,
                        metadata.longitude,
                        metadata.altitude,
                        metadata.orientation.map(i64::from),
                        metadata.duration_secs,
                        metadata.timezone_offset.map(i64::from),
                        metadata.width.map(i64::from),
                        metadata.height.map(i64::from),
                        metadata.title.as_deref(),
                        metadata.keywords.as_deref(),
                        metadata.description.as_deref(),
                        metadata.media_subtype.as_deref(),
                        metadata.burst_id.as_deref(),
                        i64::from(metadata.is_hidden),
                        i64::from(metadata.is_archived),
                        metadata.modified_at.map(|dt| dt.timestamp()),
                        i64::from(metadata.is_deleted),
                        metadata.deleted_at.map(|dt| dt.timestamp()),
                        metadata.provider_data.as_deref(),
                        metadata.metadata_hash.as_deref(),
                        i64::from(mark_for_rewrite),
                        i64::from(mark_capture_repair),
                        rewrite_at,
                        library,
                        asset_id,
                        created_at,
                        added_at,
                    ],
                )
                .map_err(|e| StateError::query("refresh_downloaded_asset_metadata", e))?;
            let durable = if updated > 0 {
                updated
            } else {
                ensure_asset_has_no_prepared_capture_repair(
                    &tx,
                    &library,
                    &asset_id,
                    "refresh_downloaded_asset_metadata",
                )?;
                // A row can leave `downloaded` mid-cycle when the provider
                // publishes new media for the same asset, which stores the
                // incoming metadata as it resets the row for re-download. The
                // metadata is durable, so report the match rather than a lost
                // write. A missing or still-stale row stays a failure.
                let durable: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM assets \
                         WHERE library = ?1 AND id = ?2 AND is_deleted = 0 \
                           AND metadata_hash IS NOT NULL AND metadata_hash = ?3 \
                           AND created_at IS ?4 AND added_at IS ?5",
                        rusqlite::params![
                            library,
                            asset_id,
                            metadata.metadata_hash.as_deref(),
                            created_at,
                            added_at
                        ],
                        |row| row.get(0),
                    )
                    .map_err(|e| StateError::query("refresh_downloaded_asset_metadata", e))?;
                usize::try_from(durable).unwrap_or(0)
            };
            if durable > 0 {
                tx.execute(
                    "UPDATE asset_metadata_paths SET \
                        metadata_write_failed_at = CASE WHEN ?3 = 1 THEN ?5 ELSE metadata_write_failed_at END, \
                        capture_repair_metadata_hash = CASE WHEN ?4 = 1 AND ( \
                            (capture_repair_metadata_hash IS NULL AND capture_repair_output_checksum IS NULL AND capture_repair_output_size IS NULL) \
                            OR (capture_repair_metadata_hash <> '' AND capture_repair_output_checksum IS NULL AND capture_repair_output_size IS NULL)) \
                            THEN ?6 ELSE capture_repair_metadata_hash END \
                     WHERE library = ?1 AND id = ?2 AND EXISTS ( \
                         SELECT 1 FROM assets a WHERE a.library = ?1 AND a.id = ?2 \
                           AND a.version_size = asset_metadata_paths.version_size \
                           AND a.checksum = asset_metadata_paths.provider_checksum \
                           AND a.status = 'downloaded' AND a.is_deleted = 0)",
                    rusqlite::params![&library, &asset_id, i64::from(mark_for_rewrite), i64::from(mark_capture_repair), rewrite_at, metadata.metadata_hash.as_deref()],
                ).map_err(|e| StateError::query("refresh_downloaded_asset_metadata::paths", e))?;
                record_metadata_capture_revision(
                    &tx,
                    &library,
                    &asset_id,
                    capture_revision,
                    rewrite_at,
                )?;
            }
            tx.commit()
                .map_err(|e| StateError::query("refresh_downloaded_asset_metadata::commit", e))?;
            Ok(durable)
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn clear_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let asset_id = asset_id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn("clear_metadata_write_failure", move |conn| {
            conn.execute(
                "UPDATE assets SET metadata_write_failed_at = NULL \
                 WHERE library = ?1 AND id = ?2 AND version_size = ?3",
                rusqlite::params![library, asset_id, version_size],
            )
            .map_err(|e| StateError::query("clear_metadata_write_failure", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        self.with_conn("get_downloaded_metadata_hashes", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT library, id, version_size, metadata_hash FROM assets \
                     WHERE status = 'downloaded' AND metadata_hash IS NOT NULL",
                )
                .map_err(|e| StateError::query("get_downloaded_metadata_hashes", e))?;
            let mut hashes: HashMap<(String, String, String), String> = HashMap::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        (
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ),
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| StateError::query("get_downloaded_metadata_hashes", e))?;
            for row in rows {
                let (key, val) =
                    row.map_err(|e| StateError::query("get_downloaded_metadata_hashes", e))?;
                hashes.insert(key, val);
            }
            Ok(hashes)
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn get_pending_metadata_rewrites(
        &self,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.get_pending_metadata_rewrites_page(None, 0, limit)
            .await
    }

    pub(crate) async fn get_pending_metadata_rewrites_page(
        &self,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError> {
        Ok(self
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                library_scope,
                offset,
                limit,
            )
            .await?
            .into_iter()
            .map(|pending| pending.asset)
            .collect())
    }

    pub(crate) async fn get_pending_metadata_rewrites_page_for_queue(
        &self,
        queue: MetadataRewriteQueue,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PendingMetadataRewrite>, StateError> {
        let libraries = library_scope.map(unique_sorted_strings);
        if libraries.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        self.with_conn("get_pending_metadata_rewrites_for_queue", move |conn| {
            query_pending_metadata_rewrites(conn, queue, libraries.as_deref(), offset, limit)
        })
        .await
    }

    pub(crate) async fn finish_metadata_rewrite(
        &self,
        pending: &PendingMetadataRewrite,
        selected_queue: MetadataRewriteQueue,
        local_checksum: Option<&str>,
        pre_rewrite_checksum: Option<&str>,
        completion: MetadataRewriteCompletion,
    ) -> Result<bool, StateError> {
        let metadata_hash = pending
            .asset
            .metadata
            .metadata_hash
            .clone()
            .ok_or_else(|| StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "the selected row has no metadata hash".into(),
            })?;
        if pre_rewrite_checksum.is_some()
            && pre_rewrite_checksum != pending.asset.local_checksum.as_deref()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "pre-rewrite checksum does not match the selected input".into(),
            });
        }
        let capture_receipt = pending.capture_repair_receipt.as_ref();
        if (selected_queue == MetadataRewriteQueue::CaptureRepair
            || completion.clears(MetadataRewriteQueue::CaptureRepair))
            && capture_receipt.is_none()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "capture-repair completion requires a typed receipt".into(),
            });
        }
        if let Some(CaptureRepairReceipt::Pending { .. }) = capture_receipt
            && completion.clears(MetadataRewriteQueue::CaptureRepair)
            && local_checksum != pending.asset.local_checksum.as_deref()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "capture-repair bytes changed without a prepared receipt".into(),
            });
        }
        if let Some(CaptureRepairReceipt::Prepared {
            output_checksum, ..
        }) = capture_receipt
            && completion.clears(MetadataRewriteQueue::CaptureRepair)
            && local_checksum != Some(output_checksum.as_str())
            && local_checksum != pending.asset.local_checksum.as_deref()
        {
            return Err(StateError::Invariant {
                operation: "finish_metadata_rewrite",
                detail: "capture-repair output does not match its prepared receipt".into(),
            });
        }

        let library = pending.asset.library.to_string();
        let asset_id = pending.asset.id.to_string();
        let version_size = pending.asset.version_size.as_str().to_owned();
        let input_checksum = pending.asset.local_checksum.clone();
        let local_checksum = local_checksum.map(str::to_owned);
        let pre_rewrite_checksum = pre_rewrite_checksum.map(str::to_owned);
        let capture_metadata_hash = capture_receipt.map(CaptureRepairReceipt::metadata_hash);
        let capture_output_checksum = capture_receipt.and_then(|receipt| match receipt {
            CaptureRepairReceipt::Pending { .. } => None,
            CaptureRepairReceipt::Prepared {
                output_checksum, ..
            } => Some(output_checksum.as_str()),
        });
        let capture_output_size = match capture_receipt {
            None | Some(CaptureRepairReceipt::Pending { .. }) => None,
            Some(CaptureRepairReceipt::Prepared { output_size, .. }) => Some(
                i64::try_from(*output_size).map_err(|_error| StateError::Invariant {
                    operation: "finish_metadata_rewrite",
                    detail: "capture-repair output size exceeds SQLite INTEGER".into(),
                })?,
            ),
        };
        let capture_metadata_hash = capture_metadata_hash.map(str::to_owned);
        let capture_output_checksum = capture_output_checksum.map(str::to_owned);
        let selected_capture = i64::from(selected_queue == MetadataRewriteQueue::CaptureRepair);
        let clear_ordinary = i64::from(completion.clears(MetadataRewriteQueue::Ordinary));
        let clear_capture = i64::from(completion.clears(MetadataRewriteQueue::CaptureRepair));
        let Some(path) = pending.asset.local_path.clone() else {
            return Ok(false);
        };
        self.with_conn_mut("finish_metadata_rewrite", move |conn| {
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("finish_metadata_rewrite::begin", e))?;
            let target = metadata_rewrite_target(&tx, &library, &asset_id, &version_size, &path)?;
            let (table, evidence) = target.sql();

            let updated = tx
                .execute(
                    &format!(
                        "UPDATE {table} SET \
                        local_checksum = ?6, \
                        download_checksum = COALESCE(download_checksum, ?7), \
                        metadata_write_failed_at = CASE WHEN ?8 = 1 \
                            THEN NULL ELSE metadata_write_failed_at END, \
                        capture_repair_metadata_hash = CASE \
                            WHEN ?9 = 1 THEN NULL \
                            ELSE capture_repair_metadata_hash END, \
                        capture_repair_output_checksum = CASE \
                            WHEN ?9 = 1 OR local_checksum IS NOT ?6 \
                                THEN NULL ELSE capture_repair_output_checksum END, \
                        capture_repair_output_size = CASE \
                            WHEN ?9 = 1 OR local_checksum IS NOT ?6 \
                                THEN NULL ELSE capture_repair_output_size END \
                     WHERE library = ?1 AND id = ?2 AND version_size = ?3 \
                       AND {evidence} AND local_checksum IS ?5 \
                       AND ((?10 = 0 AND metadata_write_failed_at IS NOT NULL) \
                         OR (?10 = 1 \
                           AND capture_repair_metadata_hash IS ?11 \
                           AND capture_repair_output_checksum IS ?12 \
                           AND capture_repair_output_size IS ?13)) \
                       AND (?9 = 0 OR ( \
                           capture_repair_metadata_hash IS ?11 \
                           AND capture_repair_output_checksum IS ?12 \
                           AND capture_repair_output_size IS ?13)) AND local_path IS ?14"
                    ),
                    rusqlite::params![
                        &library,
                        &asset_id,
                        &version_size,
                        metadata_hash,
                        input_checksum,
                        local_checksum,
                        pre_rewrite_checksum,
                        clear_ordinary,
                        clear_capture,
                        selected_capture,
                        capture_metadata_hash,
                        capture_output_checksum,
                        capture_output_size,
                        path.to_string_lossy()
                    ],
                )
                .map_err(|e| StateError::query("finish_metadata_rewrite", e))?;
            if updated > 0 && target == MetadataRewriteTarget::Catalogue {
                record_metadata_path(
                    &tx,
                    &library,
                    &asset_id,
                    &version_size,
                    MetadataPathWrite::Rewritten,
                )?;
            }
            tx.commit()
                .map_err(|e| StateError::query("finish_metadata_rewrite::commit", e))?;
            Ok(updated > 0 && completion.clears(selected_queue))
        })
        .await
    }

    pub(crate) async fn get_metadata_retry_markers(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        self.with_conn("get_metadata_retry_markers", move |conn| {
            let source = metadata_rewrite_source_sql();
            let mut stmt = conn
                .prepare(&format!("SELECT library, id, version_size FROM ({source}) WHERE metadata_write_failed_at IS NOT NULL"))
                .map_err(|e| StateError::query("get_metadata_retry_markers", e))?;
            let mut markers: HashSet<(String, String, String)> = HashSet::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|e| StateError::query("get_metadata_retry_markers", e))?;
            for row in rows {
                let key = row.map_err(|e| StateError::query("get_metadata_retry_markers", e))?;
                markers.insert(key);
            }
            Ok(markers)
        })
        .await
    }

    pub(crate) async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError> {
        self.with_conn("has_downloaded_without_metadata_hash", move |conn| {
            let exists: i64 = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM assets WHERE status = 'downloaded' \
                     AND is_deleted = 0 AND metadata_hash IS NULL)",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("has_downloaded_without_metadata_hash", e))?;
            Ok(exists != 0)
        })
        .await
    }

    pub(crate) async fn begin_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        let library = library.to_owned();
        self.with_conn_mut("begin_metadata_capture_revision", move |conn| {
            let tx = conn.transaction().map_err(|e| {
                StateError::query("begin_metadata_capture_revision::transaction", e)
            })?;
            let remaining = metadata_capture_remaining(&tx, &library, target_revision)?;
            let existing = tx
                .query_row(
                    "SELECT active_revision, pending_revision, processed_assets, \
                            failed_assets, last_error \
                     FROM metadata_capture_state WHERE library = ?1",
                    [&library],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| StateError::query("begin_metadata_capture_revision::read", e))?;
            let (mut active, previous_pending, mut processed, mut failed, mut last_error) =
                existing.unwrap_or((0, None, 0, 0, None));
            let pending = if remaining == 0 {
                active = active.max(target_revision);
                None
            } else {
                if previous_pending != Some(target_revision) {
                    processed = 0;
                    failed = 0;
                    last_error = None;
                }
                Some(target_revision)
            };
            let now = Utc::now().timestamp();
            tx.execute(
                "INSERT INTO metadata_capture_state \
                    (library, active_revision, pending_revision, processed_assets, \
                     failed_assets, last_error, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(library) DO UPDATE SET \
                    active_revision = excluded.active_revision, \
                    pending_revision = excluded.pending_revision, \
                    processed_assets = excluded.processed_assets, \
                    failed_assets = excluded.failed_assets, \
                    last_error = excluded.last_error, updated_at = excluded.updated_at",
                rusqlite::params![library, active, pending, processed, failed, last_error, now],
            )
            .map_err(|e| StateError::query("begin_metadata_capture_revision::write", e))?;
            tx.commit()
                .map_err(|e| StateError::query("begin_metadata_capture_revision::commit", e))?;
            Ok(MetadataCaptureStatus {
                library,
                active_revision: active,
                pending_revision: pending,
                processed_assets: u64::try_from(processed).unwrap_or(0),
                failed_assets: u64::try_from(failed).unwrap_or(0),
                remaining_assets: remaining,
                last_error,
            })
        })
        .await
    }

    pub(crate) async fn get_metadata_capture_candidates(
        &self,
        library: &str,
        target_revision: i64,
        limit: usize,
    ) -> Result<Vec<MetadataCaptureCandidate>, StateError> {
        let library = library.to_owned();
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with_conn("get_metadata_capture_candidates", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    r"
                    WITH stale_assets AS (
                        SELECT assets.library, assets.id
                        FROM assets
                        LEFT JOIN asset_metadata_capture_revisions AS revisions
                          ON revisions.library = assets.library
                         AND revisions.asset_id = assets.id
                        WHERE assets.library = ?1
                          AND assets.status = 'downloaded'
                          AND assets.is_deleted = 0
                          AND (revisions.revision IS NULL OR revisions.revision < ?2)
                        GROUP BY assets.library, assets.id
                        ORDER BY assets.id
                        LIMIT ?3
                    )
                    SELECT stale_assets.library, stale_assets.id,
                           COALESCE(mapping.master_record_name,
                                    owner.master_record_name,
                                    stale_assets.id),
                           COALESCE(mapping.asset_record_name,
                                    owner.asset_record_name),
                           assets.version_size, assets.checksum, assets.size_bytes
                    FROM stale_assets
                    JOIN assets
                      ON assets.library = stale_assets.library
                     AND assets.id = stale_assets.id
                     AND assets.status = 'downloaded'
                     AND assets.is_deleted = 0
                    LEFT JOIN asset_master_mappings AS mapping
                      ON mapping.library = stale_assets.library
                     AND mapping.asset_record_name = stale_assets.id
                    LEFT JOIN legacy_master_state_owners AS owner
                      ON owner.library = stale_assets.library
                     AND owner.master_record_name = stale_assets.id
                    ORDER BY stale_assets.id, assets.version_size
                    ",
                )
                .map_err(|e| StateError::query("get_metadata_capture_candidates::prepare", e))?;
            let rows = stmt
                .query_map(rusqlite::params![library, target_revision, limit], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                })
                .map_err(|e| StateError::query("get_metadata_capture_candidates::query", e))?;
            let mut candidates = Vec::<MetadataCaptureCandidate>::new();
            for row in rows {
                let (library, asset_id, master, asset, version, checksum, size) =
                    row.map_err(|e| StateError::query("get_metadata_capture_candidates::row", e))?;
                let version_size =
                    VersionSizeKey::from_str(&version).ok_or_else(|| StateError::Invariant {
                        operation: "get_metadata_capture_candidates",
                        detail: format!("unknown durable version_size {version:?}"),
                    })?;
                let evidence = MetadataCaptureVersionEvidence {
                    version_size,
                    checksum,
                    size_bytes: u64::try_from(size).unwrap_or(0),
                };
                if let Some(candidate) = candidates.last_mut()
                    && candidate.asset_id == asset_id
                {
                    candidate.versions.push(evidence);
                } else {
                    candidates.push(MetadataCaptureCandidate {
                        library,
                        asset_id,
                        master_record_name: master,
                        asset_record_name: asset,
                        versions: vec![evidence],
                    });
                }
            }
            Ok(candidates)
        })
        .await
    }

    pub(crate) async fn record_metadata_capture_failure(
        &self,
        library: &str,
        target_revision: i64,
        error: &str,
    ) -> Result<(), StateError> {
        let library = library.to_owned();
        let error = error.to_owned();
        self.with_conn("record_metadata_capture_failure", move |conn| {
            conn.execute(
                "UPDATE metadata_capture_state SET failed_assets = failed_assets + 1, \
                    last_error = ?1, updated_at = ?2 \
                 WHERE library = ?3 AND pending_revision = ?4",
                rusqlite::params![error, Utc::now().timestamp(), library, target_revision],
            )
            .map_err(|e| StateError::query("record_metadata_capture_failure", e))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn complete_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        let library = library.to_owned();
        self.with_conn_mut("complete_metadata_capture_revision", move |conn| {
            let tx = conn.transaction().map_err(|e| {
                StateError::query("complete_metadata_capture_revision::transaction", e)
            })?;
            let remaining = metadata_capture_remaining(&tx, &library, target_revision)?;
            if remaining == 0 {
                tx.execute(
                    "UPDATE metadata_capture_state SET active_revision = MAX(active_revision, ?1), \
                        pending_revision = NULL, failed_assets = 0, last_error = NULL, \
                        updated_at = ?2 \
                     WHERE library = ?3",
                    rusqlite::params![target_revision, Utc::now().timestamp(), library],
                )
                .map_err(|e| StateError::query("complete_metadata_capture_revision::write", e))?;
            }
            let status = metadata_capture_status(&tx, &library, target_revision, remaining)?;
            tx.commit()
                .map_err(|e| StateError::query("complete_metadata_capture_revision::commit", e))?;
            Ok(status)
        })
        .await
    }

    pub(crate) async fn has_metadata_capture_work(
        &self,
        libraries: &[&str],
        target_revision: i64,
    ) -> Result<bool, StateError> {
        if libraries.is_empty() {
            return Ok(false);
        }
        let libraries = unique_sorted_strings(libraries);
        self.with_conn("has_metadata_capture_work", move |conn| {
            for library in libraries {
                if metadata_capture_remaining(conn, &library, target_revision)? > 0 {
                    return Ok(true);
                }
                let pending: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM metadata_capture_state \
                         WHERE library = ?1 AND pending_revision = ?2)",
                        rusqlite::params![library, target_revision],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|value| value != 0)
                    .map_err(|e| StateError::query("has_metadata_capture_work", e))?;
                if pending {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .await
    }
}

fn metadata_capture_remaining(
    conn: &Connection,
    library: &str,
    target_revision: i64,
) -> Result<u64, StateError> {
    conn.query_row(
        "SELECT COUNT(*) FROM ( \
             SELECT assets.id FROM assets \
             LEFT JOIN asset_metadata_capture_revisions AS revisions \
               ON revisions.library = assets.library AND revisions.asset_id = assets.id \
             WHERE assets.library = ?1 AND assets.status = 'downloaded' \
               AND assets.is_deleted = 0 \
               AND (revisions.revision IS NULL OR revisions.revision < ?2) \
             GROUP BY assets.id \
         )",
        rusqlite::params![library, target_revision],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| u64::try_from(count).unwrap_or(0))
    .map_err(|e| StateError::query("metadata_capture_remaining", e))
}

fn metadata_capture_status(
    conn: &Connection,
    library: &str,
    target_revision: i64,
    remaining_assets: u64,
) -> Result<MetadataCaptureStatus, StateError> {
    let row = conn
        .query_row(
            "SELECT active_revision, pending_revision, processed_assets, \
                    failed_assets, last_error \
             FROM metadata_capture_state WHERE library = ?1",
            [library],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|e| StateError::query("metadata_capture_status", e))?;
    let (active_revision, pending_revision, processed, failed, last_error) = row.unwrap_or({
        if remaining_assets > 0 {
            (0, Some(target_revision), 0, 0, None)
        } else {
            (target_revision, None, 0, 0, None)
        }
    });
    Ok(MetadataCaptureStatus {
        library: library.to_owned(),
        active_revision,
        pending_revision,
        processed_assets: u64::try_from(processed).unwrap_or(0),
        failed_assets: u64::try_from(failed).unwrap_or(0),
        remaining_assets,
        last_error,
    })
}

#[async_trait]
impl TempFileOwnershipStore for SqliteStateDb {
    async fn claim_temp_file(&self, path: &Path) -> Result<(), StateError> {
        SqliteStateDb::claim_temp_file(self, path).await
    }

    async fn get_owned_temp_files_before(
        &self,
        claimed_before: i64,
    ) -> Result<Vec<OwnedTempFile>, StateError> {
        SqliteStateDb::get_owned_temp_files_before(self, claimed_before).await
    }

    async fn retire_temp_files(&self, paths: &[PathBuf]) -> Result<u64, StateError> {
        SqliteStateDb::retire_temp_files(self, paths).await
    }
}

#[async_trait]
impl DownloadStateStore for SqliteStateDb {
    #[cfg(test)]
    async fn should_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError> {
        SqliteStateDb::should_download(self, library, id, version_size, checksum, local_path).await
    }

    async fn upsert_seen(&self, record: &AssetRecord) -> Result<(), StateError> {
        SqliteStateDb::upsert_seen(self, record).await
    }

    async fn mark_downloaded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_downloaded(
            self,
            library,
            id,
            version_size,
            local_path,
            local_checksum,
            download_checksum,
        )
        .await
    }

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
        SqliteStateDb::mark_downloaded_with_capture_repair(
            self,
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
        self.mark_downloaded_with_checksums(
            library,
            id,
            version_size,
            local_path,
            DownloadChecksums {
                local: local_checksum,
                downloaded: download_checksum,
                source: download_checksum,
            },
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
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_failed(self, library, id, version_size, error).await
    }

    async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
        <SqliteStateDb as ReportStateStore>::get_pending_page(self, 0, u32::MAX).await
    }

    async fn get_policy_excluded_ids_for_revalidation(
        &self,
        library: &str,
    ) -> Result<Vec<String>, StateError> {
        SqliteStateDb::get_policy_excluded_ids_for_revalidation(self, library).await
    }

    async fn reset_failed(&self) -> Result<u64, StateError> {
        SqliteStateDb::reset_failed(self).await
    }

    async fn prepare_for_retry(
        &self,
        library: Option<&str>,
        error_retention: RetryErrorRetention,
    ) -> Result<(u64, u64, u64), StateError> {
        SqliteStateDb::prepare_for_retry(self, library, error_retention).await
    }

    async fn prune_source_deleted_retries(&self, library: Option<&str>) -> Result<u64, StateError> {
        SqliteStateDb::prune_source_deleted_retries(self, library).await
    }

    async fn promote_pending_to_failed(&self, seen_since: i64) -> Result<u64, StateError> {
        SqliteStateDb::promote_pending_to_failed(self, seen_since).await
    }

    async fn prune_stale_pending_not_seen_since(
        &self,
        library: &str,
        seen_since: i64,
    ) -> Result<u64, StateError> {
        SqliteStateDb::prune_stale_pending_not_seen_since(self, library, seen_since).await
    }

    async fn prune_pending_asset_versions(
        &self,
        library: &str,
        asset_versions: &[(String, String)],
    ) -> Result<u64, StateError> {
        SqliteStateDb::prune_pending_asset_versions(self, library, asset_versions).await
    }

    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_downloaded_ids(self).await
    }

    async fn get_soft_deleted_downloaded_ids(
        &self,
    ) -> Result<HashSet<(String, String)>, StateError> {
        SqliteStateDb::get_soft_deleted_downloaded_ids(self).await
    }

    async fn get_all_known_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
        SqliteStateDb::get_all_known_ids(self).await
    }

    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        SqliteStateDb::get_downloaded_checksums(self).await
    }

    async fn get_downloaded_local_paths(
        &self,
    ) -> Result<HashMap<(String, String, String), PathBuf>, StateError> {
        SqliteStateDb::get_downloaded_local_paths(self).await
    }

    async fn get_attempt_counts(&self) -> Result<HashMap<(String, String), u32>, StateError> {
        SqliteStateDb::get_attempt_counts(self).await
    }

    async fn touch_last_seen_many(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<(), StateError> {
        SqliteStateDb::touch_last_seen_many(self, library, asset_ids).await
    }

    async fn upsert_asset_master_mapping(
        &self,
        library: &str,
        asset_record_name: &str,
        master_record_name: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::upsert_asset_master_mapping(
            self,
            library,
            asset_record_name,
            master_record_name,
        )
        .await
    }

    async fn get_master_record_name_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
    ) -> Result<Option<String>, StateError> {
        SqliteStateDb::get_master_record_name_for_asset(self, library, asset_record_name).await
    }

    async fn get_asset_record_names_for_master(
        &self,
        library: &str,
        master_record_name: &str,
    ) -> Result<Vec<String>, StateError> {
        SqliteStateDb::get_asset_record_names_for_master(self, library, master_record_name).await
    }

    async fn get_asset_master_mappings(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_asset_master_mappings(self).await
    }

    async fn get_legacy_master_state_owners(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_legacy_master_state_owners(self).await
    }

    async fn claim_legacy_master_state_owner(
        &self,
        library: &str,
        master_record_name: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::claim_legacy_master_state_owner(
            self,
            library,
            master_record_name,
            asset_record_name,
        )
        .await
    }

    async fn set_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        state: AssetVerificationState,
        reason: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::set_asset_verification(self, library, id, version_size, state, reason).await
    }

    async fn clear_asset_verification(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::clear_asset_verification(self, library, id, version_size).await
    }

    async fn mark_policy_excluded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::mark_policy_excluded(self, library, id, version_size).await
    }

    async fn backfill_asset_master_mappings_from_album_memberships(
        &self,
    ) -> Result<u64, StateError> {
        SqliteStateDb::backfill_asset_master_mappings_from_album_memberships(self).await
    }

    async fn mark_soft_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_soft_deleted(self, library, asset_id, deleted_at)
            .await
            .map(|_| ())
    }

    async fn mark_soft_deleted_affected(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::mark_soft_deleted(self, library, asset_id, deleted_at).await
    }

    async fn resolve_source_deleted_affected(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::resolve_source_deleted(self, library, asset_id, deleted_at).await
    }

    async fn mark_master_family_soft_deleted_affected(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::mark_master_family_soft_deleted(
            self,
            library,
            master_record_name,
            deleted_at,
        )
        .await
    }

    async fn resolve_master_family_source_deleted_affected(
        &self,
        library: &str,
        master_record_name: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<usize, StateError> {
        SqliteStateDb::resolve_master_family_source_deleted(
            self,
            library,
            master_record_name,
            deleted_at,
        )
        .await
    }

    async fn mark_hidden_at_source(&self, library: &str, asset_id: &str) -> Result<(), StateError> {
        SqliteStateDb::mark_hidden_at_source(self, library, asset_id)
            .await
            .map(|_| ())
    }

    async fn mark_hidden_at_source_affected(
        &self,
        library: &str,
        asset_id: &str,
    ) -> Result<usize, StateError> {
        SqliteStateDb::mark_hidden_at_source(self, library, asset_id).await
    }
}

#[async_trait]
impl ImportStateStore for SqliteStateDb {
    async fn import_adopt(
        &self,
        record: &AssetRecord,
        local_path: &Path,
        local_checksum: &str,
        imported_size: u64,
        imported_mtime: Option<i64>,
    ) -> Result<(), StateError> {
        SqliteStateDb::import_adopt(
            self,
            record,
            local_path,
            local_checksum,
            imported_size,
            imported_mtime,
        )
        .await
    }

    async fn get_all_imported_records(
        &self,
        library: &str,
    ) -> Result<HashMap<(String, String), ImportedRecord>, StateError> {
        SqliteStateDb::get_all_imported_records(self, library).await
    }
}

#[async_trait]
impl ReportStateStore for SqliteStateDb {
    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        <SqliteStateDb as ReportStateStore>::get_failed_page(self, 0, u32::MAX).await
    }

    async fn get_failed_sample(&self, limit: u32) -> Result<(Vec<AssetRecord>, u64), StateError> {
        SqliteStateDb::get_failed_sample(self, limit).await
    }

    async fn get_failed_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_failed_page(self, offset, limit).await
    }

    async fn get_pending_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_pending_page(self, offset, limit).await
    }

    async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        SqliteStateDb::get_summary(self).await
    }

    async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_downloaded_page(self, offset, limit).await
    }

    async fn start_sync_run_at(&self, started_at: DateTime<Utc>) -> Result<i64, StateError> {
        SqliteStateDb::start_sync_run_at(self, started_at).await
    }

    async fn start_sync_run(&self) -> Result<i64, StateError> {
        SqliteStateDb::start_sync_run(self).await
    }

    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError> {
        SqliteStateDb::complete_sync_run(self, run_id, stats).await
    }

    async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
        SqliteStateDb::promote_orphaned_sync_runs(self).await
    }
}

#[async_trait]
impl SyncTokenStore for SqliteStateDb {
    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError> {
        SqliteStateDb::get_metadata(self, key).await
    }

    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
        SqliteStateDb::set_metadata(self, key, value).await
    }

    async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError> {
        SqliteStateDb::delete_metadata_by_prefix(self, prefix).await
    }

    async fn commit_checkpoint_transition(
        &self,
        transition: CheckpointTransition,
    ) -> Result<(), StateError> {
        SqliteStateDb::commit_checkpoint_transition(self, transition).await
    }

    async fn get_scoped_db_sync_token(
        &self,
        provider: &str,
        account: &str,
        shape_version: i64,
        scope_hash: &str,
    ) -> Result<Option<ScopedDbSyncToken>, StateError> {
        SqliteStateDb::get_scoped_db_sync_token(self, provider, account, shape_version, scope_hash)
            .await
    }

    async fn upsert_scoped_db_sync_token(
        &self,
        token: ScopedDbSyncToken,
    ) -> Result<(), StateError> {
        SqliteStateDb::upsert_scoped_db_sync_token(self, token).await
    }

    async fn delete_scoped_db_sync_tokens(&self) -> Result<u64, StateError> {
        SqliteStateDb::delete_scoped_db_sync_tokens(self).await
    }

    async fn begin_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        SqliteStateDb::begin_enum_progress(self, zone).await
    }

    async fn end_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        SqliteStateDb::end_enum_progress(self, zone).await
    }

    async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError> {
        SqliteStateDb::list_interrupted_enumerations(self).await
    }
}

#[async_trait]
impl DownloadContextStateStore for SqliteStateDb {
    async fn get_downloaded_file_records(&self) -> Result<Vec<DownloadedFileRecord>, StateError> {
        SqliteStateDb::get_downloaded_file_records(self).await
    }
}

#[async_trait]
impl MembershipStore for SqliteStateDb {
    async fn add_asset_album(
        &self,
        library: &str,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::add_asset_album(self, library, asset_id, album_name, source).await
    }

    async fn get_all_asset_albums(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        SqliteStateDb::get_all_asset_albums(self, library).await
    }

    async fn get_all_asset_people(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, StateError> {
        SqliteStateDb::get_all_asset_people(self, library).await
    }

    async fn get_asset_groupings(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<AssetGroupingRows, StateError> {
        SqliteStateDb::get_asset_groupings(self, library, asset_ids).await
    }

    async fn upsert_album_container(
        &self,
        library: &str,
        container_id: &str,
        album_name: &str,
        pass_kind: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::upsert_album_container(self, library, container_id, album_name, pass_kind)
            .await
    }

    async fn mark_album_container_deleted(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::mark_album_container_deleted(self, library, container_id).await
    }

    async fn start_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        enum_config_hash: Option<&str>,
    ) -> Result<i64, StateError> {
        SqliteStateDb::start_album_membership_snapshot(
            self,
            library,
            container_id,
            enum_config_hash,
        )
        .await
    }

    async fn add_album_membership_to_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::add_album_membership_to_snapshot(
            self,
            library,
            container_id,
            generation,
            asset_record_name,
            master_record_name,
            source,
        )
        .await
    }

    async fn upsert_album_membership_delta(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::upsert_album_membership_delta(
            self,
            library,
            container_id,
            asset_record_name,
            master_record_name,
            source,
        )
        .await
    }

    async fn mark_album_membership_deleted(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
    ) -> Result<bool, StateError> {
        SqliteStateDb::mark_album_membership_deleted(self, library, container_id, asset_record_name)
            .await
    }

    async fn complete_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
    ) -> Result<(), StateError> {
        SqliteStateDb::complete_album_membership_snapshot(self, library, container_id, generation)
            .await
    }

    async fn invalidate_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::invalidate_album_membership_snapshot(self, library, container_id).await
    }

    async fn selected_album_containers_have_complete_snapshots(
        &self,
        library: &str,
        container_ids: &[&str],
    ) -> Result<bool, StateError> {
        SqliteStateDb::selected_album_containers_have_complete_snapshots(
            self,
            library,
            container_ids,
        )
        .await
    }

    async fn get_live_selected_album_memberships_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
        selected_container_ids: &[&str],
    ) -> Result<Vec<AlbumMembershipRecord>, StateError> {
        SqliteStateDb::get_live_selected_album_memberships_for_asset(
            self,
            library,
            asset_record_name,
            selected_container_ids,
        )
        .await
    }
}

#[async_trait]
impl MetadataRewriteStore for SqliteStateDb {
    async fn record_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::record_metadata_write_failure(self, library, asset_id, version_size).await
    }

    async fn refresh_downloaded_asset_metadata(
        &self,
        library: &str,
        asset_id: &str,
        metadata: (&AssetMetadata, DateTime<Utc>, Option<DateTime<Utc>>),
        mark_for_rewrite: bool,
        mark_capture_repair: bool,
        capture_revision: i64,
    ) -> Result<usize, StateError> {
        SqliteStateDb::refresh_downloaded_asset_metadata(
            self,
            library,
            asset_id,
            metadata,
            mark_for_rewrite,
            mark_capture_repair,
            capture_revision,
        )
        .await
    }

    async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String, String), String>, StateError> {
        SqliteStateDb::get_downloaded_metadata_hashes(self).await
    }

    async fn get_metadata_retry_markers(
        &self,
    ) -> Result<HashSet<(String, String, String)>, StateError> {
        SqliteStateDb::get_metadata_retry_markers(self).await
    }

    async fn get_pending_metadata_rewrites_page(
        &self,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError> {
        SqliteStateDb::get_pending_metadata_rewrites_page(self, library_scope, offset, limit).await
    }

    async fn get_pending_metadata_rewrites_page_for_queue(
        &self,
        queue: MetadataRewriteQueue,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<PendingMetadataRewrite>, StateError> {
        SqliteStateDb::get_pending_metadata_rewrites_page_for_queue(
            self,
            queue,
            library_scope,
            offset,
            limit,
        )
        .await
    }

    async fn record_capture_repair_prepared(
        &self,
        pending: &PendingMetadataRewrite,
        output_checksum: &str,
        output_size: u64,
    ) -> Result<Option<CaptureRepairReceipt>, StateError> {
        SqliteStateDb::record_capture_repair_prepared(self, pending, output_checksum, output_size)
            .await
    }

    async fn finish_metadata_rewrite(
        &self,
        pending: &PendingMetadataRewrite,
        selected_queue: MetadataRewriteQueue,
        local_checksum: Option<&str>,
        pre_rewrite_checksum: Option<&str>,
        completion: MetadataRewriteCompletion,
    ) -> Result<bool, StateError> {
        SqliteStateDb::finish_metadata_rewrite(
            self,
            pending,
            selected_queue,
            local_checksum,
            pre_rewrite_checksum,
            completion,
        )
        .await
    }

    async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError> {
        SqliteStateDb::has_downloaded_without_metadata_hash(self).await
    }

    async fn begin_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        SqliteStateDb::begin_metadata_capture_revision(self, library, target_revision).await
    }

    async fn get_metadata_capture_candidates(
        &self,
        library: &str,
        target_revision: i64,
        limit: usize,
    ) -> Result<Vec<MetadataCaptureCandidate>, StateError> {
        SqliteStateDb::get_metadata_capture_candidates(self, library, target_revision, limit).await
    }

    async fn record_metadata_capture_failure(
        &self,
        library: &str,
        target_revision: i64,
        error: &str,
    ) -> Result<(), StateError> {
        SqliteStateDb::record_metadata_capture_failure(self, library, target_revision, error).await
    }

    async fn complete_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<MetadataCaptureStatus, StateError> {
        SqliteStateDb::complete_metadata_capture_revision(self, library, target_revision).await
    }

    async fn has_metadata_capture_work(
        &self,
        libraries: &[&str],
        target_revision: i64,
    ) -> Result<bool, StateError> {
        SqliteStateDb::has_metadata_capture_work(self, libraries, target_revision).await
    }
}

#[cfg(test)]
impl SqliteStateDb {
    pub(crate) fn fail_asset_album_writes_for_test(&self) {
        let conn = self.acquire_lock("test_fail_asset_album_writes").unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_asset_album_writes \
             BEFORE INSERT ON asset_albums \
             BEGIN SELECT RAISE(FAIL, 'simulated asset album write failure'); END;",
        )
        .unwrap();
    }

    pub(crate) fn allow_asset_album_writes_for_test(&self) {
        let conn = self.acquire_lock("test_allow_asset_album_writes").unwrap();
        conn.execute_batch("DROP TRIGGER fail_asset_album_writes")
            .unwrap();
    }

    pub(crate) fn fail_provider_metadata_refresh_for_test(&self) {
        let conn = self
            .acquire_lock("test_fail_provider_metadata_refresh")
            .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_provider_metadata_refresh \
             BEFORE UPDATE OF metadata_hash ON assets \
             BEGIN SELECT RAISE(FAIL, 'simulated provider metadata refresh failure'); END;",
        )
        .unwrap();
    }

    #[cfg(feature = "xmp")]
    pub(crate) fn fail_metadata_marker_clear_for_test(&self) {
        let conn = self
            .acquire_lock("test_fail_metadata_marker_clear")
            .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_metadata_marker_clear \
             BEFORE UPDATE OF metadata_write_failed_at ON assets \
             WHEN NEW.metadata_write_failed_at IS NULL \
             BEGIN SELECT RAISE(FAIL, 'simulated metadata marker clear failure'); END;",
        )
        .unwrap();
    }

    #[cfg(feature = "xmp")]
    pub(crate) fn fail_metadata_checksum_write_for_test(&self) {
        let conn = self
            .acquire_lock("test_fail_metadata_checksum_write")
            .unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_metadata_checksum_write \
             BEFORE UPDATE OF local_checksum ON assets \
             WHEN NEW.local_checksum IS NOT NULL \
             BEGIN SELECT RAISE(FAIL, 'simulated rewritten checksum write failure'); END;",
        )
        .unwrap();
    }

    #[cfg(feature = "xmp")]
    pub(crate) fn clear_local_checksum_for_test(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) {
        let conn = self.acquire_lock("test_clear_local_checksum").unwrap();
        conn.execute(
            "UPDATE assets SET local_checksum = NULL, \
                capture_repair_metadata_hash = NULL, \
                capture_repair_output_checksum = NULL, \
                capture_repair_output_size = NULL \
             WHERE library = ?1 AND id = ?2 AND version_size = ?3",
            rusqlite::params![library, asset_id, version_size],
        )
        .unwrap();
    }

    /// Overwrite `last_seen_at` for a specific asset in `PrimarySync`. Used
    /// by tests that need to simulate a pending row carried over from a
    /// prior sync. Callable from any test module in the crate so
    /// cross-module state tests (e.g. pipeline-level ghost-loop regression)
    /// don't have to reach for raw `rusqlite::Connection` plumbing.
    pub(crate) fn backdate_last_seen(&self, asset_id: &str, ts: i64) {
        self.backdate_last_seen_in(crate::icloud::photos::PRIMARY_ZONE_NAME, asset_id, ts);
    }

    pub(crate) fn backdate_last_seen_in(&self, library: &str, asset_id: &str, ts: i64) {
        let conn = self.acquire_lock("test_backdate_last_seen").unwrap();
        conn.execute(
            "UPDATE assets SET last_seen_at = ?1 WHERE library = ?2 AND id = ?3",
            rusqlite::params![ts, library, asset_id],
        )
        .unwrap();
    }

    pub(crate) fn clear_metadata_hash_for_test(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) {
        let conn = self.acquire_lock("test_clear_metadata_hash").unwrap();
        conn.execute(
            "UPDATE assets SET metadata_hash = NULL \
             WHERE library = ?1 AND id = ?2 AND version_size = ?3",
            rusqlite::params![library, asset_id, version_size],
        )
        .unwrap();
    }

    pub(crate) fn set_metadata_capture_revision_for_test(
        &self,
        library: &str,
        asset_id: &str,
        revision: i64,
    ) {
        let conn = self
            .acquire_lock("test_set_metadata_capture_revision")
            .unwrap();
        conn.execute(
            "INSERT INTO asset_metadata_capture_revisions \
                (library, asset_id, revision, updated_at) VALUES (?1, ?2, ?3, 0) \
             ON CONFLICT(library, asset_id) DO UPDATE SET revision = excluded.revision",
            rusqlite::params![library, asset_id, revision],
        )
        .unwrap();
    }
}

/// Column list for every `SELECT ... FROM assets` that feeds `row_to_asset_record`.
/// Keep this in sync with the indices read in `row_to_asset_record` and the
/// VALUES placeholder count in `upsert_seen`.
const ASSET_COLUMNS: &str = "id, version_size, checksum, filename, created_at, \
     added_at, size_bytes, media_type, status, downloaded_at, local_path, \
     last_seen_at, download_attempts, last_error, local_checksum, \
     source, is_favorite, rating, latitude, longitude, altitude, orientation, \
     duration_secs, timezone_offset, width, height, title, keywords, description, \
     media_subtype, burst_id, is_hidden, is_archived, modified_at, is_deleted, \
     deleted_at, provider_data, metadata_hash, library, download_checksum";

/// Total number of columns in `ASSET_COLUMNS`. Validated by a unit test that
/// asserts `row_to_asset_record` reads exactly this many indices (0..N).
const ASSET_COLUMN_COUNT: usize = 40;

fn metadata_rewrite_source_sql() -> String {
    let path_columns = ASSET_COLUMNS
        .split(',')
        .map(str::trim)
        .map(|column| {
            let owner = match column {
                "local_path" | "local_checksum" | "download_checksum" => "p",
                _ => "a",
            };
            format!("{owner}.{column}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT {ASSET_COLUMNS}, metadata_write_failed_at, capture_repair_metadata_hash, \
             capture_repair_output_checksum, capture_repair_output_size, \
             (SELECT p.source_checksum FROM asset_metadata_paths p \
              WHERE p.library = assets.library AND p.id = assets.id \
                AND p.version_size = assets.version_size AND p.local_path = assets.local_path \
                AND p.provider_checksum = assets.checksum) AS source_checksum FROM assets \
         UNION ALL SELECT {path_columns}, p.metadata_write_failed_at, p.capture_repair_metadata_hash, \
             p.capture_repair_output_checksum, p.capture_repair_output_size, p.source_checksum \
         FROM asset_metadata_paths p JOIN assets a \
           ON a.library = p.library AND a.id = p.id AND a.version_size = p.version_size \
         WHERE p.local_path IS NOT a.local_path AND p.provider_checksum = a.checksum"
    )
}

fn query_pending_metadata_rewrites(
    conn: &Connection,
    queue: MetadataRewriteQueue,
    libraries: Option<&[String]>,
    offset: usize,
    limit: usize,
) -> Result<Vec<PendingMetadataRewrite>, StateError> {
    let (queue_predicate, order) = match queue {
        MetadataRewriteQueue::Ordinary => (
            "metadata_write_failed_at IS NOT NULL",
            "metadata_write_failed_at ASC, library, id, version_size",
        ),
        MetadataRewriteQueue::CaptureRepair => (
            "(capture_repair_metadata_hash IS NOT NULL \
              OR capture_repair_output_checksum IS NOT NULL \
              OR capture_repair_output_size IS NOT NULL)",
            "library, id, version_size",
        ),
    };
    let scope = libraries
        .map(|libraries| format!(" AND library IN ({})", sqlite_placeholders(libraries.len())))
        .unwrap_or_default();
    let source = metadata_rewrite_source_sql();
    let sql = format!(
        "SELECT {ASSET_COLUMNS}, capture_repair_metadata_hash, \
            capture_repair_output_checksum, capture_repair_output_size, source_checksum \
         FROM ({source}) WHERE {queue_predicate} \
           AND status = 'downloaded' AND is_deleted = 0 AND local_path IS NOT NULL \
           {scope} ORDER BY {order}, local_path LIMIT ? OFFSET ?"
    );
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let mut params: Vec<&dyn rusqlite::ToSql> =
        Vec::with_capacity(libraries.map_or(2, |values| values.len() + 2));
    if let Some(libraries) = libraries {
        for library in libraries {
            params.push(library);
        }
    }
    params.push(&limit);
    params.push(&offset);
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| StateError::query("get_pending_metadata_rewrites_for_queue", e))?;
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params),
            row_to_pending_metadata_rewrite,
        )
        .map_err(|e| StateError::query("get_pending_metadata_rewrites_for_queue", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| StateError::query("get_pending_metadata_rewrites_for_queue", e))
}

fn row_to_pending_metadata_rewrite(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<PendingMetadataRewrite> {
    let metadata_hash: Option<String> = row.get(ASSET_COLUMN_COUNT)?;
    let output_checksum: Option<String> = row.get(ASSET_COLUMN_COUNT + 1)?;
    let output_size: Option<i64> = row.get(ASSET_COLUMN_COUNT + 2)?;
    let capture_repair_receipt = match (metadata_hash, output_checksum, output_size) {
        (None, None, None) => None,
        (Some(metadata_hash), None, None) if !metadata_hash.is_empty() => {
            Some(CaptureRepairReceipt::Pending { metadata_hash })
        }
        (Some(metadata_hash), Some(output_checksum), Some(output_size))
            if !metadata_hash.is_empty() && !output_checksum.is_empty() && output_size >= 0 =>
        {
            Some(CaptureRepairReceipt::Prepared {
                metadata_hash,
                output_checksum,
                output_size: u64::try_from(output_size).unwrap_or(0),
            })
        }
        state => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                ASSET_COLUMN_COUNT,
                rusqlite::types::Type::Null,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("malformed capture-repair receipt: {state:?}"),
                )
                .into(),
            ));
        }
    };
    Ok(PendingMetadataRewrite {
        asset: row_to_asset_record(row)?,
        capture_repair_receipt,
        source_checksum: row.get(ASSET_COLUMN_COUNT + 3)?,
    })
}

struct ManifestJoinedRow {
    asset: ManifestAssetRow,
    album_name: Option<String>,
}

fn manifest_joined_row_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ManifestJoinedRow> {
    let library: String = row.get(0)?;
    let asset_id: String = row.get(1)?;
    let version: String = row.get(2)?;
    let filename: String = row.get(3)?;
    let local_path: Option<String> = row.get(4)?;
    let checksum: String = row.get(5)?;
    let local_checksum: Option<String> = row.get(6)?;
    let download_checksum: Option<String> = row.get(7)?;
    let size_bytes: i64 = row.get(8)?;
    let created_at = decode_asset_date(row.get(9)?, 9)?;
    let added_at = row
        .get::<_, Option<f64>>(10)?
        .map(|value| decode_asset_date(value, 10))
        .transpose()?;
    let downloaded_at_ts: Option<i64> = row.get(11)?;
    let last_seen_at_ts: i64 = row.get(12)?;
    let media_type: String = row.get(13)?;
    let status: String = row.get(14)?;
    let album_name: Option<String> = row.get(15)?;

    Ok(ManifestJoinedRow {
        asset: ManifestAssetRow {
            library,
            asset_id,
            version,
            filename,
            local_path: local_path.map(PathBuf::from),
            checksum,
            local_checksum,
            download_checksum,
            size_bytes: u64::try_from(size_bytes).unwrap_or(0),
            created_at,
            added_at,
            downloaded_at: optional_ts_to_utc(downloaded_at_ts),
            last_seen_at: ts_to_utc(last_seen_at_ts),
            media_type,
            status,
            albums: Vec::new(),
        },
        album_name,
    })
}

fn ts_to_utc(ts: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .unwrap_or(DateTime::UNIX_EPOCH)
}

fn optional_ts_to_utc(ts: Option<i64>) -> Option<DateTime<Utc>> {
    ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single())
}

// Chrono's whole seconds fit exactly in f64; splitting before scaling preserves
// every millisecond even at the supported date limits (unlike timestamp_millis).
#[allow(
    clippy::cast_precision_loss,
    reason = "chrono seconds are within f64's exact integer range"
)]
fn encode_asset_date(date: DateTime<Utc>) -> f64 {
    date.timestamp() as f64 + f64::from(date.timestamp_subsec_millis()) / 1000.0
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "finite chrono-range seconds and rounded 0..=1000 milliseconds are checked before casting"
)]
fn decode_asset_date(value: f64, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    let seconds = value.floor();
    if value.is_finite()
        && seconds >= DateTime::<Utc>::MIN_UTC.timestamp() as f64
        && seconds <= DateTime::<Utc>::MAX_UTC.timestamp() as f64
    {
        let millis = ((value - seconds) * 1000.0).round() as u32;
        if let Some(date) = DateTime::from_timestamp(
            seconds as i64 + i64::from(millis / 1000),
            (millis % 1000) * 1_000_000,
        ) {
            return Ok(date);
        }
    }
    Err(rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Real,
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid asset date").into(),
    ))
}

/// Convert a database row to an `AssetRecord`.
///
/// Returns `rusqlite::Error` on column extraction failures instead of silently
/// falling back to defaults, so schema mismatches or corruption are surfaced.
fn row_to_asset_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetRecord> {
    let id: String = row.get(0)?;
    let version_size_str: String = row.get(1)?;
    let checksum: String = row.get(2)?;
    let filename: String = row.get(3)?;
    let created_at = decode_asset_date(row.get(4)?, 4)?;
    let added_at = row
        .get::<_, Option<f64>>(5)?
        .map(|value| decode_asset_date(value, 5))
        .transpose()?;
    let size_bytes: i64 = row.get(6)?;
    let media_type_str: String = row.get(7)?;
    let status_str: String = row.get(8)?;
    let downloaded_at_ts: Option<i64> = row.get(9)?;
    let local_path_str: Option<String> = row.get(10)?;
    let last_seen_at_ts: i64 = row.get(11)?;
    let download_attempts: i64 = row.get(12)?;
    let last_error: Option<String> = row.get(13)?;
    let local_checksum: Option<String> = row.get(14)?;

    let source_str: Option<String> = row.get(15)?;
    let source = source_str.map(Arc::<str>::from);
    let is_favorite: i64 = row.get(16)?;
    let rating: Option<i64> = row.get(17)?;
    let latitude: Option<f64> = row.get(18)?;
    let longitude: Option<f64> = row.get(19)?;
    let altitude: Option<f64> = row.get(20)?;
    let orientation: Option<i64> = row.get(21)?;
    let duration_secs: Option<f64> = row.get(22)?;
    let timezone_offset: Option<i64> = row.get(23)?;
    let width: Option<i64> = row.get(24)?;
    let height: Option<i64> = row.get(25)?;
    let title: Option<String> = row.get(26)?;
    let keywords: Option<String> = row.get(27)?;
    let description: Option<String> = row.get(28)?;
    let media_subtype: Option<String> = row.get(29)?;
    let burst_id: Option<String> = row.get(30)?;
    let is_hidden: i64 = row.get(31)?;
    let is_archived: i64 = row.get(32)?;
    let modified_at_ts: Option<i64> = row.get(33)?;
    let is_deleted: i64 = row.get(34)?;
    let deleted_at_ts: Option<i64> = row.get(35)?;
    let provider_data: Option<String> = row.get(36)?;
    let metadata_hash: Option<String> = row.get(37)?;
    let library: String = row.get(38)?;
    let download_checksum: Option<String> = row.get(39)?;

    let metadata = AssetMetadata {
        source,
        is_favorite: is_favorite != 0,
        rating: rating.and_then(|v| u8::try_from(v).ok()),
        latitude,
        longitude,
        altitude,
        orientation: orientation.and_then(|v| u8::try_from(v).ok()),
        duration_secs,
        timezone_offset: timezone_offset.and_then(|v| i32::try_from(v).ok()),
        width: width.and_then(|v| u32::try_from(v).ok()),
        height: height.and_then(|v| u32::try_from(v).ok()),
        title,
        keywords,
        description,
        media_subtype,
        burst_id,
        is_hidden: is_hidden != 0,
        is_archived: is_archived != 0,
        modified_at: modified_at_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
        is_deleted: is_deleted != 0,
        deleted_at: deleted_at_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
        provider_data,
        metadata_hash,
    };

    Ok(AssetRecord {
        library: Arc::from(library),
        id: id.into_boxed_str(),
        checksum: checksum.into_boxed_str(),
        filename: filename.into_boxed_str(),
        local_path: local_path_str.map(PathBuf::from),
        last_error,
        local_checksum,
        download_checksum,
        size_bytes: u64::try_from(size_bytes).unwrap_or(0),
        created_at,
        added_at,
        downloaded_at: optional_ts_to_utc(downloaded_at_ts),
        last_seen_at: ts_to_utc(last_seen_at_ts),
        download_attempts: u32::try_from(download_attempts).unwrap_or(u32::MAX),
        version_size: VersionSizeKey::from_str(&version_size_str)
            .unwrap_or(VersionSizeKey::Original),
        media_type: MediaType::from_str(&media_type_str).unwrap_or(MediaType::Photo),
        status: AssetStatus::from_str(&status_str).unwrap_or(AssetStatus::Pending),
        metadata: Arc::new(metadata),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestAssetRecord;
    use std::fs;

    fn test_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn asset_date_codec_preserves_milliseconds_and_rejects_invalid_values() {
        for seconds in [
            DateTime::<Utc>::MIN_UTC.timestamp(),
            -1,
            0,
            1_700_000_000,
            DateTime::<Utc>::MAX_UTC.timestamp(),
        ] {
            for millis in 0..1000 {
                let date = DateTime::from_timestamp(seconds, millis * 1_000_000).unwrap();
                assert_eq!(decode_asset_date(encode_asset_date(date), 4).unwrap(), date);
            }
        }
        for (value, expected_millis) in [(-0.001, -1), (-1.999, -1999), (0.9999999, 1000)] {
            assert_eq!(
                decode_asset_date(value, 4).unwrap().timestamp_millis(),
                expected_millis
            );
        }
        for invalid in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            9.223_372_036_854_776e18,
            -9.223_372_036_854_776e18,
        ] {
            assert!(decode_asset_date(invalid, 4).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn verified_source_checksum_round_trip_is_path_and_rendition_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let first = dir.path().join("first.jpg");
        let second = dir.path().join("second.jpg");
        let mut record = TestAssetRecord::new("PROVENANCE")
            .checksum("provider-v1")
            .metadata(AssetMetadata {
                metadata_hash: Some("metadata".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "PROVENANCE",
            "original",
            &first,
            "local-first",
            Some("source-first"),
            false,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
            .await
            .unwrap();
        // Import/reconciliation-style registration must not treat recovered
        // download_checksum values as source provenance for a different path.
        db.mark_downloaded(
            "PrimarySync",
            "PROVENANCE",
            "original",
            &second,
            "local-second",
            Some("recovered-second"),
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
            .await
            .unwrap();
        drop(db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
        for work in &pending {
            let expected = if work.asset.local_path.as_deref() == Some(first.as_path()) {
                Some("source-first")
            } else {
                None
            };
            assert_eq!(work.source_checksum.as_deref(), expected);
        }
        // Metadata finalization must preserve the additional path's source
        // evidence while updating the independent local/size checksum state.
        let extra = pending
            .iter()
            .find(|work| work.asset.local_path.as_deref() == Some(first.as_path()))
            .unwrap();
        db.finish_metadata_rewrite(
            extra,
            MetadataRewriteQueue::Ordinary,
            Some("local-rewritten"),
            Some("local-first"),
            MetadataRewriteCompletion::None,
        )
        .await
        .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        let extra = pending
            .iter()
            .find(|work| work.asset.local_path.as_deref() == Some(first.as_path()))
            .unwrap();
        assert_eq!(extra.source_checksum.as_deref(), Some("source-first"));
        assert_eq!(
            extra.asset.local_checksum.as_deref(),
            Some("local-rewritten")
        );

        db.mark_verified_download(
            "PrimarySync",
            "PROVENANCE",
            "original",
            &second,
            "local-second",
            Some("source-second"),
            false,
        )
        .await
        .unwrap();
        record.checksum = "provider-v2".into();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "PROVENANCE",
            "original",
            &second,
            "local-v2",
            Some("recovered-v2"),
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
            .await
            .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            pending.len(),
            1,
            "old provider paths cannot supply current metadata evidence"
        );
        assert_eq!(
            pending[0].asset.local_path.as_deref(),
            Some(second.as_path())
        );
        assert!(pending[0].source_checksum.is_none());
    }

    #[tokio::test]
    async fn verified_source_checksum_failure_rolls_back_download_finalization() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let media = dir.path().join("source.jpg");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let record = TestAssetRecord::new("SOURCE_COMMIT").build();
        db.upsert_seen(&record).await.unwrap();
        {
            let conn = db.acquire_lock("test_source_checksum_failure").unwrap();
            conn.execute_batch(
                "CREATE TEMP TRIGGER fail_source_checksum BEFORE UPDATE OF source_checksum \
                 ON asset_metadata_paths BEGIN SELECT RAISE(ABORT, 'source checksum failure'); END;"
            ).unwrap();
        }
        assert!(
            db.mark_verified_download(
                "PrimarySync",
                "SOURCE_COMMIT",
                "original",
                &media,
                "local",
                Some("source"),
                false
            )
            .await
            .is_err()
        );
        assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
        {
            let conn = db.acquire_lock("test_source_checksum_rollback").unwrap();
            let paths: i64 = conn
                .query_row("SELECT COUNT(*) FROM asset_metadata_paths", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(paths, 0);
        }
        drop(db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        db.mark_verified_download(
            "PrimarySync",
            "SOURCE_COMMIT",
            "original",
            &media,
            "local",
            Some("source"),
            false,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "SOURCE_COMMIT", "original")
            .await
            .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].source_checksum.as_deref(), Some("source"));
    }

    #[tokio::test]
    async fn owned_temp_file_claim_round_trips_and_retires_exact_path() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();
        let path = dir.path().join("asset.kei-tmp");
        let expected = crate::fs_util::absolute_lexical(&path).unwrap();

        db.claim_temp_file(&path).await.unwrap();
        db.claim_temp_file(&path).await.unwrap();

        let owned = db.get_owned_temp_files_before(i64::MAX).await.unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].path, expected);
        assert!(owned[0].claimed_at > 0);
        assert_eq!(db.retire_temp_files(&[path]).await.unwrap(), 1);
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn owned_temp_file_path_round_trip_is_lossless_for_non_utf8_names() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();
        let path = dir
            .path()
            .join(OsString::from_vec(b"asset-\xff.kei-tmp".to_vec()));

        db.claim_temp_file(&path).await.unwrap();

        let owned = db.get_owned_temp_files_before(i64::MAX).await.unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].path, path);
    }

    #[tokio::test]
    async fn asset_master_mapping_is_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        db.upsert_asset_master_mapping("PrimarySync", "asset-a", "master-primary")
            .await
            .unwrap();
        db.upsert_asset_master_mapping("SharedSync-AAAA", "asset-a", "master-shared")
            .await
            .unwrap();

        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-a")
                .await
                .unwrap()
                .as_deref(),
            Some("master-primary")
        );
        assert_eq!(
            db.get_master_record_name_for_asset("SharedSync-AAAA", "asset-a")
                .await
                .unwrap()
                .as_deref(),
            Some("master-shared")
        );
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "missing")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db.get_asset_master_mappings().await.unwrap(),
            HashSet::from([
                (
                    "PrimarySync".to_string(),
                    "asset-a".to_string(),
                    "master-primary".to_string(),
                ),
                (
                    "SharedSync-AAAA".to_string(),
                    "asset-a".to_string(),
                    "master-shared".to_string(),
                ),
            ])
        );
    }

    #[tokio::test]
    async fn legacy_master_state_owner_claim_is_atomic_and_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "master", "asset-a")
                .await
                .unwrap()
        );
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "master", "asset-a")
                .await
                .unwrap()
        );
        assert!(
            !db.claim_legacy_master_state_owner("PrimarySync", "master", "asset-b")
                .await
                .unwrap()
        );
        assert!(
            db.claim_legacy_master_state_owner("SharedSync-AAAA", "master", "asset-b")
                .await
                .unwrap()
        );
        assert_eq!(
            db.get_legacy_master_state_owners().await.unwrap(),
            HashSet::from([
                (
                    "PrimarySync".to_string(),
                    "master".to_string(),
                    "asset-a".to_string(),
                ),
                (
                    "SharedSync-AAAA".to_string(),
                    "master".to_string(),
                    "asset-b".to_string(),
                ),
            ])
        );
    }

    #[tokio::test]
    async fn asset_master_mapping_backfill_uses_unambiguous_album_history() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (library, container) in [
            ("PrimarySync", "album-a"),
            ("PrimarySync", "album-b"),
            ("SharedSync-AAAA", "album-a"),
        ] {
            db.upsert_album_container(library, container, "Album", "album")
                .await
                .unwrap();
        }

        for (library, container, asset, master) in [
            ("PrimarySync", "album-a", "asset-a", Some("master-a")),
            ("PrimarySync", "album-b", "asset-a", Some("master-a")),
            (
                "SharedSync-AAAA",
                "album-a",
                "asset-a",
                Some("master-shared"),
            ),
            (
                "PrimarySync",
                "album-a",
                "asset-ambiguous",
                Some("master-one"),
            ),
            (
                "PrimarySync",
                "album-b",
                "asset-ambiguous",
                Some("master-two"),
            ),
            ("PrimarySync", "album-a", "asset-missing", None),
        ] {
            db.upsert_album_membership_delta(library, container, asset, master, "icloud")
                .await
                .unwrap();
        }
        db.mark_album_membership_deleted("PrimarySync", "album-b", "asset-a")
            .await
            .unwrap();

        assert_eq!(
            db.backfill_asset_master_mappings_from_album_memberships()
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-a")
                .await
                .unwrap()
                .as_deref(),
            Some("master-a")
        );
        assert_eq!(
            db.get_master_record_name_for_asset("SharedSync-AAAA", "asset-a")
                .await
                .unwrap()
                .as_deref(),
            Some("master-shared")
        );
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-ambiguous")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-missing")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            db.backfill_asset_master_mappings_from_album_memberships()
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn master_family_soft_delete_marks_sibling_asset_state_rows() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let master = TestAssetRecord::new("master-a").build();
        let sibling = TestAssetRecord::new("asset-b").build();
        let unrelated = TestAssetRecord::new("asset-c").build();

        db.upsert_seen(&master).await.unwrap();
        db.upsert_seen(&sibling).await.unwrap();
        db.upsert_seen(&unrelated).await.unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-b", "master-a")
            .await
            .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-c", "master-other")
            .await
            .unwrap();

        let updated = db
            .mark_master_family_soft_deleted("PrimarySync", "master-a", None)
            .await
            .unwrap();

        assert_eq!(updated, 2);
        assert!(read_asset_writer_contract_row(&db, "master-a").is_deleted);
        assert!(read_asset_writer_contract_row(&db, "asset-b").is_deleted);
        assert!(!read_asset_writer_contract_row(&db, "asset-c").is_deleted);
    }

    #[derive(Debug)]
    struct AssetWriterContractRow {
        status: String,
        downloaded_at: Option<i64>,
        local_path: Option<String>,
        local_checksum: Option<String>,
        download_checksum: Option<String>,
        last_error: Option<String>,
        is_deleted: bool,
        deleted_at: Option<i64>,
    }

    fn read_asset_writer_contract_row(
        db: &SqliteStateDb,
        asset_id: &str,
    ) -> AssetWriterContractRow {
        let conn = db.acquire_lock("read_asset_writer_contract_row").unwrap();
        conn.query_row(
            "SELECT status, downloaded_at, local_path, local_checksum, download_checksum, \
             last_error, is_deleted, deleted_at FROM assets \
             WHERE library = 'PrimarySync' AND id = ?1 AND version_size = 'original'",
            [asset_id],
            |row| {
                let is_deleted: i64 = row.get(6)?;
                Ok(AssetWriterContractRow {
                    status: row.get(0)?,
                    downloaded_at: row.get(1)?,
                    local_path: row.get(2)?,
                    local_checksum: row.get(3)?,
                    download_checksum: row.get(4)?,
                    last_error: row.get(5)?,
                    is_deleted: is_deleted != 0,
                    deleted_at: row.get(7)?,
                })
            },
        )
        .unwrap()
    }

    fn assert_downloaded_tombstone_row(
        db: &SqliteStateDb,
        asset_id: &str,
        path: &Path,
        deleted_at: DateTime<Utc>,
    ) {
        let row = read_asset_writer_contract_row(db, asset_id);
        assert_eq!(row.status, "downloaded");
        assert!(
            row.downloaded_at.is_some(),
            "download writer state must preserve downloaded_at"
        );
        assert_eq!(row.local_path, Some(path.to_string_lossy().into_owned()));
        assert_eq!(row.local_checksum.as_deref(), Some("local_hash"));
        assert_eq!(row.download_checksum.as_deref(), Some("download_hash"));
        assert_eq!(row.last_error, None);
        assert!(row.is_deleted);
        assert_eq!(row.deleted_at, Some(deleted_at.timestamp()));
    }

    #[tokio::test]
    async fn test_open_creates_db() {
        let dir = test_dir();
        let path = dir.path().join("test.db");
        let db = SqliteStateDb::open(&path).await.unwrap();
        assert!(path.exists());
        assert_eq!(path, db.path());
    }

    /// The same (id, version_size) under two different libraries must
    /// coexist as distinct rows; without per-zone PK scope, the second
    /// `upsert_seen` would UPDATE the first row in place and silently
    /// drop the other zone's separate-asset state.
    #[tokio::test]
    async fn upsert_seen_keeps_distinct_rows_per_library() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let primary = TestAssetRecord::new("ASSET_X")
            .library("PrimarySync")
            .checksum("ck_primary")
            .build();
        let shared = TestAssetRecord::new("ASSET_X")
            .library("SharedSync-A1B2C3D4")
            .checksum("ck_shared")
            .build();
        db.upsert_seen(&primary).await.unwrap();
        db.upsert_seen(&shared).await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 2);

        // mark_downloaded for the primary row must not flip the shared row's status.
        let dir = test_dir();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"x").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ASSET_X",
            "original",
            &path,
            "lck_primary",
            None,
        )
        .await
        .unwrap();
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 1);

        // get_downloaded_ids returns the (library, id, version) triple
        // so consumers can route per-library skip decisions correctly.
        let ids = db.get_downloaded_ids().await.unwrap();
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&(
            "PrimarySync".to_string(),
            "ASSET_X".to_string(),
            "original".to_string(),
        )));
    }

    #[tokio::test]
    async fn concurrent_asset_writers_preserve_library_id_version_pk() {
        let dir = test_dir();
        let db_path = dir.path().join("concurrent-writers.db");
        let db = std::sync::Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let media_dir = dir.path().join("photos");
        std::fs::create_dir_all(&media_dir).unwrap();

        let cases = [
            (
                "PrimarySync",
                "ASSET_PK",
                VersionSizeKey::Original,
                "ck_orig",
            ),
            ("PrimarySync", "ASSET_PK", VersionSizeKey::Medium, "ck_med"),
            (
                "SharedSync-A1B2C3D4",
                "ASSET_PK",
                VersionSizeKey::Original,
                "ck_shared",
            ),
        ];
        let mut handles = Vec::new();
        for (library, id, version_size, checksum) in cases {
            let db = std::sync::Arc::clone(&db);
            let path = media_dir.join(format!("{}_{}_{}.jpg", library, id, version_size.as_str()));
            handles.push(tokio::spawn(async move {
                std::fs::write(&path, b"image-bytes").unwrap();
                let record = TestAssetRecord::new(id)
                    .library(library)
                    .version_size(version_size)
                    .checksum(checksum)
                    .build();
                db.upsert_seen(&record).await.unwrap();
                db.mark_downloaded(
                    library,
                    id,
                    version_size.as_str(),
                    &path,
                    checksum,
                    Some(checksum),
                )
                .await
                .unwrap();
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }

        let conn = db.acquire_lock("verify concurrent writer rows").unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT library, id, version_size, status FROM assets \
                 WHERE id = 'ASSET_PK' ORDER BY library, version_size",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(
            rows,
            vec![
                (
                    "PrimarySync".to_string(),
                    "ASSET_PK".to_string(),
                    "medium".to_string(),
                    "downloaded".to_string()
                ),
                (
                    "PrimarySync".to_string(),
                    "ASSET_PK".to_string(),
                    "original".to_string(),
                    "downloaded".to_string()
                ),
                (
                    "SharedSync-A1B2C3D4".to_string(),
                    "ASSET_PK".to_string(),
                    "original".to_string(),
                    "downloaded".to_string()
                ),
            ],
            "concurrent writers must preserve every library/id/version row"
        );
    }

    /// `mark_failed` must scope to one zone; the other zone's row for
    /// the same (id, version_size) keeps its prior status.
    #[tokio::test]
    async fn mark_failed_is_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for lib in ["PrimarySync", "SharedSync-AAAA"] {
            let r = TestAssetRecord::new("DUP")
                .library(lib)
                .checksum(&format!("ck_{lib}"))
                .build();
            db.upsert_seen(&r).await.unwrap();
        }
        db.mark_failed("PrimarySync", "DUP", "original", "boom")
            .await
            .unwrap();
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.failed, 1, "only PrimarySync row was marked failed");
        assert_eq!(summary.pending, 1, "SharedSync row stays pending");
    }

    #[tokio::test]
    async fn test_should_download_not_in_db() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let result = db
            .should_download(
                "PrimarySync",
                "ABC123",
                "original",
                "checksum",
                Path::new("/tmp/file.jpg"),
            )
            .await
            .unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_upsert_and_should_download_pending() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123").build();

        db.upsert_seen(&record).await.unwrap();

        // Pending assets should be downloaded
        let result = db
            .should_download(
                "PrimarySync",
                "ABC123",
                "original",
                "checksum123",
                Path::new("/tmp/file.jpg"),
            )
            .await
            .unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_mark_downloaded_then_should_not_download() {
        let dir = test_dir();
        let file_path = dir.path().join("photo.jpg");
        fs::write(&file_path, b"test content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123").build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ABC123",
            "original",
            &file_path,
            "abc123hash",
            None,
        )
        .await
        .unwrap();

        // Downloaded asset with existing file should not be downloaded
        let result = db
            .should_download(
                "PrimarySync",
                "ABC123",
                "original",
                "checksum123",
                &file_path,
            )
            .await
            .unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_should_download_file_missing() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123").build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ABC123",
            "original",
            Path::new("/nonexistent/file.jpg"),
            "abc123hash",
            None,
        )
        .await
        .unwrap();

        // Downloaded asset with missing file should be re-downloaded
        let result = db
            .should_download(
                "PrimarySync",
                "ABC123",
                "original",
                "checksum123",
                Path::new("/nonexistent/file.jpg"),
            )
            .await
            .unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_should_download_checksum_changed() {
        let dir = test_dir();
        let file_path = dir.path().join("photo.jpg");
        fs::write(&file_path, b"test content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123")
            .checksum("old_checksum")
            .build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ABC123",
            "original",
            &file_path,
            "oldhash",
            None,
        )
        .await
        .unwrap();

        // Different checksum should trigger re-download
        let result = db
            .should_download(
                "PrimarySync",
                "ABC123",
                "original",
                "new_checksum",
                &file_path,
            )
            .await
            .unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn should_download_empty_remote_checksum_does_not_skip_existing_file() {
        let dir = test_dir();
        let file_path = dir.path().join("photo.jpg");
        fs::write(&file_path, b"test content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = TestAssetRecord::new("ABC123").checksum("").build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ABC123",
            "original",
            &file_path,
            "oldhash",
            None,
        )
        .await
        .unwrap();

        let result = db
            .should_download("PrimarySync", "ABC123", "original", "", &file_path)
            .await
            .unwrap();
        assert!(
            result,
            "empty remote checksum must not hard-skip a downloaded row"
        );
    }

    #[tokio::test]
    async fn test_mark_failed_and_get_failed() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123").build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", "ABC123", "original", "Connection timeout")
            .await
            .unwrap();

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(&*failed[0].id, "ABC123");
        assert_eq!(failed[0].last_error.as_deref(), Some("Connection timeout"));
        assert_eq!(failed[0].download_attempts, 1);
    }

    #[tokio::test]
    async fn get_failed_orders_by_last_seen_desc() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for id in &["OLDEST", "MIDDLE", "NEWEST"] {
            let record = TestAssetRecord::new(id)
                .checksum(&format!("ck_{id}"))
                .filename(&format!("{}.jpg", id.to_lowercase()))
                .size(100)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_failed("PrimarySync", id, "original", "boom")
                .await
                .unwrap();
        }

        // Force a deterministic order by backdating.
        db.backdate_last_seen("OLDEST", 1_000);
        db.backdate_last_seen("MIDDLE", 2_000);
        db.backdate_last_seen("NEWEST", 3_000);

        let failed = db.get_failed().await.unwrap();
        let ids: Vec<&str> = failed.iter().map(|r| &*r.id).collect();
        assert_eq!(
            ids,
            vec!["NEWEST", "MIDDLE", "OLDEST"],
            "get_failed must sort last_seen_at DESC"
        );
    }

    #[tokio::test]
    async fn get_failed_sample_respects_limit_and_returns_total() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..5 {
            let id = format!("FAIL_{i}");
            let record = TestAssetRecord::new(&id)
                .checksum(&format!("ck_{i}"))
                .filename(&format!("{i}.jpg"))
                .size(100)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_failed("PrimarySync", &id, "original", "boom")
                .await
                .unwrap();
        }
        // Newest first: FAIL_4 > FAIL_3 > FAIL_2 ...
        for i in 0..5 {
            db.backdate_last_seen(&format!("FAIL_{i}"), 1_000 + i as i64);
        }

        let (sample, total) = db.get_failed_sample(2).await.unwrap();
        assert_eq!(total, 5, "total should reflect full failed count");
        assert_eq!(sample.len(), 2, "limit should cap returned rows");
        assert_eq!(&*sample[0].id, "FAIL_4");
        assert_eq!(&*sample[1].id, "FAIL_3");

        // limit > total returns all and the correct total
        let (sample, total) = db.get_failed_sample(100).await.unwrap();
        assert_eq!(total, 5);
        assert_eq!(sample.len(), 5);
    }

    #[tokio::test]
    async fn get_failed_sample_empty_returns_zero_total() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let (sample, total) = db.get_failed_sample(10).await.unwrap();
        assert!(sample.is_empty());
        assert_eq!(total, 0);
    }

    #[tokio::test]
    async fn get_pending_orders_by_last_seen_desc() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for id in &["OLD", "MID", "NEW"] {
            let record = TestAssetRecord::new(id)
                .checksum(&format!("ck_{id}"))
                .filename(&format!("{}.jpg", id.to_lowercase()))
                .size(100)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        db.backdate_last_seen("OLD", 1_000);
        db.backdate_last_seen("MID", 2_000);
        db.backdate_last_seen("NEW", 3_000);

        let pending = db.get_pending().await.unwrap();
        let ids: Vec<&str> = pending.iter().map(|r| &*r.id).collect();
        assert_eq!(
            ids,
            vec!["NEW", "MID", "OLD"],
            "get_pending must sort last_seen_at DESC"
        );
    }

    #[tokio::test]
    async fn test_reset_failed() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123").build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", "ABC123", "original", "Error")
            .await
            .unwrap();

        let count = db.reset_failed().await.unwrap();
        assert_eq!(count, 1);

        let failed = db.get_failed().await.unwrap();
        assert!(failed.is_empty());
    }

    #[tokio::test]
    async fn test_get_summary() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Add some assets in different states
        for i in 0..3 {
            let record = TestAssetRecord::new(&format!("PENDING_{}", i))
                .checksum(&format!("checksum_{}", i))
                .filename(&format!("photo_{}.jpg", i))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        let dir = test_dir();
        for i in 0..2 {
            let record = TestAssetRecord::new(&format!("DOWNLOADED_{}", i))
                .checksum(&format!("dl_checksum_{}", i))
                .filename(&format!("dl_photo_{}.jpg", i))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let path = dir.path().join(format!("dl_photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &format!("DOWNLOADED_{}", i),
                "original",
                &path,
                "hash",
                None,
            )
            .await
            .unwrap();
        }

        let record = TestAssetRecord::new("FAILED_1")
            .checksum("fail_checksum")
            .filename("fail_photo.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", "FAILED_1", "original", "Error")
            .await
            .unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 6);
        assert_eq!(summary.pending, 3);
        assert_eq!(summary.downloaded, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.policy_excluded, 0);
    }

    #[tokio::test]
    async fn policy_excluded_asset_is_non_actionable_until_redispatched() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = TestAssetRecord::new("POLICY_EXCLUDED")
            .checksum("excluded-checksum")
            .filename("old.mov")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.set_asset_verification(
            "PrimarySync",
            "POLICY_EXCLUDED",
            "original",
            AssetVerificationState::Unknown,
            "before policy decision",
        )
        .await
        .unwrap();

        assert!(
            db.mark_policy_excluded("PrimarySync", "POLICY_EXCLUDED", "original")
                .await
                .unwrap()
        );
        assert!(
            !db.mark_policy_excluded("PrimarySync", "POLICY_EXCLUDED", "original")
                .await
                .unwrap()
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.policy_excluded, 1);
        assert_eq!(summary.awaiting_provider_verification, 0);
        assert!(db.get_pending().await.unwrap().is_empty());

        db.upsert_seen(&record).await.unwrap();
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, AssetStatus::Pending);
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.policy_excluded, 0);
    }

    #[tokio::test]
    async fn policy_excluded_revalidation_reader_is_live_and_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (id, library) in [
            ("LIVE_PRIMARY", "PrimarySync"),
            ("DELETED_PRIMARY", "PrimarySync"),
            ("LIVE_SHARED", "SharedSync"),
        ] {
            let record = TestAssetRecord::new(id).library(library).build();
            db.upsert_seen(&record).await.unwrap();
            assert!(
                db.mark_policy_excluded(library, id, "original")
                    .await
                    .unwrap()
            );
        }
        db.resolve_source_deleted("PrimarySync", "DELETED_PRIMARY", None)
            .await
            .unwrap();

        let ids = db
            .get_policy_excluded_ids_for_revalidation("PrimarySync")
            .await
            .unwrap();

        assert_eq!(ids, vec!["LIVE_PRIMARY"]);
    }

    #[tokio::test]
    async fn source_deletion_supersedes_policy_exclusion() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = TestAssetRecord::new("EXCLUDED_DELETED")
            .checksum("excluded-deleted-checksum")
            .filename("deleted.mov")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        assert!(
            db.mark_policy_excluded("PrimarySync", "EXCLUDED_DELETED", "original")
                .await
                .unwrap()
        );

        assert_eq!(
            db.resolve_source_deleted("PrimarySync", "EXCLUDED_DELETED", None)
                .await
                .unwrap(),
            1
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.policy_excluded, 0);
        assert_eq!(summary.source_deleted, 1);
    }

    #[tokio::test]
    async fn test_sync_run_lifecycle() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let run_id = db.start_sync_run().await.unwrap();
        assert!(run_id > 0);

        let stats = SyncRunStats {
            assets_seen: 100,
            assets_downloaded: 95,
            assets_failed: 5,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };

        db.complete_sync_run(run_id, &stats).await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert!(summary.last_sync_started.is_some());
        assert!(summary.last_sync_completed.is_some());
    }

    #[tokio::test]
    async fn summary_tracks_running_sync_even_when_latest_row_completed() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let active_start = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let active_run_id = db.start_sync_run_at(active_start).await.unwrap();
        let completed_run_id = db
            .start_sync_run_at(Utc.timestamp_opt(1_700_000_030, 0).unwrap())
            .await
            .unwrap();
        assert!(
            completed_run_id > active_run_id,
            "completed row must be newest by id for this regression"
        );

        let stats = SyncRunStats {
            assets_seen: 1,
            assets_downloaded: 1,
            assets_failed: 0,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };
        db.complete_sync_run(completed_run_id, &stats)
            .await
            .unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.active_sync_started, Some(active_start));
        assert!(
            summary.last_sync_completed.is_some(),
            "latest completed row should still be available for non-active status"
        );
    }

    #[tokio::test]
    async fn summary_lists_full_enumeration_progress_markers() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        db.begin_enum_progress("SharedSync-Z").await.unwrap();
        db.begin_enum_progress("PrimarySync").await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(
            summary.active_enumeration_zones,
            vec!["PrimarySync", "SharedSync-Z"]
        );
    }

    // ── sync_runs status lifecycle ─────────────────────────────────────────

    fn status_of(db: &SqliteStateDb, run_id: i64) -> String {
        db.sync_run_snapshot_for_test(run_id).unwrap().0
    }

    #[tokio::test]
    async fn sync_run_status_is_running_after_start() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        assert_eq!(status_of(&db, run_id), "running");
    }

    #[tokio::test]
    async fn sync_run_status_is_complete_after_clean_complete() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        let stats = SyncRunStats {
            assets_seen: 1,
            assets_downloaded: 1,
            assets_failed: 0,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();
        assert_eq!(status_of(&db, run_id), "complete");
    }

    /// `enumeration_errors` must round-trip from `SyncRunStats` into the
    /// on-disk `sync_runs.enumeration_errors` column.
    #[tokio::test]
    async fn complete_sync_run_persists_enumeration_errors_column() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        let stats = SyncRunStats {
            assets_seen: 0,
            assets_downloaded: 0,
            assets_failed: 0,
            enumeration_errors: 17,
            interrupted: false,
            ..Default::default()
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();

        let conn = db.acquire_lock("test_enum_errors_column").unwrap();
        let stored: i64 = conn
            .query_row(
                "SELECT enumeration_errors FROM sync_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, 17,
            "enumeration_errors must round-trip from SyncRunStats to sync_runs row"
        );
    }

    #[tokio::test]
    async fn complete_sync_run_persists_inventory_columns() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        let stats = SyncRunStats {
            api_total_at_start: Some(95),
            api_total_at_start_partial: true,
            inventory_drop_warnings: 1,
            inventory_drop_previous_total: Some(100),
            inventory_drop_current_total: Some(95),
            inventory_drop_library: Some("PrimarySync".to_string()),
            ..Default::default()
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();

        let conn = db.acquire_lock("test_inventory_columns").unwrap();
        let stored: (
            Option<i64>,
            i64,
            i64,
            Option<i64>,
            Option<i64>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT api_total_at_start, api_total_at_start_partial, \
                        inventory_drop_detected, inventory_drop_previous_total, \
                        inventory_drop_current_total, inventory_drop_library \
                 FROM sync_runs WHERE id = ?1",
                [run_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            stored,
            (
                Some(95),
                1,
                1,
                Some(100),
                Some(95),
                Some("PrimarySync".to_string())
            )
        );
    }

    #[tokio::test]
    async fn complete_sync_run_unknown_id_returns_error() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let stats = SyncRunStats {
            assets_seen: 3,
            assets_downloaded: 2,
            assets_failed: 1,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };

        let err = db
            .complete_sync_run(999_999, &stats)
            .await
            .expect_err("unknown sync_run id must fail loudly");
        match err {
            StateError::Invariant { operation, detail } => {
                assert_eq!(operation, "complete_sync_run");
                assert!(
                    detail.contains("999999") || detail.contains("999_999"),
                    "error detail should name the missing run id, got: {detail}"
                );
            }
            other => panic!("expected StateError::Invariant, got {other:?}"),
        }

        let conn = db.acquire_lock("unknown_sync_run").unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "unknown completion must not create a row");
    }

    #[tokio::test]
    async fn sync_run_status_is_interrupted_when_flagged() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        let stats = SyncRunStats {
            assets_seen: 1,
            assets_downloaded: 0,
            assets_failed: 0,
            enumeration_errors: 0,
            interrupted: true,
            ..Default::default()
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();
        assert_eq!(status_of(&db, run_id), "interrupted");
    }

    #[tokio::test]
    async fn promote_orphaned_sync_runs_flips_running_rows() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        // Simulate two crashed runs plus one clean one
        let a = db.start_sync_run().await.unwrap();
        let b = db.start_sync_run().await.unwrap();
        let c = db.start_sync_run().await.unwrap();
        let clean = SyncRunStats {
            assets_seen: 0,
            assets_downloaded: 0,
            assets_failed: 0,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };
        db.complete_sync_run(c, &clean).await.unwrap();

        let promoted = db.promote_orphaned_sync_runs().await.unwrap();
        assert_eq!(promoted, 2);
        assert_eq!(status_of(&db, a), "interrupted");
        assert_eq!(status_of(&db, b), "interrupted");
        // The cleanly completed row must be untouched
        assert_eq!(status_of(&db, c), "complete");
    }

    #[tokio::test]
    async fn promote_orphaned_sync_runs_noop_when_none_pending() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        let stats = SyncRunStats {
            assets_seen: 0,
            assets_downloaded: 0,
            assets_failed: 0,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();

        let promoted = db.promote_orphaned_sync_runs().await.unwrap();
        assert_eq!(promoted, 0);
    }

    /// Promotion is idempotent — once an orphan has been flipped to
    /// `interrupted`, a second invocation must return 0 rows promoted and
    /// must not re-touch the row. The current implementation guards via
    /// `WHERE status = 'running'`, but no test pinned the second-call
    /// behavior. A future refactor that broadened the WHERE clause (e.g.
    /// `status != 'completed'`) would silently double-promote rows the
    /// next time init runs.
    #[tokio::test]
    async fn promote_orphaned_sync_runs_idempotent_second_call_promotes_zero() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Three running rows; flush them once.
        let a = db.start_sync_run().await.unwrap();
        let b = db.start_sync_run().await.unwrap();
        let c = db.start_sync_run().await.unwrap();
        let first = db.promote_orphaned_sync_runs().await.unwrap();
        assert_eq!(first, 3, "first call should flip all 3 running rows");

        // Capture the post-promote interrupted timestamps so we can verify
        // the second call doesn't re-touch them. Scope the lock guard so
        // it never sits across an .await on the next line.
        let (snap_a, snap_b, snap_c) = {
            let conn = db.acquire_lock("snapshot_after_first_promote").unwrap();
            let read_run = |id: i64| {
                let (status, interrupted): (String, i32) = conn
                    .query_row(
                        "SELECT status, interrupted FROM sync_runs WHERE id = ?1",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                (status, interrupted)
            };
            (read_run(a), read_run(b), read_run(c))
        };
        assert_eq!(snap_a, ("interrupted".to_string(), 1));
        assert_eq!(snap_b, ("interrupted".to_string(), 1));
        assert_eq!(snap_c, ("interrupted".to_string(), 1));

        // Second call must be a no-op.
        let second = db.promote_orphaned_sync_runs().await.unwrap();
        assert_eq!(
            second, 0,
            "second call must not re-promote already-interrupted rows"
        );

        // And no row's state changed.
        let after_a: (String, i32) = {
            let conn = db.acquire_lock("snapshot_after_second_promote").unwrap();
            conn.query_row(
                "SELECT status, interrupted FROM sync_runs WHERE id = ?1",
                [a],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(after_a, ("interrupted".to_string(), 1));
    }

    /// Corollary to the idempotency test: a freshly-completed `sync_runs` row in between
    /// invocations must not be promoted. Pins the "WHERE status = 'running'"
    /// invariant against churn — a misclassified completed run would
    /// silently corrupt operator dashboards.
    #[tokio::test]
    async fn promote_orphaned_sync_runs_does_not_touch_completed_rows() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // First batch: one row, complete it cleanly.
        let r1 = db.start_sync_run().await.unwrap();
        let stats = SyncRunStats {
            assets_seen: 1,
            assets_downloaded: 1,
            assets_failed: 0,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };
        db.complete_sync_run(r1, &stats).await.unwrap();

        // Second batch: another row, leave running.
        let r2 = db.start_sync_run().await.unwrap();
        let promoted = db.promote_orphaned_sync_runs().await.unwrap();
        assert_eq!(promoted, 1, "only the running row should be promoted");

        // r1 must still be complete.
        let conn = db.acquire_lock("verify").unwrap();
        let s1: String = conn
            .query_row("SELECT status FROM sync_runs WHERE id = ?1", [r1], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(s1, "complete");
        let s2: String = conn
            .query_row("SELECT status FROM sync_runs WHERE id = ?1", [r2], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(s2, "interrupted");
    }

    // ── enum_in_progress markers ───────────────────────────────────────────

    #[tokio::test]
    async fn begin_enum_progress_inserts_marker() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.begin_enum_progress("PrimarySync").await.unwrap();
        let zones = db.list_interrupted_enumerations().await.unwrap();
        assert_eq!(zones, vec!["PrimarySync".to_string()]);
    }

    #[tokio::test]
    async fn end_enum_progress_clears_marker() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.begin_enum_progress("PrimarySync").await.unwrap();
        db.end_enum_progress("PrimarySync").await.unwrap();
        let zones = db.list_interrupted_enumerations().await.unwrap();
        assert!(zones.is_empty());
    }

    #[tokio::test]
    async fn end_enum_progress_is_idempotent() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        // No marker set — end should be a no-op without error
        db.end_enum_progress("NotThere").await.unwrap();
        assert!(db.list_interrupted_enumerations().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_interrupted_enumerations_tracks_multiple_zones() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.begin_enum_progress("PrimarySync").await.unwrap();
        db.begin_enum_progress("SharedSync-ABC123").await.unwrap();
        let mut zones = db.list_interrupted_enumerations().await.unwrap();
        zones.sort();
        assert_eq!(zones, vec!["PrimarySync", "SharedSync-ABC123"]);
    }

    #[tokio::test]
    async fn begin_enum_progress_is_idempotent_for_same_zone() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.begin_enum_progress("PrimarySync").await.unwrap();
        db.begin_enum_progress("PrimarySync").await.unwrap();
        let zones = db.list_interrupted_enumerations().await.unwrap();
        assert_eq!(zones, vec!["PrimarySync".to_string()]);
    }

    #[tokio::test]
    async fn begin_enum_progress_preserves_original_timestamp_on_reentry() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let original_ts = "1700000000";

        fn read_marker(db: &SqliteStateDb) -> Option<String> {
            let conn = db.acquire_lock("read_marker").unwrap();
            conn.query_row(
                "SELECT value FROM metadata WHERE key = 'enum_in_progress:PrimarySync'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
        }

        // Seed an older marker timestamp directly.
        {
            let conn = db.acquire_lock("seed_marker").unwrap();
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES ('enum_in_progress:PrimarySync', ?1)",
                [original_ts],
            )
            .unwrap();
        }

        // Re-entering begin_enum_progress on a live marker must not overwrite it.
        db.begin_enum_progress("PrimarySync").await.unwrap();
        assert_eq!(
            read_marker(&db).as_deref(),
            Some(original_ts),
            "re-entering begin_enum_progress must not rewrite the original timestamp"
        );

        // After end_enum_progress, the marker clears; a subsequent begin
        // should install a fresh timestamp.
        db.end_enum_progress("PrimarySync").await.unwrap();
        db.begin_enum_progress("PrimarySync").await.unwrap();
        let fresh = read_marker(&db).expect("marker must exist after begin");
        assert_ne!(
            fresh, original_ts,
            "after end_enum_progress, a new begin should install a fresh timestamp"
        );
    }

    #[tokio::test]
    async fn promote_orphaned_sync_runs_sets_interrupted_flag_too() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        let _ = db.promote_orphaned_sync_runs().await.unwrap();

        let conn = db.acquire_lock("verify_interrupted").unwrap();
        let interrupted: i32 = conn
            .query_row(
                "SELECT interrupted FROM sync_runs WHERE id = ?1",
                [run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(interrupted, 1);
    }

    #[tokio::test]
    async fn test_upsert_preserves_status() {
        let dir = test_dir();
        let file_path = dir.path().join("photo.jpg");
        fs::write(&file_path, b"test content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123").build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ABC123",
            "original",
            &file_path,
            "abc123hash",
            None,
        )
        .await
        .unwrap();

        // Upsert again - should preserve downloaded status
        db.upsert_seen(&record).await.unwrap();

        // Should still be downloaded (file exists)
        let result = db
            .should_download(
                "PrimarySync",
                "ABC123",
                "original",
                "checksum123",
                &file_path,
            )
            .await
            .unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_get_downloaded_page() {
        let dir = test_dir();
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..3 {
            let record = TestAssetRecord::new(&format!("DL_{}", i))
                .checksum(&format!("checksum_{}", i))
                .filename(&format!("photo_{}.jpg", i))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let path = dir.path().join(format!("photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &format!("DL_{}", i),
                "original",
                &path,
                "hash",
                None,
            )
            .await
            .unwrap();
        }

        // Fetch all in one page
        let page = db.get_downloaded_page(0, 100).await.unwrap();
        assert_eq!(page.len(), 3);

        // Paginate: page of 2, then remainder
        let first = db.get_downloaded_page(0, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        let second = db.get_downloaded_page(2, 2).await.unwrap();
        assert_eq!(second.len(), 1);
        let third = db.get_downloaded_page(4, 2).await.unwrap();
        assert!(third.is_empty());
    }

    #[tokio::test]
    async fn test_get_failed_page() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..3 {
            let id = format!("FAIL_{i}");
            let record = TestAssetRecord::new(&id)
                .checksum(&format!("checksum_{i}"))
                .filename(&format!("photo_{i}.jpg"))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_failed("PrimarySync", &id, "original", "boom")
                .await
                .unwrap();
        }
        // Newest-first ordering: FAIL_2 > FAIL_1 > FAIL_0
        for i in 0..3i64 {
            db.backdate_last_seen(&format!("FAIL_{i}"), 1_000 + i);
        }

        // Fetch all in one page
        let page = db.get_failed_page(0, 100).await.unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(&*page[0].id, "FAIL_2");
        assert_eq!(&*page[2].id, "FAIL_0");

        // Paginate: page of 2, then remainder
        let first = db.get_failed_page(0, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(&*first[0].id, "FAIL_2");
        assert_eq!(&*first[1].id, "FAIL_1");
        let second = db.get_failed_page(2, 2).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(&*second[0].id, "FAIL_0");
        let third = db.get_failed_page(4, 2).await.unwrap();
        assert!(third.is_empty());
    }

    #[tokio::test]
    async fn test_get_pending_page() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..3 {
            let id = format!("PEND_{i}");
            let record = TestAssetRecord::new(&id)
                .checksum(&format!("checksum_{i}"))
                .filename(&format!("photo_{i}.jpg"))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }
        // Newest-first ordering: PEND_2 > PEND_1 > PEND_0
        for i in 0..3i64 {
            db.backdate_last_seen(&format!("PEND_{i}"), 1_000 + i);
        }

        // Fetch all in one page
        let page = db.get_pending_page(0, 100).await.unwrap();
        assert_eq!(page.len(), 3);
        assert_eq!(&*page[0].id, "PEND_2");
        assert_eq!(&*page[2].id, "PEND_0");

        // Paginate: page of 2, then remainder
        let first = db.get_pending_page(0, 2).await.unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(&*first[0].id, "PEND_2");
        assert_eq!(&*first[1].id, "PEND_1");
        let second = db.get_pending_page(2, 2).await.unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(&*second[0].id, "PEND_0");
        let third = db.get_pending_page(4, 2).await.unwrap();
        assert!(third.is_empty());
    }

    #[tokio::test]
    async fn get_failed_pending_page_scales_to_large_count() {
        // Mirror of `test_get_downloaded_page_scales_to_large_count` for the
        // failed and pending lists. Bulk-insert 10k rows, paginate through,
        // assert we never need to materialize more than `page_size` at once
        // and the total count matches.
        let db = SqliteStateDb::open_in_memory().unwrap();
        let count: usize = 10_000;
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("BEGIN").unwrap();
            let mut stmt = conn
                .prepare(
                    "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at, download_attempts, last_error)
                     VALUES ('PrimarySync', ?1, 'original', ?2, ?3, ?4, ?5, 'photo', ?6, ?4, 1, ?7)",
                )
                .unwrap();
            let now = Utc::now().timestamp();
            for i in 0..count {
                let id = format!("ASSET_{i:05}");
                let checksum = format!("cksum_{i:05}");
                let filename = format!("IMG_{i:05}.jpg");
                let status = if i % 2 == 0 { "failed" } else { "pending" };
                let err = if status == "failed" {
                    Some("boom")
                } else {
                    None
                };
                stmt.execute(rusqlite::params![
                    id,
                    checksum,
                    filename,
                    now + i as i64,
                    4096,
                    status,
                    err
                ])
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        }

        let page_size: u32 = 1000;

        // Failed: 5000 rows
        let mut total = 0usize;
        let mut offset = 0u64;
        loop {
            let page = db.get_failed_page(offset, page_size).await.unwrap();
            if page.is_empty() {
                break;
            }
            assert!(
                page.len() <= page_size as usize,
                "get_failed_page must respect the requested limit"
            );
            assert!(page.iter().all(|r| r.status == AssetStatus::Failed));
            total += page.len();
            offset += page.len() as u64;
        }
        assert_eq!(total, count / 2);

        // Pending: 5000 rows
        let mut total = 0usize;
        let mut offset = 0u64;
        loop {
            let page = db.get_pending_page(offset, page_size).await.unwrap();
            if page.is_empty() {
                break;
            }
            assert!(
                page.len() <= page_size as usize,
                "get_pending_page must respect the requested limit"
            );
            assert!(page.iter().all(|r| r.status == AssetStatus::Pending));
            total += page.len();
            offset += page.len() as u64;
        }
        assert_eq!(total, count / 2);
    }

    // ── Batch operation tests ──

    #[tokio::test]
    async fn test_get_downloaded_ids() {
        let dir = test_dir();
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Create some assets with different statuses
        for i in 0..3 {
            let record = TestAssetRecord::new(&format!("DL_{}", i))
                .checksum(&format!("checksum_{}", i))
                .filename(&format!("photo_{}.jpg", i))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let path = dir.path().join(format!("photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &format!("DL_{}", i),
                "original",
                &path,
                "hash",
                None,
            )
            .await
            .unwrap();
        }

        // Add a pending asset (should not be in downloaded IDs)
        let pending = TestAssetRecord::new("PENDING_1")
            .checksum("pending_ck")
            .filename("pending.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&pending).await.unwrap();

        let ids = db.get_downloaded_ids().await.unwrap();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&(
            "PrimarySync".to_string(),
            "DL_0".to_string(),
            "original".to_string()
        )));
        assert!(ids.contains(&(
            "PrimarySync".to_string(),
            "DL_1".to_string(),
            "original".to_string()
        )));
        assert!(ids.contains(&(
            "PrimarySync".to_string(),
            "DL_2".to_string(),
            "original".to_string()
        )));
        assert!(!ids.contains(&(
            "PrimarySync".to_string(),
            "PENDING_1".to_string(),
            "original".to_string()
        )));
    }

    #[tokio::test]
    async fn downloaded_file_records_return_compact_skip_evidence() {
        let dir = test_dir();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = TestAssetRecord::new("DOWNLOADED")
            .checksum("provider-checksum")
            .filename("downloaded.jpg")
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join("downloaded.jpg");
        db.mark_downloaded(
            "PrimarySync",
            "DOWNLOADED",
            "original",
            &path,
            "local-checksum",
            Some("download-checksum"),
        )
        .await
        .unwrap();
        db.upsert_seen(&TestAssetRecord::new("PENDING").build())
            .await
            .unwrap();

        let records = db.get_downloaded_file_records().await.unwrap();

        assert_eq!(records.len(), 1);
        let downloaded = &records[0];
        assert_eq!(downloaded.library, "PrimarySync");
        assert_eq!(downloaded.id, "DOWNLOADED");
        assert_eq!(downloaded.version_size, VersionSizeKey::Original);
        assert_eq!(downloaded.checksum, "provider-checksum");
        assert_eq!(downloaded.local_path.as_deref(), Some(path.as_path()));
        assert_eq!(downloaded.local_checksum.as_deref(), Some("local-checksum"));
        assert_eq!(
            downloaded.download_checksum.as_deref(),
            Some("download-checksum")
        );
    }

    #[tokio::test]
    async fn test_get_downloaded_checksums() {
        let dir = test_dir();
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..2 {
            let record = TestAssetRecord::new(&format!("DL_{}", i))
                .checksum(&format!("checksum_{}", i))
                .filename(&format!("photo_{}.jpg", i))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let path = dir.path().join(format!("photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &format!("DL_{}", i),
                "original",
                &path,
                "hash",
                None,
            )
            .await
            .unwrap();
        }

        let checksums = db.get_downloaded_checksums().await.unwrap();
        assert_eq!(checksums.len(), 2);
        assert_eq!(
            checksums.get(&(
                "PrimarySync".to_string(),
                "DL_0".to_string(),
                "original".to_string()
            )),
            Some(&"checksum_0".to_string())
        );
        assert_eq!(
            checksums.get(&(
                "PrimarySync".to_string(),
                "DL_1".to_string(),
                "original".to_string()
            )),
            Some(&"checksum_1".to_string())
        );
    }

    #[tokio::test]
    async fn test_get_all_known_ids() {
        let dir = test_dir();
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Create downloaded assets
        for i in 0..2 {
            let record = TestAssetRecord::new(&format!("DL_{}", i))
                .checksum(&format!("checksum_{}", i))
                .filename(&format!("photo_{}.jpg", i))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let path = dir.path().join(format!("photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &format!("DL_{}", i),
                "original",
                &path,
                "hash",
                None,
            )
            .await
            .unwrap();
        }

        // Create a pending asset
        let pending = TestAssetRecord::new("PENDING_1")
            .checksum("pending_ck")
            .filename("pending.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&pending).await.unwrap();

        // Create a failed asset
        let failed = TestAssetRecord::new("FAILED_1")
            .checksum("failed_ck")
            .filename("failed.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&failed).await.unwrap();
        db.mark_failed("PrimarySync", "FAILED_1", "original", "test error")
            .await
            .unwrap();
        let shared_same_id = TestAssetRecord::new("FAILED_1")
            .library("SharedSync-AAAA")
            .checksum("shared_failed_ck")
            .filename("shared_failed.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&shared_same_id).await.unwrap();

        let known_ids = db.get_all_known_ids().await.unwrap();
        // Should include all assets regardless of status, scoped by library.
        assert_eq!(known_ids.len(), 5);
        assert!(known_ids.contains(&("PrimarySync".to_string(), "DL_0".to_string())));
        assert!(known_ids.contains(&("PrimarySync".to_string(), "DL_1".to_string())));
        assert!(known_ids.contains(&("PrimarySync".to_string(), "PENDING_1".to_string())));
        assert!(known_ids.contains(&("PrimarySync".to_string(), "FAILED_1".to_string())));
        assert!(known_ids.contains(&("SharedSync-AAAA".to_string(), "FAILED_1".to_string())));

        // get_downloaded_ids should only return 2
        let downloaded_ids = db.get_downloaded_ids().await.unwrap();
        assert_eq!(downloaded_ids.len(), 2);
    }

    #[tokio::test]
    async fn test_retry_failed_returns_zero_when_no_failures() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // With no assets at all, reset_failed returns 0
        let count = db.reset_failed().await.unwrap();
        assert_eq!(count, 0);

        // Add a downloaded asset — still no failures
        let record = TestAssetRecord::new("DL_1")
            .checksum("ck")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let dir = test_dir();
        let path = dir.path().join("photo.jpg");
        fs::write(&path, b"content").unwrap();
        db.mark_downloaded("PrimarySync", "DL_1", "original", &path, "hash", None)
            .await
            .unwrap();

        let count = db.reset_failed().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn test_retry_failed_resets_only_failed() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();

        // Add a downloaded asset
        let dl = TestAssetRecord::new("DL_1")
            .checksum("ck1")
            .filename("photo1.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&dl).await.unwrap();
        let path = dir.path().join("photo1.jpg");
        fs::write(&path, b"content").unwrap();
        db.mark_downloaded("PrimarySync", "DL_1", "original", &path, "hash", None)
            .await
            .unwrap();

        // Add a failed asset
        let failed = TestAssetRecord::new("FAIL_1")
            .checksum("ck2")
            .filename("photo2.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&failed).await.unwrap();
        db.mark_failed("PrimarySync", "FAIL_1", "original", "download error")
            .await
            .unwrap();

        // reset_failed should reset exactly 1
        let count = db.reset_failed().await.unwrap();
        assert_eq!(count, 1);

        // After reset, the failed asset should be in known_ids but not downloaded_ids
        let known = db.get_all_known_ids().await.unwrap();
        assert_eq!(known.len(), 2);
        assert!(known.contains(&("PrimarySync".to_string(), "DL_1".to_string())));
        assert!(known.contains(&("PrimarySync".to_string(), "FAIL_1".to_string())));

        let downloaded = db.get_downloaded_ids().await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert!(downloaded.contains(&(
            "PrimarySync".to_string(),
            "DL_1".to_string(),
            "original".to_string()
        )));
    }

    #[tokio::test]
    async fn test_metadata_get_set() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Missing key returns None
        assert_eq!(db.get_metadata("config_hash").await.unwrap(), None);

        // Set and retrieve
        db.set_metadata("config_hash", "abc123").await.unwrap();
        assert_eq!(
            db.get_metadata("config_hash").await.unwrap(),
            Some("abc123".to_string())
        );

        // Overwrite
        db.set_metadata("config_hash", "def456").await.unwrap();
        assert_eq!(
            db.get_metadata("config_hash").await.unwrap(),
            Some("def456".to_string())
        );
    }

    #[tokio::test]
    async fn checkpoint_transition_is_atomic() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.set_metadata("sync_token:zone", "old-token")
            .await
            .unwrap();
        db.set_metadata("enum_config_hash", "old-hash")
            .await
            .unwrap();
        db.set_metadata("pending_enum_config_hash", "new-hash")
            .await
            .unwrap();

        db.with_conn("install_test_trigger", |conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_enum_hash_update \
                 BEFORE UPDATE ON metadata \
                 WHEN NEW.key = 'enum_config_hash' \
                 BEGIN SELECT RAISE(ABORT, 'simulated promotion failure'); END;",
            )
            .map_err(|e| StateError::query("install_test_trigger", e))?;
            Ok(())
        })
        .await
        .unwrap();

        let result = db
            .commit_checkpoint_transition(CheckpointTransition {
                metadata_updates: vec![
                    ("sync_token:zone".into(), "new-token".into()),
                    ("enum_config_hash".into(), "new-hash".into()),
                ],
                metadata_deletes: vec!["pending_enum_config_hash".into()],
            })
            .await;

        assert!(result.is_err());
        assert_eq!(
            db.get_metadata("sync_token:zone").await.unwrap().as_deref(),
            Some("old-token")
        );
        assert_eq!(
            db.get_metadata("enum_config_hash")
                .await
                .unwrap()
                .as_deref(),
            Some("old-hash")
        );
        assert_eq!(
            db.get_metadata("pending_enum_config_hash")
                .await
                .unwrap()
                .as_deref(),
            Some("new-hash")
        );
    }

    #[tokio::test]
    async fn test_delete_metadata_by_prefix() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        db.set_metadata("sync_token:zone1", "tok1").await.unwrap();
        db.set_metadata("sync_token:zone2", "tok2").await.unwrap();
        db.set_metadata("config_hash", "abc").await.unwrap();

        // Only deletes matching prefix
        let deleted = db.delete_metadata_by_prefix("sync_token:").await.unwrap();
        assert_eq!(deleted, 2);

        assert_eq!(db.get_metadata("sync_token:zone1").await.unwrap(), None);
        assert_eq!(db.get_metadata("sync_token:zone2").await.unwrap(), None);
        // Unrelated key is untouched
        assert_eq!(
            db.get_metadata("config_hash").await.unwrap(),
            Some("abc".to_string())
        );

        // No-op when nothing matches
        let deleted = db.delete_metadata_by_prefix("nonexistent:").await.unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn scoped_db_sync_token_roundtrips_by_exact_scope_key() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let row = ScopedDbSyncToken {
            provider: "icloud".to_string(),
            account: "test@example.com".to_string(),
            shape_version: 1,
            scope_hash: "scope-a".to_string(),
            selected_zones_json: r#"["PrimarySync"]"#.to_string(),
            scope_json: r#"{"coverage":{"kind":"bounded-recent-count","count":1000}}"#.to_string(),
            token: "db-token-a".to_string(),
        };

        db.upsert_scoped_db_sync_token(row.clone()).await.unwrap();

        let loaded = db
            .get_scoped_db_sync_token("icloud", "test@example.com", 1, "scope-a")
            .await
            .unwrap()
            .expect("scoped token should exist");
        assert_eq!(loaded, row);
        assert!(
            db.get_scoped_db_sync_token("icloud", "test@example.com", 1, "scope-b")
                .await
                .unwrap()
                .is_none(),
            "different scope hash must not reuse the token"
        );
    }

    #[tokio::test]
    async fn scoped_db_sync_token_upsert_preserves_created_at_and_updates_token() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let mut row = ScopedDbSyncToken {
            provider: "icloud".to_string(),
            account: "test@example.com".to_string(),
            shape_version: 1,
            scope_hash: "scope-a".to_string(),
            selected_zones_json: r#"["PrimarySync"]"#.to_string(),
            scope_json: r#"{"coverage":{"kind":"complete"}}"#.to_string(),
            token: "db-token-a".to_string(),
        };
        db.upsert_scoped_db_sync_token(row.clone()).await.unwrap();
        row.token = "db-token-b".to_string();
        db.upsert_scoped_db_sync_token(row.clone()).await.unwrap();

        let loaded = db
            .get_scoped_db_sync_token("icloud", "test@example.com", 1, "scope-a")
            .await
            .unwrap()
            .expect("scoped token should exist");
        assert_eq!(loaded.token, "db-token-b");

        let conn = db.conn.lock().unwrap();
        let (created_at, updated_at): (i64, i64) = conn
            .query_row(
                "SELECT created_at, updated_at FROM scoped_db_sync_tokens \
                 WHERE provider = 'icloud' AND account = 'test@example.com' \
                    AND shape_version = 1 AND scope_hash = 'scope-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(
            updated_at >= created_at,
            "updated_at must not move backwards"
        );
    }

    #[tokio::test]
    async fn test_touch_last_seen() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("TOUCH_1")
            .checksum("ck")
            .created_at(Utc::now() - chrono::Duration::hours(1))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();

        // Backdate last_seen_at so that touch_last_seen produces a strictly greater timestamp
        {
            let conn = db.conn.lock().unwrap();
            conn.execute(
                "UPDATE assets SET last_seen_at = last_seen_at - 5 WHERE id = 'TOUCH_1'",
                [],
            )
            .unwrap();
        }

        let original_ts: i64 = {
            let conn = db.conn.lock().unwrap();
            conn.query_row(
                "SELECT last_seen_at FROM assets WHERE id = 'TOUCH_1'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };

        // Touch last_seen_at — should set it to now(), which is > backdated value
        db.touch_last_seen_many("PrimarySync", &["TOUCH_1"])
            .await
            .unwrap();

        let updated_ts: i64 = {
            let conn = db.conn.lock().unwrap();
            conn.query_row(
                "SELECT last_seen_at FROM assets WHERE id = 'TOUCH_1'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            updated_ts > original_ts,
            "last_seen_at should be updated: {updated_ts} > {original_ts}"
        );
    }

    // touch_last_seen_many must bump every id in one transaction and
    // be a no-op for an empty slice (the producer feeds an empty set
    // on libraries that don't skip anything).
    #[tokio::test]
    async fn touch_last_seen_many_bumps_every_id_in_one_batch() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for i in 0..5 {
            let id = format!("BATCH_{i}");
            let ck = format!("ck{i}");
            let fname = format!("f{i}.jpg");
            let rec = TestAssetRecord::new(&id)
                .checksum(&ck)
                .filename(&fname)
                .size(10)
                .build();
            db.upsert_seen(&rec).await.unwrap();
            db.backdate_last_seen(&id, 100);
        }

        let ids: Vec<&str> = (0..5).map(|_| "").collect();
        // Build the slice after constructing owned strings to keep them alive.
        let id_strings: Vec<String> = (0..5).map(|i| format!("BATCH_{i}")).collect();
        let id_refs: Vec<&str> = id_strings.iter().map(String::as_str).collect();
        // (ids above is just to document the slice shape.)
        let _ = ids;

        db.touch_last_seen_many("PrimarySync", &id_refs)
            .await
            .unwrap();

        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT last_seen_at FROM assets WHERE id = ?1")
            .unwrap();
        for id in &id_refs {
            let ts: i64 = stmt.query_row([*id], |r| r.get(0)).unwrap();
            assert!(ts > 100, "row {id} must be bumped past the backdated 100");
        }
    }

    #[tokio::test]
    async fn touch_last_seen_many_empty_slice_is_noop() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        // No rows; no assertion about state needed — just verify Ok(()).
        db.touch_last_seen_many("PrimarySync", &[]).await.unwrap();
    }

    #[tokio::test]
    async fn test_get_downloaded_page_scales_to_large_count() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let count: usize = 10_000;

        // Bulk-insert records directly for speed
        {
            let conn = db.conn.lock().unwrap();
            conn.execute_batch("BEGIN").unwrap();
            let mut stmt = conn
                .prepare(
                    "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, downloaded_at, local_path, local_checksum, last_seen_at)
                     VALUES ('PrimarySync', ?1, 'original', ?2, ?3, ?4, ?5, 'photo', 'downloaded', ?4, ?6, ?2, ?4)",
                )
                .unwrap();
            let now = Utc::now().timestamp();
            for i in 0..count {
                let id = format!("ASSET_{i:05}");
                let checksum = format!("cksum_{i:05}");
                let filename = format!("IMG_{i:05}.jpg");
                let path = format!("/photos/2026/01/01/{filename}");
                stmt.execute(rusqlite::params![id, checksum, filename, now, 4096, path])
                    .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        }

        // Paginate through all records
        let page_size: u32 = 1000;
        let mut total = 0usize;
        let mut offset = 0u64;
        let mut first_id = String::new();
        let mut last_id = String::new();
        loop {
            let page = db.get_downloaded_page(offset, page_size).await.unwrap();
            if page.is_empty() {
                break;
            }
            if total == 0 {
                first_id = page[0].id.to_string();
            }
            last_id = page.last().unwrap().id.to_string();
            assert!(page.iter().all(|r| r.status == AssetStatus::Downloaded));
            total += page.len();
            offset += page.len() as u64;
        }

        assert_eq!(total, count);
        assert_eq!(first_id, "ASSET_00000");
        assert_eq!(last_id, format!("ASSET_{:05}", count - 1));
    }

    // ── Gap tests: robustness and edge cases ──

    #[tokio::test]
    async fn should_download_unknown_version_size_treated_as_pending() {
        // Arrange: insert a row with a version_size string that doesn't map to any VersionSizeKey variant
        let db = SqliteStateDb::open_in_memory().unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let now = Utc::now().timestamp();
            conn.execute(
                "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at)
                 VALUES ('PrimarySync', 'AQvz7R8kP4', 'superHD', 'a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6abcd', 'IMG_4231.HEIC', ?1, 8294400, 'photo', 'pending', ?1)",
                rusqlite::params![now],
            ).unwrap();
        }

        // Act: query should_download with the same unknown version_size
        let result = db
            .should_download(
                "PrimarySync",
                "AQvz7R8kP4",
                "superHD",
                "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6abcd",
                Path::new("/photos/2026/04/IMG_4231.HEIC"),
            )
            .await
            .unwrap();

        // Assert: pending asset should need download
        assert!(result);
    }

    #[tokio::test]
    async fn upsert_seen_then_summary_counts_accurate_across_transitions() {
        // Arrange: create assets and move them through pending -> downloaded -> failed transitions
        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();

        let now = Utc::now();
        let ids = ["AEt9xLq2V0", "AEt9xLq2V1", "AEt9xLq2V2", "AEt9xLq2V3"];
        for (i, id) in ids.iter().enumerate() {
            let record = TestAssetRecord::new(id)
                .checksum(&format!(
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8{:02x}",
                    i
                ))
                .filename(&format!("IMG_{}.JPG", 1000 + i))
                .created_at(now)
                .added_at(now - chrono::Duration::days(1))
                .size(u64::try_from(4_194_304 + i * 1024).unwrap_or(0))
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        // All 4 start as pending
        let s1 = db.get_summary().await.unwrap();
        assert_eq!(s1.total_assets, 4);
        assert_eq!(s1.pending, 4);
        assert_eq!(s1.downloaded, 0);
        assert_eq!(s1.failed, 0);

        // Act: download two, fail one, leave one pending
        let path0 = dir.path().join("IMG_1000.JPG");
        fs::write(&path0, b"JPEG data").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            ids[0],
            "original",
            &path0,
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592",
            None,
        )
        .await
        .unwrap();

        let path1 = dir.path().join("IMG_1001.JPG");
        fs::write(&path1, b"JPEG data 2").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            ids[1],
            "original",
            &path1,
            "ef2d127de37b942baad06145e54b0c619a1f22327b2ebbcfbec78f5564afe39d",
            None,
        )
        .await
        .unwrap();

        db.mark_failed(
            "PrimarySync",
            ids[2],
            "original",
            "HTTP 503 Service Unavailable",
        )
        .await
        .unwrap();

        // Assert: counts reflect exact transitions
        let s2 = db.get_summary().await.unwrap();
        assert_eq!(s2.total_assets, 4);
        assert_eq!(s2.downloaded, 2);
        assert_eq!(s2.failed, 1);
        assert_eq!(s2.pending, 1);

        // Act: reset failed back to pending
        let reset_count = db.reset_failed().await.unwrap();
        assert_eq!(reset_count, 1);

        // Assert: failed count goes to 0, pending increases
        let s3 = db.get_summary().await.unwrap();
        assert_eq!(s3.total_assets, 4);
        assert_eq!(s3.downloaded, 2);
        assert_eq!(s3.failed, 0);
        assert_eq!(s3.pending, 2);
    }

    #[tokio::test]
    async fn metadata_empty_string_key_and_value() {
        // Arrange
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Act: set metadata with an empty key
        db.set_metadata("", "some_value").await.unwrap();

        // Assert: can retrieve by empty key
        let val = db.get_metadata("").await.unwrap();
        assert_eq!(val, Some("some_value".to_string()));

        // Act: set metadata with a normal key but empty value
        db.set_metadata("last_sync_token", "").await.unwrap();

        // Assert: empty value is stored and retrievable
        let val = db.get_metadata("last_sync_token").await.unwrap();
        assert_eq!(val, Some(String::new()));

        // Act: overwrite empty key with empty value
        db.set_metadata("", "").await.unwrap();
        let val = db.get_metadata("").await.unwrap();
        assert_eq!(val, Some(String::new()));
    }

    #[tokio::test]
    async fn row_to_asset_record_unknown_status_falls_back_to_pending() {
        // Arrange: manually insert a row with a status string that doesn't match any AssetStatus variant
        let db = SqliteStateDb::open_in_memory().unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let now = Utc::now().timestamp();
            conn.execute(
                "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at)
                 VALUES ('PrimarySync', 'ABx7kQ9nR2', 'original', 'b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c', 'IMG_7892.HEIC', ?1, 6_291_456, 'photo', 'corrupted_junk', ?1)",
                rusqlite::params![now],
            ).unwrap();
        }

        // Act: retrieve via get_failed (won't match 'corrupted_junk'), and get_downloaded_page also won't match.
        // Instead, query via should_download which reads the row and parses status.
        // The unknown status falls back to Pending via AssetStatus::from_str -> unwrap_or(Pending).
        let needs_download = db
            .should_download(
                "PrimarySync",
                "ABx7kQ9nR2",
                "original",
                "b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c",
                Path::new("/photos/2026/04/IMG_7892.HEIC"),
            )
            .await
            .unwrap();

        // Assert: unknown status treated as pending, which means should download
        assert!(needs_download);

        // Also verify via summary: the unknown status won't match 'downloaded', 'pending', or 'failed'
        // COUNT(CASE WHEN ...) so it counts as part of total but not any specific bucket
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 1);
        assert_eq!(summary.downloaded, 0);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
    }

    /// T-3: Each download is reflected in the state DB immediately, not batched.
    /// After marking each of 5 files as downloaded, the summary should reflect
    /// the cumulative count at every step.
    #[tokio::test]
    async fn test_downloads_reflected_immediately_not_batched() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();

        for i in 0..5u32 {
            let id = format!("ASSET_{i}");
            let record = TestAssetRecord::new(&id)
                .checksum(&format!("checksum_{i}"))
                .filename(&format!("photo_{i}.jpg"))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();

            let path = dir.path().join(format!("photo_{i}.jpg"));
            fs::write(&path, b"jpeg data").unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                &path,
                &format!("local_ck_{i}"),
                None,
            )
            .await
            .unwrap();

            // Query immediately after each download
            let summary = db.get_summary().await.unwrap();
            assert_eq!(
                summary.downloaded,
                u64::from(i + 1),
                "after downloading asset {i}, DB should show {} downloaded",
                i + 1
            );
        }

        // Final check: all 5 are downloaded
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 5);
        assert_eq!(summary.downloaded, 5);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn sync_run_zero_value_stats() {
        // Arrange
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();

        // Act: complete the sync run with all-zero stats
        let stats = SyncRunStats {
            assets_seen: 0,
            assets_downloaded: 0,
            assets_failed: 0,
            enumeration_errors: 0,
            interrupted: false,
            ..Default::default()
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();

        // Assert: summary reflects the completed run with timestamps populated
        let summary = db.get_summary().await.unwrap();
        assert!(summary.last_sync_started.is_some());
        assert!(summary.last_sync_completed.is_some());

        // Verify the raw sync_runs row has zero values
        let (seen, downloaded, failed, interrupted): (i64, i64, i64, i64) = {
            let conn = db.conn.lock().unwrap();
            conn.query_row(
                "SELECT assets_seen, assets_downloaded, assets_failed, interrupted FROM sync_runs WHERE id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).unwrap()
        };
        assert_eq!(seen, 0);
        assert_eq!(downloaded, 0);
        assert_eq!(failed, 0);
        assert_eq!(interrupted, 0);
    }

    #[tokio::test]
    async fn reset_failed_precise_count_with_mixed_statuses() {
        // Arrange: create assets across all three statuses with multiple failed entries
        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();

        // 2 downloaded
        for i in 0..2 {
            let id = format!("ADl{}mNp3Q{}", i, i);
            let record = TestAssetRecord::new(&id)
                .checksum(&format!(
                    "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48b{}",
                    i
                ))
                .filename(&format!("IMG_{}.HEIC", 2000 + i))
                .size(5_242_880)
                .build();
            db.upsert_seen(&record).await.unwrap();
            let path = dir.path().join(format!("IMG_{}.HEIC", 2000 + i));
            fs::write(&path, b"heic payload").unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                &path,
                &format!("localhash{i}"),
                None,
            )
            .await
            .unwrap();
        }

        // 3 pending (just upserted, never transitioned)
        for i in 0..3 {
            let record = TestAssetRecord::new(&format!("APn{}rWx5Z{}", i, i))
                .checksum(&format!(
                    "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009{}",
                    i
                ))
                .filename(&format!("IMG_{}.JPG", 3000 + i))
                .size(3_145_728)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        // 4 failed
        for i in 0..4 {
            let id = format!("AFl{}kRt7Y{}", i, i);
            let record = TestAssetRecord::new(&id)
                .checksum(&format!(
                    "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab3{}",
                    i
                ))
                .filename(&format!("IMG_{}.MOV", 4000 + i))
                .size(10_485_760)
                .media_type(MediaType::Video)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_failed(
                "PrimarySync",
                &id,
                "original",
                &format!("HTTP 500 attempt {i}"),
            )
            .await
            .unwrap();
        }

        // Pre-check
        let before = db.get_summary().await.unwrap();
        assert_eq!(before.total_assets, 9);
        assert_eq!(before.downloaded, 2);
        assert_eq!(before.pending, 3);
        assert_eq!(before.failed, 4);

        // Act
        let reset_count = db.reset_failed().await.unwrap();

        // Assert: exactly 4 were reset
        assert_eq!(reset_count, 4);

        let after = db.get_summary().await.unwrap();
        assert_eq!(after.total_assets, 9);
        assert_eq!(after.downloaded, 2);
        assert_eq!(after.pending, 7); // 3 original pending + 4 reset from failed
        assert_eq!(after.failed, 0);

        // Verify the formerly-failed assets have cleared error and zero attempts
        let failed_after = db.get_failed().await.unwrap();
        assert!(failed_after.is_empty());
    }

    #[tokio::test]
    async fn prepare_for_retry_resets_failed_and_stuck_pending() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();

        // 1 downloaded (should be untouched)
        let record = TestAssetRecord::new("ADwnloaded1")
            .checksum("aaaa")
            .filename("IMG_1000.HEIC")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join("IMG_1000.HEIC");
        fs::write(&path, b"payload").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ADwnloaded1",
            "original",
            &path,
            "localhash1",
            None,
        )
        .await
        .unwrap();

        // 1 normal pending (attempts = 0, should be untouched)
        let record = TestAssetRecord::new("APending1")
            .checksum("bbbb")
            .filename("IMG_2000.JPG")
            .size(2000)
            .build();
        db.upsert_seen(&record).await.unwrap();

        // 1 stuck pending (attempts > 0, should get attempts cleared)
        let record = TestAssetRecord::new("AStuck1")
            .checksum("cccc")
            .filename("IMG_3000.JPG")
            .size(3000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        // Simulate accumulated attempts by marking failed then resetting status to pending
        // but keeping attempts high (as the old bug would produce)
        db.mark_failed("PrimarySync", "AStuck1", "original", "transient error")
            .await
            .unwrap();
        // Manually set back to pending with attempts preserved (simulating the old bug)
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE assets SET status = 'pending' WHERE id = 'AStuck1'",
                [],
            )
            .unwrap();

        // 2 failed (should transition to pending)
        for i in 0..2 {
            let id = format!("AFailed{i}");
            let record = TestAssetRecord::new(&id)
                .checksum(&format!("dddd{i}"))
                .filename(&format!("IMG_400{i}.MOV"))
                .size(5000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_failed("PrimarySync", &id, "original", "HTTP 500")
                .await
                .unwrap();
        }

        let before = db.get_summary().await.unwrap();
        assert_eq!(before.downloaded, 1);
        assert_eq!(before.pending, 2); // normal + stuck
        assert_eq!(before.failed, 2);

        let (failed_reset, pending_reset, total_pending) = db
            .prepare_for_retry(None, RetryErrorRetention::Clear)
            .await
            .unwrap();

        assert_eq!(failed_reset, 2);
        assert_eq!(pending_reset, 1); // only the stuck one
        assert_eq!(total_pending, 4); // 2 original pending + 2 reset from failed

        let after = db.get_summary().await.unwrap();
        assert_eq!(after.downloaded, 1);
        assert_eq!(after.pending, 4);
        assert_eq!(after.failed, 0);

        // Verify attempt counts are all zero now
        let attempts = db.get_attempt_counts().await.unwrap();
        assert!(attempts.is_empty(), "all attempt counts should be zero");
    }

    #[tokio::test]
    async fn prepare_for_retry_preserves_only_the_authorized_error() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (id, error) in [
            (
                "truncated",
                crate::commands::reconcile::FILE_TRUNCATED_REASON,
            ),
            ("network", "HTTP 500"),
        ] {
            let record = TestAssetRecord::new(id)
                .checksum(&format!("checksum-{id}"))
                .filename(&format!("{id}.jpg"))
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_failed("PrimarySync", id, "original", error)
                .await
                .unwrap();
        }

        db.prepare_for_retry(
            None,
            RetryErrorRetention::Preserve(crate::commands::reconcile::FILE_TRUNCATED_REASON),
        )
        .await
        .unwrap();

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 2);
        let truncated = pending
            .iter()
            .find(|row| row.id.as_ref() == "truncated")
            .unwrap();
        let network = pending
            .iter()
            .find(|row| row.id.as_ref() == "network")
            .unwrap();
        assert_eq!(
            truncated.last_error.as_deref(),
            Some(crate::commands::reconcile::FILE_TRUNCATED_REASON)
        );
        assert!(network.last_error.is_none());
    }

    #[tokio::test]
    async fn prepare_for_retry_can_scope_pending_work_by_library() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let shared = "SharedSync-ONE";

        for (library, suffix) in [("PrimarySync", "primary"), (shared, "shared")] {
            let pending_id = format!("APending-{suffix}");
            let pending = TestAssetRecord::new(&pending_id)
                .library(library)
                .checksum(&format!("pending-{suffix}"))
                .filename(&format!("pending-{suffix}.jpg"))
                .build();
            db.upsert_seen(&pending).await.unwrap();
            db.mark_failed(library, &pending_id, "original", "transient")
                .await
                .unwrap();
            db.conn
                .lock()
                .unwrap()
                .execute(
                    "UPDATE assets SET status = 'pending' WHERE library = ?1 AND id = ?2",
                    rusqlite::params![library, pending_id],
                )
                .unwrap();

            let failed_id = format!("AFailed-{suffix}");
            let failed = TestAssetRecord::new(&failed_id)
                .library(library)
                .checksum(&format!("failed-{suffix}"))
                .filename(&format!("failed-{suffix}.jpg"))
                .build();
            db.upsert_seen(&failed).await.unwrap();
            db.mark_failed(library, &failed_id, "original", "HTTP 500")
                .await
                .unwrap();
        }

        let (failed_reset, pending_reset, total_pending) = db
            .prepare_for_retry(Some("PrimarySync"), RetryErrorRetention::Clear)
            .await
            .unwrap();

        assert_eq!(failed_reset, 1);
        assert_eq!(pending_reset, 1);
        assert_eq!(
            total_pending, 2,
            "only PrimarySync pending rows should drive the retry fallback gate"
        );

        let (shared_failed_reset, shared_pending_reset, shared_total_pending) = db
            .prepare_for_retry(Some(shared), RetryErrorRetention::Clear)
            .await
            .unwrap();
        assert_eq!(shared_failed_reset, 1);
        assert_eq!(shared_pending_reset, 1);
        assert_eq!(
            shared_total_pending, 2,
            "SharedSync retry work should remain untouched until its own library pass"
        );
    }

    #[tokio::test]
    async fn promote_pending_to_failed_only_affects_pending() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let dir = test_dir();

        // 1 downloaded (should be untouched)
        let record = TestAssetRecord::new("ADownloaded")
            .checksum("aaaa")
            .filename("IMG_1000.HEIC")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join("IMG_1000.HEIC");
        fs::write(&path, b"payload").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ADownloaded",
            "original",
            &path,
            "localhash",
            None,
        )
        .await
        .unwrap();

        // 2 pending dispatched this sync (should be promoted to failed)
        for i in 0..2 {
            let id = format!("APending{i}");
            let record = TestAssetRecord::new(&id)
                .checksum(&format!("bbbb{i}"))
                .filename(&format!("IMG_200{i}.JPG"))
                .size(2000)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        // 1 already failed (should be untouched)
        let record = TestAssetRecord::new("AFailed")
            .checksum("cccc")
            .filename("IMG_3000.MOV")
            .size(3000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", "AFailed", "original", "HTTP 500")
            .await
            .unwrap();

        let before = db.get_summary().await.unwrap();
        assert_eq!(before.downloaded, 1);
        assert_eq!(before.pending, 2);
        assert_eq!(before.failed, 1);

        // Gate is `last_seen_at >= seen_since`. Use a timestamp in the past
        // so every pending asset seen in this test counts as "dispatched
        // this sync" and gets promoted.
        let past = chrono::Utc::now().timestamp() - 3600;
        let promoted = db.promote_pending_to_failed(past).await.unwrap();
        assert_eq!(promoted, 2);

        let after = db.get_summary().await.unwrap();
        assert_eq!(after.downloaded, 1);
        assert_eq!(after.pending, 0);
        assert_eq!(after.failed, 3);

        // Verify the promoted assets have the right error message
        let failed = db.get_failed().await.unwrap();
        let promoted_errors: Vec<_> = failed
            .iter()
            .filter(|a| a.id.starts_with("APending"))
            .map(|a| a.last_error.as_deref())
            .collect();
        assert_eq!(promoted_errors.len(), 2);
        for error in &promoted_errors {
            assert_eq!(*error, Some("Not resolved during sync"));
        }
    }

    #[tokio::test]
    async fn provider_verification_marker_keeps_inconclusive_row_pending() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("VERIFY_UNKNOWN").build();
        db.upsert_seen(&record).await.unwrap();
        db.set_asset_verification(
            "PrimarySync",
            "VERIFY_UNKNOWN",
            "original",
            AssetVerificationState::Unknown,
            "lookup omitted record",
        )
        .await
        .unwrap();

        let promoted = db.promote_pending_to_failed(0).await.unwrap();
        assert_eq!(promoted, 0);
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.awaiting_provider_verification, 1);

        db.clear_asset_verification("PrimarySync", "VERIFY_UNKNOWN", "original")
            .await
            .unwrap();
        assert_eq!(db.promote_pending_to_failed(0).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn open_corrupt_db_returns_error() {
        let dir = test_dir();
        let path = dir.path().join("corrupt.db");

        // Write garbage bytes (not a valid SQLite header)
        fs::write(&path, b"this is not a sqlite database at all").unwrap();

        let result = SqliteStateDb::open(&path).await;
        assert!(result.is_err(), "opening a corrupt DB should fail");

        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a database"),
            "error should indicate corruption, got: {msg}"
        );
    }

    #[tokio::test]
    async fn concurrent_mark_downloaded_all_succeed() {
        use std::sync::Arc;

        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());

        // Insert 10 pending assets
        for i in 0..10 {
            let record = TestAssetRecord::new(&format!("CONCURRENT_{i}"))
                .checksum(&format!("ck_{i}"))
                .filename(&format!("photo_{i}.jpg"))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        // Spawn 10 tasks that each mark a different asset as downloaded
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let db = Arc::clone(&db);
                tokio::spawn(async move {
                    db.mark_downloaded(
                        "PrimarySync",
                        &format!("CONCURRENT_{i}"),
                        "original",
                        Path::new(&format!("/tmp/photo_{i}.jpg")),
                        &format!("hash_{i}"),
                        None,
                    )
                    .await
                })
            })
            .collect();

        // All tasks should succeed without SQLite busy errors
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        // Verify all 10 assets are downloaded
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 10);
        assert_eq!(summary.pending, 0);
    }

    #[tokio::test]
    async fn open_truncated_db_returns_error() {
        let dir = test_dir();
        let path = dir.path().join("truncated.db");

        // Write a partial SQLite header (valid magic, but truncated)
        let mut header = b"SQLite format 3\0".to_vec();
        header.extend_from_slice(&[0u8; 16]); // truncated page header
        fs::write(&path, &header).unwrap();

        let result = SqliteStateDb::open(&path).await;
        assert!(result.is_err(), "opening a truncated DB should fail");
    }

    // Regression test for #264: on a fresh install the cookie/data directory
    // doesn't exist yet for commands that open the DB before auth runs
    // (import-existing, status, verify, reset, reconcile). SqliteStateDb::open
    // must create the parent directory itself rather than letting SQLite fail
    // with a generic "unable to open database file" (error code 14).
    #[tokio::test]
    async fn open_creates_missing_parent_directory() {
        let dir = test_dir();
        let nested = dir.path().join("does/not/exist/yet");
        let path = nested.join("state.db");

        assert!(!nested.exists(), "precondition: parent dir must be missing");

        let db = SqliteStateDb::open(&path)
            .await
            .expect("open should create the missing parent directory");

        assert!(nested.is_dir(), "parent directory should be created");
        assert!(path.is_file(), "DB file should be created");

        // Sanity: the DB is actually usable, not just opened then closed.
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 0);
        assert_eq!(summary.pending, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_returns_parent_dir_error_on_unwritable_parent() {
        use std::os::unix::fs::PermissionsExt;

        // Skip when running as root: 0o555 doesn't restrict root, so the
        // mkdir would succeed and the assertion below would falsely fail.
        // SAFETY: libc::geteuid() is a stateless POSIX FFI call with no
        // preconditions, no side effects, and a uid_t return value; it cannot
        // violate Rust memory safety.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }

        let dir = test_dir();
        let readonly = dir.path().join("readonly");
        tokio::fs::create_dir(&readonly).await.unwrap();
        tokio::fs::set_permissions(&readonly, fs::Permissions::from_mode(0o555))
            .await
            .unwrap();

        let path = readonly.join("nested/state.db");
        let result = SqliteStateDb::open(&path).await;

        // Restore writable permissions so TempDir cleanup can remove it.
        tokio::fs::set_permissions(&readonly, fs::Permissions::from_mode(0o755))
            .await
            .unwrap();

        let err = result.expect_err("expected ParentDir error on read-only parent");
        match &err {
            StateError::ParentDir { path: p, .. } => {
                assert!(
                    p.starts_with(&readonly),
                    "ParentDir path {} should be under {}",
                    p.display(),
                    readonly.display(),
                );
            }
            other => panic!("expected StateError::ParentDir, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_get_attempt_counts() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for (library, id) in [
            ("PrimarySync", "A"),
            ("PrimarySync", "B"),
            ("SharedSync-AAAA", "A"),
        ] {
            let record = TestAssetRecord::new(id)
                .library(library)
                .checksum(&format!("ck_{id}"))
                .filename(&format!("{id}.jpg"))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        db.mark_failed("PrimarySync", "A", "original", "error 1")
            .await
            .unwrap();
        db.mark_failed("PrimarySync", "A", "original", "error 2")
            .await
            .unwrap();
        db.mark_failed("PrimarySync", "A", "original", "error 3")
            .await
            .unwrap();
        db.mark_failed("PrimarySync", "B", "original", "error 1")
            .await
            .unwrap();
        db.mark_failed("SharedSync-AAAA", "A", "original", "shared error 1")
            .await
            .unwrap();

        let counts = db.get_attempt_counts().await.unwrap();
        assert_eq!(
            counts.get(&("PrimarySync".to_string(), "A".to_string())),
            Some(&3)
        );
        assert_eq!(
            counts.get(&("PrimarySync".to_string(), "B".to_string())),
            Some(&1)
        );
        assert_eq!(
            counts.get(&("SharedSync-AAAA".to_string(), "A".to_string())),
            Some(&1)
        );
    }

    #[tokio::test]
    async fn test_get_attempt_counts_empty() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let counts = db.get_attempt_counts().await.unwrap();
        assert!(counts.is_empty());
    }

    // ── Gap: mark_downloaded on non-existent record (no upsert_seen) ──

    #[tokio::test]
    async fn mark_downloaded_without_upsert_seen_returns_asset_row_missing() {
        // The UPDATE matches zero rows when the asset wasn't recorded
        // via upsert_seen. The caller must see this loudly so a missed
        // dispatch step doesn't silently drop a downloaded file.
        let db = SqliteStateDb::open_in_memory().unwrap();

        let err = db
            .mark_downloaded(
                "PrimarySync",
                "NEVER_SEEN",
                "original",
                Path::new("/tmp/never.jpg"),
                "abc123",
                None,
            )
            .await
            .expect_err("mark_downloaded on unknown asset must err");
        match err {
            StateError::AssetRowMissing {
                asset_id,
                version_size,
            } => {
                assert_eq!(asset_id, "NEVER_SEEN");
                assert_eq!(version_size, "original");
            }
            other => panic!("expected AssetRowMissing, got {other:?}"),
        }
    }

    /// A regression that increases the rate of zero-row `mark_downloaded`
    /// calls (e.g. a producer-dispatch invariant quietly broken) needs to
    /// be visible in /metrics, not only in logs / per-asset errors. Pin
    /// the counter increment so the wiring can't be silently dropped on
    /// a future refactor.
    #[tokio::test]
    async fn mark_downloaded_zero_rows_increments_metric_counter() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let before = crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.get();
        let _ = db
            .mark_downloaded(
                "PrimarySync",
                "NEVER_SEEN_FOR_METRIC",
                "original",
                Path::new("/tmp/never_metric.jpg"),
                "abc123",
                None,
            )
            .await;
        let after = crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.get();

        assert!(
            after > before,
            "counter should advance by at least 1 (other parallel tests may also \
             increment); got before={before} after={after}"
        );
    }

    /// A `mark_failed` call without a prior `upsert_seen` is a
    /// producer-dispatch invariant violation. Surface it as a typed
    /// `StateError::Invariant` so callers can't silently treat the
    /// failure as persisted, while still incrementing the metric for
    /// observability.
    #[tokio::test]
    async fn mark_failed_zero_rows_returns_invariant_and_increments_metric() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let before = crate::metrics::MARK_FAILED_ZERO_ROWS.get();
        let err = db
            .mark_failed(
                "PrimarySync",
                "NEVER_SEEN_FOR_FAILED_METRIC",
                "original",
                "simulated transient error",
            )
            .await
            .expect_err("mark_failed on unknown row must surface as Invariant");
        match &err {
            StateError::Invariant { operation, detail } => {
                assert_eq!(*operation, "mark_failed");
                assert!(
                    detail.contains("NEVER_SEEN_FOR_FAILED_METRIC"),
                    "detail must include the asset id; got: {detail}"
                );
            }
            other => panic!("expected StateError::Invariant, got {other:?}"),
        }
        let after = crate::metrics::MARK_FAILED_ZERO_ROWS.get();

        assert!(
            after > before,
            "MARK_FAILED_ZERO_ROWS must advance by at least 1 (parallel tests \
             may also increment); got before={before} after={after}"
        );

        // The asset must NOT have been inserted as a side effect.
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.total_assets, 0);
    }

    // ── import_adopt: atomic upsert + mark-downloaded ───────────────────

    #[tokio::test]
    async fn import_adopt_persists_downloaded_row_in_one_call() {
        // The whole point of import_adopt: one transactional call that
        // leaves the row fully `downloaded` with `local_path` set. No
        // separate upsert_seen + mark_downloaded sequence at the call site.
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ADOPT_ONE")
            .checksum("ck_adopt")
            .filename("photo.jpg")
            .size(2048)
            .build();
        let local_path = PathBuf::from("/tmp/photos/photo.jpg");

        db.import_adopt(
            &record,
            &local_path,
            "local-ck-abc",
            2048,
            Some(1_700_000_000),
        )
        .await
        .expect("import_adopt should succeed on a fresh row");

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 1);
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.failed, 0);

        let pages = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(pages.len(), 1);
        let row = &pages[0];
        assert_eq!(&*row.id, "ADOPT_ONE");
        assert_eq!(row.status, AssetStatus::Downloaded);
        assert_eq!(row.local_path.as_deref(), Some(local_path.as_path()));
        assert_eq!(row.local_checksum.as_deref(), Some("local-ck-abc"));
    }

    #[tokio::test]
    async fn import_adopt_is_idempotent_on_repeat_calls() {
        // Re-running an import scan over the same on-disk files must not
        // duplicate rows or drop the downloaded state — the second adopt
        // should see the existing downloaded row and leave it healthy.
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ADOPT_IDEMP")
            .checksum("ck_idemp")
            .filename("idemp.jpg")
            .size(1024)
            .build();
        let local_path = PathBuf::from("/tmp/photos/idemp.jpg");

        for _ in 0..2 {
            db.import_adopt(
                &record,
                &local_path,
                "local-ck-idemp",
                1024,
                Some(1_700_000_001),
            )
            .await
            .expect("import_adopt should be idempotent");
        }

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 1, "no duplicate row");
        assert_eq!(summary.downloaded, 1);
    }

    #[tokio::test]
    async fn import_adopt_promotes_existing_pending_row_to_downloaded() {
        // A prior interrupted scan may have left a pending row (pre-PR-5
        // behavior). Re-running the import must recover by upserting the
        // metadata and flipping the row to downloaded in one transaction.
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ADOPT_RESUME")
            .checksum("ck_resume")
            .filename("resume.jpg")
            .size(4096)
            .build();
        db.upsert_seen(&record).await.unwrap();

        // Sanity: row is pending with no local_path.
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.downloaded, 0);

        let local_path = PathBuf::from("/tmp/photos/resume.jpg");
        db.import_adopt(
            &record,
            &local_path,
            "local-ck-resume",
            4096,
            Some(1_700_000_002),
        )
        .await
        .expect("import_adopt should promote pending → downloaded");

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.downloaded, 1);

        let pages = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(pages[0].local_path.as_deref(), Some(local_path.as_path()));
        assert_eq!(pages[0].local_checksum.as_deref(), Some("local-ck-resume"));
    }

    // ── Gap: mark_failed increments download_attempts cumulatively ────

    #[tokio::test]
    async fn mark_failed_increments_attempts_cumulatively() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("RETRY_ME")
            .checksum("ck_retry")
            .filename("photo.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();

        // Fail three times
        for i in 1..=3 {
            db.mark_failed("PrimarySync", "RETRY_ME", "original", &format!("error {i}"))
                .await
                .unwrap();
        }

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].download_attempts, 3,
            "download_attempts should be 3 after three failures"
        );
        assert_eq!(
            failed[0].last_error.as_deref(),
            Some("error 3"),
            "last_error should be the most recent failure"
        );
    }

    // Promote: only the asset the producer touched this sync is a candidate.
    // Anything with a stale last_seen_at is filtered / out of scope and must
    // stay pending. See issue #211.

    #[tokio::test]
    async fn promote_pending_to_failed_skips_stale_last_seen() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // OLD_ASSET: upserted before this sync (last_seen_at 1 hour ago).
        // Stands in for filtered-out / out-of-scope / remotely deleted assets
        // whose last_seen_at didn't get refreshed this sync.
        let old_record = TestAssetRecord::new("OLD_ASSET")
            .checksum("ck_old")
            .filename("old.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&old_record).await.unwrap();
        db.backdate_last_seen("OLD_ASSET", chrono::Utc::now().timestamp() - 3600);

        // NEW_ASSET: producer called upsert_seen this sync, consumer never
        // finalized. This is the stuck-pipeline case the function exists
        // to catch.
        let new_record = TestAssetRecord::new("NEW_ASSET")
            .checksum("ck_new")
            .filename("new.jpg")
            .size(2000)
            .build();
        db.upsert_seen(&new_record).await.unwrap();

        // sync_started_at: 30 minutes ago. OLD_ASSET (1h ago) is before the
        // boundary and must be left alone. NEW_ASSET (now) is after and
        // must be promoted.
        let sync_started_at = chrono::Utc::now().timestamp() - 1800;
        let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(
            promoted, 1,
            "only NEW_ASSET (dispatched this sync) should be promoted"
        );
        assert_eq!(summary.pending, 1, "OLD_ASSET should remain pending");
        assert_eq!(summary.failed, 1, "NEW_ASSET should be failed");

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(&*failed[0].id, "NEW_ASSET");
    }

    #[tokio::test]
    async fn prune_stale_pending_not_seen_since_deletes_only_old_pending_for_library() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let old_primary = TestAssetRecord::new("OLD_PRIMARY")
            .checksum("ck_old_primary")
            .filename("old-primary.jpg")
            .size(1000)
            .build();
        let fresh_primary = TestAssetRecord::new("FRESH_PRIMARY")
            .checksum("ck_fresh_primary")
            .filename("fresh-primary.jpg")
            .size(1000)
            .build();
        let old_shared = TestAssetRecord::new("OLD_SHARED")
            .library("SharedSync")
            .checksum("ck_old_shared")
            .filename("old-shared.jpg")
            .size(1000)
            .build();
        let downloaded = TestAssetRecord::new("DOWNLOADED_PRIMARY")
            .checksum("ck_downloaded_primary")
            .filename("downloaded-primary.jpg")
            .size(1000)
            .build();

        db.upsert_seen(&old_primary).await.unwrap();
        db.upsert_seen(&fresh_primary).await.unwrap();
        db.upsert_seen(&old_shared).await.unwrap();
        db.upsert_seen(&downloaded).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DOWNLOADED_PRIMARY",
            "original",
            Path::new("/tmp/downloaded-primary.jpg"),
            "local",
            Some("download"),
        )
        .await
        .unwrap();

        let sync_started_at = chrono::Utc::now().timestamp() - 1800;
        db.backdate_last_seen("OLD_PRIMARY", sync_started_at - 10);
        db.backdate_last_seen("OLD_SHARED", sync_started_at - 10);

        let pruned = db
            .prune_stale_pending_not_seen_since("PrimarySync", sync_started_at)
            .await
            .unwrap();

        assert_eq!(pruned, 1);
        let pending = db.get_pending().await.unwrap();
        let ids: HashSet<&str> = pending.iter().map(|row| row.id.as_ref()).collect();
        assert!(!ids.contains("OLD_PRIMARY"));
        assert!(ids.contains("FRESH_PRIMARY"));
        assert!(ids.contains("OLD_SHARED"));
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1, "downloaded rows must not change");
    }

    // Regression test for #211: a pending asset the producer didn't enumerate
    // this sync (because a filter excluded it, the album scope changed, or
    // the upstream record was deleted) must not be promoted to failed.
    // Previously, prepare_for_retry + unseen + promote would loop this asset
    // between pending and failed on every sync.

    #[tokio::test]
    async fn promote_pending_to_failed_does_not_loop_filtered_asset() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Sync 1: asset enumerated, upsert_seen, then never got finalized
        // and was subsequently "lost" from the enumeration scope (e.g. user
        // added --skip-videos). We simulate that by backdating last_seen_at.
        let record = TestAssetRecord::new("GHOST")
            .checksum("ck_ghost")
            .filename("ghost.mov")
            .size(4096)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.backdate_last_seen("GHOST", chrono::Utc::now().timestamp() - 86400);

        // Sync 2 begins now. The asset is filtered out - no upsert_seen, no
        // touch_last_seen. last_seen_at stays at one_day_ago.
        let sync_2_start = chrono::Utc::now().timestamp();
        let promoted = db.promote_pending_to_failed(sync_2_start).await.unwrap();
        assert_eq!(promoted, 0, "filtered asset must not be promoted");

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.failed, 0);

        // Sync 3, 4, 5: same filter still applied. Assert the state is
        // stable across repeated calls.
        for _ in 0..3 {
            let start = chrono::Utc::now().timestamp();
            let promoted = db.promote_pending_to_failed(start).await.unwrap();
            assert_eq!(promoted, 0, "stable: filtered asset stays pending");
        }
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.pending, 1);
        assert_eq!(summary.failed, 0);
    }

    // Canary for the touch_last_seen contract: if a caller bumps
    // last_seen_at on a pending row, promote_pending_to_failed WILL promote
    // it. The touch_last_seen trait docs warn against this. This test locks
    // in that behavior so a silent regression (e.g. an unsafe touch added
    // to a skip path) is caught.

    #[tokio::test]
    async fn touch_last_seen_on_pending_row_causes_promotion_at_sync_end() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // A pending row carried over from a prior sync (backdated).
        let record = TestAssetRecord::new("PENDING_CARRYOVER")
            .checksum("ck_p")
            .filename("pending.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.backdate_last_seen("PENDING_CARRYOVER", chrono::Utc::now().timestamp() - 86400);

        // Capture sync_started_at BEFORE touch_last_seen runs.
        let sync_started_at = chrono::Utc::now().timestamp();

        // Caller violates the contract: bumps last_seen_at on a pending row.
        db.touch_last_seen_many("PrimarySync", &["PENDING_CARRYOVER"])
            .await
            .unwrap();

        let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
        assert_eq!(
            promoted, 1,
            "touch_last_seen on a pending row must cause promotion at sync end"
        );

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(&*failed[0].id, "PENDING_CARRYOVER");
    }

    // ── Gap: upsert_seen preserves downloaded status across updates ───

    #[tokio::test]
    async fn upsert_seen_preserves_downloaded_status_and_path() {
        let dir = test_dir();
        let file_path = dir.path().join("keep_me.jpg");
        fs::write(&file_path, b"content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        // Insert and mark downloaded
        let record = TestAssetRecord::new("PRESERVE")
            .checksum("ck_v1")
            .filename("keep_me.jpg")
            .size(7)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "PRESERVE",
            "original",
            &file_path,
            "hash_v1",
            None,
        )
        .await
        .unwrap();

        // Re-upsert with updated metadata for the same provider version.
        let updated = TestAssetRecord::new("PRESERVE")
            .checksum("ck_v1")
            .filename("keep_me.jpg")
            .size(7)
            .build();
        db.upsert_seen(&updated).await.unwrap();

        // Status should still be "downloaded", not reset to "pending"
        let summary = db.get_summary().await.unwrap();
        assert_eq!(
            summary.downloaded, 1,
            "upsert_seen should preserve downloaded status"
        );
        assert_eq!(
            summary.pending, 0,
            "upsert_seen should NOT reset to pending"
        );
    }

    // ── Gap: mark_downloaded with download_checksum ───────────────────

    #[tokio::test]
    async fn mark_downloaded_stores_download_checksum() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("DL_CK")
            .checksum("api_ck")
            .filename("photo.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DL_CK",
            "original",
            Path::new("/photos/photo.jpg"),
            "local_sha256",
            Some("pre_exif_sha256"),
        )
        .await
        .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DL_CK",
            "original",
            Path::new("/photos/reconciled/photo.jpg"),
            "reconciled_local_sha256",
            None,
        )
        .await
        .unwrap();

        // Verify via get_downloaded_page that the asset is downloaded
        let page = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(&*page[0].id, "DL_CK");
        assert_eq!(
            page[0].local_checksum.as_deref(),
            Some("reconciled_local_sha256"),
            "the latest local checksum should be stored"
        );
        let conn = db.acquire_lock("verify download checksum").unwrap();
        let download_checksum: Option<String> = conn
            .query_row(
                "SELECT download_checksum FROM assets \
                 WHERE library = 'PrimarySync' AND id = 'DL_CK' AND version_size = 'original'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            download_checksum.as_deref(),
            Some("pre_exif_sha256"),
            "path reconciliation without downloaded-byte evidence must preserve the prior hash"
        );
    }

    // ── v5 metadata round-trip ──────────────────────────────────────────

    #[tokio::test]
    async fn upsert_seen_persists_and_roundtrips_metadata() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let mut meta = AssetMetadata {
            source: Some("icloud".into()),
            is_favorite: true,
            rating: Some(4),
            latitude: Some(37.7749),
            longitude: Some(-122.4194),
            altitude: Some(17.0),
            orientation: Some(6),
            duration_secs: Some(12.5),
            timezone_offset: Some(-28800),
            width: Some(4032),
            height: Some(3024),
            title: Some("A caption".into()),
            keywords: Some(r#"["vacation","beach"]"#.into()),
            description: Some("A longer description".into()),
            media_subtype: Some("portrait".into()),
            burst_id: Some("burst_abc".into()),
            is_hidden: false,
            is_archived: false,
            modified_at: Some(Utc.timestamp_opt(1_700_000_000, 0).unwrap()),
            is_deleted: false,
            deleted_at: None,
            provider_data: Some(r#"{"containerId":"x"}"#.into()),
            metadata_hash: None,
        };
        meta.refresh_hash();
        let hash = meta.metadata_hash.clone().unwrap();
        let record = TestAssetRecord::new("META_1")
            .checksum("ck1")
            .filename("photo.jpg")
            .metadata(meta)
            .build();
        db.upsert_seen(&record).await.unwrap();

        let page = db.get_downloaded_page(0, 10).await.unwrap();
        assert!(
            page.is_empty(),
            "pending rows should not be in downloaded page"
        );
        // pull via get_pending to verify round-trip
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        let got = &pending[0];
        assert_eq!(got.metadata.source.as_deref(), Some("icloud"));
        assert!(got.metadata.is_favorite);
        assert_eq!(got.metadata.rating, Some(4));
        assert_eq!(got.metadata.latitude, Some(37.7749));
        assert_eq!(got.metadata.longitude, Some(-122.4194));
        assert_eq!(got.metadata.altitude, Some(17.0));
        assert_eq!(got.metadata.orientation, Some(6));
        assert_eq!(got.metadata.duration_secs, Some(12.5));
        assert_eq!(got.metadata.timezone_offset, Some(-28800));
        assert_eq!(got.metadata.width, Some(4032));
        assert_eq!(got.metadata.height, Some(3024));
        assert_eq!(got.metadata.title.as_deref(), Some("A caption"));
        assert_eq!(
            got.metadata.keywords.as_deref(),
            Some(r#"["vacation","beach"]"#)
        );
        assert_eq!(
            got.metadata.description.as_deref(),
            Some("A longer description")
        );
        assert_eq!(got.metadata.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(got.metadata.burst_id.as_deref(), Some("burst_abc"));
        assert_eq!(got.metadata.metadata_hash.as_deref(), Some(hash.as_str()));
    }

    #[tokio::test]
    async fn upsert_seen_computes_hash_when_caller_omits_it() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let meta = AssetMetadata {
            is_favorite: true,
            ..AssetMetadata::default()
        };
        let record = TestAssetRecord::new("META_2").metadata(meta).build();
        db.upsert_seen(&record).await.unwrap();
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            pending[0].metadata.metadata_hash.is_some(),
            "upsert_seen must populate metadata_hash even when caller omits it"
        );
    }

    #[tokio::test]
    async fn upsert_seen_updates_metadata_on_conflict() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let initial = TestAssetRecord::new("META_3")
            .metadata(AssetMetadata {
                is_favorite: false,
                title: Some("old".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&initial).await.unwrap();

        let updated = TestAssetRecord::new("META_3")
            .metadata(AssetMetadata {
                is_favorite: true,
                title: Some("new".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&updated).await.unwrap();

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending[0].metadata.is_favorite);
        assert_eq!(pending[0].metadata.title.as_deref(), Some("new"));
    }

    #[tokio::test]
    async fn metadata_paths_finalize_atomically_and_complete_independently() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = TestAssetRecord::new("copies").checksum("provider").build();
        db.upsert_seen(&record).await.unwrap();
        db.acquire_lock("test_path_registration_failure")
            .unwrap()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_path_registration BEFORE INSERT ON asset_metadata_paths \
             BEGIN SELECT RAISE(ABORT, 'path registration failure'); END;",
            )
            .unwrap();
        assert!(
            db.mark_downloaded(
                "PrimarySync",
                "copies",
                "original",
                Path::new("/photos/a.jpg"),
                "local-a",
                None
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.acquire_lock("test_atomic_finalize")
                .unwrap()
                .query_row(
                    "SELECT status, local_path FROM assets WHERE id = 'copies'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                )
                .unwrap(),
            ("pending".into(), None)
        );
        db.acquire_lock("test_allow_path_registration")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_path_registration")
            .unwrap();
        for (path, checksum) in [("/photos/a.jpg", "local-a"), ("/photos/b.jpg", "local-b")] {
            db.mark_downloaded(
                "PrimarySync",
                "copies",
                "original",
                Path::new(path),
                checksum,
                Some("download"),
            )
            .await
            .unwrap();
        }
        db.record_metadata_write_failure("PrimarySync", "copies", "original")
            .await
            .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                Some(&["PrimarySync"]),
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].asset.local_checksum.as_deref(), Some("local-a"));
        assert_eq!(pending[1].asset.local_checksum.as_deref(), Some("local-b"));
        assert!(
            db.finish_metadata_rewrite(
                &pending[0],
                MetadataRewriteQueue::Ordinary,
                Some("rewritten-a"),
                Some("local-a"),
                MetadataRewriteCompletion::Ordinary
            )
            .await
            .unwrap()
        );
        let remaining = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].local_path.as_deref(),
            Some(Path::new("/photos/b.jpg"))
        );
        assert_eq!(remaining[0].local_checksum.as_deref(), Some("local-b"));
        db.record_metadata_write_failure("PrimarySync", "copies", "original")
            .await
            .unwrap();
        assert!(
            !db.finish_metadata_rewrite(
                &pending[0],
                MetadataRewriteQueue::Ordinary,
                Some("stale-a"),
                Some("local-a"),
                MetadataRewriteCompletion::Ordinary
            )
            .await
            .unwrap()
        );
        let copies = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(copies.len(), 2);
        assert_eq!(copies[0].local_checksum.as_deref(), Some("rewritten-a"));
        assert_eq!(copies[1].local_checksum.as_deref(), Some("local-b"));
        assert!(
            db.finish_metadata_rewrite(
                &pending[1],
                MetadataRewriteQueue::Ordinary,
                Some("local-b"),
                None,
                MetadataRewriteCompletion::Ordinary
            )
            .await
            .unwrap()
        );
        let only_additional = db.get_pending_metadata_rewrites(1).await.unwrap();
        assert_eq!(only_additional.len(), 1);
        assert_eq!(
            only_additional[0].local_path.as_deref(),
            Some(Path::new("/photos/a.jpg"))
        );
        assert!(
            db.get_metadata_retry_markers().await.unwrap().contains(&(
                "PrimarySync".into(),
                "copies".into(),
                "original".into()
            )),
            "an additional copy must remain visible after the catalogue copy completes"
        );
    }

    #[tokio::test]
    async fn additional_path_prepared_capture_receipt_blocks_stale_metadata() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = TestAssetRecord::new("copies")
            .checksum("provider")
            .metadata(AssetMetadata {
                metadata_hash: Some("metadata-v1".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        for path in ["/photos/a.jpg", "/photos/b.jpg"] {
            db.mark_downloaded(
                "PrimarySync",
                "copies",
                "original",
                Path::new(path),
                "local",
                None,
            )
            .await
            .unwrap();
        }
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "copies",
            (&record.metadata, record.created_at, record.added_at),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
        assert!(
            db.record_capture_repair_prepared(&pending[0], "prepared", 42)
                .await
                .unwrap()
                .is_some()
        );
        db.mark_downloaded(
            "PrimarySync",
            "copies",
            "original",
            Path::new("/photos/a.jpg"),
            "local",
            None,
        )
        .await
        .unwrap();
        let switched = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            switched.len(),
            2,
            "changing the catalogue path must not hide either copy's debt"
        );
        assert!(matches!(
            switched[0].capture_repair_receipt,
            Some(CaptureRepairReceipt::Prepared { .. })
        ));
        let mut changed = (*record.metadata).clone();
        changed.metadata_hash = Some("metadata-v2".into());
        assert!(
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "copies",
                (&changed, record.created_at, record.added_at),
                true,
                true,
                METADATA_CAPTURE_REVISION
            )
            .await
            .is_err()
        );
        let changed_record = TestAssetRecord::new("copies")
            .checksum("provider")
            .metadata(changed)
            .build();
        assert!(db.upsert_seen(&changed_record).await.is_err());
        let preserved = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            preserved[0].capture_repair_receipt,
            Some(CaptureRepairReceipt::Prepared {
                metadata_hash: "metadata-v1".into(),
                output_checksum: "prepared".into(),
                output_size: 42
            })
        );
        assert_eq!(
            preserved[1].capture_repair_receipt,
            Some(CaptureRepairReceipt::Pending {
                metadata_hash: "metadata-v1".into()
            })
        );
    }

    #[tokio::test]
    async fn album_grouping_projection_has_bounded_sql_work() {
        use rusqlite::StatementStatus;

        const SINGLE_MEMBER_STEP_LIMIT: i32 = 300;
        const STEPS_PER_MEMBER_LIMIT: i32 = 150;
        for members in [128, 512] {
            let db = SqliteStateDb::open_in_memory().unwrap();
            db.upsert_album_container("PrimarySync", "trip", "Trip", "album")
                .await
                .unwrap();
            {
                let mut conn = db.acquire_lock("seed_populated_album").unwrap();
                let tx = conn.transaction().unwrap();
                for index in 0..members {
                    let child = format!("child-{index}");
                    let master = format!("master-{index}");
                    for id in [&child, &master] {
                        tx.execute(
                            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at) VALUES ('PrimarySync', ?1, 'original', 'provider', 'image.jpg', 0, 10, 'photo', 'downloaded', 0)",
                            [id],
                        ).unwrap();
                    }
                    tx.execute(
                        "INSERT INTO legacy_master_state_owners VALUES ('PrimarySync', ?1, ?2, 0)",
                        [&master, &child],
                    )
                    .unwrap();
                    // Missing master hints must not hide the durable legacy owner.
                    tx.execute("INSERT INTO asset_album_memberships VALUES ('PrimarySync', ?1, NULL, 'trip', 1, 0, 'icloud', 0)", [&child]).unwrap();
                }
                tx.commit().unwrap();
            }
            let steps = || {
                db.acquire_lock("projection_sql_work")
                    .unwrap()
                    .prepare_cached(ALBUM_GROUPING_STATE_IDS_SQL)
                    .unwrap()
                    .reset_status(StatementStatus::VmStep)
            };
            let dirty = || {
                let conn = db.acquire_lock("projection_dirty_rows").unwrap();
                conn.query_row(
                    "SELECT COUNT(*) FROM assets WHERE metadata_write_failed_at IS NOT NULL",
                    [],
                    |row| row.get::<_, i32>(0),
                )
                .unwrap()
            };
            steps();
            for expected_dirty in [2, 0] {
                db.upsert_album_membership_delta("PrimarySync", "trip", "child-0", None, "icloud")
                    .await
                    .unwrap();
                let work = steps();
                assert!(
                    work > 0 && work <= SINGLE_MEMBER_STEP_LIMIT,
                    "{members} members: {work} single-member VM steps"
                );
                assert_eq!(
                    db.get_asset_groupings("PrimarySync", &["child-0", "master-0"])
                        .await
                        .unwrap()
                        .albums,
                    [
                        ("child-0".into(), "Trip".into()),
                        ("master-0".into(), "Trip".into())
                    ]
                );
                assert_eq!(dirty(), expected_dirty);
                for id in ["child-0", "master-0"] {
                    db.clear_metadata_write_failure("PrimarySync", id, "original")
                        .await
                        .unwrap();
                }
            }
            for expected_dirty in [members * 2, 0] {
                db.upsert_album_container("PrimarySync", "trip", "Renamed", "album")
                    .await
                    .unwrap();
                let work = steps();
                assert!(
                    work > 0 && work <= members * STEPS_PER_MEMBER_LIMIT,
                    "{members} members: {work} container VM steps"
                );
                let groups = db.get_all_asset_albums("PrimarySync").await.unwrap();
                assert_eq!(groups.len(), usize::try_from(members * 2).unwrap());
                assert!(groups.iter().all(|(_, name)| name == "Renamed"));
                assert_eq!(
                    dirty(),
                    expected_dirty,
                    "unchanged projection must not queue work"
                );
                db.acquire_lock("clear_projection_debt")
                    .unwrap()
                    .execute("UPDATE assets SET metadata_write_failed_at = NULL", [])
                    .unwrap();
            }
            assert_eq!(dirty(), 0);
        }
    }

    #[tokio::test]
    async fn album_grouping_projection_respects_legacy_state_ownership() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for id in ["master", "child-b"] {
            db.upsert_seen(&TestAssetRecord::new(id).build())
                .await
                .unwrap();
        }
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", "master", "child-a")
                .await
                .unwrap()
        );
        db.upsert_album_container("PrimarySync", "trip", "Trip", "album")
            .await
            .unwrap();
        db.upsert_album_membership_delta(
            "PrimarySync",
            "trip",
            "child-b",
            Some("master"),
            "icloud",
        )
        .await
        .unwrap();
        assert!(
            db.get_asset_groupings("PrimarySync", &["master"])
                .await
                .unwrap()
                .albums
                .is_empty()
        );
        assert_eq!(
            db.get_asset_groupings("PrimarySync", &["child-b"])
                .await
                .unwrap()
                .albums,
            [("child-b".into(), "Trip".into())]
        );
        db.upsert_album_membership_delta(
            "PrimarySync",
            "trip",
            "child-a",
            Some("master"),
            "icloud",
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_asset_groupings("PrimarySync", &["master"])
                .await
                .unwrap()
                .albums,
            [("master".into(), "Trip".into())]
        );
        db.mark_album_membership_deleted("PrimarySync", "trip", "child-b")
            .await
            .unwrap();
        assert!(
            db.get_asset_groupings("PrimarySync", &["child-b"])
                .await
                .unwrap()
                .albums
                .is_empty()
        );
        assert_eq!(
            db.get_asset_groupings("PrimarySync", &["master"])
                .await
                .unwrap()
                .albums,
            [("master".into(), "Trip".into())]
        );
    }

    #[tokio::test]
    async fn album_grouping_mutations_keep_atomic_retry_evidence() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (library, id) in [
            ("PrimarySync", "child"),
            ("PrimarySync", "sibling"),
            ("SharedSync", "child"),
        ] {
            db.upsert_seen(&TestAssetRecord::new(id).library(library).build())
                .await
                .unwrap();
        }
        db.add_asset_album("PrimarySync", "child", "External", "external-import")
            .await
            .unwrap();
        db.upsert_album_container("PrimarySync", "trip", "Trip", "album")
            .await
            .unwrap();
        let generation = db
            .start_album_membership_snapshot("PrimarySync", "trip", None)
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "trip",
            generation,
            "child",
            Some("master"),
            "icloud",
        )
        .await
        .unwrap();
        let groups = db
            .get_asset_groupings("PrimarySync", &["child"])
            .await
            .unwrap();
        assert_eq!(
            groups.albums,
            [
                ("child".into(), "External".into()),
                ("child".into(), "Trip".into())
            ]
        );
        db.clear_metadata_write_failure("PrimarySync", "child", "original")
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "trip",
            generation,
            "child",
            Some("master"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot("PrimarySync", "trip", generation)
            .await
            .unwrap();
        let dirty = || {
            db.acquire_lock("test_grouping_markers")
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE metadata_write_failed_at IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        assert_eq!(dirty(), 0, "replayed membership must not queue a rewrite");
        db.acquire_lock("test_grouping_marker_failure").unwrap().execute_batch(
            "CREATE TEMP TRIGGER fail_grouping_marker BEFORE UPDATE OF metadata_write_failed_at ON assets \
             BEGIN SELECT RAISE(ABORT, 'grouping marker failure'); END;",
        ).unwrap();
        assert!(
            db.mark_album_membership_deleted("PrimarySync", "trip", "child")
                .await
                .is_err()
        );
        assert_eq!(
            db.get_asset_groupings("PrimarySync", &["child"])
                .await
                .unwrap()
                .albums,
            groups.albums
        );
        assert_eq!(
            db.get_live_selected_album_memberships_for_asset("PrimarySync", "child", &["trip"])
                .await
                .unwrap()
                .len(),
            1
        );
        db.acquire_lock("test_grouping_marker_recovery")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_grouping_marker")
            .unwrap();
        db.mark_album_membership_deleted("PrimarySync", "trip", "child")
            .await
            .unwrap();
        assert_eq!(
            dirty(),
            1,
            "only the changed child in this library is dirty"
        );
        assert_eq!(
            db.get_asset_groupings("PrimarySync", &["child"])
                .await
                .unwrap()
                .albums,
            [("child".into(), "External".into())]
        );
        db.clear_metadata_write_failure("PrimarySync", "child", "original")
            .await
            .unwrap();
        db.mark_album_membership_deleted("PrimarySync", "trip", "child")
            .await
            .unwrap();
        assert_eq!(dirty(), 0, "replayed tombstones are a no-op");

        db.upsert_album_membership_delta("PrimarySync", "trip", "child", Some("master"), "icloud")
            .await
            .unwrap();
        db.clear_metadata_write_failure("PrimarySync", "child", "original")
            .await
            .unwrap();
        let next = db
            .start_album_membership_snapshot("PrimarySync", "trip", None)
            .await
            .unwrap();
        assert_eq!(
            db.get_asset_groupings("PrimarySync", &["child"])
                .await
                .unwrap()
                .albums,
            groups.albums,
            "an interrupted empty snapshot cannot remove the prior membership"
        );
        assert_eq!(dirty(), 0);
        db.complete_album_membership_snapshot("PrimarySync", "trip", next)
            .await
            .unwrap();
        assert_eq!(
            db.get_asset_groupings("PrimarySync", &["child"])
                .await
                .unwrap()
                .albums,
            [("child".into(), "External".into())]
        );
        assert_eq!(
            dirty(),
            1,
            "completed snapshot removal must queue a rewrite"
        );
        db.clear_metadata_write_failure("PrimarySync", "child", "original")
            .await
            .unwrap();
        let steady = db
            .start_album_membership_snapshot("PrimarySync", "trip", None)
            .await
            .unwrap();
        db.complete_album_membership_snapshot("PrimarySync", "trip", steady)
            .await
            .unwrap();
        assert_eq!(dirty(), 0);
    }

    #[tokio::test]
    async fn add_asset_album_is_idempotent() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
            .await
            .unwrap();
        let conn = db.acquire_lock("test").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_albums WHERE asset_id = 'A1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn add_asset_album_respects_source_namespace() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "A1", "Favorites", "external-import")
            .await
            .unwrap();
        let conn = db.acquire_lock("test").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_albums WHERE asset_id = 'A1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    /// v9 PK adds `library`: same `(asset_id, album_name, source)` triple
    /// in two libraries must round-trip as two distinct rows. Pre-v9 this
    /// silently collapsed via `INSERT OR IGNORE`. Assertions filter by
    /// `library` so a regression that wrote both rows under the same zone
    /// (the exact bug v9 prevents) cannot pass with COUNT(*) = 2.
    #[tokio::test]
    async fn add_asset_album_keeps_distinct_rows_per_library() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("PrimarySync", "SHARED_ID", "Favorites", "icloud")
            .await
            .unwrap();
        db.add_asset_album("SharedSync-A1B2C3D4", "SHARED_ID", "Favorites", "icloud")
            .await
            .unwrap();
        let conn = db.acquire_lock("test").unwrap();
        let primary_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_albums \
                 WHERE asset_id = 'SHARED_ID' AND library = 'PrimarySync'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let shared_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_albums \
                 WHERE asset_id = 'SHARED_ID' AND library = 'SharedSync-A1B2C3D4'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            primary_count, 1,
            "PrimarySync must hold exactly one row for SHARED_ID"
        );
        assert_eq!(
            shared_count, 1,
            "SharedSync-A1B2C3D4 must hold exactly one row for SHARED_ID"
        );
    }

    #[tokio::test]
    async fn get_all_asset_people_returns_every_pair() {
        // asset_people has no production writer yet; insert test rows via raw
        // SQL so this covers the read path without adding a trait method that
        // would sit unused in production builds.
        let db = SqliteStateDb::open_in_memory().unwrap();
        {
            let conn = db.acquire_lock("seed").unwrap();
            for (aid, person) in [("A1", "Alice"), ("A1", "Bob"), ("A2", "Alice")] {
                conn.execute(
                    "INSERT INTO asset_people (library, asset_id, person_name) \
                     VALUES ('PrimarySync', ?1, ?2)",
                    rusqlite::params![aid, person],
                )
                .unwrap();
            }
        }
        let rows = db.get_all_asset_people("PrimarySync").await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.contains(&("A1".into(), "Alice".into())));
        assert!(rows.contains(&("A1".into(), "Bob".into())));
        assert!(rows.contains(&("A2".into(), "Alice".into())));
    }

    #[tokio::test]
    async fn get_all_asset_albums_returns_every_pair() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "A1", "Trip", "icloud")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "A2", "Favorites", "icloud")
            .await
            .unwrap();
        let rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.contains(&("A1".into(), "Favorites".into())));
        assert!(rows.contains(&("A1".into(), "Trip".into())));
        assert!(rows.contains(&("A2".into(), "Favorites".into())));
    }

    /// Reads must also be library-scoped: a SharedSync row with the same
    /// asset_id must NOT bleed into PrimarySync's grouping load.
    #[tokio::test]
    async fn get_all_asset_albums_is_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("PrimarySync", "ID", "Vacation", "icloud")
            .await
            .unwrap();
        db.add_asset_album("SharedSync-AB", "ID", "Family", "icloud")
            .await
            .unwrap();

        let primary = db.get_all_asset_albums("PrimarySync").await.unwrap();
        let shared = db.get_all_asset_albums("SharedSync-AB").await.unwrap();
        assert_eq!(primary, vec![("ID".into(), "Vacation".into())]);
        assert_eq!(shared, vec![("ID".into(), "Family".into())]);
    }

    #[tokio::test]
    async fn get_asset_groupings_is_bounded_and_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (library, asset_id, album) in [
            ("PrimarySync", "TARGET", "Vacation"),
            ("PrimarySync", "OTHER", "Work"),
            ("SharedSync-AB", "TARGET", "Family"),
        ] {
            db.add_asset_album(library, asset_id, album, "icloud")
                .await
                .unwrap();
        }
        {
            let conn = db.acquire_lock("seed people").unwrap();
            for (library, asset_id, person) in [
                ("PrimarySync", "TARGET", "Alice"),
                ("PrimarySync", "OTHER", "Bob"),
                ("SharedSync-AB", "TARGET", "Casey"),
            ] {
                conn.execute(
                    "INSERT INTO asset_people (library, asset_id, person_name) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![library, asset_id, person],
                )
                .unwrap();
            }
        }

        let rows = db
            .get_asset_groupings("PrimarySync", &["TARGET"])
            .await
            .unwrap();
        assert_eq!(rows.albums, vec![("TARGET".into(), "Vacation".into())]);
        assert_eq!(rows.people, vec![("TARGET".into(), "Alice".into())]);
    }

    #[tokio::test]
    async fn album_membership_snapshot_complete_prunes_only_own_container() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (library, container, album) in [
            ("PrimarySync", "container-a", "Vacation"),
            ("PrimarySync", "container-b", "Family"),
            ("SharedSync-AB", "container-a", "Shared Vacation"),
        ] {
            db.upsert_album_container(library, container, album, "album")
                .await
                .unwrap();
            let gen1 = db
                .start_album_membership_snapshot(library, container, Some("hash-1"))
                .await
                .unwrap();
            db.add_album_membership_to_snapshot(
                library,
                container,
                gen1,
                "asset-record-old",
                Some("master-old"),
                "icloud",
            )
            .await
            .unwrap();
            db.complete_album_membership_snapshot(library, container, gen1)
                .await
                .unwrap();
        }

        let gen2 = db
            .start_album_membership_snapshot("PrimarySync", "container-a", Some("hash-2"))
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "container-a",
            gen2,
            "asset-record-new",
            Some("master-new"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot("PrimarySync", "container-a", gen2)
            .await
            .unwrap();

        let primary_a_old = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-record-old",
                &["container-a"],
            )
            .await
            .unwrap();
        assert!(
            primary_a_old.is_empty(),
            "completing generation 2 must prune stale generation 1 rows for the same container",
        );

        let primary_b_old = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-record-old",
                &["container-b"],
            )
            .await
            .unwrap();
        assert_eq!(
            primary_b_old.len(),
            1,
            "pruning container-a must not delete container-b memberships",
        );

        let shared_old = db
            .get_live_selected_album_memberships_for_asset(
                "SharedSync-AB",
                "asset-record-old",
                &["container-a"],
            )
            .await
            .unwrap();
        assert_eq!(
            shared_old.len(),
            1,
            "pruning PrimarySync must not delete another library's same container id",
        );
    }

    #[tokio::test]
    async fn incomplete_album_snapshot_leaves_previous_complete_snapshot_trusted() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
            .await
            .unwrap();
        let gen1 = db
            .start_album_membership_snapshot("PrimarySync", "container-a", Some("hash-1"))
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "container-a",
            gen1,
            "asset-record-old",
            Some("master-old"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot("PrimarySync", "container-a", gen1)
            .await
            .unwrap();

        let gen2 = db
            .start_album_membership_snapshot("PrimarySync", "container-a", Some("hash-2"))
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "container-a",
            gen2,
            "asset-record-new",
            Some("master-new"),
            "icloud",
        )
        .await
        .unwrap();

        assert!(
            db.selected_album_containers_have_complete_snapshots("PrimarySync", &["container-a"])
                .await
                .unwrap(),
            "a running replacement snapshot must not hide the previous complete generation",
        );
        let conn = db.acquire_lock("test").unwrap();
        let statuses: Vec<String> = conn
            .prepare(
                "SELECT status FROM album_membership_snapshots \
                 WHERE library = 'PrimarySync' AND container_id = 'container-a' \
                 ORDER BY generation",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            statuses,
            vec!["complete".to_string(), "running".to_string()]
        );
    }

    #[tokio::test]
    async fn album_membership_lookups_are_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (library, container, master) in [
            ("PrimarySync", "container-a", "master-primary"),
            ("SharedSync-AB", "container-a", "master-shared"),
        ] {
            db.upsert_album_container(library, container, "Vacation", "album")
                .await
                .unwrap();
            let generation = db
                .start_album_membership_snapshot(library, container, None)
                .await
                .unwrap();
            db.add_album_membership_to_snapshot(
                library,
                container,
                generation,
                "same-asset-record",
                Some(master),
                "icloud",
            )
            .await
            .unwrap();
            db.complete_album_membership_snapshot(library, container, generation)
                .await
                .unwrap();
        }

        let primary = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "same-asset-record",
                &["container-a"],
            )
            .await
            .unwrap();
        let shared = db
            .get_live_selected_album_memberships_for_asset(
                "SharedSync-AB",
                "same-asset-record",
                &["container-a"],
            )
            .await
            .unwrap();
        assert_eq!(primary.len(), 1);
        assert_eq!(shared.len(), 1);
        assert_eq!(
            primary[0].master_record_name.as_deref(),
            Some("master-primary")
        );
        assert_eq!(
            shared[0].master_record_name.as_deref(),
            Some("master-shared")
        );
    }

    #[tokio::test]
    async fn album_relation_delta_add_and_delete_update_live_membership() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
            .await
            .unwrap();

        let known = db
            .upsert_album_membership_delta(
                "PrimarySync",
                "container-a",
                "asset-record-a",
                Some("master-a"),
                "icloud",
            )
            .await
            .unwrap();
        assert!(known, "selected album container should be known");
        let live = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-record-a",
                &["container-a"],
            )
            .await
            .unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].master_record_name.as_deref(), Some("master-a"));

        let known = db
            .mark_album_membership_deleted("PrimarySync", "container-a", "asset-record-a")
            .await
            .unwrap();
        assert!(known, "delete should still know the album container");
        let live = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                "asset-record-a",
                &["container-a"],
            )
            .await
            .unwrap();
        assert!(live.is_empty(), "relation delete must hide live membership");
    }

    #[tokio::test]
    async fn album_delta_delete_invalidates_snapshot() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
            .await
            .unwrap();
        let generation = db
            .start_album_membership_snapshot("PrimarySync", "container-a", None)
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "container-a",
            generation,
            "asset-record-a",
            Some("master-a"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot("PrimarySync", "container-a", generation)
            .await
            .unwrap();
        assert!(
            db.selected_album_containers_have_complete_snapshots("PrimarySync", &["container-a"])
                .await
                .unwrap()
        );

        db.mark_album_container_deleted("PrimarySync", "container-a")
            .await
            .unwrap();
        db.invalidate_album_membership_snapshot("PrimarySync", "container-a")
            .await
            .unwrap();

        assert!(
            !db.selected_album_containers_have_complete_snapshots("PrimarySync", &["container-a"])
                .await
                .unwrap(),
            "deleted album containers must not remain trusted"
        );
    }

    #[tokio::test]
    async fn unmaterialized_relations_do_not_create_compatibility_groupings() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("PrimarySync", "master-old", "Legacy Album", "icloud")
            .await
            .unwrap();
        db.upsert_album_container("PrimarySync", "container-a", "Trusted Album", "album")
            .await
            .unwrap();
        let generation = db
            .start_album_membership_snapshot("PrimarySync", "container-a", None)
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            "container-a",
            generation,
            "asset-record-new",
            Some("master-new"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot("PrimarySync", "container-a", generation)
            .await
            .unwrap();

        let legacy_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
        assert_eq!(
            legacy_rows,
            vec![("master-old".to_string(), "Legacy Album".to_string())],
            "relations without a catalogued state identity must not create compatibility groupings",
        );
    }

    #[tokio::test]
    async fn mark_soft_deleted_sets_flags_across_versions() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let orig = TestAssetRecord::new("DEL_1").checksum("c1").build();
        let med = TestAssetRecord::new("DEL_1")
            .checksum("c2")
            .version_size(VersionSizeKey::Medium)
            .build();
        db.upsert_seen(&orig).await.unwrap();
        db.upsert_seen(&med).await.unwrap();
        let when = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let updated = db
            .mark_soft_deleted("PrimarySync", "DEL_1", Some(when))
            .await
            .unwrap();

        assert_eq!(updated, 2);
        assert!(db.get_pending().await.unwrap().is_empty());
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 2);
        assert_eq!(summary.source_deleted, 2);
    }

    #[tokio::test]
    async fn resolve_source_deleted_retains_history_and_preserves_downloaded() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let pending = TestAssetRecord::new("SRC_DEL")
            .version_size(VersionSizeKey::Original)
            .checksum("pending")
            .build();
        let failed = TestAssetRecord::new("SRC_DEL")
            .version_size(VersionSizeKey::Medium)
            .checksum("failed")
            .build();
        let downloaded = TestAssetRecord::new("SRC_DEL")
            .version_size(VersionSizeKey::Thumb)
            .checksum("downloaded")
            .build();
        db.upsert_seen(&pending).await.unwrap();
        db.upsert_seen(&failed).await.unwrap();
        db.upsert_seen(&downloaded).await.unwrap();
        db.mark_failed(
            "PrimarySync",
            "SRC_DEL",
            VersionSizeKey::Medium.as_str(),
            "prior failure",
        )
        .await
        .unwrap();
        let dir = test_dir();
        let path = dir.path().join("source-deleted-thumb.jpg");
        std::fs::write(&path, b"x").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "SRC_DEL",
            VersionSizeKey::Thumb.as_str(),
            &path,
            "local_hash",
            Some("download_hash"),
        )
        .await
        .unwrap();

        let deleted_at = Utc.timestamp_opt(1_700_000_003, 0).unwrap();
        let updated = db
            .resolve_source_deleted("PrimarySync", "SRC_DEL", Some(deleted_at))
            .await
            .unwrap();

        assert_eq!(updated, 3);
        assert!(db.get_pending().await.unwrap().is_empty());
        assert!(db.get_failed().await.unwrap().is_empty());
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert_eq!(downloaded[0].version_size, VersionSizeKey::Thumb);
        assert!(downloaded[0].metadata.is_deleted);
        assert_eq!(downloaded[0].metadata.deleted_at, Some(deleted_at));
        assert_eq!(downloaded[0].local_path.as_deref(), Some(path.as_path()));
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 3);
        assert_eq!(summary.source_deleted, 3);
    }

    #[tokio::test]
    async fn resolve_master_family_source_deleted_excludes_pending_siblings() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_seen(&TestAssetRecord::new("MASTER_FAMILY").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("asset-FAMILY-SIBLING").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("OTHER_MASTER").build())
            .await
            .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "asset-FAMILY-SIBLING", "MASTER_FAMILY")
            .await
            .unwrap();

        let updated = db
            .resolve_master_family_source_deleted("PrimarySync", "MASTER_FAMILY", None)
            .await
            .unwrap();

        assert_eq!(updated, 2);
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.as_ref(), "OTHER_MASTER");
        assert_eq!(db.get_summary().await.unwrap().source_deleted, 2);
    }

    #[tokio::test]
    async fn source_deleted_retries_are_retained_but_not_actionable() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_seen(&TestAssetRecord::new("PENDING_DELETED").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("FAILED_DELETED").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("DOWNLOADED_DELETED").build())
            .await
            .unwrap();
        db.upsert_seen(&TestAssetRecord::new("PENDING_LIVE").build())
            .await
            .unwrap();
        db.upsert_seen(
            &TestAssetRecord::new("SHARED_DELETED")
                .library("SharedSync-AAAA")
                .build(),
        )
        .await
        .unwrap();
        db.mark_failed(
            "PrimarySync",
            "FAILED_DELETED",
            VersionSizeKey::Original.as_str(),
            "prior failure",
        )
        .await
        .unwrap();
        let dir = test_dir();
        let path = dir.path().join("downloaded-deleted.jpg");
        std::fs::write(&path, b"x").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DOWNLOADED_DELETED",
            VersionSizeKey::Original.as_str(),
            &path,
            "local_hash",
            Some("download_hash"),
        )
        .await
        .unwrap();
        for asset_id in [
            "PENDING_DELETED",
            "FAILED_DELETED",
            "DOWNLOADED_DELETED",
            "SHARED_DELETED",
        ] {
            let library = if asset_id == "SHARED_DELETED" {
                "SharedSync-AAAA"
            } else {
                "PrimarySync"
            };
            db.mark_soft_deleted(library, asset_id, None).await.unwrap();
        }

        let pruned = db
            .prune_source_deleted_retries(Some("PrimarySync"))
            .await
            .unwrap();

        assert_eq!(pruned, 0);
        assert!(db.get_failed().await.unwrap().is_empty());
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        assert!(pending.iter().any(|record| {
            record.library.as_ref() == "PrimarySync" && record.id.as_ref() == "PENDING_LIVE"
        }));
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert_eq!(downloaded[0].id.as_ref(), "DOWNLOADED_DELETED");
        assert!(downloaded[0].metadata.is_deleted);
        assert_eq!(downloaded[0].local_path.as_deref(), Some(path.as_path()));
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 5);
        assert_eq!(summary.source_deleted, 4);
    }

    #[tokio::test]
    async fn mark_soft_deleted_then_mark_downloaded_preserves_tombstone() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("DEL_DL_1")
            .checksum("remote_hash")
            .build();
        db.upsert_seen(&rec).await.unwrap();
        db.mark_failed("PrimarySync", "DEL_DL_1", "original", "prior failure")
            .await
            .unwrap();
        let deleted_at = Utc.timestamp_opt(1_700_000_001, 0).unwrap();
        db.mark_soft_deleted("PrimarySync", "DEL_DL_1", Some(deleted_at))
            .await
            .unwrap();

        let dir = test_dir();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"x").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DEL_DL_1",
            "original",
            &path,
            "local_hash",
            Some("download_hash"),
        )
        .await
        .unwrap();

        assert_downloaded_tombstone_row(&db, "DEL_DL_1", &path, deleted_at);
    }

    #[tokio::test]
    async fn mark_downloaded_then_mark_soft_deleted_preserves_download_state() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("DL_DEL_1")
            .checksum("remote_hash")
            .build();
        db.upsert_seen(&rec).await.unwrap();

        let dir = test_dir();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, b"x").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "DL_DEL_1",
            "original",
            &path,
            "local_hash",
            Some("download_hash"),
        )
        .await
        .unwrap();

        let deleted_at = Utc.timestamp_opt(1_700_000_002, 0).unwrap();
        db.mark_soft_deleted("PrimarySync", "DL_DEL_1", Some(deleted_at))
            .await
            .unwrap();

        assert_downloaded_tombstone_row(&db, "DL_DEL_1", &path, deleted_at);
    }

    #[tokio::test]
    async fn mark_hidden_at_source_sets_flag() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("HID_1").build();
        db.upsert_seen(&rec).await.unwrap();
        let updated = db
            .mark_hidden_at_source("PrimarySync", "HID_1")
            .await
            .unwrap();
        assert_eq!(updated, 1);
        let pending = db.get_pending().await.unwrap();
        assert!(pending[0].metadata.is_hidden);
    }

    #[tokio::test]
    async fn source_state_transitions_report_zero_rows() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let deleted = db
            .mark_soft_deleted("PrimarySync", "MISSING_DELETE", None)
            .await
            .unwrap();
        let hidden = db
            .mark_hidden_at_source("PrimarySync", "MISSING_HIDDEN")
            .await
            .unwrap();

        assert_eq!(deleted, 0);
        assert_eq!(hidden, 0);
    }

    /// #707 review: new provider media returns a downloaded row to pending and
    /// stores the incoming metadata in the same statement. A refresh that then
    /// matches no downloaded row has still found its metadata durable, so it
    /// must not be reported as a lost write and hold the provider checkpoint.
    #[tokio::test]
    async fn refresh_reports_metadata_already_durable_on_a_row_that_left_downloaded() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let mut edited = AssetMetadata {
            is_favorite: true,
            rating: Some(5),
            ..AssetMetadata::default()
        };
        edited.refresh_hash();

        let seeded = TestAssetRecord::new("MOVED_ON").checksum("ck_v1").build();
        db.upsert_seen(&seeded).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "MOVED_ON",
            "original",
            std::path::Path::new("/tmp/moved-on.jpg"),
            "local_v1",
            None,
        )
        .await
        .unwrap();

        // New media for the same asset: the row returns to pending and carries
        // the edited metadata forward.
        let republished = TestAssetRecord::new("MOVED_ON")
            .checksum("ck_v2")
            .metadata(edited.clone())
            .build();
        db.upsert_seen(&republished).await.unwrap();

        let matched = db
            .refresh_downloaded_asset_metadata(
                "PrimarySync",
                "MOVED_ON",
                (&edited, republished.created_at, republished.added_at),
                true,
                false,
                METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap();
        assert!(
            matched > 0,
            "metadata already stored by the reset must count as durable"
        );
        for dates in [
            (
                republished.created_at + chrono::Duration::milliseconds(1),
                republished.added_at,
            ),
            (republished.created_at, Some(DateTime::UNIX_EPOCH)),
        ] {
            assert_eq!(
                db.refresh_downloaded_asset_metadata(
                    "PrimarySync",
                    "MOVED_ON",
                    (&edited, dates.0, dates.1),
                    true,
                    false,
                    METADATA_CAPTURE_REVISION,
                )
                .await
                .unwrap(),
                0,
                "same hash cannot vouch for stale dates"
            );
        }

        let stale = db
            .refresh_downloaded_asset_metadata(
                "PrimarySync",
                "ABSENT",
                (&edited, republished.created_at, republished.added_at),
                true,
                false,
                METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap();
        assert_eq!(stale, 0, "a missing row is still a lost write");
    }

    #[tokio::test]
    async fn record_and_clear_metadata_write_failure_roundtrip() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("MWF_1").build();
        db.upsert_seen(&rec).await.unwrap();

        // Initially, the marker column is NULL.
        let ts_initial: Option<i64> = {
            let conn = db.acquire_lock("test").unwrap();
            conn.query_row(
                "SELECT metadata_write_failed_at FROM assets WHERE id = 'MWF_1'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(ts_initial.is_none());

        // Set the marker.
        db.record_metadata_write_failure("PrimarySync", "MWF_1", "original")
            .await
            .unwrap();
        let ts_after_set: Option<i64> = {
            let conn = db.acquire_lock("test").unwrap();
            conn.query_row(
                "SELECT metadata_write_failed_at FROM assets WHERE id = 'MWF_1'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            ts_after_set.is_some(),
            "marker should be set after record_metadata_write_failure"
        );

        // Clear the marker after a successful retry.
        db.clear_metadata_write_failure("PrimarySync", "MWF_1", "original")
            .await
            .unwrap();
        let ts_after_clear: Option<i64> = {
            let conn = db.acquire_lock("test").unwrap();
            conn.query_row(
                "SELECT metadata_write_failed_at FROM assets WHERE id = 'MWF_1'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(
            ts_after_clear.is_none(),
            "marker should be cleared after clear_metadata_write_failure"
        );
    }

    #[tokio::test]
    async fn has_downloaded_without_metadata_hash_returns_false_on_empty() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        assert!(!db.has_downloaded_without_metadata_hash().await.unwrap());
    }

    #[tokio::test]
    async fn has_downloaded_without_metadata_hash_skips_pending() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("P1").build();
        db.upsert_seen(&rec).await.unwrap();
        assert!(!db.has_downloaded_without_metadata_hash().await.unwrap());
    }

    #[tokio::test]
    async fn has_downloaded_without_metadata_hash_detects_missing_hash() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("D1").build();
        db.upsert_seen(&rec).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "D1",
            "original",
            Path::new("/a.jpg"),
            "h",
            None,
        )
        .await
        .unwrap();
        // Manually null the hash to simulate a pre-v5 row
        {
            let conn = db.acquire_lock("test").unwrap();
            conn.execute("UPDATE assets SET metadata_hash = NULL WHERE id = 'D1'", [])
                .unwrap();
        }
        assert!(db.has_downloaded_without_metadata_hash().await.unwrap());
    }

    #[tokio::test]
    async fn has_downloaded_without_metadata_hash_skips_soft_deleted() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("D1").build();
        db.upsert_seen(&rec).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "D1",
            "original",
            Path::new("/a.jpg"),
            "h",
            None,
        )
        .await
        .unwrap();
        {
            let conn = db.acquire_lock("test").unwrap();
            conn.execute("UPDATE assets SET metadata_hash = NULL WHERE id = 'D1'", [])
                .unwrap();
        }
        db.mark_soft_deleted("PrimarySync", "D1", None)
            .await
            .unwrap();
        // A soft-deleted row is never re-enumerated, so its NULL hash must not drive full enumeration.
        assert!(!db.has_downloaded_without_metadata_hash().await.unwrap());
    }

    #[tokio::test]
    async fn refresh_downloaded_asset_metadata_updates_every_live_downloaded_version() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for version in [VersionSizeKey::Original, VersionSizeKey::Medium] {
            let record = TestAssetRecord::new("PHOTO")
                .version_size(version)
                .metadata(AssetMetadata {
                    metadata_hash: Some("stale-hash".to_string()),
                    ..AssetMetadata::default()
                })
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "PHOTO",
                version.as_str(),
                Path::new("/photo.jpg"),
                "checksum",
                None,
            )
            .await
            .unwrap();
        }

        let metadata = AssetMetadata {
            rating: Some(4),
            ..AssetMetadata::default()
        };
        let expected_hash = metadata.compute_hash();
        let created = DateTime::from_timestamp_millis(1_700_000_000_123).unwrap();
        let added = DateTime::from_timestamp_millis(-1).unwrap();
        {
            let conn = db.acquire_lock("inject refresh commit failure").unwrap();
            conn.execute_batch(
                "CREATE TRIGGER fail_capture_revision BEFORE INSERT ON asset_metadata_capture_revisions
                 BEGIN SELECT RAISE(FAIL, 'injected revision failure'); END;",
            ).unwrap();
        }
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "PHOTO",
            (&metadata, created, Some(added)),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap_err();
        let unchanged = db.get_downloaded_page(0, 10).await.unwrap();
        assert!(unchanged.iter().all(|row| row.created_at != created
            && row.added_at.is_none()
            && row.metadata.rating.is_none()
            && row.metadata.metadata_hash.as_deref() == Some("stale-hash")));
        for queue in [
            MetadataRewriteQueue::Ordinary,
            MetadataRewriteQueue::CaptureRepair,
        ] {
            assert!(
                db.get_pending_metadata_rewrites_page_for_queue(queue, None, 0, 10)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        db.acquire_lock("remove refresh failure")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_capture_revision")
            .unwrap();
        let updated = db
            .refresh_downloaded_asset_metadata(
                "PrimarySync",
                "PHOTO",
                (&metadata, created, Some(added)),
                true,
                true,
                METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap();
        assert_eq!(updated, 2);

        let rewrites = db.get_pending_metadata_rewrites(10).await.unwrap();
        assert_eq!(rewrites.len(), 2);
        for record in rewrites {
            assert_eq!(record.library.as_ref(), "PrimarySync");
            assert_eq!(record.id.as_ref(), "PHOTO");
            assert!(matches!(
                record.version_size,
                VersionSizeKey::Original | VersionSizeKey::Medium
            ));
            assert_eq!(record.created_at, created);
            assert_eq!(record.added_at, Some(added));
            assert_eq!(record.metadata.rating, Some(4));
            assert_eq!(
                record.metadata.metadata_hash.as_deref(),
                Some(expected_hash.as_str())
            );
        }

        // The refresh must not disturb download state: #707 requires every
        // live downloaded version to keep its status, local path, checksums
        // and download timestamp while its metadata is replaced.
        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), 2, "both versions must remain downloaded");
        for record in &downloaded {
            assert_eq!(record.local_path.as_deref(), Some(Path::new("/photo.jpg")));
            assert_eq!(record.local_checksum.as_deref(), Some("checksum"));
            assert_eq!(record.checksum.as_ref(), "checksum123");
            assert_eq!(record.metadata.rating, Some(4));
        }
    }

    async fn seed_downloaded_capture_asset(
        db: &SqliteStateDb,
        id: &str,
        metadata_hash: &str,
        provider_checksum: &str,
        local_checksum: &str,
    ) {
        let record = TestAssetRecord::new(id)
            .checksum(provider_checksum)
            .metadata(AssetMetadata {
                metadata_hash: Some(metadata_hash.to_owned()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            id,
            "original",
            Path::new("/photos/capture.jpg"),
            local_checksum,
            None,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capture_repair_receipt_is_guarded_and_markers_retire_independently() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_downloaded_capture_asset(&db, "CAPTURE", "metadata-v1", "provider-v1", "local-v1")
            .await;
        let metadata = AssetMetadata {
            metadata_hash: Some("metadata-v1".into()),
            ..AssetMetadata::default()
        };
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "CAPTURE",
            (&metadata, DateTime::UNIX_EPOCH, None),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();

        let mut capture = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            capture.capture_repair_receipt,
            Some(CaptureRepairReceipt::Pending {
                metadata_hash: "metadata-v1".into()
            })
        );
        let prepared = db
            .record_capture_repair_prepared(&capture, "local-v2", 2048)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            prepared,
            CaptureRepairReceipt::Prepared {
                metadata_hash: "metadata-v1".into(),
                output_checksum: "local-v2".into(),
                output_size: 2048,
            }
        );
        capture.capture_repair_receipt = Some(prepared);
        capture = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        let reprepared = db
            .record_capture_repair_prepared(&capture, "local-v3", 3072)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            reprepared,
            CaptureRepairReceipt::Prepared {
                metadata_hash: "metadata-v1".into(),
                output_checksum: "local-v3".into(),
                output_size: 3072,
            },
            "a crash-left prepared receipt may be replaced while input bytes still match"
        );
        capture.capture_repair_receipt = Some(reprepared);

        assert!(
            db.finish_metadata_rewrite(
                &capture,
                MetadataRewriteQueue::CaptureRepair,
                Some("local-v3"),
                Some("local-v1"),
                MetadataRewriteCompletion::CaptureRepair,
            )
            .await
            .unwrap()
        );
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .is_empty()
        );
        let ordinary = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            ordinary.len(),
            1,
            "capture completion must keep generic debt"
        );
        assert!(
            db.finish_metadata_rewrite(
                &ordinary[0],
                MetadataRewriteQueue::Ordinary,
                Some("local-v3"),
                None,
                MetadataRewriteCompletion::Ordinary,
            )
            .await
            .unwrap()
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );

        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded[0].local_checksum.as_deref(), Some("local-v3"));
        assert_eq!(downloaded[0].download_checksum.as_deref(), Some("local-v1"));

        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "CAPTURE",
            (&metadata, DateTime::UNIX_EPOCH, None),
            false,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let pending_no_write = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        db.record_capture_repair_prepared(&pending_no_write, "unused-output", 3072)
            .await
            .unwrap()
            .unwrap();
        let prepared_no_write = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            db.finish_metadata_rewrite(
                &prepared_no_write,
                MetadataRewriteQueue::CaptureRepair,
                Some("local-v3"),
                None,
                MetadataRewriteCompletion::CaptureRepair,
            )
            .await
            .unwrap(),
            "verified no-write completion may retain the selected input checksum"
        );
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .is_empty()
        );

        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "CAPTURE",
            (&metadata, DateTime::UNIX_EPOCH, None),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let both = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert!(
            db.finish_metadata_rewrite(
                &both[0],
                MetadataRewriteQueue::Ordinary,
                Some("local-v3"),
                None,
                MetadataRewriteCompletion::Both,
            )
            .await
            .unwrap()
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn capture_repair_refresh_preserves_receipt_until_publication_is_finalised() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_downloaded_capture_asset(&db, "REFRESH", "metadata-v1", "provider-v1", "local-v1")
            .await;

        let unchanged = AssetMetadata {
            metadata_hash: Some("metadata-v1".into()),
            ..AssetMetadata::default()
        };
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "REFRESH",
            (&unchanged, DateTime::UNIX_EPOCH, None),
            true,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .is_empty(),
            "ordinary refresh must not create capture debt"
        );

        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "REFRESH",
            (&unchanged, DateTime::UNIX_EPOCH, None),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let mut pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        let stale_pending = pending.clone();
        pending.capture_repair_receipt = db
            .record_capture_repair_prepared(&pending, "local-v2", 2048)
            .await
            .unwrap();

        let mut addition_only = pending.asset.clone();
        addition_only.added_at = Some(DateTime::from_timestamp_millis(1).unwrap());
        db.upsert_seen(&addition_only).await.unwrap();
        let stored = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(stored[0].added_at, addition_only.added_at);
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "REFRESH",
            (&unchanged, DateTime::UNIX_EPOCH, None),
            false,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let preserved = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            preserved[0].capture_repair_receipt,
            pending.capture_repair_receipt
        );
        assert_eq!(preserved[0].asset.created_at, pending.asset.created_at);
        assert_eq!(preserved[0].asset.added_at, None);

        let ordinary = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert!(
            !db.finish_metadata_rewrite(
                &ordinary[0],
                MetadataRewriteQueue::Ordinary,
                Some("local-ordinary"),
                Some("local-v1"),
                MetadataRewriteCompletion::None,
            )
            .await
            .unwrap(),
            "checksum-only completion must not retire the selected queue"
        );
        let ordinary_after_checksum = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(ordinary_after_checksum.len(), 1);
        assert_eq!(
            ordinary_after_checksum[0].asset.local_checksum.as_deref(),
            Some("local-ordinary")
        );
        assert_eq!(
            ordinary_after_checksum[0]
                .asset
                .download_checksum
                .as_deref(),
            Some("local-v1")
        );
        assert!(
            db.finish_metadata_rewrite(
                &ordinary_after_checksum[0],
                MetadataRewriteQueue::Ordinary,
                Some("local-ordinary"),
                None,
                MetadataRewriteCompletion::Ordinary,
            )
            .await
            .unwrap()
        );
        let mut pending_after_ordinary = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            pending_after_ordinary.capture_repair_receipt,
            Some(CaptureRepairReceipt::Pending {
                metadata_hash: "metadata-v1".into()
            }),
            "ordinary checksum changes must preserve capture intent as pending"
        );
        pending_after_ordinary.capture_repair_receipt = db
            .record_capture_repair_prepared(&pending_after_ordinary, "local-v2", 2048)
            .await
            .unwrap();

        let changed = AssetMetadata {
            metadata_hash: Some("metadata-v2".into()),
            rating: Some(5),
            ..AssetMetadata::default()
        };
        assert!(
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "REFRESH",
                (&changed, DateTime::UNIX_EPOCH, None),
                false,
                false,
                METADATA_CAPTURE_REVISION,
            )
            .await
            .is_err(),
            "new provider metadata must wait until published repair bytes are finalised"
        );
        let preserved_after_change = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            preserved_after_change[0].capture_repair_receipt,
            pending_after_ordinary.capture_repair_receipt
        );
        assert!(
            db.record_capture_repair_prepared(&stale_pending, "stale-output", 2048)
                .await
                .unwrap()
                .is_none(),
            "a stale metadata/input guard must not replace the reset receipt"
        );

        let incremental = TestAssetRecord::new("REFRESH")
            .checksum("provider-v1")
            .metadata(AssetMetadata {
                metadata_hash: Some("metadata-v3".into()),
                rating: Some(4),
                ..AssetMetadata::default()
            })
            .build();
        assert!(matches!(
            db.upsert_seen(&incremental).await,
            Err(StateError::Invariant {
                operation: "upsert_seen",
                ..
            })
        ));
        let incrementally_preserved = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            incrementally_preserved[0].capture_repair_receipt,
            pending_after_ordinary.capture_repair_receipt
        );
    }

    #[tokio::test]
    async fn prepared_capture_receipt_blocks_metadata_refresh_for_every_version() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_downloaded_capture_asset(
            &db,
            "MULTI_REFRESH",
            "metadata-v1",
            "provider-v1",
            "local-original",
        )
        .await;
        let medium = TestAssetRecord::new("MULTI_REFRESH")
            .version_size(VersionSizeKey::Medium)
            .checksum("provider-medium")
            .metadata(AssetMetadata {
                metadata_hash: Some("metadata-v1".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&medium).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "MULTI_REFRESH",
            "medium",
            Path::new("/photos/capture-medium.jpg"),
            "local-medium",
            None,
        )
        .await
        .unwrap();
        let current = AssetMetadata {
            metadata_hash: Some("metadata-v1".into()),
            ..AssetMetadata::default()
        };
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "MULTI_REFRESH",
            (&current, DateTime::UNIX_EPOCH, None),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let mut pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .into_iter()
            .find(|pending| pending.asset.version_size == VersionSizeKey::Original)
            .unwrap();
        pending.capture_repair_receipt = db
            .record_capture_repair_prepared(&pending, "prepared-original", 2048)
            .await
            .unwrap();
        assert!(pending.capture_repair_receipt.is_some());

        let changed = AssetMetadata {
            metadata_hash: Some("metadata-v2".into()),
            rating: Some(5),
            ..AssetMetadata::default()
        };
        let mut incoming = medium;
        for (metadata, created_at) in [
            (changed, DateTime::UNIX_EPOCH),
            (current, DateTime::from_timestamp_millis(1).unwrap()),
        ] {
            incoming.metadata = Arc::new(metadata);
            incoming.created_at = created_at;
            for (version, checksum) in [
                (VersionSizeKey::Original, "provider-v1"),
                (VersionSizeKey::Medium, "provider-medium"),
                (VersionSizeKey::Thumb, "provider-thumb"),
                (VersionSizeKey::Medium, "provider-replacement"),
            ] {
                incoming.version_size = version;
                incoming.checksum = checksum.into();
                assert!(matches!(
                    db.upsert_seen(&incoming).await,
                    Err(StateError::Invariant {
                        operation: "upsert_seen",
                        ..
                    })
                ));
            }
            assert!(
                db.refresh_downloaded_asset_metadata(
                    "PrimarySync",
                    "MULTI_REFRESH",
                    (&incoming.metadata, created_at, None),
                    false,
                    false,
                    METADATA_CAPTURE_REVISION,
                )
                .await
                .is_err()
            );
        }

        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        let versions = downloaded
            .iter()
            .filter(|record| record.id.as_ref() == "MULTI_REFRESH")
            .collect::<Vec<_>>();
        assert_eq!(versions.len(), 2);
        assert!(
            versions.iter().all(|record| {
                record.metadata.metadata_hash.as_deref() == Some("metadata-v1")
                    && record.created_at == DateTime::UNIX_EPOCH
                    && record.added_at.is_none()
            }),
            "a blocked version must prevent partial asset-level metadata refresh"
        );
        let preserved = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            preserved
                .iter()
                .find(|row| row.asset.version_size == VersionSizeKey::Original)
                .unwrap()
                .capture_repair_receipt,
            pending.capture_repair_receipt
        );
    }

    #[tokio::test]
    async fn prepared_capture_receipt_blocks_source_deletion_transitions() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_downloaded_capture_asset(
            &db,
            "CAPTURE_DELETE",
            "metadata-v1",
            "provider-v1",
            "local-v1",
        )
        .await;
        let metadata = AssetMetadata {
            metadata_hash: Some("metadata-v1".into()),
            ..AssetMetadata::default()
        };
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "CAPTURE_DELETE",
            (&metadata, DateTime::UNIX_EPOCH, None),
            true,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        db.record_capture_repair_prepared(&pending, "prepared-output", 2048)
            .await
            .unwrap();

        assert!(matches!(
            db.mark_soft_deleted("PrimarySync", "CAPTURE_DELETE", None)
                .await,
            Err(StateError::Invariant {
                operation: "mark_soft_deleted",
                ..
            })
        ));
        assert!(matches!(
            db.resolve_source_deleted("PrimarySync", "CAPTURE_DELETE", None)
                .await,
            Err(StateError::Invariant {
                operation: "resolve_source_deleted",
                ..
            })
        ));
        db.upsert_asset_master_mapping("PrimarySync", "CAPTURE_DELETE", "MASTER_DELETE")
            .await
            .unwrap();
        assert!(matches!(
            db.mark_master_family_soft_deleted("PrimarySync", "MASTER_DELETE", None)
                .await,
            Err(StateError::Invariant {
                operation: "mark_master_family_soft_deleted",
                ..
            })
        ));
        assert!(matches!(
            db.resolve_master_family_source_deleted("PrimarySync", "MASTER_DELETE", None)
                .await,
            Err(StateError::Invariant {
                operation: "resolve_master_family_source_deleted",
                ..
            })
        ));
        assert_eq!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .len(),
            1
        );
    }

    #[tokio::test]
    async fn capture_repair_receipt_fails_closed_and_tracks_installed_bytes() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let mut record = TestAssetRecord::new("ADOPT_CAPTURE")
            .checksum("provider-v1")
            .metadata(AssetMetadata {
                metadata_hash: Some("metadata-v1".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.import_adopt(
            &record,
            Path::new("/photos/adopted.jpg"),
            "local-v1",
            2048,
            Some(1),
        )
        .await
        .unwrap();
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "ADOPT_CAPTURE",
            (&record.metadata, record.created_at, record.added_at),
            false,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let mut pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        pending.capture_repair_receipt = db
            .record_capture_repair_prepared(&pending, "local-v2", 2048)
            .await
            .unwrap();

        record.created_at += chrono::Duration::milliseconds(1);
        db.import_adopt(
            &record,
            Path::new("/photos/adopted.jpg"),
            "local-v1",
            2048,
            Some(1),
        )
        .await
        .unwrap_err();
        record.created_at = pending.asset.created_at;
        assert_eq!(
            db.get_downloaded_page(0, 10).await.unwrap()[0].created_at,
            record.created_at
        );
        db.import_adopt(
            &record,
            Path::new("/photos/readopted.jpg"),
            "local-v1",
            2048,
            Some(2),
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()[0]
                .capture_repair_receipt,
            pending.capture_repair_receipt,
            "byte-identical re-adoption must preserve prepared evidence"
        );

        db.import_adopt(
            &record,
            Path::new("/photos/readopted.jpg"),
            "local-changed",
            2048,
            Some(3),
        )
        .await
        .unwrap();
        let preserved = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(preserved.len(), 1);
        assert_eq!(
            preserved[0].asset.local_path.as_deref(),
            Some(Path::new("/photos/adopted.jpg")),
            "changing one copy must not erase another path's prepared receipt"
        );
        db.import_adopt(
            &record,
            Path::new("/photos/adopted.jpg"),
            "local-changed",
            2048,
            Some(4),
        )
        .await
        .unwrap();
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap()
            .is_empty(),
            "changed bytes at the same path invalidate capture evidence"
        );

        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "ADOPT_CAPTURE",
            (&record.metadata, record.created_at, record.added_at),
            false,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
        for row in &pending {
            assert!(
                db.record_capture_repair_prepared(row, "local-v3", 2048)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        record.checksum = "provider-v2".into();
        record.created_at += chrono::Duration::milliseconds(1);
        db.upsert_seen(&record).await.unwrap();
        let rows = db.get_pending().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].created_at, record.created_at);
        assert_eq!(rows[0].checksum, record.checksum);
        let receipt: (Option<String>, Option<String>, Option<i64>) = {
            let conn = db.acquire_lock("read invalidated capture receipt").unwrap();
            conn.query_row(
                "SELECT capture_repair_metadata_hash, capture_repair_output_checksum, \
                    capture_repair_output_size FROM assets WHERE id = 'ADOPT_CAPTURE'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(receipt, (None, None, None));

        seed_downloaded_capture_asset(&db, "MALFORMED", "metadata-v1", "provider-v1", "local-v1")
            .await;
        {
            let conn = db.acquire_lock("write malformed capture receipt").unwrap();
            conn.execute(
                "UPDATE assets SET capture_repair_output_checksum = 'orphaned' \
                 WHERE id = 'MALFORMED'",
                [],
            )
            .unwrap();
        }
        let changed = AssetMetadata {
            metadata_hash: Some("metadata-v2".into()),
            rating: Some(5),
            ..AssetMetadata::default()
        };
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "MALFORMED",
            (&changed, DateTime::UNIX_EPOCH, None),
            false,
            true,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                10,
            )
            .await
            .is_err(),
            "partial capture receipt must fail closed"
        );
    }

    #[tokio::test]
    async fn metadata_capture_revision_repair_is_resumable_and_preserves_file_state() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = TestAssetRecord::new("ASSET_CHILD")
            .checksum("provider-checksum")
            .filename("photo.jpg")
            .size(2048)
            .metadata(AssetMetadata {
                metadata_hash: Some("revision-zero".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ASSET_CHILD",
            "original",
            Path::new("/photos/photo.jpg"),
            "local-checksum",
            Some("provider-checksum"),
        )
        .await
        .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", "ASSET_CHILD", "MASTER")
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("test_metadata_capture_revision").unwrap();
            conn.execute(
                "UPDATE asset_metadata_capture_revisions SET revision = 0 \
                 WHERE library = 'PrimarySync' AND asset_id = 'ASSET_CHILD'",
                [],
            )
            .unwrap();
        }

        let started = db
            .begin_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        assert_eq!(started.active_revision, 0);
        assert_eq!(started.pending_revision, Some(METADATA_CAPTURE_REVISION));
        assert_eq!(started.remaining_assets, 1);

        let candidates = db
            .get_metadata_capture_candidates("PrimarySync", METADATA_CAPTURE_REVISION, 10)
            .await
            .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].asset_id, "ASSET_CHILD");
        assert_eq!(candidates[0].master_record_name, "MASTER");
        assert_eq!(
            candidates[0].asset_record_name.as_deref(),
            Some("ASSET_CHILD")
        );
        assert_eq!(candidates[0].versions.len(), 1);
        assert_eq!(candidates[0].versions[0].size_bytes, 2048);

        db.record_metadata_capture_failure(
            "PrimarySync",
            METADATA_CAPTURE_REVISION,
            "temporary provider failure",
        )
        .await
        .unwrap();
        let still_pending = db
            .complete_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        assert_eq!(still_pending.remaining_assets, 1);
        assert_eq!(still_pending.failed_assets, 1);

        let metadata = AssetMetadata {
            rating: Some(5),
            ..AssetMetadata::default()
        };
        db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "ASSET_CHILD",
            (&metadata, record.created_at, record.added_at),
            true,
            false,
            METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
        let completed = db
            .complete_metadata_capture_revision("PrimarySync", METADATA_CAPTURE_REVISION)
            .await
            .unwrap();
        assert_eq!(completed.active_revision, METADATA_CAPTURE_REVISION);
        assert_eq!(completed.pending_revision, None);
        assert_eq!(completed.remaining_assets, 0);
        assert_eq!(completed.processed_assets, 1);
        assert_eq!(completed.failed_assets, 0);

        let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert_eq!(
            downloaded[0].local_path.as_deref(),
            Some(Path::new("/photos/photo.jpg"))
        );
        assert_eq!(
            downloaded[0].local_checksum.as_deref(),
            Some("local-checksum")
        );
        assert_eq!(
            downloaded[0].download_checksum.as_deref(),
            Some("provider-checksum")
        );
        assert_eq!(downloaded[0].metadata.rating, Some(5));
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn metadata_capture_candidates_are_bounded_and_library_scoped() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        for (library, id) in [
            ("PrimarySync", "A"),
            ("PrimarySync", "B"),
            ("PrimarySync", "C"),
            ("SharedSync-AAAA", "D"),
        ] {
            let mut record = TestAssetRecord::new(id).build();
            record.library = Arc::from(library);
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                library,
                id,
                "original",
                Path::new("/photos/photo.jpg"),
                "local-checksum",
                None,
            )
            .await
            .unwrap();
        }
        {
            let conn = db.acquire_lock("test_metadata_capture_scope").unwrap();
            conn.execute("DELETE FROM asset_metadata_capture_revisions", [])
                .unwrap();
        }

        assert!(
            db.has_metadata_capture_work(&["PrimarySync"], METADATA_CAPTURE_REVISION)
                .await
                .unwrap()
        );
        let primary = db
            .get_metadata_capture_candidates("PrimarySync", METADATA_CAPTURE_REVISION, 2)
            .await
            .unwrap();
        assert_eq!(primary.len(), 2);
        assert!(primary.iter().all(|row| row.library == "PrimarySync"));
        let shared = db
            .get_metadata_capture_candidates("SharedSync-AAAA", METADATA_CAPTURE_REVISION, 2)
            .await
            .unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].asset_id, "D");
    }

    #[test]
    fn asset_column_count_matches_projection() {
        let counted = ASSET_COLUMNS.split(',').count();
        assert_eq!(
            counted, ASSET_COLUMN_COUNT,
            "ASSET_COLUMN_COUNT out of sync with ASSET_COLUMNS"
        );
    }

    /// End-to-end: PhotoAsset constructed from a realistic CloudKit JSON pair
    /// flows through metadata extraction, `upsert_seen`, and round-trips out
    /// of the DB with all fields intact. Guards against regressions in any
    /// link of the pipeline.
    #[tokio::test]
    async fn photo_asset_metadata_roundtrips_through_upsert_and_read() {
        use crate::icloud::photos::PhotoAsset;
        use base64::Engine;
        use serde_json::json;

        fn b64(bytes: &[u8]) -> String {
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }
        fn bplist(value: plist::Value) -> Vec<u8> {
            let mut out = Vec::new();
            plist::to_writer_binary(&mut out, &value).unwrap();
            out
        }

        let mut loc_dict = plist::Dictionary::new();
        loc_dict.insert("lat".into(), plist::Value::Real(37.7749));
        loc_dict.insert("lon".into(), plist::Value::Real(-122.4194));
        loc_dict.insert("alt".into(), plist::Value::Real(17.0));
        let loc_bp = bplist(plist::Value::Dictionary(loc_dict));

        let keywords_bp = bplist(plist::Value::Array(vec![
            plist::Value::String("vacation".into()),
            plist::Value::String("beach".into()),
        ]));

        let master = json!({
            "recordName": "RT_1",
            "fields": {
                "itemType": {"value": "public.jpeg"},
                "filenameEnc": {"value": "img.jpg", "type": "STRING"},
                "resOriginalRes": {"value": {"size": 10, "downloadURL": "https://p01.icloud-content.com/x", "fileChecksum": "ck"}},
                "resOriginalFileType": {"value": "public.jpeg"},
            },
        });
        let asset_json = json!({
            "fields": {
                "assetDate": {"value": 1736899200123_f64},
                "addedDate": {"value": 1736985600789_f64},
                "isFavorite": {"value": 1},
                "orientation": {"value": 6},
                "duration": {"value": 12.5},
                "timeZoneOffset": {"value": -28800},
                "captionEnc": {"value": "Beach day", "type": "STRING"},
                "extendedDescEnc": {"value": "Long description", "type": "STRING"},
                "keywordsEnc": {"value": b64(&keywords_bp), "type": "ENCRYPTED_BYTES"},
                "locationEnc": {"value": b64(&loc_bp), "type": "ENCRYPTED_BYTES"},
                "resOriginalWidth": {"value": 4032},
                "resOriginalHeight": {"value": 3024},
                "assetSubtypeV2": {"value": 16},
                "burstId": {"value": "burst_x"},
                "recordChangeTag": {"value": "tag42"},
            },
        });
        let photo = PhotoAsset::new(master, asset_json);

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = AssetRecord::new_pending(
            Arc::from("PrimarySync"),
            photo.id().to_string(),
            VersionSizeKey::Original,
            "ck".to_string(),
            photo.filename().unwrap_or("").to_string(),
            photo.created(),
            Some(photo.added_date()),
            10,
            MediaType::Photo,
        )
        .with_metadata_arc(photo.metadata_arc());
        db.upsert_seen(&record).await.unwrap();

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        let got = &pending[0];
        let created = DateTime::from_timestamp_millis(1_736_899_200_123).unwrap();
        let added = Some(DateTime::from_timestamp_millis(1_736_985_600_789).unwrap());
        assert_eq!((got.created_at, got.added_at), (created, added));
        let manifest = db.get_manifest_assets().await.unwrap();
        assert_eq!(
            (manifest[0].created_at, manifest[0].added_at),
            (created, added)
        );
        let m = &got.metadata;
        assert_eq!(m.source.as_deref(), Some("icloud"));
        assert!(m.is_favorite);
        assert_eq!(m.rating, Some(5));
        assert_eq!(m.latitude, Some(37.7749));
        assert_eq!(m.longitude, Some(-122.4194));
        assert_eq!(m.altitude, Some(17.0));
        assert_eq!(m.orientation, Some(6));
        assert_eq!(m.duration_secs, Some(12.5));
        assert_eq!(m.timezone_offset, Some(-28800));
        assert_eq!(m.width, Some(4032));
        assert_eq!(m.height, Some(3024));
        assert_eq!(m.title.as_deref(), Some("Beach day"));
        assert_eq!(m.description.as_deref(), Some("Long description"));
        assert_eq!(m.keywords.as_deref(), Some(r#"["vacation","beach"]"#));
        assert_eq!(m.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(m.burst_id.as_deref(), Some("burst_x"));
        assert!(m.metadata_hash.is_some());

        let provider = m
            .provider_data
            .as_deref()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .expect("provider_data should be valid JSON");
        assert_eq!(provider["recordChangeTag"], json!("tag42"));
        assert_eq!(provider["assetSubtypeV2"], json!(16));

        let legacy_created = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        for (literal, added) in [
            ("1700000001", DateTime::from_timestamp(1_700_000_001, 0)),
            ("NULL", None),
        ] {
            db.acquire_lock("legacy dates")
                .unwrap()
                .execute_batch(&format!(
                    "UPDATE assets SET created_at = 1700000000, added_at = {literal}"
                ))
                .unwrap();
            let row = db.get_pending().await.unwrap().pop().unwrap();
            let manifest = db.get_manifest_assets().await.unwrap().pop().unwrap();
            for dates in [
                (row.created_at, row.added_at),
                (manifest.created_at, manifest.added_at),
            ] {
                assert_eq!(dates, (legacy_created, added));
            }
        }
        for column in ["created_at", "added_at"] {
            db.acquire_lock("invalid dates")
                .unwrap()
                .execute_batch(&format!(
                    "UPDATE assets SET created_at = 0, added_at = NULL;
                 UPDATE assets SET {column} = 'invalid'"
                ))
                .unwrap();
            assert!(db.get_pending().await.is_err(), "{column}");
            assert!(db.get_manifest_assets().await.is_err(), "{column}");
        }
    }

    // WAL mid-transaction rollback invariant: a second connection that
    // begins a transaction modifying committed rows, then is dropped
    // without calling commit(), must not leave those modifications
    // visible to any subsequent SqliteStateDb::open(). This stands in
    // for the crash-between-BEGIN-and-COMMIT scenario (OOM kill, power
    // loss, SIGKILL) that WAL mode is supposed to make safe.
    //
    // If kei ever flipped journal_mode away from WAL (or mis-used
    // autocommit so every write lands without tx grouping), a single
    // interrupted mark_downloaded batch could leave half-complete state
    // that the next sync would read as truth, silently drifting the DB
    // from the file system.
    #[tokio::test]
    async fn wal_uncommitted_transaction_is_invisible_on_reopen() {
        use rusqlite::Connection;

        let dir = test_dir();
        let path = dir.path().join("wal_rollback.db");
        let file_path = dir.path().join("keeper.jpg");
        fs::write(&file_path, b"bytes").unwrap();

        // Step 1: open, commit a downloaded row, close.
        {
            let db = SqliteStateDb::open(&path).await.unwrap();
            let record = TestAssetRecord::new("WAL_KEEPER")
                .checksum("ck_w")
                .filename("keeper.jpg")
                .size(5)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "WAL_KEEPER",
                "original",
                &file_path,
                "localhash",
                None,
            )
            .await
            .unwrap();
        }

        // Step 2: open a raw rusqlite connection, begin an explicit
        // transaction that would corrupt the downloaded row, then drop
        // the connection WITHOUT committing. rusqlite's Connection Drop
        // rolls back any open transaction — which is exactly the
        // observable state SQLite exposes after a hard crash mid-tx.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("BEGIN; UPDATE assets SET status = 'failed', last_error = 'would be set by crashed tx' WHERE id = 'WAL_KEEPER'; ").unwrap();
            // Confirm the UPDATE actually landed inside the open tx, so the
            // final assertion is proving rollback rather than a silent no-op
            // (e.g. row missing, UPDATE matching zero rows).
            let mid_tx_status: String = conn
                .query_row(
                    "SELECT status FROM assets WHERE id = 'WAL_KEEPER'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(mid_tx_status, "failed");
            // Drop without commit → rollback.
            drop(conn);
        }

        // Step 3: reopen. The rolled-back UPDATE must not be visible.
        let db = SqliteStateDb::open(&path).await.unwrap();
        let ids = db.get_downloaded_ids().await.unwrap();
        assert!(
            ids.contains(&("PrimarySync".into(), "WAL_KEEPER".into(), "original".into())),
            "committed downloaded row must survive an adjacent uncommitted \
             transaction's rollback; got downloaded set: {ids:?}"
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(
            summary.downloaded, 1,
            "rolled-back UPDATE must not alter the committed status"
        );
        assert_eq!(
            summary.failed, 0,
            "WAL_KEEPER must not have been promoted to failed by the \
             rolled-back transaction"
        );
    }

    // Full-sync conservatism invariant: an asset downloaded in a prior sync
    // that is absent from every page of the current full enumeration must
    // remain in the state DB as status='downloaded', untouched. Full sync
    // does NOT infer "remotely deleted" from "not seen on any page" — that
    // inference is reserved for incremental sync's explicit delete events.
    //
    // If a regression ever added a "sweep assets not seen this sync" pass
    // to the full-sync path, users would silently lose local copies of
    // assets that briefly dropped out of view (album scope change, filter
    // tweak, pagination hiccup).
    #[tokio::test]
    async fn full_sync_absent_downloaded_asset_stays_downloaded() {
        let dir = test_dir();
        let file_path = dir.path().join("keeper.heic");
        fs::write(&file_path, b"image-bytes").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        // Sync N (prior run): asset KEEPER_1 enumerated + downloaded. Then
        // we backdate last_seen_at so it looks like the upsert happened
        // well before the current sync's start boundary.
        let record = TestAssetRecord::new("KEEPER_1")
            .checksum("ck_keeper")
            .filename("keeper.heic")
            .size(11)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "KEEPER_1",
            "original",
            &file_path,
            "localhash",
            None,
        )
        .await
        .unwrap();
        let prior_sync_ts = chrono::Utc::now().timestamp() - 86_400;
        db.backdate_last_seen("KEEPER_1", prior_sync_ts);

        let summary_before = db.get_summary().await.unwrap();
        assert_eq!(summary_before.downloaded, 1);
        let ids_before = db.get_downloaded_ids().await.unwrap();
        assert!(ids_before.contains(&("PrimarySync".into(), "KEEPER_1".into(), "original".into())));

        // Sync N+1 begins now. Producer enumerates zero assets (absent from
        // every page). Nothing calls upsert_seen for KEEPER_1. At sync end,
        // promote_pending_to_failed runs with the new sync_started_at.
        let sync_started_at = chrono::Utc::now().timestamp();
        let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
        assert_eq!(
            promoted, 0,
            "full-sync with zero enumerated assets must not promote anything; \
             KEEPER_1's last_seen_at predates sync_started_at AND its status \
             is downloaded, so both filters protect it"
        );

        // Downloaded row is intact: same status, still in the downloaded set.
        let summary_after = db.get_summary().await.unwrap();
        assert_eq!(
            summary_after.downloaded, 1,
            "downloaded count must be unchanged after a zero-asset sync cycle"
        );
        assert_eq!(summary_after.failed, 0);
        assert_eq!(summary_after.pending, 0);

        let ids_after = db.get_downloaded_ids().await.unwrap();
        assert!(
            ids_after.contains(&("PrimarySync".into(), "KEEPER_1".into(), "original".into())),
            "KEEPER_1 must remain in the downloaded set after a full sync that \
             didn't re-enumerate it"
        );

        // last_seen_at was NOT refreshed (nothing touched it). A caller that
        // later wants to implement "assets not seen for N syncs" can use the
        // stale timestamp as a signal, but it must be an opt-in policy, not
        // a silent cleanup.
        let failed = db.get_failed().await.unwrap();
        assert!(
            failed.is_empty(),
            "the asset that wasn't enumerated this sync must not appear in the failed set"
        );
    }

    /// Read-side counterpart to `upsert_seen_keeps_distinct_rows_per_library`
    /// and `mark_failed_is_library_scoped`: those tests already pin the write
    /// side, this one pins that the bulk-loader queries used by the download
    /// hot path surface per-zone rows without collapsing them on the shared
    /// `(id, version_size)` pair the v8 PK split was created to disambiguate.
    #[tokio::test]
    async fn multi_library_read_queries_scope_per_zone() {
        let dir = test_dir();
        let db = SqliteStateDb::open_in_memory().unwrap();

        const ID: &str = "SHARED_ID";
        const PRIMARY: &str = "PrimarySync";
        const SHARED: &str = "SharedSync-A1B2C3D4";

        for (library, ck) in [(PRIMARY, "ck_primary"), (SHARED, "ck_shared")] {
            let record = TestAssetRecord::new(ID)
                .library(library)
                .checksum(ck)
                .filename("photo.jpg")
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        let primary_path = dir.path().join(PRIMARY).join("photo.jpg");
        let shared_path = dir.path().join(SHARED).join("photo.jpg");
        for path in [&primary_path, &shared_path] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }
        db.mark_downloaded(PRIMARY, ID, "original", &primary_path, "lh_primary", None)
            .await
            .unwrap();
        db.mark_downloaded(SHARED, ID, "original", &shared_path, "lh_shared", None)
            .await
            .unwrap();

        let checksums = db.get_downloaded_checksums().await.unwrap();
        let triple = |lib: &str| (lib.to_string(), ID.to_string(), "original".to_string());
        assert_eq!(
            checksums.get(&triple(PRIMARY)),
            Some(&"ck_primary".to_string())
        );
        assert_eq!(
            checksums.get(&triple(SHARED)),
            Some(&"ck_shared".to_string())
        );
    }

    #[tokio::test]
    async fn mark_downloaded_fails_when_asset_row_missing() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let result = db
            .mark_downloaded(
                "PrimarySync",
                "NONEXISTENT_42",
                "original",
                Path::new("/tmp/codex/kei/photo.jpg"),
                "abc123hash",
                None,
            )
            .await;

        assert!(
            result.is_err(),
            "mark_downloaded on an absent row must fail"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                StateError::AssetRowMissing {
                    ref asset_id,
                    ref version_size,
                } if asset_id == "NONEXISTENT_42" && version_size == "original"
            ),
            "expected AssetRowMissing with correct ids, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn sync_run_killed_mid_pass_preserves_downloaded_rows_on_next_open() {
        let dir = test_dir();
        let db_path = dir.path().join("crash_test.db");

        let file_path_1 = dir.path().join("photo1.jpg");
        let file_path_2 = dir.path().join("photo2.jpg");
        std::fs::write(&file_path_1, b"photo 1 content").unwrap();
        std::fs::write(&file_path_2, b"photo 2 content").unwrap();

        {
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            let _run_id = db.start_sync_run().await.unwrap();

            for (id, path) in [
                ("A1", &file_path_1),
                ("A2", &file_path_2),
                ("A3", &file_path_1),
            ] {
                let record = TestAssetRecord::new(id).build();
                db.upsert_seen(&record).await.unwrap();
                if id != "A3" {
                    db.mark_downloaded("PrimarySync", id, "original", path, "hash", None)
                        .await
                        .unwrap();
                }
            }
            // Drop without calling complete_sync_run — simulates kill -9
        }

        let db2 = SqliteStateDb::open(&db_path).await.unwrap();
        let promoted = db2.promote_orphaned_sync_runs().await.unwrap();
        assert_eq!(
            promoted, 1,
            "the orphaned running sync_run must be promoted"
        );

        let summary = db2.get_summary().await.unwrap();
        assert_eq!(
            summary.downloaded, 2,
            "the two downloaded assets must survive the crash"
        );
        assert_eq!(
            summary.pending, 1,
            "the one pending asset must still be pending"
        );
    }

    #[tokio::test]
    async fn open_db_at_future_schema_version_returns_loud_error() {
        let dir = test_dir();
        let db_path = dir.path().join("future.db");

        {
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            db.with_conn("bump_version", |conn| {
                conn.pragma_update(
                    None,
                    "user_version",
                    crate::state::schema::SCHEMA_VERSION + 1,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        }

        let result = SqliteStateDb::open(&db_path).await;
        assert!(result.is_err(), "opening a future-version DB must fail");
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                StateError::UnsupportedSchemaVersion { found, expected }
                    if found == crate::state::schema::SCHEMA_VERSION + 1
                    && expected == crate::state::schema::SCHEMA_VERSION
            ),
            "expected UnsupportedSchemaVersion, got: {err:?}"
        );
    }

    /// Corrupted state DB file (random bytes, truncated SQLite header)
    /// must be detected on open, not silently misread or panicked.
    #[tokio::test]
    async fn corrupted_db_file_detected_on_open() {
        let dir = test_dir();
        let db_path = dir.path().join("corrupt.db");
        std::fs::write(&db_path, b"not a valid sqlite database file!").unwrap();
        let result = SqliteStateDb::open(&db_path).await;
        assert!(result.is_err(), "corrupted file must fail to open");
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("not a database")
                || err.to_string().contains("file is not a database"),
            "error must indicate the file is not a valid DB, got: {err:?}"
        );
    }

    /// Truncated state DB (valid SQLite header, short body) must be
    /// detected on open or first query.
    #[tokio::test]
    async fn truncated_db_file_detected_on_open() {
        let dir = test_dir();
        let db_path = dir.path().join("trunc.db");
        // Write the SQLite magic header but nothing else
        let header: &[u8] = b"SQLite format 3\x00";
        std::fs::write(&db_path, header).unwrap();
        let result = SqliteStateDb::open(&db_path).await;
        assert!(result.is_err(), "truncated SQLite file must fail");
    }
}
