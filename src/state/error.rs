//! Error types for the state tracking module.

use std::path::PathBuf;

use thiserror::Error;

/// Errors that can occur during state database operations.
#[derive(Error, Debug)]
pub enum StateError {
    #[error(
        "Provider work projection is full; source observations and unfinished obligations are retained."
    )]
    ProviderWorkFull,
    #[error(
        "Provider work provenance or admission evidence is inconsistent; unfinished obligations are retained."
    )]
    ProviderWorkInvalid,
    #[error(
        "Provider generation conflicts with unfinished projected work; retaining the original obligation."
    )]
    ProviderWorkConflict,
    #[error(
        "Provider catalog projection is full; retaining captured observations and the current checkpoint. No automatic pruning is permitted."
    )]
    ProviderCatalogFull,
    #[error(
        "Provider catalog source provenance or projection is inconsistent; retaining observations and the current checkpoint."
    )]
    ProviderCatalogInvalid,
    #[error(
        "Provider shadow inbox is full; retaining observations and the current checkpoint. No automatic pruning is permitted."
    )]
    ProviderInboxFull,
    #[error(
        "Provider shadow inbox identity or payload is inconsistent; retaining the current checkpoint."
    )]
    ProviderInboxInvalid,
    #[error(
        "State database has no verified account owner. Use `kei migrate-state --legacy-db <PATH> --confirm-ownership` after stopping other kei processes; legacy files are preserved."
    )]
    AccountOwnerMissing,
    #[error(
        "State database account ownership does not match this configured or authenticated account; refusing state access."
    )]
    AccountOwnerMismatch,
    #[error(
        "Fresh authentication did not provide a usable account identity; state cannot be bound safely."
    )]
    AccountIdentityUnavailable,
    #[error(
        "Legacy state requires explicit verified ownership migration. Use `kei migrate-state --legacy-db <PATH> --confirm-ownership`; do not infer ownership from the filename."
    )]
    LegacyAccountMigrationRequired,
    #[error(
        "Legacy state changed or was busy during adoption. Stop other kei processes and retry; source files remain intact."
    )]
    AccountMigrationBusy,
    #[error(
        "The staged account migration failed SQLite integrity validation; source files remain intact."
    )]
    AccountMigrationInvalid,
    #[error(
        "Could not read the explicit legacy database as valid SQLite state; source files remain intact."
    )]
    AccountMigrationSourceUnreadable,

    /// Failed to create the parent directory for the database file.
    ///
    /// `SqliteStateDb::open` creates the parent directory before opening
    /// so SQLite doesn't fail with a generic "unable to open database
    /// file" on a fresh install (issue #264). This variant surfaces the
    /// underlying mkdir failure (e.g. permission denied) distinctly from
    /// a SQLite-level open error.
    #[error("Could not create the state database directory {path}: {source}")]
    ParentDir {
        path: PathBuf,
        source: std::io::Error,
    },

    /// Failed to resolve a temporary download path into a stable absolute
    /// ownership key.
    #[error("Could not resolve temporary download path {path}: {source}")]
    TempPath {
        path: PathBuf,
        source: std::io::Error,
    },

    /// Failed to open or create the database file.
    #[error("Could not open the state database at {path}: {source}")]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },

    /// Failed to run a database migration.
    #[error("Could not update the state database schema: {0}")]
    Migration(#[from] rusqlite::Error),

    /// A query failed.
    #[error("State database operation failed while {operation}: {source}")]
    Query {
        operation: &'static str,
        #[source]
        source: rusqlite::Error,
    },

    /// Failed to acquire the database lock (mutex poisoned).
    #[error("Could not lock the state database ({0})")]
    LockPoisoned(String),

    /// Failed to spawn a blocking task.
    #[error("Could not start a background database task: {0}")]
    Spawn(#[from] tokio::task::JoinError),

    /// The database schema version is newer than supported.
    #[error(
        "This state database is from a newer kei version (schema {found}); this kei supports schema {expected}"
    )]
    UnsupportedSchemaVersion { found: i32, expected: i32 },

    /// The database schema must be migrated before a read-only command can
    /// inspect it.
    #[error(
        "This state database uses schema {found}; schema {expected} is required for this read-only command. Run a normal kei command that updates state first."
    )]
    ReadOnlySchemaTooOld { found: i32, expected: i32 },

    /// A producer-dispatch invariant was violated — typically a write
    /// path was reached without the corresponding `upsert_seen` having
    /// run first. The asset row didn't exist, so the operation became a
    /// no-op. Surface it loudly rather than silently swallow.
    #[error("State database consistency check failed while {operation}: {detail}")]
    Invariant {
        operation: &'static str,
        detail: String,
    },

    /// `mark_downloaded` matched zero rows. The asset row should have
    /// been upserted before this call; its absence indicates a missed
    /// upsert step or out-of-band row deletion.
    #[error(
        "Could not mark asset {asset_id} ({version_size}) downloaded because it was not in the state database"
    )]
    AssetRowMissing {
        asset_id: String,
        version_size: String,
    },
}

impl StateError {
    /// Create a Query error from a rusqlite error.
    pub fn query(operation: &'static str, source: rusqlite::Error) -> Self {
        Self::Query { operation, source }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rusqlite_error() -> rusqlite::Error {
        // Open an in-memory DB and provoke a real error via invalid SQL.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("INVALID SQL", []).unwrap_err()
    }

    #[test]
    fn query_display_format() {
        let err = StateError::Query {
            operation: "test_op",
            source: make_rusqlite_error(),
        };
        let display = err.to_string();
        assert!(
            display.starts_with("State database operation failed while test_op: "),
            "unexpected display: {display}"
        );
    }

    #[test]
    fn query_helper_creates_correct_variant() {
        let rusqlite_err = make_rusqlite_error();
        let err = StateError::query("some_operation", rusqlite_err);
        match &err {
            StateError::Query {
                operation,
                source: _,
            } => {
                assert_eq!(*operation, "some_operation");
            }
            other => panic!("expected Query variant, got {:?}", other),
        }
    }

    #[test]
    fn lock_poisoned_display_format() {
        let err = StateError::LockPoisoned("get_metadata: poisoned".to_string());
        assert_eq!(
            err.to_string(),
            "Could not lock the state database (get_metadata: poisoned)"
        );
    }

    #[test]
    fn unsupported_schema_version_display_includes_both_versions() {
        let err = StateError::UnsupportedSchemaVersion {
            found: 5,
            expected: 3,
        };
        let display = err.to_string();
        assert!(
            display.contains("5") && display.contains("3"),
            "expected both version numbers in display, got: {display}"
        );
        assert_eq!(
            display,
            "This state database is from a newer kei version (schema 5); this kei supports schema 3"
        );
    }

    #[test]
    fn migration_from_rusqlite_error() {
        let rusqlite_err = make_rusqlite_error();
        let expected_msg = rusqlite_err.to_string();
        let err: StateError = rusqlite_err.into();
        match &err {
            StateError::Migration(_) => {}
            other => panic!("expected Migration variant, got {:?}", other),
        }
        assert!(
            err.to_string().contains(&expected_msg),
            "display should contain rusqlite message, got: {}",
            err
        );
    }

    #[test]
    fn open_error_display_includes_path() {
        let err = StateError::Open {
            path: PathBuf::from("/tmp/codex/kei/test.db"),
            source: make_rusqlite_error(),
        };
        let display = err.to_string();
        assert!(
            display.contains("/tmp/codex/kei/test.db"),
            "expected path in display, got: {display}"
        );
        assert!(
            display.starts_with("Could not open the state database at"),
            "unexpected prefix: {display}"
        );
    }

    #[test]
    fn parent_dir_error_display_includes_path() {
        let err = StateError::ParentDir {
            path: PathBuf::from("/tmp/codex/kei/missing/dir"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let display = err.to_string();
        assert!(
            display.contains("/tmp/codex/kei/missing/dir"),
            "expected path in display, got: {display}"
        );
        assert!(
            display.starts_with("Could not create the state database directory"),
            "unexpected prefix: {display}"
        );
        assert!(
            display.contains("denied"),
            "expected source message in display, got: {display}"
        );
    }
}
