//! State database trait and `SQLite` implementation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension};

use super::error::StateError;
use super::schema;
use super::types::{
    AssetMetadata, AssetRecord, AssetStatus, MediaType, SyncRunStats, SyncSummary, VersionSizeKey,
};

/// Fallback provider identifier when `AssetMetadata::source` is unset.
///
/// The `assets.source` column is NOT NULL (v5 migration defaults pre-existing
/// rows to "icloud"). Test fixtures and legacy call sites that don't populate
/// metadata get the same value written here so that inserts always succeed.
/// Real provider adapters (iCloud, Takeout, etc.) set `source` explicitly;
/// this fallback is a safety net, not the intended write path.
const DEFAULT_SOURCE: &str = "icloud";

/// Trait for state database operations.
///
/// This trait is object-safe and can be used with `Arc<dyn StateDb>` for
/// shared access across async tasks.
#[async_trait]
pub trait StateDb: Send + Sync {
    /// Check if an asset should be downloaded.
    ///
    /// Returns true if:
    /// - The asset is not in the database
    /// - The asset's checksum has changed
    /// - The asset was downloaded but the local file no longer exists
    /// - The asset is in pending or failed status
    ///
    /// Note: In the optimized flow, the caller pre-loads downloaded IDs and
    /// checksums using `get_downloaded_ids()` and `get_downloaded_checksums()`
    /// for O(1) skip decisions, falling back to filesystem checks for edge cases.
    #[cfg(test)]
    async fn should_download(
        &self,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError>;

    /// Insert or update an asset record after the producer commits it for
    /// download.
    ///
    /// Updates `last_seen_at` and preserves existing download status.
    ///
    /// **Invariant (see issue #211):** Call only from the producer dispatch
    /// path, after an asset has passed every filter and skip check and a
    /// download task has been created. `promote_pending_to_failed` treats
    /// `last_seen_at >= sync_started_at` as "the producer handed this off to
    /// the consumer this sync", so any call that bumps `last_seen_at` without
    /// a matching consumer finalization (`mark_downloaded` / `mark_failed`)
    /// will cause the asset to be promoted to `failed`. If you need to touch
    /// `last_seen_at` for an asset the consumer will not finalize (trust-state
    /// fast-skip, on-disk dedup, filtered-out, etc.), use `touch_last_seen`
    /// on rows that already have a terminal status, not `upsert_seen`.
    async fn upsert_seen(&self, record: &AssetRecord) -> Result<(), StateError>;

    /// Mark an asset as successfully downloaded.
    async fn mark_downloaded(
        &self,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), StateError>;

    /// Mark an asset as failed with an error message.
    async fn mark_failed(
        &self,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError>;

    /// Get all failed assets.
    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError>;

    /// Most-recently-seen failed rows up to `limit`, alongside the total
    /// failed count. Used by the sync-report writer to avoid loading every
    /// failed row into memory on an account with thousands of failures.
    async fn get_failed_sample(&self, limit: u32) -> Result<(Vec<AssetRecord>, u64), StateError>;

    /// Get all pending assets.
    async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError>;

    /// Get a summary of the database state.
    async fn get_summary(&self) -> Result<SyncSummary, StateError>;

    /// Get a page of downloaded assets, ordered by rowid.
    ///
    /// Returns up to `limit` records starting from `offset`.
    /// Returns an empty `Vec` when no more records remain.
    async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError>;

    /// Start a new sync run and return its ID.
    async fn start_sync_run(&self) -> Result<i64, StateError>;

    /// Complete a sync run with statistics.
    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError>;

    /// Promote any `sync_runs` rows left in `status='running'` to
    /// `status='interrupted'` (with `interrupted=1`). These are rows from a
    /// prior process that was SIGKILL'd or crashed without calling
    /// `complete_sync_run`. Called once at process startup, immediately
    /// after migrations. Returns the number of rows promoted.
    async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError>;

    /// Mark the start of a full enumeration for `zone`. Pairs with
    /// `end_enum_progress` on successful completion; a remaining marker at
    /// next startup means the enumeration was interrupted. Stores the start
    /// timestamp so the operator can age the marker.
    async fn begin_enum_progress(&self, zone: &str) -> Result<(), StateError>;

    /// Clear the in-progress enumeration marker for `zone`. Idempotent.
    async fn end_enum_progress(&self, zone: &str) -> Result<(), StateError>;

    /// Return the zone names of any enumerations that started but never
    /// ended. Read once at process startup so the operator is warned that
    /// the next full sync will re-enumerate from scratch until resume
    /// support lands.
    async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError>;

    /// Reset all failed assets to pending status.
    ///
    /// Returns the number of assets reset.
    async fn reset_failed(&self) -> Result<u64, StateError>;

    /// Reset all non-downloaded assets for a fresh sync attempt.
    ///
    /// Moves failed -> pending and clears stale attempt counts on pending
    /// assets, all in one lock acquisition. Returns
    /// (failed_reset, pending_reset, total_pending).
    async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), StateError>;

    /// Promote stuck pending assets to failed.
    ///
    /// Called at the end of a non-interrupted sync run. Promotes pending
    /// assets that the producer dispatched this sync (`last_seen_at >=
    /// seen_since`) but that the consumer never finalized via
    /// `mark_downloaded` or `mark_failed`. These are stuck-pipeline cases,
    /// not filter or album-scope exclusions.
    ///
    /// Assets whose `last_seen_at` predates this sync (filtered out, album
    /// scope changed, remotely deleted, or otherwise not re-enumerated) are
    /// left alone - they are not failures, and promoting them causes the
    /// pending -> failed -> pending ghost loop documented in issue #211.
    ///
    /// Returns the number of assets promoted.
    async fn promote_pending_to_failed(&self, seen_since: i64) -> Result<u64, StateError>;

    // ── Bulk read operations ──

    /// Get all downloaded asset IDs as (id, `version_size`) pairs.
    ///
    /// Used at sync start to pre-load downloaded state for O(1) skip decisions.
    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String)>, StateError>;

    /// Get all known asset IDs (any status: downloaded, pending, failed).
    ///
    /// Used in retry-only mode to distinguish assets that were previously
    /// synced from new assets discovered on iCloud.
    async fn get_all_known_ids(&self) -> Result<HashSet<String>, StateError>;

    /// Get downloaded asset IDs with their checksums.
    ///
    /// Returns a map of (id, `version_size`) -> checksum for downloaded assets.
    /// Used to detect checksum changes without querying the DB per asset.
    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String), String>, StateError>;

    /// Get per-asset maximum download attempt counts for failed assets.
    ///
    /// Returns a map of asset_id -> max(download_attempts).
    async fn get_attempt_counts(&self) -> Result<HashMap<String, u32>, StateError>;

    /// Get a metadata value by key.
    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError>;

    /// Set a metadata key-value pair (insert or update).
    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError>;

    /// Delete all metadata entries whose key starts with `prefix`.
    /// Returns the number of rows deleted.
    async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError>;

    /// Bump `last_seen_at` on every row in `asset_ids` to the same
    /// timestamp inside a single transaction. Used by the early skip path
    /// to avoid path resolution on mostly-synced libraries.
    ///
    /// Caller must ensure every ID already has a terminal status
    /// (`downloaded` or `failed`); touching a `pending` row will cause
    /// `promote_pending_to_failed` to promote it to `failed` at sync end —
    /// see `upsert_seen` docs and issue #211.
    async fn touch_last_seen_many(&self, asset_ids: &[&str]) -> Result<(), StateError>;

    /// Sample up to `limit` local paths of downloaded assets.
    /// Used to spot-check that "downloaded" files still exist on disk.
    async fn sample_downloaded_paths(&self, limit: usize) -> Result<Vec<PathBuf>, StateError>;

    // ── v5 metadata operations ──

    /// Record an album membership entry. Idempotent via `INSERT OR IGNORE`.
    ///
    /// Called during album enumeration when an asset is seen in an album. The
    /// `(asset_id, album_name, source)` composite key namespaces memberships
    /// per provider so that multiple providers can coexist.
    async fn add_asset_album(
        &self,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError>;

    /// Bulk-load every `(asset_id, album_name)` from `asset_albums`. Used at
    /// sync start to populate the in-memory groupings index — downstream
    /// writers look up album memberships without a per-asset DB hit.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    async fn get_all_asset_albums(&self) -> Result<Vec<(String, String)>, StateError>;

    /// Bulk-load every `(asset_id, person_name)` from `asset_people`.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    async fn get_all_asset_people(&self) -> Result<Vec<(String, String)>, StateError>;

    /// Mark an asset as soft-deleted (all versions under `asset_id`).
    ///
    /// Updates `is_deleted` and optional `deleted_at` timestamp. Does not
    /// remove the row so that history survives and consumers can still reach
    /// the local file.
    async fn mark_soft_deleted(
        &self,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<(), StateError>;

    /// Mark an asset as hidden at source.
    async fn mark_hidden_at_source(&self, asset_id: &str) -> Result<(), StateError>;

    /// Record that a metadata write (EXIF embed or sidecar) failed for this
    /// asset-version pair after the bytes landed on disk. Sets
    /// `metadata_write_failed_at` to the current timestamp. The metadata-only
    /// rewrite path consumes this to retry on subsequent syncs even when the
    /// file checksum matches.
    async fn record_metadata_write_failure(
        &self,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError>;

    /// Pre-load every `(asset_id, version_size) -> metadata_hash` for
    /// downloaded assets. Used at sync start to detect metadata-only changes
    /// (file checksum matches but e.g. keywords / favorite / GPS drifted)
    /// without a per-asset DB hit in the producer hot path.
    async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String), String>, StateError>;

    /// Pre-load the set of `(asset_id, version_size)` pairs that have a
    /// non-null `metadata_write_failed_at`. These need a metadata rewrite
    /// on the next sync regardless of whether the provider reports a
    /// hash change.
    async fn get_metadata_retry_markers(&self) -> Result<HashSet<(String, String)>, StateError>;

    /// Fetch up to `limit` downloaded asset rows that carry a metadata
    /// rewrite marker AND have a local_path pointing at an on-disk file.
    /// Used by the metadata-rewrite worker to re-apply EXIF/XMP without
    /// re-downloading bytes.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    async fn get_pending_metadata_rewrites(
        &self,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError>;

    /// Update just the `metadata_hash` column for an asset-version pair
    /// after a successful metadata rewrite. Leaves every other column alone.
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    async fn update_metadata_hash(
        &self,
        asset_id: &str,
        version_size: &str,
        metadata_hash: &str,
    ) -> Result<(), StateError>;

    /// Clear the metadata-write-failed marker for an asset-version pair
    /// after a successful rewrite.
    async fn clear_metadata_write_failure(
        &self,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError>;

    /// Whether any downloaded asset still has `metadata_hash IS NULL`.
    ///
    /// Used at sync start to log a one-time backfill notice after the v5
    /// upgrade. Returns false on a fresh DB or once the backfill sync
    /// completes. Early-exits on the first matching row via `EXISTS`, avoiding
    /// a full COUNT scan.
    async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError>;
}

/// `SQLite` implementation of the state database.
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
    pub async fn open(path: &Path) -> Result<Self, StateError> {
        let path = path.to_path_buf();
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
        })
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
    fn acquire_lock(
        &self,
        operation: &str,
    ) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>, StateError> {
        self.conn
            .lock()
            .map_err(|e| StateError::LockPoisoned(format!("{operation}: {e}")))
    }

    /// Run a synchronous rusqlite closure on the blocking pool with
    /// `&Connection` access. This is the correct entry point for every
    /// read-path StateDb method.
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

/// Drain a rusqlite row iterator into `Vec<T>`, dropping parse failures but
/// logging each at `debug!` and summarising the drop count at `warn!` so a
/// corrupted row never silently disappears from a bulk loader.
fn collect_rows_with_warn<T, I>(rows: I, label: &'static str) -> Result<Vec<T>, StateError>
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
    Ok(out)
}

#[async_trait]
impl StateDb for SqliteStateDb {
    #[cfg(test)]
    async fn should_download(
        &self,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError> {
        let id_owned = id.to_owned();
        let version_size_owned = version_size.to_owned();
        let result: Option<(String, String, Option<String>)> = self
            .with_conn("should_download", move |conn| {
                conn.query_row(
                    "SELECT status, checksum, local_path FROM assets WHERE id = ?1 AND version_size = ?2",
                    [&id_owned, &version_size_owned],
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
                    AssetStatus::Pending | AssetStatus::Failed => Ok(true),
                }
            }
        }
    }

    async fn upsert_seen(&self, record: &AssetRecord) -> Result<(), StateError> {
        let record = record.clone();
        self.with_conn("upsert_seen", move |conn| {
            let last_seen_at = Utc::now().timestamp();

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
            let metadata_hash: Option<&str> =
                meta.metadata_hash.as_deref().or(computed_hash.as_deref());

            let mut stmt = conn
                .prepare_cached(
                    r"
                INSERT INTO assets (
                    id, version_size, checksum, filename, created_at, added_at,
                    size_bytes, media_type, status, last_seen_at,
                    source, is_favorite, rating, latitude, longitude, altitude,
                    orientation, duration_secs, timezone_offset, width, height,
                    title, keywords, description, media_subtype, burst_id,
                    is_hidden, is_archived, modified_at, is_deleted, deleted_at,
                    provider_data, metadata_hash
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9,
                        ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
                        ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32)
                ON CONFLICT(id, version_size) DO UPDATE SET
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
                    metadata_hash = excluded.metadata_hash
                ",
                )
                .map_err(|e| StateError::query("upsert_seen::prepare", e))?;
            stmt.execute(rusqlite::params![
                &record.id,
                record.version_size.as_str(),
                &record.checksum,
                &record.filename,
                record.created_at.timestamp(),
                record.added_at.map(|dt| dt.timestamp()),
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

            Ok(())
        })
        .await
    }

    async fn mark_downloaded(
        &self,
        id: &str,
        version_size: &str,
        local_path: &Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), StateError> {
        let downloaded_at = Utc::now().timestamp();
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let local_path = local_path.to_path_buf();
        let local_checksum = local_checksum.to_owned();
        let download_checksum = download_checksum.map(str::to_owned);

        self.with_conn("mark_downloaded", move |conn| {
            let rows = conn
                .execute(
                    "UPDATE assets SET status = 'downloaded', downloaded_at = ?1, local_path = ?2, \
                     local_checksum = ?3, download_checksum = ?4, last_error = NULL \
                     WHERE id = ?5 AND version_size = ?6",
                    rusqlite::params![
                        downloaded_at,
                        local_path.to_string_lossy(),
                        &local_checksum,
                        download_checksum.as_deref(),
                        &id,
                        &version_size
                    ],
                )
                .map_err(|e| StateError::query("mark_downloaded", e))?;

            if rows == 0 {
                tracing::warn!(
                    id = %id,
                    version_size = %version_size,
                    "mark_downloaded matched 0 rows — asset may not have been recorded via upsert_seen"
                );
                crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.inc();
            }

            Ok(())
        })
        .await
    }

    async fn mark_failed(
        &self,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError> {
        let id = id.to_owned();
        let version_size = version_size.to_owned();
        let error = error.to_owned();

        self.with_conn("mark_failed", move |conn| {
            let rows = conn
                .execute(
                    "UPDATE assets SET status = 'failed', download_attempts = download_attempts + 1, \
                     last_error = ?1 WHERE id = ?2 AND version_size = ?3",
                    rusqlite::params![&error, &id, &version_size],
                )
                .map_err(|e| StateError::query("mark_failed", e))?;

            if rows == 0 {
                tracing::warn!(
                    id = %id,
                    version_size = %version_size,
                    "mark_failed matched 0 rows; caller must upsert_seen before mark_failed \
                     (producer-dispatch invariant). Failure not persisted"
                );
            }

            Ok(())
        })
        .await
    }

    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_failed", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' \
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

    async fn get_failed_sample(&self, limit: u32) -> Result<(Vec<AssetRecord>, u64), StateError> {
        self.with_conn("get_failed_sample", move |conn| {
            let total: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'failed'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("get_failed_sample", e))?;

            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'failed' \
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

    async fn get_pending(&self) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_pending", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets WHERE status = 'pending' \
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

    async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        self.with_conn("get_summary", move |conn| {
            let (total_assets, downloaded, pending, failed, downloaded_bytes) = conn
                .query_row(
                    "SELECT \
                         COUNT(*), \
                         COUNT(CASE WHEN status = 'downloaded' THEN 1 END), \
                         COUNT(CASE WHEN status = 'pending' THEN 1 END), \
                         COUNT(CASE WHEN status = 'failed' THEN 1 END), \
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
                        ))
                    },
                )
                .map(|(t, d, p, f, b)| {
                    (
                        u64::try_from(t).unwrap_or(0),
                        u64::try_from(d).unwrap_or(0),
                        u64::try_from(p).unwrap_or(0),
                        u64::try_from(f).unwrap_or(0),
                        u64::try_from(b).unwrap_or(0),
                    )
                })
                .map_err(|e| StateError::query("get_summary", e))?;

            let last_sync: Option<(Option<i64>, Option<i64>)> = conn
                .query_row(
                    "SELECT started_at, completed_at FROM sync_runs ORDER BY id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| StateError::query("get_summary", e))?;

            let (last_sync_started, last_sync_completed) = match last_sync {
                Some((started, completed)) => (
                    started.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                    completed.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
                ),
                None => (None, None),
            };

            Ok(SyncSummary {
                total_assets,
                downloaded,
                pending,
                failed,
                downloaded_bytes,
                last_sync_completed,
                last_sync_started,
            })
        })
        .await
    }

    async fn get_downloaded_page(
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

    async fn start_sync_run(&self) -> Result<i64, StateError> {
        let started_at = Utc::now().timestamp();
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

    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError> {
        let completed_at = Utc::now().timestamp();
        let assets_seen = i64::try_from(stats.assets_seen).unwrap_or(i64::MAX);
        let assets_downloaded = i64::try_from(stats.assets_downloaded).unwrap_or(i64::MAX);
        let assets_failed = i64::try_from(stats.assets_failed).unwrap_or(i64::MAX);
        let interrupted_i32 = i32::from(stats.interrupted);
        let status = if stats.interrupted {
            "interrupted"
        } else {
            "complete"
        };

        self.with_conn("complete_sync_run", move |conn| {
            conn.execute(
                "UPDATE sync_runs SET completed_at = ?1, assets_seen = ?2, assets_downloaded = ?3, \
                 assets_failed = ?4, interrupted = ?5, status = ?6 WHERE id = ?7",
                rusqlite::params![
                    completed_at,
                    assets_seen,
                    assets_downloaded,
                    assets_failed,
                    interrupted_i32,
                    status,
                    run_id
                ],
            )
            .map_err(|e| StateError::query("complete_sync_run", e))?;

            Ok(())
        })
        .await
    }

    async fn promote_orphaned_sync_runs(&self) -> Result<u64, StateError> {
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

    async fn begin_enum_progress(&self, zone: &str) -> Result<(), StateError> {
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

    async fn end_enum_progress(&self, zone: &str) -> Result<(), StateError> {
        let key = format!("enum_in_progress:{zone}");
        self.with_conn("end_enum_progress", move |conn| {
            conn.execute("DELETE FROM metadata WHERE key = ?1", [key])
                .map_err(|e| StateError::query("end_enum_progress", e))?;
            Ok(())
        })
        .await
    }

    async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, StateError> {
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

    async fn reset_failed(&self) -> Result<u64, StateError> {
        let (failed, _, _) = self.prepare_for_retry().await?;
        Ok(failed)
    }

    async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), StateError> {
        self.with_conn("prepare_for_retry", move |conn| {
            let failed = conn
                .execute(
                    "UPDATE assets SET status = 'pending', download_attempts = 0, last_error = NULL \
                     WHERE status = 'failed'",
                    [],
                )
                .map_err(|e| StateError::query("prepare_for_retry", e))? as u64;

            let pending = conn
                .execute(
                    "UPDATE assets SET download_attempts = 0, last_error = NULL \
                     WHERE status = 'pending' AND download_attempts > 0",
                    [],
                )
                .map_err(|e| StateError::query("prepare_for_retry", e))? as u64;

            let total_pending: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM assets WHERE status = 'pending'",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("prepare_for_retry", e))?;
            #[allow(clippy::cast_sign_loss, reason = "SQL COUNT(*) is always non-negative")]
            let total_pending = total_pending as u64;

            Ok((failed, pending, total_pending))
        })
        .await
    }

    async fn promote_pending_to_failed(&self, seen_since: i64) -> Result<u64, StateError> {
        self.with_conn("promote_pending_to_failed", move |conn| {
            // Only promote assets the producer dispatched this sync (last_seen_at
            // was bumped by upsert_seen at or after sync_started_at) that never
            // reached mark_downloaded or mark_failed. See the trait doc comment
            // and issue #211 for the rationale.
            let promoted = conn
                .execute(
                    "UPDATE assets SET status = 'failed', last_error = 'Not resolved during sync' \
                     WHERE status = 'pending' AND last_seen_at >= ?1",
                    rusqlite::params![seen_since],
                )
                .map_err(|e| StateError::query("promote_pending_to_failed", e))?
                as u64;

            Ok(promoted)
        })
        .await
    }

    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
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
                .prepare_cached("SELECT id, version_size FROM assets WHERE status = 'downloaded'")
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;

            let mut ids = HashSet::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_downloaded_ids", e))?;
            for row in rows {
                ids.insert(row.map_err(|e| StateError::query("get_downloaded_ids", e))?);
            }

            Ok(ids)
        })
        .await
    }

    async fn get_all_known_ids(&self) -> Result<HashSet<String>, StateError> {
        self.with_conn("get_all_known_ids", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT DISTINCT id FROM assets")
                .map_err(|e| StateError::query("get_all_known_ids", e))?;

            let ids = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| StateError::query("get_all_known_ids", e))?
                .collect::<Result<HashSet<_>, _>>()
                .map_err(|e| StateError::query("get_all_known_ids", e))?;

            Ok(ids)
        })
        .await
    }

    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String), String>, StateError> {
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
                    "SELECT id, version_size, checksum FROM assets WHERE status = 'downloaded'",
                )
                .map_err(|e| StateError::query("get_downloaded_checksums", e))?;

            let mut checksums = HashMap::with_capacity(count);
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        row.get::<_, String>(2)?,
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

    async fn get_attempt_counts(&self) -> Result<HashMap<String, u32>, StateError> {
        self.with_conn("get_attempt_counts", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id, MAX(download_attempts) FROM assets \
                     WHERE download_attempts > 0 GROUP BY id",
                )
                .map_err(|e| StateError::query("get_attempt_counts", e))?;

            let counts = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let count: i64 = row.get(1)?;
                    Ok((id, u32::try_from(count).unwrap_or(u32::MAX)))
                })
                .map_err(|e| StateError::query("get_attempt_counts", e))?
                .collect::<Result<HashMap<_, _>, _>>()
                .map_err(|e| StateError::query("get_attempt_counts", e))?;

            Ok(counts)
        })
        .await
    }

    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError> {
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

    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
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

    async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError> {
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

    async fn touch_last_seen_many(&self, asset_ids: &[&str]) -> Result<(), StateError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = asset_ids.iter().map(|s| (*s).to_owned()).collect();
        self.with_conn_mut("touch_last_seen_many", move |conn| {
            let now = Utc::now().timestamp();
            let tx = conn
                .transaction()
                .map_err(|e| StateError::query("touch_last_seen_many::begin", e))?;
            {
                let mut stmt = tx
                    .prepare_cached("UPDATE assets SET last_seen_at = ?1 WHERE id = ?2")
                    .map_err(|e| StateError::query("touch_last_seen_many::prepare", e))?;
                for id in &ids {
                    stmt.execute(rusqlite::params![now, id])
                        .map_err(|e| StateError::query("touch_last_seen_many::execute", e))?;
                }
            }
            tx.commit()
                .map_err(|e| StateError::query("touch_last_seen_many::commit", e))?;
            Ok(())
        })
        .await
    }

    async fn sample_downloaded_paths(&self, limit: usize) -> Result<Vec<PathBuf>, StateError> {
        self.with_conn("sample_downloaded_paths", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT local_path FROM assets WHERE status = 'downloaded' \
                     AND local_path IS NOT NULL ORDER BY RANDOM() LIMIT ?1",
                )
                .map_err(|e| StateError::query("sample_downloaded_paths", e))?;

            let rows = stmt
                .query_map(rusqlite::params![limit as i64], |row| {
                    let p: String = row.get(0)?;
                    Ok(PathBuf::from(p))
                })
                .map_err(|e| StateError::query("sample_downloaded_paths", e))?;
            collect_rows_with_warn(rows, "sample_downloaded_paths")
        })
        .await
    }

    async fn add_asset_album(
        &self,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError> {
        let asset_id = asset_id.to_owned();
        let album_name = album_name.to_owned();
        let source = source.to_owned();
        self.with_conn("add_asset_album", move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO asset_albums (asset_id, album_name, source) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![asset_id, album_name, source],
            )
            .map_err(|e| StateError::query("add_asset_album", e))?;
            Ok(())
        })
        .await
    }

    async fn get_all_asset_albums(&self) -> Result<Vec<(String, String)>, StateError> {
        self.with_conn("get_all_asset_albums", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT asset_id, album_name FROM asset_albums")
                .map_err(|e| StateError::query("get_all_asset_albums", e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_asset_albums", e))?;
            collect_rows_with_warn(rows, "get_all_asset_albums")
        })
        .await
    }

    async fn get_all_asset_people(&self) -> Result<Vec<(String, String)>, StateError> {
        self.with_conn("get_all_asset_people", move |conn| {
            let mut stmt = conn
                .prepare_cached("SELECT asset_id, person_name FROM asset_people")
                .map_err(|e| StateError::query("get_all_asset_people", e))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| StateError::query("get_all_asset_people", e))?;
            collect_rows_with_warn(rows, "get_all_asset_people")
        })
        .await
    }

    async fn mark_soft_deleted(
        &self,
        asset_id: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<(), StateError> {
        let asset_id = asset_id.to_owned();
        self.with_conn("mark_soft_deleted", move |conn| {
            conn.execute(
                "UPDATE assets SET is_deleted = 1, deleted_at = COALESCE(?1, deleted_at) \
                 WHERE id = ?2",
                rusqlite::params![deleted_at.map(|dt| dt.timestamp()), asset_id],
            )
            .map_err(|e| StateError::query("mark_soft_deleted", e))?;
            Ok(())
        })
        .await
    }

    async fn mark_hidden_at_source(&self, asset_id: &str) -> Result<(), StateError> {
        let asset_id = asset_id.to_owned();
        self.with_conn("mark_hidden_at_source", move |conn| {
            conn.execute(
                "UPDATE assets SET is_hidden = 1 WHERE id = ?1",
                rusqlite::params![asset_id],
            )
            .map_err(|e| StateError::query("mark_hidden_at_source", e))?;
            Ok(())
        })
        .await
    }

    async fn record_metadata_write_failure(
        &self,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let ts = Utc::now().timestamp();
        let asset_id = asset_id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn("record_metadata_write_failure", move |conn| {
            conn.execute(
                "UPDATE assets SET metadata_write_failed_at = ?1 \
                 WHERE id = ?2 AND version_size = ?3",
                rusqlite::params![ts, asset_id, version_size],
            )
            .map_err(|e| StateError::query("record_metadata_write_failure", e))?;
            Ok(())
        })
        .await
    }

    async fn clear_metadata_write_failure(
        &self,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), StateError> {
        let asset_id = asset_id.to_owned();
        let version_size = version_size.to_owned();
        self.with_conn("clear_metadata_write_failure", move |conn| {
            conn.execute(
                "UPDATE assets SET metadata_write_failed_at = NULL \
                 WHERE id = ?1 AND version_size = ?2",
                rusqlite::params![asset_id, version_size],
            )
            .map_err(|e| StateError::query("clear_metadata_write_failure", e))?;
            Ok(())
        })
        .await
    }

    async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<HashMap<(String, String), String>, StateError> {
        self.with_conn("get_downloaded_metadata_hashes", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id, version_size, metadata_hash FROM assets \
                     WHERE status = 'downloaded' AND metadata_hash IS NOT NULL",
                )
                .map_err(|e| StateError::query("get_downloaded_metadata_hashes", e))?;
            let mut hashes: HashMap<(String, String), String> = HashMap::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        row.get::<_, String>(2)?,
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

    async fn get_pending_metadata_rewrites(
        &self,
        limit: usize,
    ) -> Result<Vec<AssetRecord>, StateError> {
        self.with_conn("get_pending_metadata_rewrites", move |conn| {
            let sql = format!(
                "SELECT {ASSET_COLUMNS} FROM assets \
                 WHERE metadata_write_failed_at IS NOT NULL \
                   AND status = 'downloaded' \
                   AND local_path IS NOT NULL \
                 ORDER BY metadata_write_failed_at ASC \
                 LIMIT ?1"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| StateError::query("get_pending_metadata_rewrites", e))?;
            let rows = stmt
                .query_map(
                    [i64::try_from(limit).unwrap_or(i64::MAX)],
                    row_to_asset_record,
                )
                .map_err(|e| StateError::query("get_pending_metadata_rewrites", e))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| StateError::query("get_pending_metadata_rewrites", e))
        })
        .await
    }

    async fn get_metadata_retry_markers(&self) -> Result<HashSet<(String, String)>, StateError> {
        self.with_conn("get_metadata_retry_markers", move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT id, version_size FROM assets \
                     WHERE metadata_write_failed_at IS NOT NULL",
                )
                .map_err(|e| StateError::query("get_metadata_retry_markers", e))?;
            let mut markers: HashSet<(String, String)> = HashSet::new();
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
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

    async fn update_metadata_hash(
        &self,
        asset_id: &str,
        version_size: &str,
        metadata_hash: &str,
    ) -> Result<(), StateError> {
        let asset_id = asset_id.to_owned();
        let version_size = version_size.to_owned();
        let metadata_hash = metadata_hash.to_owned();
        self.with_conn("update_metadata_hash", move |conn| {
            conn.execute(
                "UPDATE assets SET metadata_hash = ?1 \
                 WHERE id = ?2 AND version_size = ?3",
                rusqlite::params![metadata_hash, asset_id, version_size],
            )
            .map_err(|e| StateError::query("update_metadata_hash", e))?;
            Ok(())
        })
        .await
    }

    async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, StateError> {
        self.with_conn("has_downloaded_without_metadata_hash", move |conn| {
            let exists: i64 = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM assets WHERE status = 'downloaded' \
                     AND metadata_hash IS NULL)",
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| StateError::query("has_downloaded_without_metadata_hash", e))?;
            Ok(exists != 0)
        })
        .await
    }
}

#[cfg(test)]
impl SqliteStateDb {
    /// Overwrite `last_seen_at` for a specific asset. Used by tests that need
    /// to simulate a pending row carried over from a prior sync. Callable
    /// from any test module in the crate so cross-module state tests (e.g.
    /// pipeline-level ghost-loop regression) don't have to reach for raw
    /// `rusqlite::Connection` plumbing.
    pub(crate) fn backdate_last_seen(&self, asset_id: &str, ts: i64) {
        let conn = self.acquire_lock("test_backdate_last_seen").unwrap();
        conn.execute(
            "UPDATE assets SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![ts, asset_id],
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
     deleted_at, provider_data, metadata_hash";

/// Total number of columns in `ASSET_COLUMNS`. Validated by a unit test that
/// asserts `row_to_asset_record` reads exactly this many indices (0..N).
#[cfg(test)]
const ASSET_COLUMN_COUNT: usize = 38;

/// Convert a database row to an `AssetRecord`.
///
/// Returns `rusqlite::Error` on column extraction failures instead of silently
/// falling back to defaults, so schema mismatches or corruption are surfaced.
fn row_to_asset_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetRecord> {
    let id: String = row.get(0)?;
    let version_size_str: String = row.get(1)?;
    let checksum: String = row.get(2)?;
    let filename: String = row.get(3)?;
    let created_at_ts: i64 = row.get(4)?;
    let added_at_ts: Option<i64> = row.get(5)?;
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
    let source = source_str.map(|s| crate::string_interner::intern(&s));
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
        id: id.into_boxed_str(),
        checksum: checksum.into_boxed_str(),
        filename: filename.into_boxed_str(),
        local_path: local_path_str.map(PathBuf::from),
        last_error,
        local_checksum,
        size_bytes: u64::try_from(size_bytes).unwrap_or(0),
        created_at: Utc
            .timestamp_opt(created_at_ts, 0)
            .single()
            .unwrap_or(DateTime::UNIX_EPOCH),
        added_at: added_at_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
        downloaded_at: downloaded_at_ts.and_then(|ts| Utc.timestamp_opt(ts, 0).single()),
        last_seen_at: Utc
            .timestamp_opt(last_seen_at_ts, 0)
            .single()
            .unwrap_or(DateTime::UNIX_EPOCH),
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

    #[tokio::test]
    async fn test_open_creates_db() {
        let dir = test_dir();
        let path = dir.path().join("test.db");
        let db = SqliteStateDb::open(&path).await.unwrap();
        assert!(path.exists());
        assert_eq!(db.path(), path);
    }

    #[tokio::test]
    async fn test_should_download_not_in_db() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let result = db
            .should_download("ABC123", "original", "checksum", Path::new("/tmp/file.jpg"))
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
        db.mark_downloaded("ABC123", "original", &file_path, "abc123hash", None)
            .await
            .unwrap();

        // Downloaded asset with existing file should not be downloaded
        let result = db
            .should_download("ABC123", "original", "checksum123", &file_path)
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
        db.mark_downloaded("ABC123", "original", &file_path, "oldhash", None)
            .await
            .unwrap();

        // Different checksum should trigger re-download
        let result = db
            .should_download("ABC123", "original", "new_checksum", &file_path)
            .await
            .unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_mark_failed_and_get_failed() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("ABC123").build();

        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("ABC123", "original", "Connection timeout")
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
            db.mark_failed(id, "original", "boom").await.unwrap();
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
            db.mark_failed(&id, "original", "boom").await.unwrap();
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
        db.mark_failed("ABC123", "original", "Error").await.unwrap();

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
        db.mark_failed("FAILED_1", "original", "Error")
            .await
            .unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 6);
        assert_eq!(summary.pending, 3);
        assert_eq!(summary.downloaded, 2);
        assert_eq!(summary.failed, 1);
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
            interrupted: false,
        };

        db.complete_sync_run(run_id, &stats).await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert!(summary.last_sync_started.is_some());
        assert!(summary.last_sync_completed.is_some());
    }

    // ── sync_runs status lifecycle ─────────────────────────────────────────

    fn status_of(db: &SqliteStateDb, run_id: i64) -> String {
        let conn = db.acquire_lock("test_status_of").unwrap();
        conn.query_row(
            "SELECT status FROM sync_runs WHERE id = ?1",
            [run_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
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
            interrupted: false,
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();
        assert_eq!(status_of(&db, run_id), "complete");
    }

    #[tokio::test]
    async fn sync_run_status_is_interrupted_when_flagged() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let run_id = db.start_sync_run().await.unwrap();
        let stats = SyncRunStats {
            assets_seen: 1,
            assets_downloaded: 0,
            assets_failed: 0,
            interrupted: true,
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
            interrupted: false,
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
            interrupted: false,
        };
        db.complete_sync_run(run_id, &stats).await.unwrap();

        let promoted = db.promote_orphaned_sync_runs().await.unwrap();
        assert_eq!(promoted, 0);
    }

    /// CG-14: promotion is idempotent — once an orphan has been flipped to
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

    /// CG-14 corollary: a freshly-completed `sync_runs` row in between
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
            interrupted: false,
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
        db.mark_downloaded("ABC123", "original", &file_path, "abc123hash", None)
            .await
            .unwrap();

        // Upsert again - should preserve downloaded status
        db.upsert_seen(&record).await.unwrap();

        // Should still be downloaded (file exists)
        let result = db
            .should_download("ABC123", "original", "checksum123", &file_path)
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
            db.mark_downloaded(&format!("DL_{}", i), "original", &path, "hash", None)
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
            db.mark_downloaded(&format!("DL_{}", i), "original", &path, "hash", None)
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
        assert!(ids.contains(&("DL_0".to_string(), "original".to_string())));
        assert!(ids.contains(&("DL_1".to_string(), "original".to_string())));
        assert!(ids.contains(&("DL_2".to_string(), "original".to_string())));
        assert!(!ids.contains(&("PENDING_1".to_string(), "original".to_string())));
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
            db.mark_downloaded(&format!("DL_{}", i), "original", &path, "hash", None)
                .await
                .unwrap();
        }

        let checksums = db.get_downloaded_checksums().await.unwrap();
        assert_eq!(checksums.len(), 2);
        assert_eq!(
            checksums.get(&("DL_0".to_string(), "original".to_string())),
            Some(&"checksum_0".to_string())
        );
        assert_eq!(
            checksums.get(&("DL_1".to_string(), "original".to_string())),
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
            db.mark_downloaded(&format!("DL_{}", i), "original", &path, "hash", None)
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
        db.mark_failed("FAILED_1", "original", "test error")
            .await
            .unwrap();

        let known_ids = db.get_all_known_ids().await.unwrap();
        // Should include all 4 assets regardless of status
        assert_eq!(known_ids.len(), 4);
        assert!(known_ids.contains("DL_0"));
        assert!(known_ids.contains("DL_1"));
        assert!(known_ids.contains("PENDING_1"));
        assert!(known_ids.contains("FAILED_1"));

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
        db.mark_downloaded("DL_1", "original", &path, "hash", None)
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
        db.mark_downloaded("DL_1", "original", &path, "hash", None)
            .await
            .unwrap();

        // Add a failed asset
        let failed = TestAssetRecord::new("FAIL_1")
            .checksum("ck2")
            .filename("photo2.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&failed).await.unwrap();
        db.mark_failed("FAIL_1", "original", "download error")
            .await
            .unwrap();

        // reset_failed should reset exactly 1
        let count = db.reset_failed().await.unwrap();
        assert_eq!(count, 1);

        // After reset, the failed asset should be in known_ids but not downloaded_ids
        let known = db.get_all_known_ids().await.unwrap();
        assert_eq!(known.len(), 2);
        assert!(known.contains("DL_1"));
        assert!(known.contains("FAIL_1"));

        let downloaded = db.get_downloaded_ids().await.unwrap();
        assert_eq!(downloaded.len(), 1);
        assert!(downloaded.contains(&("DL_1".to_string(), "original".to_string())));
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
        db.touch_last_seen_many(&["TOUCH_1"]).await.unwrap();

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

        db.touch_last_seen_many(&id_refs).await.unwrap();

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
        db.touch_last_seen_many(&[]).await.unwrap();
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
                    "INSERT INTO assets (id, version_size, checksum, filename, created_at, size_bytes, media_type, status, downloaded_at, local_path, local_checksum, last_seen_at)
                     VALUES (?1, 'original', ?2, ?3, ?4, ?5, 'photo', 'downloaded', ?4, ?6, ?2, ?4)",
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
                "INSERT INTO assets (id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at)
                 VALUES ('AQvz7R8kP4', 'superHD', 'a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6abcd', 'IMG_4231.HEIC', ?1, 8294400, 'photo', 'pending', ?1)",
                rusqlite::params![now],
            ).unwrap();
        }

        // Act: query should_download with the same unknown version_size
        let result = db
            .should_download(
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
            ids[1],
            "original",
            &path1,
            "ef2d127de37b942baad06145e54b0c619a1f22327b2ebbcfbec78f5564afe39d",
            None,
        )
        .await
        .unwrap();

        db.mark_failed(ids[2], "original", "HTTP 503 Service Unavailable")
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
                "INSERT INTO assets (id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at)
                 VALUES ('ABx7kQ9nR2', 'original', 'b5bb9d8014a0f9b1d61e21e796d78dccdf1352f23cd32812f4850b878ae4944c', 'IMG_7892.HEIC', ?1, 6_291_456, 'photo', 'corrupted_junk', ?1)",
                rusqlite::params![now],
            ).unwrap();
        }

        // Act: retrieve via get_failed (won't match 'corrupted_junk'), and get_downloaded_page also won't match.
        // Instead, query via should_download which reads the row and parses status.
        // The unknown status falls back to Pending via AssetStatus::from_str -> unwrap_or(Pending).
        let needs_download = db
            .should_download(
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
            db.mark_downloaded(&id, "original", &path, &format!("local_ck_{i}"), None)
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
            interrupted: false,
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
            db.mark_downloaded(&id, "original", &path, &format!("localhash{i}"), None)
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
            db.mark_failed(&id, "original", &format!("HTTP 500 attempt {i}"))
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
        db.mark_downloaded("ADwnloaded1", "original", &path, "localhash1", None)
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
        db.mark_failed("AStuck1", "original", "transient error")
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
            db.mark_failed(&id, "original", "HTTP 500").await.unwrap();
        }

        let before = db.get_summary().await.unwrap();
        assert_eq!(before.downloaded, 1);
        assert_eq!(before.pending, 2); // normal + stuck
        assert_eq!(before.failed, 2);

        let (failed_reset, pending_reset, total_pending) = db.prepare_for_retry().await.unwrap();

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
        db.mark_downloaded("ADownloaded", "original", &path, "localhash", None)
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
        db.mark_failed("AFailed", "original", "HTTP 500")
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

    #[tokio::test]
    async fn test_get_attempt_counts() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for id in ["A", "B"] {
            let record = TestAssetRecord::new(id)
                .checksum(&format!("ck_{id}"))
                .filename(&format!("{id}.jpg"))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
        }

        db.mark_failed("A", "original", "error 1").await.unwrap();
        db.mark_failed("A", "original", "error 2").await.unwrap();
        db.mark_failed("A", "original", "error 3").await.unwrap();
        db.mark_failed("B", "original", "error 1").await.unwrap();

        let counts = db.get_attempt_counts().await.unwrap();
        assert_eq!(counts.get("A"), Some(&3));
        assert_eq!(counts.get("B"), Some(&1));
    }

    #[tokio::test]
    async fn test_get_attempt_counts_empty() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let counts = db.get_attempt_counts().await.unwrap();
        assert!(counts.is_empty());
    }

    // ── Gap: mark_downloaded on non-existent record (no upsert_seen) ──

    #[tokio::test]
    async fn mark_downloaded_without_upsert_seen_succeeds_with_zero_rows() {
        // mark_downloaded does an UPDATE, not an UPSERT. If the asset was
        // never recorded via upsert_seen, it updates 0 rows and logs a
        // warning. This should NOT return an error -- it's a graceful no-op.
        let db = SqliteStateDb::open_in_memory().unwrap();

        let result = db
            .mark_downloaded(
                "NEVER_SEEN",
                "original",
                Path::new("/tmp/never.jpg"),
                "abc123",
                None,
            )
            .await;
        assert!(
            result.is_ok(),
            "mark_downloaded on unknown asset should succeed (0-row update)"
        );

        // Verify: the asset is NOT in the DB (it was never inserted)
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 0);
        assert_eq!(summary.total_assets, 0);
    }

    /// Robustness review NB-1 (2026-04-25): a `mark_downloaded` call
    /// without a prior `upsert_seen` is self-healing in 1 cycle, but a
    /// regression that increases the rate (producer-dispatch invariant
    /// quietly broken) needs to be visible in /metrics rather than only
    /// in logs. Pin the counter increment so the wiring can't be silently
    /// dropped on a future refactor.
    #[tokio::test]
    async fn mark_downloaded_zero_rows_increments_metric_counter() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let before = crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.get();
        db.mark_downloaded(
            "NEVER_SEEN_FOR_METRIC",
            "original",
            Path::new("/tmp/never_metric.jpg"),
            "abc123",
            None,
        )
        .await
        .expect("mark_downloaded on unknown row should succeed");
        let after = crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.get();

        assert!(
            after > before,
            "counter should advance by at least 1 (other parallel tests may also \
             increment); got before={before} after={after}"
        );
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
            db.mark_failed("RETRY_ME", "original", &format!("error {i}"))
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
        db.touch_last_seen_many(&["PENDING_CARRYOVER"])
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
        db.mark_downloaded("PRESERVE", "original", &file_path, "hash_v1", None)
            .await
            .unwrap();

        // Re-upsert with updated metadata (e.g., checksum changed in iCloud)
        let updated = TestAssetRecord::new("PRESERVE")
            .checksum("ck_v2")
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
            "DL_CK",
            "original",
            Path::new("/photos/photo.jpg"),
            "local_sha256",
            Some("pre_exif_sha256"),
        )
        .await
        .unwrap();

        // Verify via get_downloaded_page that the asset is downloaded
        let page = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(&*page[0].id, "DL_CK");
        assert_eq!(
            page[0].local_checksum.as_deref(),
            Some("local_sha256"),
            "local_checksum should be stored"
        );
    }

    // ── Gap: sample_downloaded_paths returns actual paths ─────────────

    #[tokio::test]
    async fn sample_downloaded_paths_returns_stored_paths() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..5 {
            let record = TestAssetRecord::new(&format!("SAMPLE_{i}"))
                .checksum(&format!("ck_{i}"))
                .filename(&format!("photo_{i}.jpg"))
                .size(1000)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                &format!("SAMPLE_{i}"),
                "original",
                Path::new(&format!("/photos/photo_{i}.jpg")),
                &format!("hash_{i}"),
                None,
            )
            .await
            .unwrap();
        }

        let paths = db.sample_downloaded_paths(10).await.unwrap();
        assert!(
            !paths.is_empty(),
            "should return at least some downloaded paths"
        );
        assert!(
            paths.len() <= 5,
            "should not return more than the number of downloaded assets"
        );
        for p in &paths {
            assert!(
                p.to_str().unwrap().starts_with("/photos/"),
                "path should match what was stored: {:?}",
                p
            );
        }
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
    async fn add_asset_album_is_idempotent() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("A1", "Favorites", "icloud")
            .await
            .unwrap();
        db.add_asset_album("A1", "Favorites", "icloud")
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
        db.add_asset_album("A1", "Favorites", "icloud")
            .await
            .unwrap();
        db.add_asset_album("A1", "Favorites", "takeout")
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

    #[tokio::test]
    async fn get_all_asset_people_returns_every_pair() {
        // asset_people has no production writer yet (reserved for future
        // provider adapters); insert test rows via raw SQL so this covers
        // the read path without adding a trait method that would sit unused
        // in production builds.
        let db = SqliteStateDb::open_in_memory().unwrap();
        {
            let conn = db.acquire_lock("seed").unwrap();
            for (aid, person) in [("A1", "Alice"), ("A1", "Bob"), ("A2", "Alice")] {
                conn.execute(
                    "INSERT INTO asset_people (asset_id, person_name) VALUES (?1, ?2)",
                    rusqlite::params![aid, person],
                )
                .unwrap();
            }
        }
        let rows = db.get_all_asset_people().await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.contains(&("A1".into(), "Alice".into())));
        assert!(rows.contains(&("A1".into(), "Bob".into())));
        assert!(rows.contains(&("A2".into(), "Alice".into())));
    }

    #[tokio::test]
    async fn get_all_asset_albums_returns_every_pair() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.add_asset_album("A1", "Favorites", "icloud")
            .await
            .unwrap();
        db.add_asset_album("A1", "Trip", "icloud").await.unwrap();
        db.add_asset_album("A2", "Favorites", "icloud")
            .await
            .unwrap();
        let rows = db.get_all_asset_albums().await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.contains(&("A1".into(), "Favorites".into())));
        assert!(rows.contains(&("A1".into(), "Trip".into())));
        assert!(rows.contains(&("A2".into(), "Favorites".into())));
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
        db.mark_soft_deleted("DEL_1", Some(when)).await.unwrap();

        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 2);
        for rec in &pending {
            assert!(
                rec.metadata.is_deleted,
                "is_deleted should be set for all versions"
            );
            assert_eq!(rec.metadata.deleted_at, Some(when));
        }
    }

    #[tokio::test]
    async fn mark_hidden_at_source_sets_flag() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        let rec = TestAssetRecord::new("HID_1").build();
        db.upsert_seen(&rec).await.unwrap();
        db.mark_hidden_at_source("HID_1").await.unwrap();
        let pending = db.get_pending().await.unwrap();
        assert!(pending[0].metadata.is_hidden);
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
        db.record_metadata_write_failure("MWF_1", "original")
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
        db.clear_metadata_write_failure("MWF_1", "original")
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
        db.mark_downloaded("D1", "original", Path::new("/a.jpg"), "h", None)
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
        loc_dict.insert("lng".into(), plist::Value::Real(-122.4194));
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
                "assetDate": {"value": 1736899200000_f64},
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
            db.mark_downloaded("WAL_KEEPER", "original", &file_path, "localhash", None)
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
            ids.contains(&("WAL_KEEPER".into(), "original".into())),
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
        db.mark_downloaded("KEEPER_1", "original", &file_path, "localhash", None)
            .await
            .unwrap();
        let prior_sync_ts = chrono::Utc::now().timestamp() - 86_400;
        db.backdate_last_seen("KEEPER_1", prior_sync_ts);

        let summary_before = db.get_summary().await.unwrap();
        assert_eq!(summary_before.downloaded, 1);
        let ids_before = db.get_downloaded_ids().await.unwrap();
        assert!(ids_before.contains(&("KEEPER_1".into(), "original".into())));

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
            ids_after.contains(&("KEEPER_1".into(), "original".into())),
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
}
