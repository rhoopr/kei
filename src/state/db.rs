//! SQLite state facade and connection lifecycle.
//!
//! Store roles and shared records are re-exported from `contracts`. Child modules
//! own the SQL for assets, checkpoints, membership, provider identity, metadata,
//! temporary files, imports, reconciliation, and reports. `asset_writes` owns
//! shared transactional writes; `rows` owns shared projections and decoding.
//! Connections and blocking-pool dispatch remain here. Operations retain their
//! existing transaction boundaries and external paths.
//!
//! Tests formerly under `state::db::tests` now use each owner's `tests` module,
//! or `connection_tests` for connection lifecycle. Test names are unchanged.
//! `cargo test state::` includes every relocated test.

mod asset_writes;
mod assets;
mod checkpoints;
mod contracts;
mod identity;
mod import;
mod membership;
mod metadata;
mod reconciliation;
mod reports;
mod rows;
mod temp_files;

pub use contracts::{
    AlbumMembershipRecord, CaptureRepairReceipt, DownloadStateStore, ImportStateStore,
    ImportedRecord, MembershipStore, MetadataRewriteCompletion, MetadataRewriteQueue,
    MetadataRewriteStore, PendingMetadataRewrite, ReportStateStore, SyncTokenStore,
};
pub(crate) use contracts::{
    AssetGroupingRows, AssetVerificationState, CheckpointTransition, DownloadContextStateStore,
    DownloadedFileRecord, ManifestAssetRow, OwnedTempFile, ReconciliationCatalogPath,
    ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    ReconciliationStateStore, RetryErrorRetention, ScopedDbSyncToken, TempFileOwnershipStore,
};

#[cfg(test)]
mod connection_tests;
#[cfg(test)]
mod test_support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::state::error::StateError;
use crate::state::schema;

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
