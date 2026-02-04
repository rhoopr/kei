//! State database trait and SQLite implementation.

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
    #[allow(dead_code)]
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
    ) -> Result<(), StateError>;

    /// Mark an asset as failed with an error message.
    ///
    /// Note: The download engine uses `mark_failed_batch` for efficiency.
    /// This method is retained for API completeness with `mark_downloaded`.
    #[allow(dead_code)]
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

    /// Get all downloaded assets.
    async fn get_all_downloaded(&self) -> Result<Vec<AssetRecord>, StateError>;

    /// Start a new sync run and return its ID.
    async fn start_sync_run(&self) -> Result<i64, StateError>;

    /// Complete a sync run with statistics.
    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError>;

    /// Reset all failed assets to pending status.
    ///
    /// Returns the number of assets reset.
    async fn reset_failed(&self) -> Result<u64, StateError>;

    // ── Batch operations for performance optimization ──

    /// Get all downloaded asset IDs as (id, version_size) pairs.
    ///
    /// Used at sync start to pre-load downloaded state for O(1) skip decisions.
    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String)>, StateError>;

    /// Get downloaded asset IDs with their checksums.
    ///
    /// Returns a map of (id, version_size) -> checksum for downloaded assets.
    /// Used to detect checksum changes without querying the DB per asset.
    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String), String>, StateError>;

    /// Batch insert or update asset records after seeing them during sync.
    async fn upsert_seen_batch(&self, records: &[AssetRecord]) -> Result<(), StateError>;

    /// Batch mark assets as successfully downloaded.
    ///
    /// Used by the download engine to reduce per-download DB overhead.
    async fn mark_downloaded_batch(
        &self,
        items: &[(String, String, PathBuf)],
    ) -> Result<(), StateError>;

    /// Batch mark assets as failed with error messages.
    ///
    /// Used by the download engine to reduce per-download DB overhead.
    async fn mark_failed_batch(&self, items: &[(String, String, String)])
        -> Result<(), StateError>;
}

/// SQLite implementation of the state database.
pub struct SqliteStateDb {
    /// Wrapped in Mutex because rusqlite::Connection is not Sync.
    /// All operations use spawn_blocking to avoid blocking the async runtime.
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
}

#[async_trait]
impl StateDb for SqliteStateDb {
    async fn should_download(
        &self,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &Path,
    ) -> Result<bool, StateError> {
        // Query DB in a separate scope to ensure MutexGuard is dropped before any await
        let result: Option<(String, String, Option<String>)> = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| StateError::Query(e.to_string()))?;

            conn.query_row(
                "SELECT status, checksum, local_path FROM assets WHERE id = ?1 AND version_size = ?2",
                [id, version_size],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(StateError::query)?
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

        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        // Use INSERT OR REPLACE to handle both insert and update
        // But preserve existing status, downloaded_at, local_path, download_attempts, last_error
        conn.execute(
            r#"
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
            "#,
            rusqlite::params![
                &record.id,
                record.version_size.as_str(),
                &record.checksum,
                &record.filename,
                record.created_at.timestamp(),
                record.added_at.map(|dt| dt.timestamp()),
                record.size_bytes as i64,
                record.media_type.as_str(),
                last_seen_at,
            ],
        )
        .map_err(StateError::query)?;

        Ok(())
    }

    async fn mark_downloaded(
        &self,
        id: &str,
        version_size: &str,
        local_path: &Path,
    ) -> Result<(), StateError> {
        let downloaded_at = Utc::now().timestamp();

        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        conn.execute(
            "UPDATE assets SET status = 'downloaded', downloaded_at = ?1, local_path = ?2, last_error = NULL WHERE id = ?3 AND version_size = ?4",
            rusqlite::params![downloaded_at, local_path.to_string_lossy(), id, version_size],
        )
        .map_err(StateError::query)?;

        Ok(())
    }

    async fn mark_failed(
        &self,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), StateError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        conn.execute(
            "UPDATE assets SET status = 'failed', download_attempts = download_attempts + 1, last_error = ?1 WHERE id = ?2 AND version_size = ?3",
            rusqlite::params![error, id, version_size],
        )
        .map_err(StateError::query)?;

        Ok(())
    }

    async fn get_failed(&self) -> Result<Vec<AssetRecord>, StateError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, version_size, checksum, filename, created_at, added_at, size_bytes, media_type, status, downloaded_at, local_path, last_seen_at, download_attempts, last_error FROM assets WHERE status = 'failed'",
            )
            .map_err(StateError::query)?;

        let records = stmt
            .query_map([], |row| Ok(row_to_asset_record(row)))
            .map_err(StateError::query)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StateError::query)?;

        Ok(records)
    }

    async fn get_summary(&self) -> Result<SyncSummary, StateError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let total_assets: u64 = conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(StateError::query)? as u64;

        let downloaded: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE status = 'downloaded'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(StateError::query)? as u64;

        let pending: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE status = 'pending'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(StateError::query)? as u64;

        let failed: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE status = 'failed'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(StateError::query)? as u64;

        let last_sync: Option<(Option<i64>, Option<i64>)> = conn
            .query_row(
                "SELECT started_at, completed_at FROM sync_runs ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StateError::query)?;

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

    async fn get_all_downloaded(&self) -> Result<Vec<AssetRecord>, StateError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, version_size, checksum, filename, created_at, added_at, size_bytes, media_type, status, downloaded_at, local_path, last_seen_at, download_attempts, last_error FROM assets WHERE status = 'downloaded'",
            )
            .map_err(StateError::query)?;

        let records = stmt
            .query_map([], |row| Ok(row_to_asset_record(row)))
            .map_err(StateError::query)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StateError::query)?;

        Ok(records)
    }

    async fn start_sync_run(&self) -> Result<i64, StateError> {
        let started_at = Utc::now().timestamp();

        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        conn.execute(
            "INSERT INTO sync_runs (started_at) VALUES (?1)",
            [started_at],
        )
        .map_err(StateError::query)?;

        let id = conn.last_insert_rowid();
        Ok(id)
    }

    async fn complete_sync_run(&self, run_id: i64, stats: &SyncRunStats) -> Result<(), StateError> {
        let completed_at = Utc::now().timestamp();
        let assets_seen = stats.assets_seen as i64;
        let assets_downloaded = stats.assets_downloaded as i64;
        let assets_failed = stats.assets_failed as i64;
        let interrupted = if stats.interrupted { 1 } else { 0 };

        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        conn.execute(
            "UPDATE sync_runs SET completed_at = ?1, assets_seen = ?2, assets_downloaded = ?3, assets_failed = ?4, interrupted = ?5 WHERE id = ?6",
            rusqlite::params![completed_at, assets_seen, assets_downloaded, assets_failed, interrupted, run_id],
        )
        .map_err(StateError::query)?;

        Ok(())
    }

    async fn reset_failed(&self) -> Result<u64, StateError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let rows = conn
            .execute(
                "UPDATE assets SET status = 'pending', download_attempts = 0, last_error = NULL WHERE status = 'failed'",
                [],
            )
            .map_err(StateError::query)?;

        Ok(rows as u64)
    }

    async fn get_downloaded_ids(&self) -> Result<HashSet<(String, String)>, StateError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let mut stmt = conn
            .prepare_cached("SELECT id, version_size FROM assets WHERE status = 'downloaded'")
            .map_err(StateError::query)?;

        let ids = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(StateError::query)?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(StateError::query)?;

        Ok(ids)
    }

    async fn get_downloaded_checksums(
        &self,
    ) -> Result<HashMap<(String, String), String>, StateError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let mut stmt = conn
            .prepare_cached(
                "SELECT id, version_size, checksum FROM assets WHERE status = 'downloaded'",
            )
            .map_err(StateError::query)?;

        let checksums = stmt
            .query_map([], |row| {
                Ok((
                    (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(StateError::query)?
            .collect::<Result<HashMap<_, _>, _>>()
            .map_err(StateError::query)?;

        Ok(checksums)
    }

    async fn upsert_seen_batch(&self, records: &[AssetRecord]) -> Result<(), StateError> {
        if records.is_empty() {
            return Ok(());
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let last_seen_at = Utc::now().timestamp();

        // Use a transaction for atomicity and better performance
        conn.execute("BEGIN TRANSACTION", [])
            .map_err(StateError::query)?;

        let result = (|| {
            let mut stmt = conn
                .prepare_cached(
                    r#"
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
                    "#,
                )
                .map_err(StateError::query)?;

            for record in records {
                stmt.execute(rusqlite::params![
                    record.id,
                    record.version_size.as_str(),
                    record.checksum,
                    record.filename,
                    record.created_at.timestamp(),
                    record.added_at.map(|dt| dt.timestamp()),
                    record.size_bytes as i64,
                    record.media_type.as_str(),
                    last_seen_at,
                ])
                .map_err(StateError::query)?;
            }

            Ok::<_, StateError>(())
        })();

        match result {
            Ok(()) => {
                conn.execute("COMMIT", []).map_err(StateError::query)?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    async fn mark_downloaded_batch(
        &self,
        items: &[(String, String, PathBuf)],
    ) -> Result<(), StateError> {
        if items.is_empty() {
            return Ok(());
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        let downloaded_at = Utc::now().timestamp();

        conn.execute("BEGIN TRANSACTION", [])
            .map_err(StateError::query)?;

        let result = (|| {
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE assets SET status = 'downloaded', downloaded_at = ?1, local_path = ?2, last_error = NULL WHERE id = ?3 AND version_size = ?4",
                )
                .map_err(StateError::query)?;

            for (id, version_size, local_path) in items {
                stmt.execute(rusqlite::params![
                    downloaded_at,
                    local_path.to_string_lossy(),
                    id,
                    version_size,
                ])
                .map_err(StateError::query)?;
            }

            Ok::<_, StateError>(())
        })();

        match result {
            Ok(()) => {
                conn.execute("COMMIT", []).map_err(StateError::query)?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    async fn mark_failed_batch(
        &self,
        items: &[(String, String, String)],
    ) -> Result<(), StateError> {
        if items.is_empty() {
            return Ok(());
        }

        let conn = self
            .conn
            .lock()
            .map_err(|e| StateError::Query(e.to_string()))?;

        conn.execute("BEGIN TRANSACTION", [])
            .map_err(StateError::query)?;

        let result = (|| {
            let mut stmt = conn
                .prepare_cached(
                    "UPDATE assets SET status = 'failed', download_attempts = download_attempts + 1, last_error = ?1 WHERE id = ?2 AND version_size = ?3",
                )
                .map_err(StateError::query)?;

            for (id, version_size, error) in items {
                stmt.execute(rusqlite::params![error, id, version_size])
                    .map_err(StateError::query)?;
            }

            Ok::<_, StateError>(())
        })();

        match result {
            Ok(()) => {
                conn.execute("COMMIT", []).map_err(StateError::query)?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }
}

/// Convert a database row to an AssetRecord.
fn row_to_asset_record(row: &rusqlite::Row<'_>) -> AssetRecord {
    let id: String = row.get(0).unwrap_or_default();
    let version_size_str: String = row.get(1).unwrap_or_default();
    let checksum: String = row.get(2).unwrap_or_default();
    let filename: String = row.get(3).unwrap_or_default();
    let created_at_ts: i64 = row.get(4).unwrap_or(0);
    let added_at_ts: Option<i64> = row.get(5).ok();
    let size_bytes: i64 = row.get(6).unwrap_or(0);
    let media_type_str: String = row.get(7).unwrap_or_default();
    let status_str: String = row.get(8).unwrap_or_default();
    let downloaded_at_ts: Option<i64> = row.get(9).ok();
    let local_path_str: Option<String> = row.get(10).ok();
    let last_seen_at_ts: i64 = row.get(11).unwrap_or(0);
    let download_attempts: i64 = row.get(12).unwrap_or(0);
    let last_error: Option<String> = row.get(13).ok();

    AssetRecord {
        id,
        checksum,
        filename,
        local_path: local_path_str.map(PathBuf::from),
        last_error,
        size_bytes: size_bytes as u64,
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
        download_attempts: download_attempts as u32,
        version_size: VersionSizeKey::from_str(&version_size_str)
            .unwrap_or(VersionSizeKey::Original),
        media_type: MediaType::from_str(&media_type_str).unwrap_or(MediaType::Photo),
        status: AssetStatus::from_str(&status_str).unwrap_or(AssetStatus::Pending),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("claude")
            .join("state_db_tests")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_open_creates_db() {
        let dir = test_dir("open_creates");
        let path = dir.join("test.db");
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

        let record = AssetRecord::new_pending(
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "checksum123".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            12345,
            MediaType::Photo,
        );

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
        let dir = test_dir("mark_downloaded");
        let file_path = dir.join("photo.jpg");
        fs::write(&file_path, b"test content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = AssetRecord::new_pending(
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "checksum123".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            12345,
            MediaType::Photo,
        );

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("ABC123", "original", &file_path)
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

        let record = AssetRecord::new_pending(
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "checksum123".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            12345,
            MediaType::Photo,
        );

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("ABC123", "original", Path::new("/nonexistent/file.jpg"))
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
        let dir = test_dir("checksum_changed");
        let file_path = dir.join("photo.jpg");
        fs::write(&file_path, b"test content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = AssetRecord::new_pending(
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "old_checksum".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            12345,
            MediaType::Photo,
        );

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("ABC123", "original", &file_path)
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

        let record = AssetRecord::new_pending(
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "checksum123".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            12345,
            MediaType::Photo,
        );

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

        let record = AssetRecord::new_pending(
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "checksum123".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            12345,
            MediaType::Photo,
        );

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
            let record = AssetRecord::new_pending(
                format!("PENDING_{}", i),
                VersionSizeKey::Original,
                format!("checksum_{}", i),
                format!("photo_{}.jpg", i),
                Utc::now(),
                None,
                1000,
                MediaType::Photo,
            );
            db.upsert_seen(&record).await.unwrap();
        }

        let dir = test_dir("get_summary");
        for i in 0..2 {
            let record = AssetRecord::new_pending(
                format!("DOWNLOADED_{}", i),
                VersionSizeKey::Original,
                format!("dl_checksum_{}", i),
                format!("dl_photo_{}.jpg", i),
                Utc::now(),
                None,
                1000,
                MediaType::Photo,
            );
            db.upsert_seen(&record).await.unwrap();
            let path = dir.join(format!("dl_photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(&format!("DOWNLOADED_{}", i), "original", &path)
                .await
                .unwrap();
        }

        let record = AssetRecord::new_pending(
            "FAILED_1".to_string(),
            VersionSizeKey::Original,
            "fail_checksum".to_string(),
            "fail_photo.jpg".to_string(),
            Utc::now(),
            None,
            1000,
            MediaType::Photo,
        );
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
        let dir = test_dir("upsert_preserves");
        let file_path = dir.join("photo.jpg");
        fs::write(&file_path, b"test content").unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();

        let record = AssetRecord::new_pending(
            "ABC123".to_string(),
            VersionSizeKey::Original,
            "checksum123".to_string(),
            "photo.jpg".to_string(),
            Utc::now(),
            None,
            12345,
            MediaType::Photo,
        );

        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("ABC123", "original", &file_path)
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
    async fn test_get_all_downloaded() {
        let dir = test_dir("get_all_downloaded");
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..3 {
            let record = AssetRecord::new_pending(
                format!("DL_{}", i),
                VersionSizeKey::Original,
                format!("checksum_{}", i),
                format!("photo_{}.jpg", i),
                Utc::now(),
                None,
                1000,
                MediaType::Photo,
            );
            db.upsert_seen(&record).await.unwrap();
            let path = dir.join(format!("photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(&format!("DL_{}", i), "original", &path)
                .await
                .unwrap();
        }

        let downloaded = db.get_all_downloaded().await.unwrap();
        assert_eq!(downloaded.len(), 3);
    }

    // ── Batch operation tests ──

    #[tokio::test]
    async fn test_get_downloaded_ids() {
        let dir = test_dir("get_downloaded_ids");
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Create some assets with different statuses
        for i in 0..3 {
            let record = AssetRecord::new_pending(
                format!("DL_{}", i),
                VersionSizeKey::Original,
                format!("checksum_{}", i),
                format!("photo_{}.jpg", i),
                Utc::now(),
                None,
                1000,
                MediaType::Photo,
            );
            db.upsert_seen(&record).await.unwrap();
            let path = dir.join(format!("photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(&format!("DL_{}", i), "original", &path)
                .await
                .unwrap();
        }

        // Add a pending asset (should not be in downloaded IDs)
        let pending = AssetRecord::new_pending(
            "PENDING_1".to_string(),
            VersionSizeKey::Original,
            "pending_ck".to_string(),
            "pending.jpg".to_string(),
            Utc::now(),
            None,
            1000,
            MediaType::Photo,
        );
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
        let dir = test_dir("get_downloaded_checksums");
        let db = SqliteStateDb::open_in_memory().unwrap();

        for i in 0..2 {
            let record = AssetRecord::new_pending(
                format!("DL_{}", i),
                VersionSizeKey::Original,
                format!("checksum_{}", i),
                format!("photo_{}.jpg", i),
                Utc::now(),
                None,
                1000,
                MediaType::Photo,
            );
            db.upsert_seen(&record).await.unwrap();
            let path = dir.join(format!("photo_{}.jpg", i));
            fs::write(&path, b"content").unwrap();
            db.mark_downloaded(&format!("DL_{}", i), "original", &path)
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
    async fn test_upsert_seen_batch() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        let records: Vec<AssetRecord> = (0..5)
            .map(|i| {
                AssetRecord::new_pending(
                    format!("BATCH_{}", i),
                    VersionSizeKey::Original,
                    format!("checksum_{}", i),
                    format!("photo_{}.jpg", i),
                    Utc::now(),
                    None,
                    1000 + i as u64,
                    MediaType::Photo,
                )
            })
            .collect();

        db.upsert_seen_batch(&records).await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 5);
        assert_eq!(summary.pending, 5);
    }

    #[tokio::test]
    async fn test_upsert_seen_batch_empty() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_seen_batch(&[]).await.unwrap();
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.total_assets, 0);
    }

    #[tokio::test]
    async fn test_mark_downloaded_batch() {
        let dir = test_dir("mark_downloaded_batch");
        let db = SqliteStateDb::open_in_memory().unwrap();

        // First insert assets
        let records: Vec<AssetRecord> = (0..3)
            .map(|i| {
                AssetRecord::new_pending(
                    format!("BATCH_{}", i),
                    VersionSizeKey::Original,
                    format!("checksum_{}", i),
                    format!("photo_{}.jpg", i),
                    Utc::now(),
                    None,
                    1000,
                    MediaType::Photo,
                )
            })
            .collect();
        db.upsert_seen_batch(&records).await.unwrap();

        // Mark them as downloaded in batch
        let items: Vec<(String, String, PathBuf)> = (0..3)
            .map(|i| {
                let path = dir.join(format!("photo_{}.jpg", i));
                fs::write(&path, b"content").unwrap();
                (format!("BATCH_{}", i), "original".to_string(), path)
            })
            .collect();

        db.mark_downloaded_batch(&items).await.unwrap();

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 3);
        assert_eq!(summary.pending, 0);
    }

    #[tokio::test]
    async fn test_mark_downloaded_batch_empty() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.mark_downloaded_batch(&[]).await.unwrap();
    }

    #[tokio::test]
    async fn test_mark_failed_batch() {
        let db = SqliteStateDb::open_in_memory().unwrap();

        // First insert assets
        let records: Vec<AssetRecord> = (0..3)
            .map(|i| {
                AssetRecord::new_pending(
                    format!("FAIL_{}", i),
                    VersionSizeKey::Original,
                    format!("checksum_{}", i),
                    format!("photo_{}.jpg", i),
                    Utc::now(),
                    None,
                    1000,
                    MediaType::Photo,
                )
            })
            .collect();
        db.upsert_seen_batch(&records).await.unwrap();

        // Mark them as failed in batch
        let items: Vec<(String, String, String)> = (0..3)
            .map(|i| {
                (
                    format!("FAIL_{}", i),
                    "original".to_string(),
                    format!("Error {}", i),
                )
            })
            .collect();

        db.mark_failed_batch(&items).await.unwrap();

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 3);

        // Check each has the correct error
        for record in &failed {
            let idx: usize = record.id.strip_prefix("FAIL_").unwrap().parse().unwrap();
            assert_eq!(record.last_error, Some(format!("Error {}", idx)));
            assert_eq!(record.download_attempts, 1);
        }
    }

    #[tokio::test]
    async fn test_mark_failed_batch_empty() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.mark_failed_batch(&[]).await.unwrap();
    }
}
