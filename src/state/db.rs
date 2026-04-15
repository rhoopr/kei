//! State database trait and `SQLite` implementation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension};

use super::error::StateError;
use super::schema;
use super::types::{
    AssetRecord, AssetStatus, MediaType, SyncRunStats, SyncSummary, VersionSizeKey,
};

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

    /// Insert or update an asset record after seeing it during sync.
    ///
    /// Updates `last_seen_at` and preserves existing download status.
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

    /// Promote stale pending assets to failed.
    ///
    /// Called at the end of a non-interrupted sync run. Only promotes pending
    /// assets whose `last_seen_at` is before `seen_since` - assets seen
    /// during this sync (including on-disk skips) are left alone.
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

    /// Update `last_seen_at` for all versions of an asset without requiring
    /// full metadata. Used by the early skip path to avoid path resolution.
    async fn touch_last_seen(&self, asset_id: &str) -> Result<(), StateError>;

    /// Sample up to `limit` local paths of downloaded assets.
    /// Used to spot-check that "downloaded" files still exist on disk.
    async fn sample_downloaded_paths(&self, limit: usize) -> Result<Vec<PathBuf>, StateError>;
}

/// `SQLite` implementation of the state database.
pub struct SqliteStateDb {
    /// Wrapped in Mutex because `rusqlite::Connection` is not Sync.
    /// Operations hold the lock briefly for fast `SQLite` queries (WAL mode).
    /// Only `open()` uses `spawn_blocking` for the heavier initial setup.
    conn: Mutex<Connection>,
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
            conn: Mutex::new(conn),
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
            conn: Mutex::new(conn),
            path: PathBuf::from(":memory:"),
        })
    }

    /// Get the path to the database file.
    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Acquire the database lock, adding the operation name to any error.
    fn acquire_lock(
        &self,
        operation: &str,
    ) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>, StateError> {
        self.conn
            .lock()
            .map_err(|e| StateError::LockPoisoned(format!("{operation}: {e}")))
    }
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
        // Query DB in a separate scope to ensure MutexGuard is dropped before any await
        let result: Option<(String, String, Option<String>)> = {
            let conn = self.acquire_lock("should_download")?;

            conn.query_row(
                "SELECT status, checksum, local_path FROM assets WHERE id = ?1 AND version_size = ?2",
                [id, version_size],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| StateError::query("should_download", e))?
        };

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
        let last_seen_at = Utc::now().timestamp();

        let conn = self.acquire_lock("upsert_seen")?;

        // Use INSERT OR REPLACE to handle both insert and update
        // But preserve existing status, downloaded_at, local_path, download_attempts, last_error
        conn.execute(
            r"
            INSERT INTO assets (id, version_size, checksum, filename, created_at, added_at, size_bytes, media_type, status, last_seen_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9)
            ON CONFLICT(id, version_size) DO UPDATE SET
                checksum = excluded.checksum,
                filename = excluded.filename,
                created_at = excluded.created_at,
                added_at = excluded.added_at,
                size_bytes = excluded.size_bytes,
                media_type = excluded.media_type,
                last_seen_at = excluded.last_seen_at
            ",
            rusqlite::params![
                &record.id,
                record.version_size.as_str(),
                &record.checksum,
                &record.filename,
                record.created_at.timestamp(),
                record.added_at.map(|dt| dt.timestamp()),
                i64::try_from(record.size_bytes).unwrap_or(i64::MAX),
                record.media_type.as_str(),
                last_seen_at,
            ],
        )
        .map_err(|e| StateError::query("upsert_seen", e))?;

        Ok(())
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

        let conn = self.acquire_lock("mark_downloaded")?;

        let rows = conn
            .execute(
                "UPDATE assets SET status = 'downloaded', downloaded_at = ?1, local_path = ?2, \
                 local_checksum = ?3, download_checksum = ?4, last_error = NULL \
                 WHERE id = ?5 AND version_size = ?6",
                rusqlite::params![
                    downloaded_at,
                    local_path.to_string_lossy(),
                    local_checksum,
                    download_checksum,
                    id,
                    version_size
                ],
            )
            .map_err(|e| StateError::query("mark_downloaded", e))?;

        if rows == 0 {
            tracing::warn!(
                id,
                version_size,
                "mark_downloaded matched 0 rows — asset may not have been recorded via upsert_seen"
            );
        }

        Ok(())
    }

    async fn mark_failed(
        &self,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError> {
        let conn = self.acquire_lock("mark_failed")?;

        let rows = conn
            .execute(
                "UPDATE assets SET status = 'failed', download_attempts = download_attempts + 1, \
                 last_error = ?1 WHERE id = ?2 AND version_size = ?3",
                rusqlite::params![error, id, version_size],
            )
            .map_err(|e| StateError::query("mark_failed", e))?;

        if rows == 0 {
            tracing::warn!(
                id,
                version_size,
                "mark_failed matched 0 rows — asset may not have been recorded via upsert_seen"
            );
        }

        Ok(())
    }

    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        let conn = self.acquire_lock("get_failed")?;

        let mut stmt = conn
            .prepare(
                "SELECT id, version_size, checksum, filename, created_at, added_at, size_bytes, media_type, status, downloaded_at, local_path, last_seen_at, download_attempts, last_error, local_checksum FROM assets WHERE status = 'failed'",
            )
            .map_err(|e| StateError::query("get_failed", e))?;

        let records = stmt
            .query_map([], row_to_asset_record)
            .map_err(|e| StateError::query("get_failed", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| StateError::query("get_failed", e))?;

        Ok(records)
    }

    async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        let conn = self.acquire_lock("get_summary")?;

        let (total_assets, downloaded, pending, failed) = conn
            .query_row(
                "SELECT \
                     COUNT(*), \
                     COUNT(CASE WHEN status = 'downloaded' THEN 1 END), \
                     COUNT(CASE WHEN status = 'pending' THEN 1 END), \
                     COUNT(CASE WHEN status = 'failed' THEN 1 END) \
                 FROM assets",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .map(|(t, d, p, f)| {
                (
                    u64::try_from(t).unwrap_or(0),
                    u64::try_from(d).unwrap_or(0),
                    u64::try_from(p).unwrap_or(0),
                    u64::try_from(f).unwrap_or(0),
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
            last_sync_completed,
            last_sync_started,
        })
    }

    async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<AssetRecord>, StateError> {
        let conn = self.acquire_lock("get_downloaded_page")?;

        let mut stmt = conn
            .prepare(
                "SELECT id, version_size, checksum, filename, created_at, added_at, size_bytes, media_type, status, downloaded_at, local_path, last_seen_at, download_attempts, last_error, local_checksum FROM assets WHERE status = 'downloaded' ORDER BY rowid LIMIT ?1 OFFSET ?2",
            )
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
    }

    async fn start_sync_run(&self) -> Result<i64, StateError> {
        let started_at = Utc::now().timestamp();

        let conn = self.acquire_lock("start_sync_run")?;

        conn.execute(
            "INSERT INTO sync_runs (started_at) VALUES (?1)",
            [started_at],
        )
        .map_err(|e| StateError::query("start_sync_run", e))?;

        let id = conn.last_insert_rowid();
        Ok(id)
    }

    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError> {
        let completed_at = Utc::now().timestamp();
        let assets_seen = i64::try_from(stats.assets_seen).unwrap_or(i64::MAX);
        let assets_downloaded = i64::try_from(stats.assets_downloaded).unwrap_or(i64::MAX);
        let assets_failed = i64::try_from(stats.assets_failed).unwrap_or(i64::MAX);
        let interrupted = i32::from(stats.interrupted);

        let conn = self.acquire_lock("complete_sync_run")?;

        conn.execute(
            "UPDATE sync_runs SET completed_at = ?1, assets_seen = ?2, assets_downloaded = ?3, assets_failed = ?4, interrupted = ?5 WHERE id = ?6",
            rusqlite::params![completed_at, assets_seen, assets_downloaded, assets_failed, interrupted, run_id],
        )
        .map_err(|e| StateError::query("complete_sync_run", e))?;

        Ok(())
    }

    async fn reset_failed(&self) -> Result<u64, StateError> {
        let (failed, _, _) = self.prepare_for_retry().await?;
        Ok(failed)
    }

    async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), StateError> {
        let conn = self.acquire_lock("prepare_for_retry")?;

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
        let total_pending = total_pending as u64;

        Ok((failed, pending, total_pending))
    }

    async fn promote_pending_to_failed(&self, seen_since: i64) -> Result<u64, StateError> {
        let conn = self.acquire_lock("promote_pending_to_failed")?;

        let promoted =
            conn.execute(
                "UPDATE assets SET status = 'failed', last_error = 'Not resolved during sync' \
                 WHERE status = 'pending' AND (last_seen_at IS NULL OR last_seen_at < ?1)",
                rusqlite::params![seen_since],
            )
            .map_err(|e| StateError::query("promote_pending_to_failed", e))? as u64;

        Ok(promoted)
    }

    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
        let conn = self.acquire_lock("get_downloaded_ids")?;

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
    }

    async fn get_all_known_ids(&self) -> Result<HashSet<String>, StateError> {
        let conn = self.acquire_lock("get_all_known_ids")?;

        let mut stmt = conn
            .prepare_cached("SELECT DISTINCT id FROM assets")
            .map_err(|e| StateError::query("get_all_known_ids", e))?;

        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| StateError::query("get_all_known_ids", e))?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|e| StateError::query("get_all_known_ids", e))?;

        Ok(ids)
    }

    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String), String>, StateError> {
        let conn = self.acquire_lock("get_downloaded_checksums")?;

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
            let (key, val) = row.map_err(|e| StateError::query("get_downloaded_checksums", e))?;
            checksums.insert(key, val);
        }

        Ok(checksums)
    }

    async fn get_attempt_counts(&self) -> Result<HashMap<String, u32>, StateError> {
        let conn = self.acquire_lock("get_attempt_counts")?;

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
    }

    async fn get_metadata(&self, key: &str) -> Result<Option<String>, StateError> {
        let conn = self.acquire_lock("get_metadata")?;

        let value = conn
            .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .optional()
            .map_err(|e| StateError::query("get_metadata", e))?;

        Ok(value)
    }

    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), StateError> {
        let conn = self.acquire_lock("set_metadata")?;

        conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )
        .map_err(|e| StateError::query("set_metadata", e))?;

        Ok(())
    }

    async fn delete_metadata_by_prefix(&self, prefix: &str) -> Result<u64, StateError> {
        let conn = self.acquire_lock("delete_metadata_by_prefix")?;

        let deleted = conn
            .execute(
                "DELETE FROM metadata WHERE key LIKE ?1",
                [format!("{prefix}%")],
            )
            .map_err(|e| StateError::query("delete_metadata_by_prefix", e))?;

        Ok(deleted as u64)
    }

    async fn touch_last_seen(&self, asset_id: &str) -> Result<(), StateError> {
        let conn = self.acquire_lock("touch_last_seen")?;

        let now = Utc::now().timestamp();
        conn.execute(
            "UPDATE assets SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![now, asset_id],
        )
        .map_err(|e| StateError::query("touch_last_seen", e))?;

        Ok(())
    }

    async fn sample_downloaded_paths(&self, limit: usize) -> Result<Vec<PathBuf>, StateError> {
        let conn = self.acquire_lock("sample_downloaded_paths")?;

        let mut stmt = conn
            .prepare_cached(
                "SELECT local_path FROM assets WHERE status = 'downloaded' \
                 AND local_path IS NOT NULL ORDER BY RANDOM() LIMIT ?1",
            )
            .map_err(|e| StateError::query("sample_downloaded_paths", e))?;

        let paths = stmt
            .query_map(rusqlite::params![limit as i64], |row| {
                let p: String = row.get(0)?;
                Ok(PathBuf::from(p))
            })
            .map_err(|e| StateError::query("sample_downloaded_paths", e))?
            .filter_map(Result::ok)
            .collect();

        Ok(paths)
    }
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

    Ok(AssetRecord {
        id,
        checksum,
        filename,
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
        assert_eq!(failed[0].id, "ABC123");
        assert_eq!(failed[0].last_error.as_deref(), Some("Connection timeout"));
        assert_eq!(failed[0].download_attempts, 1);
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
        db.touch_last_seen("TOUCH_1").await.unwrap();

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
                first_id = page[0].id.clone();
            }
            last_id = page.last().unwrap().id.clone();
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

        // 2 pending (should be promoted to failed)
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

        // Use a future timestamp so all pending assets are considered stale
        let future = chrono::Utc::now().timestamp() + 3600;
        let promoted = db.promote_pending_to_failed(future).await.unwrap();
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

    // ── Gap: promote_pending_to_failed boundary -- seen assets preserved ──

    #[tokio::test]
    async fn promote_pending_to_failed_preserves_recently_seen() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Insert an "old" asset with last_seen_at far in the past
        let old_record = TestAssetRecord::new("OLD_ASSET")
            .checksum("ck_old")
            .filename("old.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&old_record).await.unwrap();

        // Manually backdate OLD_ASSET's last_seen_at to 1 hour ago
        {
            let conn = db.acquire_lock("test_setup").unwrap();
            let old_ts = chrono::Utc::now().timestamp() - 3600;
            conn.execute(
                "UPDATE assets SET last_seen_at = ?1 WHERE id = 'OLD_ASSET'",
                rusqlite::params![old_ts],
            )
            .unwrap();
        }

        // Insert a "new" asset -- its last_seen_at will be now()
        let new_record = TestAssetRecord::new("NEW_ASSET")
            .checksum("ck_new")
            .filename("new.jpg")
            .size(2000)
            .build();
        db.upsert_seen(&new_record).await.unwrap();

        // Boundary: 30 minutes ago -- OLD_ASSET (1h ago) is before,
        // NEW_ASSET (now) is after
        let boundary = chrono::Utc::now().timestamp() - 1800;
        let promoted = db.promote_pending_to_failed(boundary).await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(
            promoted, 1,
            "only OLD_ASSET (seen before boundary) should be promoted"
        );
        assert_eq!(summary.pending, 1, "NEW_ASSET should remain pending");
        assert_eq!(summary.failed, 1, "OLD_ASSET should be failed");
    }

    // ── Gap: promote_pending_to_failed with very old last_seen_at ────

    #[tokio::test]
    async fn promote_pending_to_failed_promotes_stale_last_seen() {
        // Assets with last_seen_at from a previous sync cycle (e.g., epoch)
        // should be promoted when the boundary is recent.
        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = TestAssetRecord::new("STALE_ASSET")
            .checksum("ck_stale")
            .filename("stale.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();

        // Backdate to epoch
        {
            let conn = db.acquire_lock("test_setup").unwrap();
            conn.execute(
                "UPDATE assets SET last_seen_at = 0 WHERE id = 'STALE_ASSET'",
                [],
            )
            .unwrap();
        }

        let now = chrono::Utc::now().timestamp();
        let promoted = db.promote_pending_to_failed(now).await.unwrap();
        assert_eq!(
            promoted, 1,
            "asset with epoch last_seen_at should be promoted"
        );
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
        assert_eq!(page[0].id, "DL_CK");
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
}
