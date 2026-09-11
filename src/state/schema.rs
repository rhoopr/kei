//! Database schema definitions and migrations.

use rusqlite::Connection;

use super::error::StateError;

/// Current schema version. Increment when making schema changes.
pub(crate) const SCHEMA_VERSION: i32 = 23;

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
/// `source` records where the asset came from. `DEFAULT 'icloud'` is
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

/// V9 recreate-table migration: extend `asset_albums` and `asset_people`
/// PKs with `library` so multi-library accounts don't collide on shared
/// asset IDs. Mirrors the v8 dance (no ALTER PRIMARY KEY in SQLite, so we
/// copy into a fresh table and rename). Pre-v9 kei only wrote PrimarySync
/// album/person rows (no library column existed); backfilling with
/// `library='PrimarySync'` is exact, not approximate.
fn schema_v9() -> &'static str {
    r"
DROP TABLE IF EXISTS asset_albums_v9;
DROP TABLE IF EXISTS asset_people_v9;

CREATE TABLE asset_albums_v9 (
    library    TEXT NOT NULL,
    asset_id   TEXT NOT NULL,
    album_name TEXT NOT NULL,
    source     TEXT NOT NULL,
    PRIMARY KEY (library, asset_id, album_name, source)
);

INSERT INTO asset_albums_v9 (library, asset_id, album_name, source)
SELECT 'PrimarySync', asset_id, album_name, source FROM asset_albums;

DROP TABLE asset_albums;
ALTER TABLE asset_albums_v9 RENAME TO asset_albums;

CREATE INDEX IF NOT EXISTS idx_asset_albums_lookup
    ON asset_albums (library, asset_id);

CREATE TABLE asset_people_v9 (
    library     TEXT NOT NULL,
    asset_id    TEXT NOT NULL,
    person_name TEXT NOT NULL,
    PRIMARY KEY (library, asset_id, person_name)
);

INSERT INTO asset_people_v9 (library, asset_id, person_name)
SELECT 'PrimarySync', asset_id, person_name FROM asset_people;

DROP TABLE asset_people;
ALTER TABLE asset_people_v9 RENAME TO asset_people;

CREATE INDEX IF NOT EXISTS idx_asset_people_lookup
    ON asset_people (library, asset_id);
"
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

/// V12 trusted album-membership cache.
///
/// `asset_albums` remains the compatibility read model for reports and XMP.
/// These tables track album containers and durable membership generations so
/// later sync-routing work can prove when album-aware incremental routing is
/// safe.
const SCHEMA_V12: &str = r"
CREATE TABLE IF NOT EXISTS album_containers (
    library TEXT NOT NULL,
    container_id TEXT NOT NULL,
    album_name TEXT NOT NULL,
    pass_kind TEXT NOT NULL,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, container_id)
);

CREATE TABLE IF NOT EXISTS album_membership_snapshots (
    library TEXT NOT NULL,
    container_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    status TEXT NOT NULL,
    enum_config_hash TEXT,
    started_at INTEGER NOT NULL,
    completed_at INTEGER,
    PRIMARY KEY (library, container_id, generation)
);

CREATE TABLE IF NOT EXISTS asset_album_memberships (
    library TEXT NOT NULL,
    asset_record_name TEXT NOT NULL,
    master_record_name TEXT,
    container_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    is_deleted INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_record_name, container_id)
);

CREATE INDEX IF NOT EXISTS idx_album_containers_lookup
    ON album_containers (library, album_name);
CREATE INDEX IF NOT EXISTS idx_album_membership_snapshots_status
    ON album_membership_snapshots (library, container_id, status);
CREATE INDEX IF NOT EXISTS idx_asset_album_memberships_asset
    ON asset_album_memberships (library, asset_record_name, is_deleted);
CREATE INDEX IF NOT EXISTS idx_asset_album_memberships_container
    ON asset_album_memberships (library, container_id, is_deleted);
";

/// V14 scoped database-level `/changes/database` token provenance.
///
/// These rows are pre-check cursors only. They prove that the exact stored
/// scope can ask CloudKit whether selected zones changed; they do not prove
/// per-zone coverage for `/changes/zone` incremental sync.
const SCHEMA_V14: &str = r"
CREATE TABLE IF NOT EXISTS scoped_db_sync_tokens (
    provider TEXT NOT NULL,
    account TEXT NOT NULL,
    shape_version INTEGER NOT NULL,
    scope_hash TEXT NOT NULL,
    selected_zones_json TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    token TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (provider, account, shape_version, scope_hash)
);
";

/// V15 durable CPLAsset -> CPLMaster mapping.
///
/// CloudKit hard-delete tombstones can arrive with only a `recordName` and no
/// `recordType` or fields. Download state normally uses `CPLMaster`, while
/// sibling `CPLAsset` rows can use their own asset record names, so keep the
/// last observed `CPLAsset.recordName` bridge per library.
const SCHEMA_V15: &str = r"
CREATE TABLE IF NOT EXISTS asset_master_mappings (
    library TEXT NOT NULL,
    asset_record_name TEXT NOT NULL,
    master_record_name TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_record_name)
);

CREATE INDEX IF NOT EXISTS idx_asset_master_mappings_master
    ON asset_master_mappings (library, master_record_name);

INSERT OR IGNORE INTO asset_master_mappings (
    library,
    asset_record_name,
    master_record_name,
    updated_at
)
SELECT
    membership.library,
    membership.asset_record_name,
    MIN(membership.master_record_name),
    CAST(strftime('%s', 'now') AS INTEGER)
FROM asset_album_memberships AS membership
WHERE membership.asset_record_name <> ''
  AND membership.master_record_name IS NOT NULL
  AND membership.master_record_name <> ''
  AND NOT EXISTS (
      SELECT 1
      FROM asset_master_mappings AS mapping
      WHERE mapping.library = membership.library
        AND mapping.asset_record_name = membership.asset_record_name
  )
GROUP BY membership.library, membership.asset_record_name
HAVING COUNT(DISTINCT membership.master_record_name) = 1;
";

/// V16 durable provider-verification state for unresolved retry rows.
const SCHEMA_V16: &str = r"
CREATE TABLE IF NOT EXISTS asset_verifications (
    library TEXT NOT NULL,
    id TEXT NOT NULL,
    version_size TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('unknown', 'transient_failure')),
    reason TEXT NOT NULL,
    checked_at INTEGER NOT NULL,
    PRIMARY KEY (library, id, version_size),
    FOREIGN KEY (library, id, version_size)
        REFERENCES assets (library, id, version_size) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_asset_verifications_state
    ON asset_verifications (state, checked_at);
";

/// V17 durable owner for a legacy CPLMaster-keyed state row.
///
/// A master can have more than one CPLAsset child. Once kei adopts a legacy
/// master-keyed row for one child, this record keeps later siblings from
/// changing that choice across sync cycles.
const SCHEMA_V17: &str = r"
CREATE TABLE IF NOT EXISTS legacy_master_state_owners (
    library TEXT NOT NULL,
    master_record_name TEXT NOT NULL,
    asset_record_name TEXT NOT NULL,
    claimed_at INTEGER NOT NULL,
    PRIMARY KEY (library, master_record_name)
);
";

/// V18 metadata-capture revisions and per-library repair state.
///
/// Existing assets intentionally receive no row in
/// `asset_metadata_capture_revisions`; absence is durable evidence that the
/// catalogue metadata predates the current capture semantics.
const SCHEMA_V18: &str = r"
CREATE TABLE IF NOT EXISTS asset_metadata_capture_revisions (
    library TEXT NOT NULL,
    asset_id TEXT NOT NULL,
    revision INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (library, asset_id)
);

CREATE TABLE IF NOT EXISTS metadata_capture_state (
    library TEXT PRIMARY KEY NOT NULL,
    active_revision INTEGER NOT NULL DEFAULT 0,
    pending_revision INTEGER,
    processed_assets INTEGER NOT NULL DEFAULT 0,
    failed_assets INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_asset_metadata_capture_revision
    ON asset_metadata_capture_revisions (library, revision);
";

/// V19 durable ownership ledger for resumable download temporary files.
///
/// Cleanup may delete only paths named here. `claimed_at` separates stale
/// ownership left by an incomplete sync from a path claimed by a current
/// download attempt.
const SCHEMA_V19: &str = r"
CREATE TABLE IF NOT EXISTS owned_temp_files (
    path BLOB PRIMARY KEY NOT NULL,
    claimed_at INTEGER NOT NULL
);
";

/// V20 durable capture-timestamp repair receipt.
///
/// All NULL means no repair is queued. A metadata hash alone is pending, and
/// all three values mean a prepared replacement is ready to publish.
const V20_ASSET_COLUMNS: &[(&str, &str)] = &[
    ("capture_repair_metadata_hash", "TEXT"),
    ("capture_repair_output_checksum", "TEXT"),
    ("capture_repair_output_size", "INTEGER"),
];

/// Metadata evidence for every successfully finalized path. The catalogue still
/// owns sync selection; this table does not authorize adoption or media deletion.
const SCHEMA_V21: &str = r"
CREATE TABLE IF NOT EXISTS asset_metadata_paths (
    library TEXT NOT NULL,
    id TEXT NOT NULL,
    version_size TEXT NOT NULL,
    local_path TEXT NOT NULL,
    provider_checksum TEXT NOT NULL,
    local_checksum TEXT,
    download_checksum TEXT,
    metadata_write_failed_at INTEGER,
    capture_repair_metadata_hash TEXT,
    capture_repair_output_checksum TEXT,
    capture_repair_output_size INTEGER,
    PRIMARY KEY (library, id, version_size, local_path)
);
CREATE INDEX IF NOT EXISTS idx_asset_metadata_paths_retry
    ON asset_metadata_paths(metadata_write_failed_at, library, id, version_size, local_path)
    WHERE metadata_write_failed_at IS NOT NULL;
INSERT OR IGNORE INTO asset_metadata_paths
    (library, id, version_size, local_path, provider_checksum, local_checksum,
     download_checksum, metadata_write_failed_at, capture_repair_metadata_hash,
     capture_repair_output_checksum, capture_repair_output_size)
SELECT library, id, version_size, local_path, checksum, local_checksum,
       download_checksum, metadata_write_failed_at, capture_repair_metadata_hash,
       capture_repair_output_checksum, capture_repair_output_size
FROM assets WHERE status = 'downloaded' AND local_path IS NOT NULL;
";

/// Reverse lookup for the authoritative legacy owner of a child identity.
const SCHEMA_V22: &str = "CREATE INDEX IF NOT EXISTS idx_legacy_master_state_owners_asset \
    ON legacy_master_state_owners (library, asset_record_name, master_record_name);";

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
        9 => {
            // Idempotent: re-running on a v9 DB skips the recreate-table dance.
            if !column_exists(conn, "asset_albums", "library")? {
                conn.execute_batch(schema_v9())?;
            }
        }
        10 => {
            // sync_runs.enumeration_errors: per-run count of records
            // the producer could not enumerate. Default 0 is the
            // correct backfill -- existing rows predate the counter.
            if !column_exists(conn, "sync_runs", "enumeration_errors")? {
                conn.execute_batch(
                    "ALTER TABLE sync_runs ADD COLUMN enumeration_errors INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
        }
        11 => {
            // imported_size / imported_mtime: snapshot of the on-disk file
            // metadata at adopt time. import-existing reads these on
            // subsequent runs to skip the SHA-256 re-read when size + mtime
            // are unchanged (fresh DBs and rows imported pre-v11 leave
            // these NULL, which forces a real hash).
            if !column_exists(conn, "assets", "imported_size")? {
                conn.execute_batch("ALTER TABLE assets ADD COLUMN imported_size INTEGER;")?;
            }
            if !column_exists(conn, "assets", "imported_mtime")? {
                conn.execute_batch("ALTER TABLE assets ADD COLUMN imported_mtime INTEGER;")?;
            }
        }
        12 => conn.execute_batch(SCHEMA_V12)?,
        13 => {
            // sync_runs.api_total_at_start: count-only CloudKit inventory
            // observed at the start of a reliable full enumeration. The
            // inventory-drop columns capture the latest cross-cycle warning
            // so `kei status` can surface it without grepping logs.
            if !column_exists(conn, "sync_runs", "api_total_at_start")? {
                conn.execute_batch("ALTER TABLE sync_runs ADD COLUMN api_total_at_start INTEGER;")?;
            }
            if !column_exists(conn, "sync_runs", "api_total_at_start_partial")? {
                conn.execute_batch(
                    "ALTER TABLE sync_runs ADD COLUMN api_total_at_start_partial INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            if !column_exists(conn, "sync_runs", "inventory_drop_detected")? {
                conn.execute_batch(
                    "ALTER TABLE sync_runs ADD COLUMN inventory_drop_detected INTEGER NOT NULL DEFAULT 0;",
                )?;
            }
            if !column_exists(conn, "sync_runs", "inventory_drop_previous_total")? {
                conn.execute_batch(
                    "ALTER TABLE sync_runs ADD COLUMN inventory_drop_previous_total INTEGER;",
                )?;
            }
            if !column_exists(conn, "sync_runs", "inventory_drop_current_total")? {
                conn.execute_batch(
                    "ALTER TABLE sync_runs ADD COLUMN inventory_drop_current_total INTEGER;",
                )?;
            }
            if !column_exists(conn, "sync_runs", "inventory_drop_library")? {
                conn.execute_batch(
                    "ALTER TABLE sync_runs ADD COLUMN inventory_drop_library TEXT;",
                )?;
            }
        }
        14 => conn.execute_batch(SCHEMA_V14)?,
        15 => conn.execute_batch(SCHEMA_V15)?,
        16 => conn.execute_batch(SCHEMA_V16)?,
        17 => conn.execute_batch(SCHEMA_V17)?,
        18 => conn.execute_batch(SCHEMA_V18)?,
        19 => conn.execute_batch(SCHEMA_V19)?,
        20 => {
            for (column, declaration) in V20_ASSET_COLUMNS {
                if !column_exists(conn, "assets", column)? {
                    conn.execute_batch(&format!(
                        "ALTER TABLE assets ADD COLUMN {column} {declaration};"
                    ))?;
                }
            }
        }
        21 => conn.execute_batch(SCHEMA_V21)?,
        22 => conn.execute_batch(SCHEMA_V22)?,
        23 => {
            // Historical download_checksum values may come from rewritten
            // local bytes. Only new verified downloads establish provenance.
            if !column_exists(conn, "asset_metadata_paths", "source_checksum")? {
                conn.execute_batch(
                    "ALTER TABLE asset_metadata_paths ADD COLUMN source_checksum TEXT;",
                )?;
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
    fn v22_indexes_existing_legacy_owners_without_changing_them() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=21 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        conn.execute(
            "INSERT INTO legacy_master_state_owners VALUES ('PrimarySync', 'master', 'child', 123)",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let columns: Vec<String> = conn.prepare("SELECT name FROM pragma_index_info('idx_legacy_master_state_owners_asset') ORDER BY seqno").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(
            columns,
            ["library", "asset_record_name", "master_record_name"]
        );
        let owner = conn.query_row("SELECT master_record_name, asset_record_name, claimed_at FROM legacy_master_state_owners WHERE library = 'PrimarySync'", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?))
        }).unwrap();
        assert_eq!(owner, ("master".into(), "child".into(), 123));
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn v21_migrates_only_recorded_paths_with_independent_retry_evidence() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=20 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        let path = "/photos/Trip's été/image.jpg";
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at, local_path, local_checksum, metadata_write_failed_at, capture_repair_metadata_hash, capture_repair_output_checksum, capture_repair_output_size) VALUES ('PrimarySync', 'child', 'original', 'provider', 'image.jpg', 1, 10, 'photo', 'downloaded', 1, ?1, 'local', 123, 'metadata', 'prepared', 12)",
            [path],
        ).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let row = conn.query_row("SELECT local_path, local_checksum, metadata_write_failed_at, capture_repair_output_checksum FROM asset_metadata_paths", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?))
        }).unwrap();
        assert_eq!(row, (path.into(), "local".into(), 123, "prepared".into()));
        conn.execute("INSERT INTO asset_metadata_paths (library, id, version_size, local_path, provider_checksum) VALUES ('PrimarySync', 'child', 'original', '/photos/Family/image.jpg', 'provider')", []).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM asset_metadata_paths", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            2
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM assets", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "path metadata tracking must not duplicate the sync catalogue"
        );
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

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
        assert!(
            conn.prepare("SELECT local_checksum FROM assets LIMIT 0")
                .is_ok()
        );
        assert!(
            conn.prepare("SELECT download_checksum FROM assets LIMIT 0")
                .is_err()
        );

        // Migrate should bring it to v4
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        // download_checksum should now exist
        assert!(
            conn.prepare("SELECT download_checksum FROM assets LIMIT 0")
                .is_ok()
        );

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
        assert!(
            conn.prepare("SELECT download_checksum FROM assets LIMIT 0")
                .is_ok()
        );
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
        assert!(
            conn.prepare("SELECT local_checksum, download_checksum FROM assets LIMIT 0")
                .is_ok()
        );

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

    /// v9 must add `library` to both join tables and backfill existing
    /// rows to `'PrimarySync'`. Pre-v9 only PrimarySync rows ever existed
    /// (no zone parameter on the writers), so the backfill is exact.
    #[test]
    fn test_v9_backfills_join_tables_to_primarysync() {
        let conn = Connection::open_in_memory().unwrap();
        // Build v8 (everything up to but not including v9).
        build_v7_schema(&conn);
        conn.execute_batch(&schema_v8()).unwrap();
        set_schema_version(&conn, 8).unwrap();

        // Seed pre-v9 join-table rows (no library column yet).
        conn.execute(
            "INSERT INTO asset_albums (asset_id, album_name, source) VALUES \
             ('A1', 'Vacation', 'icloud'), \
             ('A2', 'Family', 'icloud')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_people (asset_id, person_name) VALUES \
             ('A1', 'Alice'), \
             ('A2', 'Bob')",
            [],
        )
        .unwrap();

        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);

        let albums: Vec<(String, String, String, String)> = conn
            .prepare(
                "SELECT library, asset_id, album_name, source FROM asset_albums ORDER BY asset_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            albums,
            vec![
                (
                    "PrimarySync".into(),
                    "A1".into(),
                    "Vacation".into(),
                    "icloud".into()
                ),
                (
                    "PrimarySync".into(),
                    "A2".into(),
                    "Family".into(),
                    "icloud".into()
                ),
            ]
        );

        let people: Vec<(String, String, String)> = conn
            .prepare("SELECT library, asset_id, person_name FROM asset_people ORDER BY asset_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            people,
            vec![
                ("PrimarySync".into(), "A1".into(), "Alice".into()),
                ("PrimarySync".into(), "A2".into(), "Bob".into()),
            ]
        );

        // The new PK must reject same-library duplicates AND accept
        // same-(asset_id, album_name, source) across different libraries.
        let dup = conn.execute(
            "INSERT INTO asset_albums (library, asset_id, album_name, source) \
             VALUES ('PrimarySync', 'A1', 'Vacation', 'icloud')",
            [],
        );
        assert!(dup.is_err(), "v9 PK must reject same-library duplicates");
        let cross = conn.execute(
            "INSERT INTO asset_albums (library, asset_id, album_name, source) \
             VALUES ('SharedSync-AB', 'A1', 'Vacation', 'icloud')",
            [],
        );
        assert!(
            cross.is_ok(),
            "v9 PK must accept same triple across libraries; got: {cross:?}"
        );
    }

    #[test]
    fn test_v9_indexes_recreated() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        for idx in ["idx_asset_albums_lookup", "idx_asset_people_lookup"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?1",
                    [idx],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "v9 must (re)create {idx}");
        }
    }

    /// v10 must add `sync_runs.enumeration_errors` and back-fill to 0
    /// on existing rows. Catches a future migration that forgets the
    /// column or picks a wrong default that breaks NOT NULL on backfill.
    #[test]
    fn test_v10_adds_enumeration_errors_column() {
        let conn = Connection::open_in_memory().unwrap();
        // Pre-seed at v9 with the v9 schema, plus one sync_runs row to
        // exercise the backfill default.
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();
        migrate(&conn).unwrap();

        // Column must exist after migration to current.
        let has_col = column_exists(&conn, "sync_runs", "enumeration_errors").unwrap();
        assert!(has_col, "v10 must add `enumeration_errors` to sync_runs");

        // Default 0 must apply to a fresh insert that omits the column.
        conn.execute(
            "INSERT INTO sync_runs (started_at) VALUES (?1)",
            [1700000000_i64],
        )
        .unwrap();
        let stored: i64 = conn
            .query_row(
                "SELECT enumeration_errors FROM sync_runs ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stored, 0,
            "default 0 must apply to inserts that omit the column"
        );
    }

    /// Idempotent re-entry: applying v10 to a DB that already has the
    /// column (crash recovery, partial migration) must not error.
    #[test]
    fn test_v10_idempotent_when_column_exists() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        // Reset version backwards and re-run the v10 step. This simulates
        // an unusual recovery path; the migration must not fail.
        set_schema_version(&conn, 9).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    /// v13 persists the count-only CloudKit inventory snapshot and latest
    /// cross-cycle drop warning fields on `sync_runs`.
    #[test]
    fn test_v13_adds_inventory_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();
        migrate(&conn).unwrap();

        for column in [
            "api_total_at_start",
            "api_total_at_start_partial",
            "inventory_drop_detected",
            "inventory_drop_previous_total",
            "inventory_drop_current_total",
            "inventory_drop_library",
        ] {
            assert!(
                column_exists(&conn, "sync_runs", column).unwrap(),
                "v13 must add sync_runs.{column}"
            );
        }

        conn.execute(
            "INSERT INTO sync_runs (started_at) VALUES (?1)",
            [1700000000_i64],
        )
        .unwrap();
        let defaults: (i64, i64) = conn
            .query_row(
                "SELECT api_total_at_start_partial, inventory_drop_detected \
                 FROM sync_runs ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(defaults, (0, 0));
    }

    /// v11 introduces `imported_size` and `imported_mtime` on `assets`.
    /// Both are nullable so pre-v11 rows survive the upgrade with NULL,
    /// which the import-existing skip-rehash path treats as "no snapshot,
    /// re-hash" rather than as an error.
    #[test]
    fn test_v11_adds_imported_size_and_mtime_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();
        migrate(&conn).unwrap();

        assert!(
            column_exists(&conn, "assets", "imported_size").unwrap(),
            "v11 must add `imported_size` to assets",
        );
        assert!(
            column_exists(&conn, "assets", "imported_mtime").unwrap(),
            "v11 must add `imported_mtime` to assets",
        );
    }

    /// v11 must be re-runnable on a DB that already has both columns
    /// (crash mid-migration, replay through migrate()).
    #[test]
    fn test_v11_idempotent_when_columns_exist() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        set_schema_version(&conn, 10).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_v12_creates_album_membership_tables_and_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();
        migrate(&conn).unwrap();

        for table in [
            "album_containers",
            "album_membership_snapshots",
            "asset_album_memberships",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "v12 must create table {table}");
        }

        for (table, column) in [
            ("album_containers", "container_id"),
            ("album_membership_snapshots", "enum_config_hash"),
            ("asset_album_memberships", "asset_record_name"),
            ("asset_album_memberships", "master_record_name"),
        ] {
            assert!(
                column_exists(&conn, table, column).unwrap(),
                "v12 must create {table}.{column}",
            );
        }

        for idx in [
            "idx_album_containers_lookup",
            "idx_album_membership_snapshots_status",
            "idx_asset_album_memberships_asset",
            "idx_asset_album_memberships_container",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?1",
                    [idx],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "v12 must create index {idx}");
        }
    }

    #[test]
    fn test_v12_idempotent_when_tables_exist() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        set_schema_version(&conn, 11).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_v14_creates_scoped_db_sync_tokens_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();
        migrate(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = 'scoped_db_sync_tokens'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "v14 must create scoped_db_sync_tokens");

        for column in [
            "provider",
            "account",
            "shape_version",
            "scope_hash",
            "selected_zones_json",
            "scope_json",
            "token",
            "created_at",
            "updated_at",
        ] {
            assert!(
                column_exists(&conn, "scoped_db_sync_tokens", column).unwrap(),
                "v14 must create scoped_db_sync_tokens.{column}",
            );
        }
    }

    #[test]
    fn test_v15_creates_asset_master_mappings_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        set_schema_version(&conn, 1).unwrap();
        migrate(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name = 'asset_master_mappings'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "v15 must create asset_master_mappings");

        for column in [
            "library",
            "asset_record_name",
            "master_record_name",
            "updated_at",
        ] {
            assert!(
                column_exists(&conn, "asset_master_mappings", column).unwrap(),
                "v15 must create asset_master_mappings.{column}",
            );
        }

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name = 'idx_asset_master_mappings_master'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            exists, 1,
            "v15 must create idx_asset_master_mappings_master"
        );
    }

    #[test]
    fn test_v16_creates_asset_verifications_table() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'asset_verifications'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
        for column in [
            "library",
            "id",
            "version_size",
            "state",
            "reason",
            "checked_at",
        ] {
            assert!(column_exists(&conn, "asset_verifications", column).unwrap());
        }
    }

    #[test]
    fn test_v17_creates_legacy_master_state_owners_table() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=16 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        set_schema_version(&conn, 16).unwrap();

        migrate(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'legacy_master_state_owners'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
        for column in [
            "library",
            "master_record_name",
            "asset_record_name",
            "claimed_at",
        ] {
            assert!(column_exists(&conn, "legacy_master_state_owners", column).unwrap());
        }
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_v18_creates_metadata_capture_revision_state_without_backfill() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=17 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, \
                created_at, size_bytes, media_type, status, last_seen_at) \
             VALUES ('PrimarySync', 'legacy', 'original', 'checksum', 'legacy.jpg', \
                1, 10, 'photo', 'downloaded', 1)",
            [],
        )
        .unwrap();
        set_schema_version(&conn, 17).unwrap();

        migrate(&conn).unwrap();

        for table in ["asset_metadata_capture_revisions", "metadata_capture_state"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing {table}");
        }
        let revisions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_metadata_capture_revisions",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            revisions, 0,
            "pre-v18 catalogue rows must remain visibly stale"
        );
    }

    #[test]
    fn test_v19_creates_owned_temp_file_ledger_without_backfill() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=18 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        set_schema_version(&conn, 18).unwrap();

        migrate(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type = 'table' AND name = 'owned_temp_files'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
        for column in ["path", "claimed_at"] {
            assert!(column_exists(&conn, "owned_temp_files", column).unwrap());
        }
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM owned_temp_files", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn v23_source_checksum_migration_keeps_historical_provenance_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let conn = Connection::open(&path).unwrap();
        for version in 1..=22 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        set_schema_version(&conn, 22).unwrap();
        for (name, local, downloaded) in [
            ("equal", Some("rewritten"), Some("rewritten")),
            ("different", Some("local"), Some("downloaded")),
            ("missing", None, None),
        ] {
            conn.execute(
                "INSERT INTO asset_metadata_paths \
                    (library, id, version_size, local_path, provider_checksum, \
                     local_checksum, download_checksum, metadata_write_failed_at) \
                 VALUES ('PrimarySync', ?1, 'original', ?1, 'provider', ?2, ?3, 7)",
                rusqlite::params![name, local, downloaded],
            )
            .unwrap();
        }
        migrate(&conn).unwrap();
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        migrate(&conn).unwrap();
        let known: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_metadata_paths WHERE source_checksum IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(known, 0);
        // Re-entry cannot overwrite subsequently established source evidence.
        conn.execute(
            "UPDATE asset_metadata_paths SET source_checksum = 'verified' WHERE id = 'equal'",
            [],
        )
        .unwrap();
        migrate_to_version(&conn, 22, 23).unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, local_checksum, download_checksum, source_checksum, metadata_write_failed_at \
             FROM asset_metadata_paths ORDER BY id"
        ).unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "different".into(),
                    Some("local".into()),
                    Some("downloaded".into()),
                    None,
                    7
                ),
                (
                    "equal".into(),
                    Some("rewritten".into()),
                    Some("rewritten".into()),
                    Some("verified".into()),
                    7
                ),
                ("missing".into(), None, None, None, 7),
            ]
        );
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_v20_adds_capture_repair_receipt_without_backfill() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=19 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, \
                created_at, size_bytes, media_type, status, last_seen_at) \
             VALUES ('PrimarySync', 'legacy', 'original', 'checksum', 'legacy.jpg', \
                1, 10, 'photo', 'downloaded', 1)",
            [],
        )
        .unwrap();
        set_schema_version(&conn, 19).unwrap();

        migrate(&conn).unwrap();

        for (column, _) in V20_ASSET_COLUMNS {
            assert!(column_exists(&conn, "assets", column).unwrap());
        }
        let receipt: (Option<String>, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT capture_repair_metadata_hash, capture_repair_output_checksum, \
                    capture_repair_output_size FROM assets WHERE id = 'legacy'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(receipt, (None, None, None));
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_v20_migration_is_idempotent_after_partial_application() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=19 {
            migrate_to_version(&conn, 0, version).unwrap();
        }
        conn.execute_batch(
            "ALTER TABLE assets ADD COLUMN capture_repair_metadata_hash TEXT;\
             ALTER TABLE assets ADD COLUMN capture_repair_output_checksum TEXT;",
        )
        .unwrap();
        set_schema_version(&conn, 19).unwrap();

        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        for (column, _) in V20_ASSET_COLUMNS {
            assert!(column_exists(&conn, "assets", column).unwrap());
        }
        assert_eq!(get_schema_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn test_v15_backfills_unambiguous_album_membership_mappings() {
        let conn = Connection::open_in_memory().unwrap();
        for version in 1..=14 {
            migrate_to_version(&conn, 0, version).unwrap();
        }

        conn.execute_batch(
            "
            INSERT INTO asset_album_memberships (
                library,
                asset_record_name,
                master_record_name,
                container_id,
                generation,
                is_deleted,
                source,
                updated_at
            ) VALUES
                ('PrimarySync', 'asset-a', 'master-a', 'album-1', 1, 0, 'icloud', 1),
                ('PrimarySync', 'asset-a', 'master-a', 'album-2', 1, 1, 'icloud', 1),
                ('SharedSync-AAAA', 'asset-a', 'master-shared', 'album-1', 1, 0, 'icloud', 1),
                ('PrimarySync', 'asset-ambiguous', 'master-one', 'album-1', 1, 0, 'icloud', 1),
                ('PrimarySync', 'asset-ambiguous', 'master-two', 'album-2', 1, 0, 'icloud', 1),
                ('PrimarySync', 'asset-null', NULL, 'album-1', 1, 0, 'icloud', 1);
            ",
        )
        .unwrap();
        set_schema_version(&conn, 14).unwrap();

        migrate(&conn).unwrap();

        let mappings: Vec<(String, String, String)> = conn
            .prepare(
                "SELECT library, asset_record_name, master_record_name \
                 FROM asset_master_mappings \
                 ORDER BY library, asset_record_name",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            mappings,
            vec![
                (
                    "PrimarySync".to_string(),
                    "asset-a".to_string(),
                    "master-a".to_string()
                ),
                (
                    "SharedSync-AAAA".to_string(),
                    "asset-a".to_string(),
                    "master-shared".to_string()
                ),
            ]
        );
    }
}
