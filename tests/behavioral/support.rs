//! Existing fixtures shared across behavioral areas; no command assertions or policy.

use crate::common;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
static CLEAN_CMD_ID: AtomicUsize = AtomicUsize::new(0);

/// Helper: run kei with env scrubbed and a temp data-dir so it never
/// touches real config/cookies.
pub(super) fn clean_cmd() -> assert_cmd::Command {
    let mut cmd = common::cmd();
    let default_data_dir = std::env::temp_dir().join(format!(
        "kei-behavioral-{}-{}",
        std::process::id(),
        CLEAN_CMD_ID.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&default_data_dir).unwrap();
    cmd.env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env_remove("KEI_CONFIG")
        .env_remove("KEI_DATA_DIR")
        .env_remove("KEI_DOWNLOAD_DIR")
        .env_remove("KEI_LOG_LEVEL")
        .env_remove("KEI_NO_AUTO_CONFIG")
        .env_remove("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS")
        .env("KEI_DATA_DIR", default_data_dir)
        .timeout(TIMEOUT);
    cmd
}

pub(super) fn write_sync_config(config_path: &std::path::Path, download_dir: &str) {
    std::fs::write(
        config_path,
        format!(
            "[download]\ndirectory = {}\n",
            common::toml_string(download_dir)
        ),
    )
    .unwrap();
}

/// Sanitize a username the same way the binary does (alphanumeric + underscore).
pub(super) fn sanitize_username(username: &str) -> String {
    username
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Schema version mirrored by `create_state_db` below. Must equal
/// `crate::state::schema::SCHEMA_VERSION` (the production constant). The
/// `state::behavioral_helper_schema_matches_production` test pins this so
/// any schema bump in `src/state/schema.rs` fails the suite until this
/// helper is updated to match, preventing silent drift between the
/// helper's "fresh DB" shape and what the binary expects.
pub(super) const HELPER_SCHEMA_VERSION: i32 = 25;

/// Create a state DB at the expected path for the given username inside
/// `data_dir`. Mirrors the current schema from `src/state/schema.rs`
/// so the binary's migrate() loop is a no-op when it opens these DBs,
/// i.e. tests run against the same shape
/// production code writes on a fresh install. Bump `HELPER_SCHEMA_VERSION`
/// and the DDL below together whenever schema.rs changes; the
/// `state::behavioral_helper_schema_matches_production` meta test enforces it.
pub(super) fn create_state_db(data_dir: &std::path::Path, username: &str) -> rusqlite::Connection {
    let db_name = format!("{}.db", sanitize_username(username));
    let db_path = data_dir.join(db_name);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r"

CREATE TABLE IF NOT EXISTS asset_metadata_paths (
    library TEXT NOT NULL,
    id TEXT NOT NULL,
    version_size TEXT NOT NULL,
    local_path TEXT NOT NULL,
    provider_checksum TEXT NOT NULL,
    local_checksum TEXT,
    download_checksum TEXT,
    source_checksum TEXT,
    metadata_write_failed_at INTEGER,
    capture_repair_metadata_hash TEXT,
    capture_repair_output_checksum TEXT,
    capture_repair_output_size INTEGER,
    PRIMARY KEY (library, id, version_size, local_path)
);
CREATE INDEX IF NOT EXISTS idx_asset_metadata_paths_retry
    ON asset_metadata_paths(metadata_write_failed_at, library, id, version_size, local_path)
    WHERE metadata_write_failed_at IS NOT NULL;
        CREATE TABLE IF NOT EXISTS assets (
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
            imported_size INTEGER,
            imported_mtime INTEGER,
            capture_repair_metadata_hash TEXT,
            capture_repair_output_checksum TEXT,
            capture_repair_output_size INTEGER,
            PRIMARY KEY (library, id, version_size)
        );
        CREATE INDEX IF NOT EXISTS idx_assets_status ON assets(status);
        CREATE INDEX IF NOT EXISTS idx_assets_local_path ON assets(local_path);
        CREATE INDEX IF NOT EXISTS idx_assets_checksum ON assets(checksum);
        CREATE INDEX IF NOT EXISTS idx_assets_metadata_hash
            ON assets (metadata_hash) WHERE status = 'downloaded';

        CREATE TABLE IF NOT EXISTS sync_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at INTEGER NOT NULL,
            completed_at INTEGER,
            assets_seen INTEGER DEFAULT 0,
            assets_downloaded INTEGER DEFAULT 0,
            assets_failed INTEGER DEFAULT 0,
            interrupted INTEGER DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'running',
            enumeration_errors INTEGER NOT NULL DEFAULT 0,
            api_total_at_start INTEGER,
            api_total_at_start_partial INTEGER NOT NULL DEFAULT 0,
            inventory_drop_detected INTEGER NOT NULL DEFAULT 0,
            inventory_drop_previous_total INTEGER,
            inventory_drop_current_total INTEGER,
            inventory_drop_library TEXT
        );

        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS asset_albums (
            library    TEXT NOT NULL,
            asset_id   TEXT NOT NULL,
            album_name TEXT NOT NULL,
            source     TEXT NOT NULL,
            PRIMARY KEY (library, asset_id, album_name, source)
        );
        CREATE INDEX IF NOT EXISTS idx_asset_albums_lookup
            ON asset_albums (library, asset_id);

        CREATE TABLE IF NOT EXISTS asset_people (
            library     TEXT NOT NULL,
            asset_id    TEXT NOT NULL,
            person_name TEXT NOT NULL,
            PRIMARY KEY (library, asset_id, person_name)
        );
        CREATE INDEX IF NOT EXISTS idx_asset_people_lookup
            ON asset_people (library, asset_id);

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

        CREATE TABLE IF NOT EXISTS asset_master_mappings (
            library TEXT NOT NULL,
            asset_record_name TEXT NOT NULL,
            master_record_name TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (library, asset_record_name)
        );
        CREATE INDEX IF NOT EXISTS idx_asset_master_mappings_master
            ON asset_master_mappings (library, master_record_name);

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

        CREATE TABLE IF NOT EXISTS legacy_master_state_owners (
            library TEXT NOT NULL,
            master_record_name TEXT NOT NULL,
            asset_record_name TEXT NOT NULL,
            claimed_at INTEGER NOT NULL,
            PRIMARY KEY (library, master_record_name)
        );

        CREATE INDEX IF NOT EXISTS idx_legacy_master_state_owners_asset
            ON legacy_master_state_owners (library, asset_record_name, master_record_name);

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

        CREATE TABLE IF NOT EXISTS owned_temp_files (
            path BLOB PRIMARY KEY NOT NULL,
            claimed_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS reconciliation_paths (
            library TEXT NOT NULL,
            id TEXT NOT NULL,
            version_size TEXT NOT NULL,
            requested_path_key TEXT NOT NULL,
            destination_path_key TEXT NOT NULL,
            destination_path TEXT NOT NULL,
            provider_checksum TEXT NOT NULL,
            provider_size INTEGER NOT NULL CHECK (provider_size >= 0 OR (provider_size = -1 AND provider_checksum = '')),
            PRIMARY KEY (library, id, version_size, requested_path_key, provider_checksum, provider_size)
        );
        CREATE INDEX IF NOT EXISTS idx_reconciliation_paths_destination
            ON reconciliation_paths(destination_path_key);

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
        ",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", HELPER_SCHEMA_VERSION)
        .unwrap();
    conn
}

/// Insert an asset row into the state DB. The `library` column defaults
/// to `'PrimarySync'` to match the production v7→v8 backfill, where
/// pre-v8 rows (which had no library column) all came from PrimarySync.
pub(super) fn insert_asset(
    conn: &rusqlite::Connection,
    id: &str,
    status: &str,
    filename: &str,
    local_path: Option<&str>,
    last_error: Option<&str>,
    local_checksum: Option<&str>,
) {
    conn.execute(
        "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, \
         size_bytes, media_type, status, local_path, last_seen_at, last_error, \
         local_checksum, downloaded_at) \
         VALUES ('PrimarySync', ?1, 'original', 'abc', ?2, 1700000000, 1000, 'photo', ?3, ?4, \
         1700000000, ?5, ?6, CASE WHEN ?3 = 'downloaded' THEN 1700000000 ELSE NULL END)",
        rusqlite::params![id, filename, status, local_path, last_error, local_checksum],
    )
    .unwrap();
}

pub(super) fn write_fake_two_factor_config(
    dir: &std::path::Path,
    username: &str,
) -> std::path::PathBuf {
    let download_dir = dir.join("photos");
    std::fs::create_dir_all(&download_dir).unwrap();
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[auth]\nusername = {}\n\n[download]\ndirectory = {}\n",
            common::toml_string(username),
            common::toml_string(&download_dir.to_string_lossy()),
        ),
    )
    .unwrap();
    config_path
}

/// Build a `kei sync` invocation pre-populated with username, fresh tempdir
/// config/data directories, and `--only-print-filenames` so the
/// run exits before auth. Returns the live `Command` so callers can append
/// flag-specific args. Tempdirs are leaked into the binary (which never
/// touches them, as these tests bail in `Config::build`).
pub(super) fn sync_cmd_for_validation() -> assert_cmd::Command {
    sync_cmd_for_config_body("")
}

pub(super) fn sync_cmd_for_config_body(body: &str) -> assert_cmd::Command {
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = {}\n{body}",
            common::toml_string(dl_dir.path().to_str().unwrap())
        ),
    )
    .unwrap();
    let mut cmd = clean_cmd();
    cmd.args(["sync", "--config", config_path.to_str().unwrap()]);
    // Tempdirs leak intentionally: tests bail before sync touches them, and
    // OS-level tmpfs cleanup handles the directories at process exit.
    let _ = dir.keep();
    let _ = dl_dir.keep();
    cmd
}
