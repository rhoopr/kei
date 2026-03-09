//! Error types for the state tracking module.

use std::path::PathBuf;

use thiserror::Error;

/// Errors that can occur during state database operations.
#[derive(Error, Debug)]
pub enum StateError {
    /// Failed to open or create the database file.
    #[error("Failed to open database at {path}: {source}")]
    Open {
        path: PathBuf,
        source: rusqlite::Error,
    },

    /// Failed to run a database migration.
    #[error("Database migration failed: {0}")]
    Migration(#[from] rusqlite::Error),

    /// A query failed.
    #[error("Database query failed: {0}")]
    Query(String),

    /// Failed to spawn a blocking task.
    #[error("Failed to spawn blocking task: {0}")]
    Spawn(#[from] tokio::task::JoinError),

    /// The database schema version is newer than supported.
    #[error("Database schema version {found} is newer than supported version {expected}")]
    UnsupportedSchemaVersion { found: i32, expected: i32 },
}

impl StateError {
    /// Create a Query error from a rusqlite error.
    pub fn query(source: rusqlite::Error) -> Self {
        Self::Query(source.to_string())
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
        let err = StateError::Query("something broke".to_string());
        assert_eq!(err.to_string(), "Database query failed: something broke");
    }

    #[test]
    fn query_helper_creates_correct_variant() {
        let rusqlite_err = make_rusqlite_error();
        let msg = rusqlite_err.to_string();
        let err = StateError::query(rusqlite_err);
        match &err {
            StateError::Query(s) => assert_eq!(s, &msg),
            other => panic!("expected Query variant, got {:?}", other),
        }
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
            "Database schema version 5 is newer than supported version 3"
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
            path: PathBuf::from("/tmp/claude/test.db"),
            source: make_rusqlite_error(),
        };
        let display = err.to_string();
        assert!(
            display.contains("/tmp/claude/test.db"),
            "expected path in display, got: {display}"
        );
        assert!(
            display.starts_with("Failed to open database at"),
            "unexpected prefix: {display}"
        );
    }
}
