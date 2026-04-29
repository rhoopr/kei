//! Database schema definitions and migrations.

use rusqlite::Connection;

use super::error::StateError;

/// Current schema version. Increment when making schema changes.
pub(crate) const SCHEMA_VERSION: i32 = 8;

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
        match migrate_to_version(conn, current_version, version) {
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

/// V5 metadata columns added to the `assets` table.
///
/// `source` records which provider ingested the asset. `DEFAULT 'icloud'` is
/// correct for migration because every pre-v5 row came from iCloud sync; new
/// inserts always set `source` explicitly.
const V5_ASSET_COLUMNS: &[(&str, &str)] = &[
    ("source", "TEXT NOT NULL DEFAULT 'icloud'"),
    ("is_favorite", "INTEGER NOT NULL DEFAULT 0"),
    ("rating", "INTEGER"),
    ("latitude", "REAL"),
    ("longitude", "REAL"),
    ("altitude", "REAL"),
    ("orientation", "INTEGER"),
    ("duration_secs", "REAL"),
    ("timezone_offset", "INTEGER"),
    ("width", "INTEGER"),
    ("height", "INTEGER"),
    ("title", "TEXT"),
    ("keywords", "TEXT"),
    ("description", "TEXT"),
    ("media_subtype", "TEXT"),
    ("burst_id", "TEXT"),
    ("is_hidden", "INTEGER NOT NULL DEFAULT 0"),
    ("is_archived", "INTEGER NOT NULL DEFAULT 0"),
    ("modified_at", "INTEGER"),
    ("is_deleted", "INTEGER NOT NULL DEFAULT 0"),
    ("deleted_at", "INTEGER"),
    ("provider_data", "TEXT"),
    ("metadata_hash", "TEXT"),
];

/// V5 table/index DDL executed after the ALTER TABLE pass.
const SCHEMA_V5_TABLES: &str = r"
CREATE TABLE IF NOT EXISTS asset_albums (
    asset_id   TEXT NOT NULL,
    album_name TEXT NOT NULL,
    source     TEXT NOT NULL,
    PRIMARY KEY (asset_id, album_name, source)
);

CREATE TABLE IF NOT EXISTS asset_people (
    asset_id    TEXT NOT NULL,
    person_name TEXT NOT NULL,
    PRIMARY KEY (asset_id, person_name)
);

CREATE INDEX IF NOT EXISTS idx_assets_metadata_hash
    ON assets (metadata_hash) WHERE status = 'downloaded';
";

/// V8 recreate-table migration: change PK from (id, version_size) to
/// (library, id, version_size). SQLite cannot ALTER PRIMARY KEY in place,
/// so we copy into a fresh table, drop, and rename.
///
/// Columns carried forward from the pre-v8 `assets` table into `assets_v8`.
/// Single source of truth for the INSERT and SELECT lists in [`schema_v8`]
/// so a column-order swap can't sneak through type-compatible cells (e.g.
/// `local_path` <-> `local_checksum`, both TEXT). The `library` column is
/// new in v8 and gets its constant `'PrimarySync'` from the migration body
/// itself; it's not part of this list.
const PRESERVED_COLUMNS_V8: &[&str] = &[
    "id",
    "version_size",
    "checksum",
    "filename",
    "created_at",
    "added_at",
    "size_bytes",
    "media_type",
    "status",
    "downloaded_at",
    "local_path",
    "last_seen_at",
    "download_attempts",
    "last_error",
    "local_checksum",
    "download_checksum",
    "source",
    "is_favorite",
    "rating",
    "latitude",
    "longitude",
    "altitude",
    "orientation",
    "duration_secs",
    "timezone_offset",
    "width",
    "height",
    "title",
    "keywords",
    "description",
    "media_subtype",
    "burst_id",
    "is_hidden",
    "is_archived",
    "modified_at",
    "is_deleted",
    "deleted_at",
    "provider_data",
    "metadata_hash",
    "metadata_write_failed_at",
];

/// Full v8 migration script. `assets_v8` is left over only on a mid-migration
/// crash; the SAVEPOINT wrapper in `migrate()` rolls it back. The leading
/// `DROP TABLE IF EXISTS` is belt-and-braces in case a prior migration
/// attempt landed outside the savepoint somehow.
fn schema_v8() -> String {
    let preserved = PRESERVED_COLUMNS_V8.join(", ");
    format!(
        r"
DROP TABLE IF EXISTS assets_v8;

CREATE TABLE assets_v8 (
    library TEXT NOT NULL,
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
    local_checksum TEXT,
    download_checksum TEXT,
    source TEXT NOT NULL DEFAULT 'icloud',
    is_favorite INTEGER NOT NULL DEFAULT 0,
    rating INTEGER,
    latitude REAL,
    longitude REAL,
    altitude REAL,
    orientation INTEGER,
    duration_secs REAL,
    timezone_offset INTEGER,
    width INTEGER,
    height INTEGER,
    title TEXT,
    keywords TEXT,
    description TEXT,
    media_subtype TEXT,
    burst_id TEXT,
    is_hidden INTEGER NOT NULL DEFAULT 0,
    is_archived INTEGER NOT NULL DEFAULT 0,
    modified_at INTEGER,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    deleted_at INTEGER,
    provider_data TEXT,
    metadata_hash TEXT,
    metadata_write_failed_at INTEGER,
    PRIMARY KEY (library, id, version_size)
);

INSERT INTO assets_v8 (library, {preserved})
SELECT 'PrimarySync', {preserved} FROM assets;

DROP TABLE assets;
ALTER TABLE assets_v8 RENAME TO assets;

CREATE INDEX IF NOT EXISTS idx_assets_status ON assets(status);
CREATE INDEX IF NOT EXISTS idx_assets_local_path ON assets(local_path);
CREATE INDEX IF NOT EXISTS idx_assets_checksum ON assets(checksum);
CREATE INDEX IF NOT EXISTS idx_assets_metadata_hash ON assets (metadata_hash) WHERE status = 'downloaded';
"
    )
}

/// Check whether a column exists on a table using `PRAGMA table_info`.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, StateError> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| StateError::query("column_exists", e))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| StateError::query("column_exists", e))?
        .any(|name| name.is_ok_and(|n| n == column));
    Ok(exists)
}

/// Apply migration for a specific version.
///
/// `start_version` is the schema version the DB carried when `migrate()`
/// was entered (before any steps ran); some migrations only want to
/// execute their one-shot side effects on the initial crossing, not on
/// subsequent re-entries through unusual paths.
fn migrate_to_version(
    conn: &Connection,
    start_version: i32,
    version: i32,
) -> Result<(), StateError> {
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
        5 => {
            for (col, decl) in V5_ASSET_COLUMNS {
                if !column_exists(conn, "assets", col)? {
                    conn.execute_batch(&format!("ALTER TABLE assets ADD COLUMN {col} {decl};"))?;
                }
            }
            conn.execute_batch(SCHEMA_V5_TABLES)?;
            // Invalidate sync tokens only on the first crossing from <5 to 5
            // so the backfill pass populates metadata for every asset without
            // re-downloading files. If this arm ever re-runs (e.g., someone
            // PRAGMA user_version=0's the DB), skip the DELETE so we don't
            // force another full re-enumeration on a v5 DB that already has
            // metadata populated.
            if start_version < 5 {
                conn.execute("DELETE FROM metadata WHERE key LIKE 'sync_token:%'", [])?;
            }
        }
        6 => {
            // metadata_write_failed_at: epoch timestamp of the most recent
            // metadata write (EXIF/XMP embed or sidecar) that failed after
            // the media bytes landed. NULL means no pending retry. The
            // metadata-only rewrite path consumes this to re-drive the
            // writer on subsequent syncs, since checksum-based skip logic
            // otherwise hides the asset forever.
            if !column_exists(conn, "assets", "metadata_write_failed_at")? {
                conn.execute_batch(
                    "ALTER TABLE assets ADD COLUMN metadata_write_failed_at INTEGER;",
                )?;
            }
        }
        7 => {
            // sync_runs.status lifecycle: explicit string column so a
            // SIGKILL'd process leaves a detectable "running" row that the
            // next startup can promote to "interrupted". Backfill existing
            // rows from the (completed_at, interrupted) pair.
            if !column_exists(conn, "sync_runs", "status")? {
                conn.execute_batch(
                    "ALTER TABLE sync_runs ADD COLUMN status TEXT NOT NULL DEFAULT 'running';",
                )?;
                conn.execute(
                    "UPDATE sync_runs SET status = CASE \
                        WHEN completed_at IS NULL THEN 'interrupted' \
                        WHEN interrupted = 1      THEN 'interrupted' \
                        ELSE 'complete' \
                     END",
                    [],
                )?;
            }
        }
        8 => {
            // Per-zone scope on the assets PK. Pre-v8 PK was
            // (id, version_size); post-v8 PK is (library, id, version_size)
            // so the same asset ID across multiple SharedSync zones can no
            // longer collide in the state DB. Pre-v8 kei only ever wrote
            // PrimarySync data (no library column existed and no call path
            // took a zone parameter), so backfilling every surviving row
            // with library='PrimarySync' is exact, not approximate.
            if !column_exists(conn, "assets", "library")? {
                conn.execute_batch(&schema_v8())?;
            }
        }
        other => {
            return Err(StateError::UnsupportedSchemaVersion {
                found: other,
                expected: SCHEMA_VERSION,
            });
        }
    }
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
        assert_eq!(count, 4); // status, local_path, checksum, metadata_hash
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

    // ── Gap: v3 to v4 migration specifically ───────────────────────

    #[test]
    fn test_v3_to_v4_migration() {
        let conn = Connection::open_in_memory().unwrap();
        // Set up a v3 database
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        set_schema_version(&conn, 3).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), 3);

        // Verify local_checksum exists but download_checksum does not
        assert!(conn
            .prepare("SELECT local_checksum FROM assets LIMIT 0")
            .is_ok());
        assert!(conn
            .prepare("SELECT download_checksum FROM assets LIMIT 0")
            .is_err());

        // Migrate should bring it to v4
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // download_checksum should now exist
        assert!(conn
            .prepare("SELECT download_checksum FROM assets LIMIT 0")
            .is_ok());

        // Verify data survives migration: insert a row using all columns
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, \
             size_bytes, media_type, last_seen_at, local_checksum, download_checksum) \
             VALUES ('PrimarySync', 'test', 'original', 'ck', 'photo.jpg', 0, 100, 'photo', 0, \
             'local', 'dl')",
            [],
        )
        .unwrap();
        let (lc, dc): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT local_checksum, download_checksum FROM assets WHERE id = 'test'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(lc.as_deref(), Some("local"));
        assert_eq!(dc.as_deref(), Some("dl"));
    }

    // ── Gap: v4 idempotent when download_checksum already exists ─────

    #[test]
    fn test_v4_migration_idempotent_when_column_exists() {
        let conn = Connection::open_in_memory().unwrap();
        // Set up v3 database, then manually add v4 column
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        set_schema_version(&conn, 3).unwrap();
        conn.execute_batch("ALTER TABLE assets ADD COLUMN download_checksum TEXT")
            .unwrap();

        // Migration should succeed (idempotent) and advance to v4
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Column should be usable
        assert!(conn
            .prepare("SELECT download_checksum FROM assets LIMIT 0")
            .is_ok());
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
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, \
             size_bytes, media_type, last_seen_at) \
             VALUES ('PrimarySync', 'test', 'original', 'ck', 'photo.jpg', 0, 100, 'photo', 0)",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // ── V5 metadata migration ────────────────────────────────────────

    #[test]
    fn test_v5_adds_all_metadata_columns() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        for (col, _) in V5_ASSET_COLUMNS {
            let sql = format!("SELECT {col} FROM assets LIMIT 0");
            assert!(
                conn.prepare(&sql).is_ok(),
                "column {col} missing after migration"
            );
        }
    }

    #[test]
    fn test_v5_creates_asset_albums_and_people_tables() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM asset_albums", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM asset_people", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_v5_backfills_source_as_icloud_for_existing_rows() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a v4 database with an existing row
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        set_schema_version(&conn, 4).unwrap();
        conn.execute(
            "INSERT INTO assets (id, version_size, checksum, filename, created_at, size_bytes, media_type, last_seen_at) \
             VALUES ('legacy', 'original', 'ck', 'photo.jpg', 0, 100, 'photo', 0)",
            [],
        ).unwrap();

        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        let source: String = conn
            .query_row("SELECT source FROM assets WHERE id = 'legacy'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(source, "icloud");
    }

    #[test]
    fn test_v5_invalidates_sync_tokens() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        set_schema_version(&conn, 4).unwrap();
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('sync_token:PrimarySync', 'abc'), \
             ('sync_token:SharedSync-xyz', 'def'), ('other:key', 'keep')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let tokens: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata WHERE key LIKE 'sync_token:%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tokens, 0);
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata WHERE key = 'other:key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);
    }

    /// If the v5 migration arm is re-entered on an already-v5 DB (e.g.,
    /// someone ran `PRAGMA user_version = 0` to re-run migrations), sync
    /// tokens must NOT be wiped again: the invalidation is a one-shot
    /// upgrade side effect, not a recurring v5 behaviour.
    #[test]
    fn test_v5_does_not_reinvalidate_on_reentry() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // The fresh DB is now at v5. Simulate a stored sync token and a
        // re-entry of the migration arm.
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('sync_token:PrimarySync', 'post-v5')",
            [],
        )
        .unwrap();
        set_schema_version(&conn, 4).unwrap();

        // migrate() now observes start_version=4 and runs the v5 arm,
        // which SHOULD NOT wipe the token because start_version < 5 is
        // what we gate on. Token was inserted AFTER the first v5 ran, so
        // it represents real state the user accumulated post-upgrade —
        // test the gate by setting user_version back to 5 and calling
        // migrate again; then lower to 4 once more to trigger re-entry.
        set_schema_version(&conn, 5).unwrap();
        migrate(&conn).unwrap();
        let tokens: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM metadata WHERE key = 'sync_token:PrimarySync'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            tokens, 1,
            "tokens accumulated post-v5 must survive a no-op migrate() call"
        );
    }

    #[test]
    fn test_v5_idempotent_when_columns_exist() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        set_schema_version(&conn, 4).unwrap();
        // Pre-add a subset of v5 columns (simulates crash mid-migration)
        conn.execute_batch("ALTER TABLE assets ADD COLUMN source TEXT NOT NULL DEFAULT 'icloud'")
            .unwrap();
        conn.execute_batch("ALTER TABLE assets ADD COLUMN is_favorite INTEGER NOT NULL DEFAULT 0")
            .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
        for (col, _) in V5_ASSET_COLUMNS {
            assert!(
                conn.prepare(&format!("SELECT {col} FROM assets LIMIT 0"))
                    .is_ok(),
                "column {col} missing after idempotent migration"
            );
        }
    }

    #[test]
    fn test_v5_metadata_hash_index_exists() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let has_index: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_assets_metadata_hash'",
                [],
                |row| row.get::<_, i64>(0).map(|_| true),
            )
            .unwrap_or(false);
        assert!(has_index);
    }

    // ── v7 sync_runs.status migration ──────────────────────────────────────

    #[test]
    fn test_v7_adds_status_column_and_backfills() {
        let conn = Connection::open_in_memory().unwrap();
        // Simulate a v6 DB with a mix of runs
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        for (col, decl) in V5_ASSET_COLUMNS {
            conn.execute_batch(&format!("ALTER TABLE assets ADD COLUMN {col} {decl};"))
                .unwrap();
        }
        conn.execute_batch(SCHEMA_V5_TABLES).unwrap();
        conn.execute_batch("ALTER TABLE assets ADD COLUMN metadata_write_failed_at INTEGER;")
            .unwrap();
        set_schema_version(&conn, 6).unwrap();

        // Insert three historical sync_runs:
        //   1: clean (completed_at set, interrupted=0)      -> 'complete'
        //   2: flagged interrupted (completed_at set, =1)   -> 'interrupted'
        //   3: crashed (completed_at IS NULL)               -> 'interrupted'
        conn.execute(
            "INSERT INTO sync_runs (id, started_at, completed_at, interrupted) \
             VALUES (1, 100, 200, 0), (2, 300, 400, 1), (3, 500, NULL, 0)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        let status = |id: i64| -> String {
            conn.query_row("SELECT status FROM sync_runs WHERE id = ?1", [id], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
        };
        assert_eq!(status(1), "complete");
        assert_eq!(status(2), "interrupted");
        assert_eq!(status(3), "interrupted");
    }

    #[test]
    fn test_v7_idempotent_when_column_exists() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Second call must be a no-op — status column already exists
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_v7_fresh_db_has_status_column() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert!(conn.prepare("SELECT status FROM sync_runs LIMIT 0").is_ok());
    }

    // ── v8 per-zone PK migration ──────────────────────────────────────

    /// Build a v7 schema by hand (every prior ALTER applied) so v8 tests can
    /// seed pre-v8 rows and observe what the recreate-table dance does.
    fn build_v7_schema(conn: &Connection) {
        conn.execute_batch(SCHEMA_V1).unwrap();
        conn.execute_batch(SCHEMA_V2).unwrap();
        conn.execute_batch(SCHEMA_V3).unwrap();
        conn.execute_batch(SCHEMA_V4).unwrap();
        for (col, decl) in V5_ASSET_COLUMNS {
            conn.execute_batch(&format!("ALTER TABLE assets ADD COLUMN {col} {decl};"))
                .unwrap();
        }
        conn.execute_batch(SCHEMA_V5_TABLES).unwrap();
        conn.execute_batch("ALTER TABLE assets ADD COLUMN metadata_write_failed_at INTEGER;")
            .unwrap();
        conn.execute_batch(
            "ALTER TABLE sync_runs ADD COLUMN status TEXT NOT NULL DEFAULT 'running';",
        )
        .unwrap();
        set_schema_version(conn, 7).unwrap();
    }

    #[test]
    fn test_v8_fresh_db_has_library_column_and_pk() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Library column exists.
        assert!(conn.prepare("SELECT library FROM assets LIMIT 0").is_ok());

        // Composite PK enforces (library, id, version_size). Distinct
        // libraries with the same (id, version_size) coexist.
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, \
             size_bytes, media_type, last_seen_at) \
             VALUES ('PrimarySync', 'A', 'original', 'ck1', 'a.jpg', 0, 1, 'photo', 0), \
             ('SharedSync-XYZ', 'A', 'original', 'ck2', 'a.jpg', 0, 1, 'photo', 0)",
            [],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM assets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Same (library, id, version_size) twice must conflict.
        let dup = conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, \
             size_bytes, media_type, last_seen_at) \
             VALUES ('PrimarySync', 'A', 'original', 'ck3', 'a.jpg', 0, 1, 'photo', 0)",
            [],
        );
        assert!(
            dup.is_err(),
            "PRIMARY KEY (library, id, version_size) must reject duplicate triple"
        );
    }

    #[test]
    fn test_v8_backfills_existing_rows_to_primarysync() {
        let conn = Connection::open_in_memory().unwrap();
        build_v7_schema(&conn);

        // Seed two rows under the pre-v8 (id, version_size) PK with all
        // pre-v8 columns populated so we can verify they survive verbatim.
        conn.execute(
            "INSERT INTO assets (id, version_size, checksum, filename, created_at, size_bytes, \
             media_type, last_seen_at, status, downloaded_at, local_path, local_checksum, \
             download_checksum, source, is_favorite, rating, latitude, longitude, altitude, \
             orientation, duration_secs, timezone_offset, width, height, title, keywords, \
             description, media_subtype, burst_id, is_hidden, is_archived, modified_at, \
             is_deleted, deleted_at, provider_data, metadata_hash, metadata_write_failed_at) \
             VALUES \
             ('LEGACY_1', 'original', 'ck1', 'a.jpg', 100, 500, 'photo', 200, 'downloaded', \
             150, '/x/a.jpg', 'lck1', 'dck1', 'icloud', 1, 5, 37.0, -122.0, 30.0, 1, NULL, \
             -28800, 4032, 3024, 't', 'k', 'd', 'photo', 'b', 0, 0, 95, 0, NULL, '{}', 'h1', NULL), \
             ('LEGACY_2', 'medium', 'ck2', 'b.jpg', 110, 600, 'video', 210, 'pending', \
             NULL, NULL, NULL, NULL, 'icloud', 0, NULL, NULL, NULL, NULL, NULL, 12.5, NULL, \
             1920, 1080, NULL, NULL, NULL, NULL, NULL, 0, 1, NULL, 0, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Both rows survived under library='PrimarySync', metadata intact.
        let rows: Vec<(String, String, String, String, String, i64)> = conn
            .prepare(
                "SELECT library, id, version_size, checksum, filename, size_bytes \
                      FROM assets ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            (
                "PrimarySync".to_string(),
                "LEGACY_1".to_string(),
                "original".to_string(),
                "ck1".to_string(),
                "a.jpg".to_string(),
                500
            )
        );
        assert_eq!(
            rows[1],
            (
                "PrimarySync".to_string(),
                "LEGACY_2".to_string(),
                "medium".to_string(),
                "ck2".to_string(),
                "b.jpg".to_string(),
                600
            )
        );

        // Spot-check a few non-key columns made the trip.
        let (status, lp, mh): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, local_path, metadata_hash FROM assets WHERE id = 'LEGACY_1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "downloaded");
        assert_eq!(lp.as_deref(), Some("/x/a.jpg"));
        assert_eq!(mh.as_deref(), Some("h1"));
    }

    #[test]
    fn test_v8_indexes_recreated() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' \
                 AND tbl_name='assets' AND name LIKE 'idx_assets_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // status, local_path, checksum, metadata_hash — all four must be
        // present after the table-recreate dance.
        assert_eq!(count, 4);
    }

    #[test]
    fn test_v8_idempotent_after_partial_recovery() {
        let conn = Connection::open_in_memory().unwrap();
        // Fresh DB lands at SCHEMA_VERSION (v8). Re-running migrate must
        // be a no-op — column_exists guard skips the recreate-table.
        migrate(&conn).unwrap();
        let v_before = get_schema_version(&conn).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), v_before);
    }

    #[test]
    fn v8_migration_preserves_every_column_value() {
        // The v8 INSERT/SELECT/CREATE block hand-lists 41 columns three
        // times. A future v9 (or any later recreate-table dance) that
        // mismatches the INSERT column list against the SELECT projection
        // would silently swap two type-compatible columns (TEXT/TEXT, or
        // INTEGER/INTEGER) and corrupt every existing row. The other v8
        // tests spot-check ~10 columns; this one drives via PRAGMA so the
        // assertion stays exhaustive without manual upkeep when columns
        // are added.
        use rusqlite::types::Value;

        let conn = Connection::open_in_memory().unwrap();
        build_v7_schema(&conn);

        // Enumerate every v7 column dynamically. Each row is (name, type).
        let cols: Vec<(String, String)> = conn
            .prepare("PRAGMA table_info('assets')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            cols.len() >= 30,
            "v7 should expose at least 30 asset columns; got {}",
            cols.len()
        );

        // Per-column distinct sentinel keyed by name + index. Embedding the
        // column name in TEXT sentinels means a v9 INSERT that swaps two
        // TEXT columns (e.g. metadata_hash <-> provider_data) lands a value
        // tagged with the wrong column name and the assertion below fires.
        fn sentinel(col: &str, ty: &str, idx: usize) -> Value {
            // PK fields need stable values: id is the primary lookup key
            // and must round-trip verbatim; version_size is part of the PK
            // and constrained to known sizes elsewhere.
            if col == "id" {
                return Value::Text(format!("ID_SENTINEL_{idx}"));
            }
            if col == "version_size" {
                return Value::Text("original".to_string());
            }
            let upper = ty.to_ascii_uppercase();
            if upper.contains("INT") {
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "idx is a small column index, never overflows i64"
                )]
                let v = 1000_i64 + idx as i64;
                Value::Integer(v)
            } else if upper.contains("REAL") || upper.contains("FLOA") {
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "idx is a small column index, exact in f64"
                )]
                let v = 1.5_f64 + idx as f64;
                Value::Real(v)
            } else {
                Value::Text(format!("SENTINEL_{col}_{idx}"))
            }
        }

        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "INSERT INTO assets ({}) VALUES ({})",
            names.join(", "),
            placeholders.join(", ")
        );
        let values: Vec<Value> = cols
            .iter()
            .enumerate()
            .map(|(i, (name, ty))| sentinel(name, ty, i))
            .collect();
        conn.execute(&sql, rusqlite::params_from_iter(values.iter()))
            .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // Read every v7 column back. v8 added `library` (backfilled to
        // 'PrimarySync'); we verify it separately below.
        for (idx, (name, ty)) in cols.iter().enumerate() {
            let actual: Value = conn
                .query_row(
                    &format!("SELECT {name} FROM assets WHERE id = 'ID_SENTINEL_0'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_else(|e| panic!("v8 lost column `{name}`: {e}"));
            let expected = sentinel(name, ty, idx);
            assert_eq!(
                actual, expected,
                "v8 migration mismatch on column `{name}` (type `{ty}`); \
                 likely INSERT/SELECT column-list swap"
            );
        }

        // v8 backfilled `library` to PrimarySync for surviving rows.
        let lib: String = conn
            .query_row(
                "SELECT library FROM assets WHERE id = 'ID_SENTINEL_0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lib, "PrimarySync");
    }

    #[test]
    fn test_v8_leftover_assets_v8_table_does_not_block_migration() {
        let conn = Connection::open_in_memory().unwrap();
        build_v7_schema(&conn);
        // Simulate a prior interrupted attempt that left assets_v8 behind
        // (e.g. process killed mid-DDL outside the SAVEPOINT, somehow).
        conn.execute_batch(
            "CREATE TABLE assets_v8 (library TEXT NOT NULL, id TEXT NOT NULL, \
             version_size TEXT NOT NULL, PRIMARY KEY (library, id, version_size));",
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
        // assets_v8 should be gone (renamed to assets).
        let leftover: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='assets_v8'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0);
    }
}
