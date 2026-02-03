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
