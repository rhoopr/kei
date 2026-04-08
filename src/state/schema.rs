//! Database schema definitions and migrations.

use rusqlite::Connection;

use super::error::StateError;

/// Current schema version. Increment when making schema changes.
pub const SCHEMA_VERSION: i32 = 4;

/// Schema DDL for version 1.
const SCHEMA_V1: &str = r"
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
";

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
/// Each migration step is wrapped in a SAVEPOINT so that a failure rolls back
/// only the current step, leaving the database at the last successfully applied version.
pub(crate) fn migrate(conn: &Connection) -> Result<(), StateError> {
    let current_version = get_schema_version(conn)?;

    if current_version > SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchemaVersion {
            found: current_version,
            expected: SCHEMA_VERSION,
        });
    }

    for version in (current_version + 1)..=SCHEMA_VERSION {
        conn.execute_batch("SAVEPOINT migration")?;
        match migrate_to_version(conn, version) {
            Ok(()) => conn.execute_batch("RELEASE migration")?,
            Err(e) => {
                if let Err(rollback_err) = conn.execute_batch("ROLLBACK TO migration") {
                    tracing::error!(
                        version,
                        migration_error = %e,
                        rollback_error = %rollback_err,
                        "Migration rollback failed — database may be inconsistent"
                    );
                }
                return Err(e);
            }
        }
    }

    Ok(())
}

/// Schema DDL for version 2 migration: add key-value metadata table.
const SCHEMA_V2: &str = r"
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
";

/// Schema DDL for version 3 migration: add locally-computed checksum column.
const SCHEMA_V3: &str = "ALTER TABLE assets ADD COLUMN local_checksum TEXT;";

/// Schema DDL for version 4 migration: add pre-EXIF download checksum column.
const SCHEMA_V4: &str = "ALTER TABLE assets ADD COLUMN download_checksum TEXT;";

/// Check whether a column exists on a table using `PRAGMA table_info`.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, StateError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| StateError::query(&e))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| StateError::query(&e))?
        .any(|name| name.is_ok_and(|n| n == column));
    Ok(exists)
}

/// Apply migration for a specific version.
fn migrate_to_version(conn: &Connection, version: i32) -> Result<(), StateError> {
    match version {
        1 => conn.execute_batch(SCHEMA_V1)?,
        2 => conn.execute_batch(SCHEMA_V2)?,
        3 => {
            // Idempotent: skip ALTER if column already exists (e.g. crash recovery)
            if !column_exists(conn, "assets", "local_checksum")? {
                conn.execute_batch(SCHEMA_V3)?;
            }
        }
        4 => {
            if !column_exists(conn, "assets", "download_checksum")? {
                conn.execute_batch(SCHEMA_V4)?;
            }
        }
        other => {
            return Err(StateError::Query(format!(
                "No migration defined for version {other}"
            )));
        }
    };
    set_schema_version(conn, version)?;
    tracing::info!(version, "Migrated database schema");
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

    #[test]
    fn test_metadata_table_created() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // Verify metadata table exists
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metadata", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_v1_to_v2_migration() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a v1 database
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), 1);

        // Migrate should bring it to current version
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Metadata table should exist
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metadata", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_v2_to_current_migration() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a v2 database
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        set_schema_version(&conn, 2).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), 2);

        // Migrate should bring it to current version
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Verify local_checksum column exists
        let has_column: bool = conn
            .prepare("SELECT local_checksum FROM assets LIMIT 0")
            .is_ok();
        assert!(
            has_column,
            "local_checksum column should exist after migration"
        );
    }

    #[test]
    fn test_v3_migration_idempotent_when_column_exists() {
        let conn = Connection::open_in_memory().unwrap();
        // Set up a v2 database
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        set_schema_version(&conn, 2).unwrap();

        // Manually add the local_checksum column (simulates crash recovery)
        conn.execute_batch("ALTER TABLE assets ADD COLUMN local_checksum TEXT")
            .unwrap();

        // Migration should succeed despite column already existing
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Database should still be usable
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_v1_to_current_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();

        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // All migration artifacts should be present
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metadata", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        let has_column: bool = conn
            .prepare("SELECT local_checksum FROM assets LIMIT 0")
            .is_ok();
        assert!(has_column);
    }

    #[test]
    fn test_recovery_after_crash_during_migration() {
        let conn = Connection::open_in_memory().unwrap();
        // Set up a v2 database with the v3 column pre-existing
        // (simulates crash after ALTER but before version update)
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        set_schema_version(&conn, 2).unwrap();
        conn.execute_batch("ALTER TABLE assets ADD COLUMN local_checksum TEXT")
            .unwrap();

        // Migration succeeds (idempotent) and advances version
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Database fully functional
        let has_column: bool = conn
            .prepare("SELECT local_checksum FROM assets LIMIT 0")
            .is_ok();
        assert!(has_column);
    }

    /// T-9: Simulate crash after V3+V4 columns added but version left at V2.
    /// Re-running migration must not fail with "duplicate column name".
    #[test]
    fn test_recovery_after_crash_during_v4_migration() {
        let conn = Connection::open_in_memory().unwrap();
        // Set up V1+V2 schema, then manually add both V3 and V4 columns
        // without bumping the version — simulates crash after ALTER but
        // before the version was persisted.
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        set_schema_version(&conn, 2).unwrap();
        conn.execute_batch("ALTER TABLE assets ADD COLUMN local_checksum TEXT")
            .unwrap();
        conn.execute_batch("ALTER TABLE assets ADD COLUMN download_checksum TEXT")
            .unwrap();

        // Migration should succeed (idempotent column checks)
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Both columns should exist and be queryable
        assert!(conn
            .prepare("SELECT local_checksum, download_checksum FROM assets LIMIT 0")
            .is_ok());

        // Database should be fully usable (insert + query round-trip)
        conn.execute(
            "INSERT INTO assets (id, version_size, checksum, filename, created_at, size_bytes, media_type, last_seen_at) \
             VALUES ('test', 'original', 'ck', 'photo.jpg', 0, 100, 'photo', 0)",
            [],
        ).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
