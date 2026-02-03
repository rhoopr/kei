//! Database schema definitions and migrations.

use rusqlite::Connection;

use super::error::StateError;

/// Current schema version. Increment when making schema changes.
pub const SCHEMA_VERSION: i32 = 1;

/// Schema DDL for version 1.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS assets (
    id TEXT NOT NULL,
    version_size TEXT NOT NULL,
    checksum TEXT NOT NULL,
    filename TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    added_at INTEGER,
    size_bytes INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    downloaded_at INTEGER,
    local_path TEXT,
    last_seen_at INTEGER NOT NULL,
    download_attempts INTEGER DEFAULT 0,
    last_error TEXT,
    PRIMARY KEY (id, version_size)
);

CREATE INDEX IF NOT EXISTS idx_assets_status ON assets(status);
CREATE INDEX IF NOT EXISTS idx_assets_local_path ON assets(local_path);
CREATE INDEX IF NOT EXISTS idx_assets_checksum ON assets(checksum);

CREATE TABLE IF NOT EXISTS sync_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    assets_seen INTEGER DEFAULT 0,
    assets_downloaded INTEGER DEFAULT 0,
    assets_failed INTEGER DEFAULT 0,
    interrupted INTEGER DEFAULT 0
);
"#;

/// Get the current schema version from the database.
pub(crate) fn get_schema_version(conn: &Connection) -> Result<i32, StateError> {
    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    Ok(version)
}

/// Set the schema version in the database.
fn set_schema_version(conn: &Connection, version: i32) -> Result<(), StateError> {
    conn.pragma_update(None, "user_version", version)?;
    Ok(())
}

/// Initialize or migrate the database schema.
///
/// This function is idempotent and safe to call on both new and existing databases.
pub(crate) fn migrate(conn: &Connection) -> Result<(), StateError> {
    let current_version = get_schema_version(conn)?;

    if current_version > SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchemaVersion {
            found: current_version,
            expected: SCHEMA_VERSION,
        });
    }

    if current_version == 0 {
        // Fresh database — apply full schema
        conn.execute_batch(SCHEMA_V1)?;
        set_schema_version(conn, SCHEMA_VERSION)?;
        tracing::debug!("Initialized database schema at version {}", SCHEMA_VERSION);
    } else if current_version < SCHEMA_VERSION {
        // Run incremental migrations
        for version in (current_version + 1)..=SCHEMA_VERSION {
            migrate_to_version(conn, version)?;
        }
    }

    Ok(())
}

/// Apply migration for a specific version.
fn migrate_to_version(conn: &Connection, version: i32) -> Result<(), StateError> {
    // Future migrations go here, e.g.:
    // match version {
    //     2 => { conn.execute_batch("ALTER TABLE assets ADD COLUMN new_field TEXT")?; }
    //     _ => {}
    // }
    // For now, version 1 just applies the base schema
    if version != SCHEMA_VERSION {
        tracing::warn!(
            "Unexpected schema version {}, applying base schema",
            version
        );
    }
    conn.execute_batch(SCHEMA_V1)?;
    set_schema_version(conn, version)?;
    tracing::info!("Migrated database to schema version {}", version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fresh_db_migration() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_idempotent_migration() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // Should be no-op
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_unsupported_version() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let result = migrate(&conn);
        assert!(matches!(
            result,
            Err(StateError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn test_tables_created() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Verify assets table exists
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify sync_runs table exists
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sync_runs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_indexes_created() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Verify indexes exist by querying sqlite_master
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_assets_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3); // status, local_path, checksum
    }
}
