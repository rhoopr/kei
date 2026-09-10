//! Behavioral tests -- exercise real execution paths without credentials.
//!
//! These tests run the actual binary and verify outputs, exit codes,
//! config resolution, command routing, and error messages.
//! No network, no iCloud credentials required.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod common;

use predicates::prelude::*;
use rusqlite::OptionalExtension;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);
static CLEAN_CMD_ID: AtomicUsize = AtomicUsize::new(0);

/// Helper: run kei with env scrubbed and a temp data-dir so it never
/// touches real config/cookies.
fn clean_cmd() -> assert_cmd::Command {
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

#[test]
fn auth_retry_detects_cloudkit_401_as_stale_session() {
    let msg = "import scan aborted: fetcher returned error: HTTP 401 for https://p121-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/private/records/query";
    assert!(
        common::is_retryable_auth_failure(msg),
        "CloudKit 401 should refresh auth and retry the live command"
    );
}

fn write_sync_config(config_path: &std::path::Path, download_dir: &str) {
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
fn sanitize_username(username: &str) -> String {
    username
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// Schema version mirrored by `create_state_db` below. Must equal
/// `crate::state::schema::SCHEMA_VERSION` (the production constant). The
/// `behavioral_helper_schema_matches_production` test below pins this so
/// any schema bump in `src/state/schema.rs` fails the suite until this
/// helper is updated to match, preventing silent drift between the
/// helper's "fresh DB" shape and what the binary expects.
const HELPER_SCHEMA_VERSION: i32 = 22;

/// Create a state DB at the expected path for the given username inside
/// `data_dir`. Mirrors the current schema from `src/state/schema.rs`
/// so the binary's migrate() loop is a no-op when it opens these DBs,
/// i.e. tests run against the same shape
/// production code writes on a fresh install. Bump `HELPER_SCHEMA_VERSION`
/// and the DDL below together whenever schema.rs changes; the
/// `behavioral_helper_schema_matches_production` meta test enforces it.
fn create_state_db(data_dir: &std::path::Path, username: &str) -> rusqlite::Connection {
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
fn insert_asset(
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

/// Pin the helper schema version against the binary's
/// production constant. The binary writes a fresh DB at
/// `state::schema::SCHEMA_VERSION` (currently 22). The helper above
/// claims to "Mirror the latest schema" and must therefore land on the
/// same version. Otherwise existing tests rely on the binary's
/// migrate() loop to fill in columns and we lose end-to-end coverage of
/// the fresh-DB path.
///
/// `state::schema::SCHEMA_VERSION` is `pub(crate)` so we can't import
/// it from an integration test; pin the literal value here and
/// document the bump procedure in the doc-comment. Production-side
/// tests (`src/state/schema.rs::tests::*`) already exercise the
/// migration constant directly.
#[test]
fn behavioral_helper_schema_matches_production() {
    // Production version as of this commit. Bump in lockstep with
    // `pub(crate) const SCHEMA_VERSION` in `src/state/schema.rs` *and*
    // update the DDL in `create_state_db` above to match the new
    // shape. The fresh-DB DDL emitted by a real binary run can be
    // dumped via `sqlite3 <db> '.schema'` for reference.
    const PRODUCTION_SCHEMA_VERSION: i32 = 22;
    assert_eq!(
        HELPER_SCHEMA_VERSION, PRODUCTION_SCHEMA_VERSION,
        "behavioral.rs::create_state_db schema is out of sync with \
         src/state/schema.rs::SCHEMA_VERSION (helper={HELPER_SCHEMA_VERSION}, \
         production={PRODUCTION_SCHEMA_VERSION}). Bump both, plus the DDL \
         block in create_state_db, then update this test."
    );
}

#[cfg(debug_assertions)]
#[test]
fn sync_report_json_offline_binary_boundary_writes_current_schema() {
    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("photos");
    let report_path = dir.path().join("sync_report.json");
    let config_path = dir.path().join("config.toml");
    let username = "offline-report@example.com";
    std::fs::write(
        &config_path,
        format!(
            "\
[auth]
username = {username}

[download]
directory = {download_dir}
folder_structure = \"%Y/%m/%d\"
threads = 3

[filters]
media = [\"photos\", \"videos\"]

[report]
json = {report_path}

[ui]
friendly = false
",
            username = common::toml_string(username),
            download_dir = common::toml_string(&download_dir.to_string_lossy()),
            report_path = common::toml_string(&report_path.to_string_lossy()),
        ),
    )
    .unwrap();

    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kei"))
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env_remove("KEI_CONFIG")
        .env_remove("KEI_DATA_DIR")
        .env_remove("KEI_DOWNLOAD_DIR")
        .env_remove("KEI_LOG_LEVEL")
        .env_remove("KEI_NO_AUTO_CONFIG")
        .env("KEI_DATA_DIR", &data_dir)
        .env("KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS", "1")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .output()
        .expect("run kei");
    assert!(
        output.status.success(),
        "kei sync failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let body = std::fs::read_to_string(&report_path).expect("sync_report.json");
    let json: serde_json::Value = serde_json::from_str(&body).expect("valid report JSON");
    assert_eq!(json["version"], "3", "schema version");
    assert!(json["kei_version"].as_str().is_some_and(|v| !v.is_empty()));
    assert!(
        json["timestamp"]
            .as_str()
            .is_some_and(|ts| chrono::DateTime::parse_from_rfc3339(ts).is_ok()),
        "timestamp must be RFC3339: {}",
        json["timestamp"]
    );
    assert_eq!(json["status"], "success");
    assert_eq!(json["options"]["username"], username);
    assert_eq!(
        json["options"]["download_dir"],
        download_dir.display().to_string()
    );
    assert_eq!(json["options"]["folder_structure"], "%Y/%m/%d");
    assert_eq!(json["options"]["threads"], 3);
    assert_eq!(
        json["options"]["media"],
        serde_json::json!(["photos", "videos"])
    );
    assert_eq!(json["options"]["dry_run"], false);
    assert_eq!(json["stats"]["assets_seen"], 3);
    assert_eq!(json["stats"]["downloaded"], 2);
    assert_eq!(json["stats"]["skipped"]["by_state"], 1);
    assert_eq!(json["stats"]["bytes_downloaded"], 4096);
    assert_eq!(json["stats"]["disk_bytes_written"], 4096);
    assert_eq!(json["stats"]["photos_downloaded"], 1);
    assert_eq!(json["stats"]["videos_downloaded"], 1);
    assert!(
        json.get("failed_assets").is_none(),
        "clean success report should omit failed_assets: {json}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// config show: resolved config output
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn config_show_outputs_valid_toml() {
    let out = clean_cmd()
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should be parseable TOML
    assert!(
        toml::from_str::<toml::Value>(&stdout).is_ok(),
        "config show should produce valid TOML, got:\n{stdout}"
    );
}
#[test]
fn config_show_contains_username() {
    clean_cmd()
        .env("ICLOUD_USERNAME", "myuser@icloud.com")
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("myuser@icloud.com"));
}
#[test]
fn config_show_reflects_directory_from_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/my/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "cli@example.com")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("/my/photos"));
}
#[test]
fn config_show_rejects_toml_with_password() {
    // `[auth] password` is banned; `config show` should fail loudly with
    // the migration message rather than silently dropping the field.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword = \"super_secret_value\"\n",
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("super_secret_value"),
        "password must not appear in stdout even on rejection, got:\n{stdout}"
    );
}
#[test]
fn config_show_reflects_toml_values() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[auth]
username = "toml@example.com"

[download]
directory = "/toml/photos"
threads = 4
"#,
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("toml@example.com"), "stdout: {stdout}");
    assert!(stdout.contains("/toml/photos"), "stdout: {stdout}");
    assert!(
        stdout.contains("4"),
        "threads should be 4, stdout: {stdout}"
    );
}

#[test]
fn config_show_preserves_top_level_toml_values() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let data_dir = dir.path().join("data");
    let data_dir_string = data_dir.to_string_lossy();
    std::fs::write(
        &config_path,
        format!(
            r#"
data_dir = {}
log_level = "debug"

[auth]
username = "toml@example.com"

[download]
directory = "/toml/photos"
"#,
            common::toml_string(&data_dir_string)
        ),
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: toml::Value = toml::from_str(&stdout).unwrap();
    assert_eq!(
        parsed.get("data_dir").and_then(toml::Value::as_str),
        Some(data_dir_string.as_ref()),
        "stdout: {stdout}"
    );
    assert_eq!(
        parsed.get("log_level").and_then(toml::Value::as_str),
        Some("debug"),
        "stdout: {stdout}"
    );
}

#[test]
fn config_show_omits_derived_top_level_values_when_unset() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let download_dir = dir.path().join("photos");
    std::fs::write(
        &config_path,
        format!(
            r#"
[auth]
username = "toml@example.com"

[download]
directory = {}
"#,
            common::toml_string(&download_dir.to_string_lossy())
        ),
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: toml::Value = toml::from_str(&stdout).unwrap();
    assert!(
        parsed.get("data_dir").is_none(),
        "config show must not serialize derived data_dir when TOML omitted it; stdout: {stdout}"
    );
    assert!(
        parsed.get("log_level").is_none(),
        "config show must not serialize default log_level when TOML omitted it; stdout: {stdout}"
    );
}

#[test]
fn config_show_emits_unfiled_false_when_explicit() {
    // The cli.rs help-shadow test for --unfiled only verifies clap parses;
    // it does not pin the resolved value all the way through Config::build
    // → Selection → to_toml. A clap-default flip or selector regression
    // that swallowed the explicit `false` would land green
    // there. `to_toml()` only emits `unfiled` when the resolved value
    // differs from the `true` default, so an explicit `false` is the case
    // we can observe directly in `kei config show` output.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[auth]
username = "x@x.com"

[filters]
unfiled = false
"#,
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: toml::Value = toml::from_str(&stdout).expect("config show must emit valid TOML");
    let unfiled = parsed
        .get("filters")
        .and_then(|f| f.get("unfiled"))
        .and_then(toml::Value::as_bool);
    assert_eq!(
        unfiled,
        Some(false),
        "config show must round-trip explicit `unfiled = false`; got:\n{stdout}"
    );
}
#[test]
fn config_show_cli_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[auth]
username = "toml@example.com"
"#,
    )
    .unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "cli@example.com")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("cli@example.com"));
}

// ═══════════════════════════════════════════════════════════════════════
// Error messages: missing required args
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn login_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["login"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}
#[test]
fn list_albums_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["list", "albums"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}
#[test]
fn password_set_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["password", "set"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn password_set_headless_accepts_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let password_file = dir.path().join("icloud-password");
    let data_dir = dir.path().join("data");
    let secret = "file-source-secret";
    std::fs::write(&password_file, format!("{secret}\n")).unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", &data_dir)
        .args([
            "password",
            "--password-file",
            password_file.to_str().unwrap(),
            "set",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Password stored in"))
        .stdout(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains(secret).not());
}

#[test]
fn password_set_headless_without_source_fails_before_prompt() {
    let dir = tempfile::tempdir().unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "set"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("stdin is not a terminal"))
        .stderr(predicate::str::contains(
            "kei password --password-file <PATH> set",
        ))
        .stderr(predicate::str::contains(
            "kei password --password-command <COMMAND> set",
        ));
}

fn write_fake_two_factor_config(dir: &std::path::Path, username: &str) -> std::path::PathBuf {
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

fn assert_foreground_two_factor_failure(command: &str) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_fake_two_factor_config(dir.path(), "test@example.com");

    clean_cmd()
        .env("KEI_DATA_DIR", dir.path().join("data"))
        .env("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS", "1")
        .args([command, "--config", config_path.to_str().unwrap()])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("kei login get-code"))
        .stderr(predicate::str::contains("kei login submit-code <CODE>"));
}

#[test]
fn login_two_factor_required_exits_with_auth_code() {
    assert_foreground_two_factor_failure("login");
}

#[test]
fn one_shot_sync_two_factor_required_exits_with_auth_code() {
    assert_foreground_two_factor_failure("sync");
}

#[test]
fn service_two_factor_required_keeps_waiting_for_submitted_code() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let config_path = write_fake_two_factor_config(dir.path(), username);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join(format!("{}.session", sanitize_username(username))),
        "{}\n",
    )
    .unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kei"))
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env_remove("KEI_CONFIG")
        .env("KEI_DATA_DIR", &data_dir)
        .env("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS", "1")
        .args(["service", "run", "--config", config_path.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_secs(1));
    if let Some(status) = child.try_wait().unwrap() {
        let output = child.wait_with_output().unwrap();
        panic!(
            "service exited instead of waiting for 2FA: {status}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Waiting for 2FA code submission"),
        "service did not enter durable 2FA wait:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}
#[test]
fn password_clear_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["password", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}
#[test]
fn password_backend_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["password", "backend"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

/// `password backend` against a fresh cookie dir prints the credential
/// backend name. The backend choice is platform-dependent (OS keyring
/// when available, encrypted file fallback), so we only assert the
/// output is non-empty and exit is clean.
#[test]
fn password_backend_prints_backend_name() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "backend"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "password backend must print the backend name, got empty stdout"
    );
}

/// `password clear` against a cookie dir with no stored credential
/// surfaces a clear error rather than silently succeeding. Locks in the
/// "not idempotent" contract so nobody changes the behaviour without
/// noticing the operator-visible impact.
#[test]
fn password_clear_on_empty_store_errors() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No stored credential"));
}
#[test]
fn sync_requires_username() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn service_run_uses_sync_worker_path_and_requires_username() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args([
            "service",
            "run",
            "--config",
            config_path.to_str().unwrap(),
            "--no-progress-bar",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("starting kei service worker"))
        .stderr(predicate::str::contains(
            "service mode: applied default watch interval",
        ))
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn sync_requires_directory() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["sync"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set [download].directory"));
}
#[test]
fn import_existing_requires_directory() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["import-existing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set [download].directory"));
}
#[test]
fn import_existing_rejects_nonexistent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[download]\ndirectory = \"/does/not/exist/anywhere\"\n",
    )
    .unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["--config", config.to_str().unwrap(), "import-existing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Could not read download directory /does/not/exist/anywhere",
        ));
}

// ═══════════════════════════════════════════════════════════════════════
// No-DB paths: commands that hit the DB but none exists
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn status_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}
#[test]
fn verify_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}
#[test]
fn reset_state_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "state", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}
#[test]
fn reset_sync_token_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "sync-token"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

// ═══════════════════════════════════════════════════════════════════════
// password backend: shows backend name without auth
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn password_backend_shows_a_backend_name() {
    let dir = tempfile::tempdir().unwrap();
    // Output is one of: "encrypted-file", "keyring", or "none"
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "backend"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("encrypted-file")
                .or(predicate::str::contains("keyring"))
                .or(predicate::str::contains("none")),
        );
}
#[test]
fn password_clear_without_stored_credential_errors() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No stored credential"));
}
#[test]
fn password_backend_with_empty_data_dir_reports_none() {
    // Fresh data dir with no keyring entry (keyring may still report for the
    // username if it was set outside this test), so we use an unlikely
    // username to minimize false positives.
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "unlikely-empty-store@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "backend"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("none") || stdout.contains("keyring"),
        "expected 'none' (or 'keyring' if system keyring returns stale entry), got: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Env var behavior: KEI_* vars actually resolve
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn kei_data_dir_env_resolves_in_status() {
    // KEI_DATA_DIR env var should be used for the data directory
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path().to_str().unwrap())
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}
#[test]
fn icloud_username_env_resolves_in_config_show() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@icloud.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("env@icloud.com"));
}
#[test]
fn icloud_username_env_resolves_without_cli_flag() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@icloud.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("env@icloud.com"));
}

// ═══════════════════════════════════════════════════════════════════════
// First-run auto-config
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn first_run_auto_config_creates_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    // sync will fail at auth, but auto-config fires before auth.
    // Use --config pointing at non-existent file in existing directory.
    clean_cmd()
        .env("ICLOUD_USERNAME", "auto@example.com")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure(); // fails at auth, but config file should have been created

    assert!(
        config_path.exists(),
        "auto-config should create config file at {}",
        config_path.display()
    );
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("auto@example.com"),
        "auto-config should contain username, got:\n{content}"
    );
}
#[test]
fn first_run_auto_config_does_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# existing config\n").unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success();

    let content = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(
        content, "# existing config\n",
        "auto-config must not overwrite existing file"
    );
}

#[test]
fn legacy_icloudpd_rs_paths_are_not_auto_copied_on_startup() {
    let home = tempfile::tempdir().unwrap();
    let old_config = home.path().join(".config/icloudpd-rs/config.toml");
    let old_cookie_dir = home.path().join(".icloudpd-rs");
    let config_path = home.path().join("current-config.toml");

    std::fs::create_dir_all(old_config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&old_cookie_dir).unwrap();
    std::fs::write(&old_config, "[auth]\nusername = \"legacy@example.com\"\n").unwrap();
    std::fs::write(old_cookie_dir.join("session.json"), "{}").unwrap();
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"current@example.com\"\n[download]\ndirectory = \"/tmp/codex/kei/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .env("HOME", home.path())
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("current@example.com"))
        .stdout(predicate::str::contains("legacy@example.com").not());

    assert!(
        !home.path().join(".config/kei/config.toml").exists(),
        "startup must not copy legacy icloudpd-rs config into kei paths"
    );
    assert!(
        !home
            .path()
            .join(".config/kei/cookies/session.json")
            .exists(),
        "startup must not copy legacy icloudpd-rs session files into kei paths"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Config validation: malformed/invalid TOML
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn config_malformed_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "this is not valid toml {{{").unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("parse").or(predicate::str::contains("expected")));
}
#[test]
fn config_unknown_toml_field() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nbogus = true\n").unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown field"));
}
#[test]
fn config_empty_username_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"\"\n\n[download]\ndirectory = \"/photos\"\n",
    )
    .unwrap();

    // config show calls Config::build which checks for empty username
    // only when a username source is present in TOML. Since TOML sets
    // username = "", the build path validates it.
    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cannot be empty"));
}
#[test]
fn config_toml_password_field_rejected() {
    // `[auth] password` is not accepted, empty or otherwise.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword = \"\"\n\n[download]\ndirectory = \"/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("`[auth].password`"))
        .stderr(predicate::str::contains("kei password set"));
}

// On Windows, `--password-command` / `[auth] password_command` is rejected
// at config::build before the "pick one" check runs, so the assertion on
// "pick one" doesn't hold. Unix covers the path this test is guarding.
#[cfg(unix)]
#[test]
fn config_multiple_password_sources_in_toml() {
    // Both `password_file` and `password_command` set in the same TOML is
    // still rejected with "pick one".
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword_file = \"/tmp/pw\"\npassword_command = \"echo hi\"\n\n[download]\ndirectory = \"/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Pick one"));
}
#[test]
fn config_strftime_folder_structure_accepted() {
    // Full strftime support: %B (month name), %q, etc. are no longer rejected.
    // The process may fail auth, but it should NOT fail config validation.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y/%B/%d\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        // Should get past config validation (no "unrecognized format token" error).
        // Fails on auth, not on config.
        .stderr(predicate::str::contains("unrecognized format token").not());
}
#[test]
fn config_valid_folder_structure_ymd() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y/%m/%d\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("%Y/%m/%d"));
}
#[test]
fn config_valid_folder_structure_ym() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y-%m\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("%Y-%m"));
}
#[test]
fn config_valid_folder_structure_ymdh() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y/%m/%d/%H\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("%Y/%m/%d/%H"));
}
#[test]
fn config_folder_structure_none() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"none\"\n",
    )
    .unwrap();

    // "none" is a special value that should be accepted (no error)
    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("none"));
}
#[test]
fn config_watch_interval_below_60_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n\n[watch]\ninterval = 30\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "watch interval must be in 60..=86400 seconds, got 30",
        ));
}
#[test]
fn config_retry_delay_toml_key_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n\n[download.retry]\ndelay = 5\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown field `delay`"));
}
#[test]
fn config_threads_num_toml_key_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nthreads_num = 4\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown field `threads_num`"));
}

// ═══════════════════════════════════════════════════════════════════════
// Config resolution: TOML / CLI / env merge via config show
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn config_resolution_toml_only() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"tomluser@example.com\"\n\n[download]\ndirectory = \"/toml/dir\"\n",
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tomluser@example.com"), "stdout: {stdout}");
    assert!(stdout.contains("/toml/dir"), "stdout: {stdout}");
}
#[test]
fn config_resolution_toml_username_used() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"toml@example.com\"\n").unwrap();

    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("toml@example.com"));
}
#[test]
fn config_resolution_env_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"toml@example.com\"\n").unwrap();

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "env@example.com")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Env should override TOML
    assert!(
        stdout.contains("env@example.com"),
        "env should override TOML, stdout: {stdout}"
    );
}
#[test]
fn config_resolution_env_username_used_without_toml() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("env@example.com"));
}
#[test]
fn config_resolution_default_values() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Default threads = 10 using the canonical TOML spelling.
    assert!(
        stdout.contains("threads = 10"),
        "default threads should be 10, stdout: {stdout}"
    );
    assert!(
        !stdout.contains("threads_num"),
        "serialized config should use the new `threads` key, not `threads_num`: {stdout}"
    );
    // Default folder_structure = "%Y/%m/%d"
    assert!(
        stdout.contains("%Y/%m/%d"),
        "default folder_structure should be %Y/%m/%d, stdout: {stdout}"
    );
}
#[test]
fn config_show_does_not_read_password_file_contents() {
    // `config show` may echo the `password_file` path back to the user, but
    // it must never open the file and leak its contents. This guards against
    // accidental eager resolution in future refactors of the config pipeline.
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw");
    std::fs::write(&pw_file, "my_secret_pw\n").unwrap();
    let config_path = dir.path().join("config.toml");
    // Use TOML literal strings (single quotes) for the path so Windows
    // paths like `C:\Users\...` don't get interpreted as `\U...` escapes.
    std::fs::write(
        &config_path,
        format!(
            "[auth]\nusername = \"x@x.com\"\npassword_file = '{}'\n",
            pw_file.display()
        ),
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("my_secret_pw"),
        "config show must not dereference password_file, stdout: {stdout}"
    );
    // The path itself is expected to appear (it's a config value, not a secret).
    assert!(
        stdout.contains(&pw_file.display().to_string()),
        "password_file path should be echoed back, stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Auto-config behavior
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn auto_config_suppressed_by_env() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    // KEI_NO_AUTO_CONFIG=1 should prevent creation of the config file
    clean_cmd()
        .env("KEI_NO_AUTO_CONFIG", "1")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure(); // fails at auth

    assert!(
        !config_path.exists(),
        "KEI_NO_AUTO_CONFIG=1 should suppress config file creation"
    );
}
#[test]
#[cfg(unix)]
fn auto_config_has_0600_perms() {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure(); // fails at auth

    assert!(config_path.exists(), "config file should be created");
    let mode = std::fs::metadata(&config_path).unwrap().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "auto-config file should have 0600 permissions, got {:o}",
        mode
    );
}

// ═══════════════════════════════════════════════════════════════════════
// State DB pre-seeded tests: status
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn status_shows_counts() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some("/p/photo1.jpg"),
        None,
        None,
    );
    insert_asset(
        &conn,
        "a2",
        "downloaded",
        "photo2.jpg",
        Some("/p/photo2.jpg"),
        None,
        None,
    );
    insert_asset(
        &conn,
        "a3",
        "downloaded",
        "photo3.jpg",
        Some("/p/photo3.jpg"),
        None,
        None,
    );
    insert_asset(
        &conn,
        "a4",
        "failed",
        "photo4.jpg",
        None,
        Some("timeout"),
        None,
    );
    insert_asset(&conn, "a5", "pending", "photo5.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Total:      5"), "stdout: {stdout}");
    assert!(stdout.contains("Downloaded: 3"), "stdout: {stdout}");
    assert!(stdout.contains("Failed:     1"), "stdout: {stdout}");
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
}

#[test]
fn status_shows_safe_backup_summary_after_clean_sync() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some("/p/photo1.jpg"),
        None,
        None,
    );
    conn.execute(
        "INSERT INTO asset_metadata_capture_revisions \
            (library, asset_id, revision, updated_at) \
         VALUES ('PrimarySync', 'a1', 1, ?1)",
        [1_700_000_000_i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, assets_seen, assets_downloaded,
            assets_failed, interrupted, enumeration_errors
         ) VALUES (?1, ?2, 'complete', 1, 1, 0, 0, 0)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "Backup status: safe - last sync completed and no pending or failed assets are recorded"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_treats_policy_excluded_assets_as_safe_and_visible() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "excluded",
        "policy_excluded",
        "old.mov",
        None,
        None,
        None,
    );
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, assets_seen, assets_downloaded,
            assets_failed, interrupted, enumeration_errors
         ) VALUES (?1, ?2, 'complete', 1, 0, 0, 0, 0)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Pending:    0"), "stdout: {stdout}");
    assert!(stdout.contains("Policy excluded: 1"), "stdout: {stdout}");
    assert!(
        stdout.contains(
            "Backup status: safe - last sync completed and no pending or failed assets are recorded"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_shows_unsafe_backup_summary_with_last_run_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "failed",
        "photo1.jpg",
        None,
        Some("timeout"),
        None,
    );
    insert_asset(&conn, "a2", "pending", "photo2.jpg", None, None, None);
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, assets_seen, assets_downloaded,
            assets_failed, interrupted, enumeration_errors
         ) VALUES (?1, ?2, 'complete', 2, 0, 1, 0, 2)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(
            "Backup status: unsafe - 1 asset failed in the last sync; 2 enumeration errors occurred in the last sync; 1 failed asset remains; 1 pending asset remains"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_shows_api_total_and_inventory_warning() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some("/p/photo1.jpg"),
        None,
        None,
    );
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, api_total_at_start,
            inventory_drop_detected, inventory_drop_previous_total,
            inventory_drop_current_total, inventory_drop_library
         ) VALUES (?1, ?2, 'complete', ?3, 1, ?4, ?5, ?6)",
        rusqlite::params![
            1_700_000_000_i64,
            1_700_000_010_i64,
            95_i64,
            100_i64,
            95_i64,
            "PrimarySync"
        ],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Last API total at start: 95"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(
            "Inventory warning: PrimarySync dropped 5 assets since the previous comparable full run (100 -> 95)"
        ),
        "stdout: {stdout}"
    );
}

#[test]
fn status_shows_partial_api_total() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    conn.execute(
        "INSERT INTO sync_runs (
            started_at, completed_at, status, api_total_at_start,
            api_total_at_start_partial
         ) VALUES (?1, ?2, 'complete', ?3, 1)",
        rusqlite::params![1_700_000_000_i64, 1_700_000_010_i64, 95_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Last API total at start: partial, 95"),
        "stdout: {stdout}"
    );
}

#[test]
fn status_prefers_running_sync_over_newer_completed_row() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    conn.execute(
        "INSERT INTO sync_runs (started_at, status) VALUES (?1, 'running')",
        rusqlite::params![1_700_000_000_i64],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sync_runs (started_at, completed_at, status) \
         VALUES (?1, ?2, 'complete')",
        rusqlite::params![1_700_000_030_i64, 1_700_000_040_i64],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Sync in progress:   started 2023-11-14 22:13:20 UTC"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("Last sync completed:"),
        "active status must not imply the whole sync completed: {stdout}"
    );
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
}

#[test]
fn status_shows_full_enumeration_progress_marker() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        rusqlite::params!["enum_in_progress:PrimarySync", "1700000000"],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Full enumeration in progress: PrimarySync"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
}

#[test]
fn status_failed_shows_error_messages() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(
        &conn,
        "a1",
        "failed",
        "photo1.jpg",
        None,
        Some("connection reset"),
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--failed"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("connection reset"), "stdout: {stdout}");
}

// ═══════════════════════════════════════════════════════════════════════
// State DB pre-seeded tests: verify
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn verify_all_files_present() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let file_path = dir.path().join("photo1.jpg");
    std::fs::write(&file_path, "photo data").unwrap();

    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some(file_path.to_str().unwrap()),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Verified:  1"), "stdout: {stdout}");
    assert!(stdout.contains("Missing:   0"), "stdout: {stdout}");
}
#[test]
fn verify_detects_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let file_path = dir.path().join("gone.jpg");
    // Don't create the file -- it should be detected as missing

    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "gone.jpg",
        Some(file_path.to_str().unwrap()),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MISSING"), "stdout: {stdout}");
}
#[test]
fn verify_checksums_match() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let file_content = b"known content for checksum";
    let file_path = dir.path().join("checked.jpg");
    std::fs::write(&file_path, file_content).unwrap();

    // Pre-computed SHA-256 of b"known content for checksum"
    let checksum = "bce5852bddb57da7abc94da047da866544b87abb1b3c36612ac0e56f5d5bd611";

    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "checked.jpg",
        Some(file_path.to_str().unwrap()),
        None,
        Some(checksum),
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify", "--checksums"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Verified:  1"), "stdout: {stdout}");
}
#[test]
fn verify_checksums_mismatch() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let file_path = dir.path().join("bad.jpg");
    {
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(b"actual content").unwrap();
    }

    // Use a wrong checksum
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "bad.jpg",
        Some(file_path.to_str().unwrap()),
        None,
        Some("0000000000000000000000000000000000000000000000000000000000000000"),
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify", "--checksums"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CORRUPTED"), "stdout: {stdout}");
}

/// CG-1 (2026-05-03 test review): if a future refactor of the
/// `CORRUPTED:` line in `run_verify` drops the asset id from the
/// printed output, operators can see "1 corrupted" without any way
/// to find which asset. This test pins the contract that the asset
/// id reaches stdout for every corrupted entry. Sibling to
/// `verify_checksums_mismatch` so the existing test stays focused
/// on exit-code + summary text and this one stays focused on the
/// per-asset trace.
#[test]
fn verify_checksums_mismatch_emits_asset_id_in_output() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let file_path = dir.path().join("bad.jpg");
    {
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(b"actual content").unwrap();
    }

    let asset_id = "ASSET_FOR_CG1_VERIFY";
    insert_asset(
        &conn,
        asset_id,
        "downloaded",
        "bad.jpg",
        Some(file_path.to_str().unwrap()),
        None,
        Some("0000000000000000000000000000000000000000000000000000000000000000"),
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify", "--checksums"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CORRUPTED"),
        "expected CORRUPTED line, stdout: {stdout}"
    );
    assert!(
        stdout.contains(asset_id),
        "expected asset id {asset_id} in CORRUPTED line, stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// State DB pre-seeded tests: reset
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn reset_state_deletes_db() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo.jpg",
        Some("/p/photo.jpg"),
        None,
        None,
    );
    drop(conn);

    let db_path = dir
        .path()
        .join(format!("{}.db", sanitize_username(username)));
    assert!(db_path.exists(), "DB should exist before reset");

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "state", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(!db_path.exists(), "DB file should be deleted after reset");
    assert!(
        stdout.contains("deleted"),
        "should print 'deleted', stdout: {stdout}"
    );
}
#[test]
fn reset_sync_token_clears_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('sync_token:PrimarySync', 'tok-abc')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('db_sync_token', 'db-tok-123')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO scoped_db_sync_tokens \
            (provider, account, shape_version, scope_hash, selected_zones_json, scope_json, token, created_at, updated_at) \
         VALUES ('icloud', 'test@example.com', 1, 'scope-a', '[\"PrimarySync\"]', '{\"scope\":\"a\"}', 'scoped-tok-123', 1, 1)",
        [],
    )
    .unwrap();
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "sync-token", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Cleared sync tokens"), "stdout: {stdout}");

    // Verify tokens are actually gone
    let db_path = dir
        .path()
        .join(format!("{}.db", sanitize_username(username)));
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let zone_token: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'sync_token:PrimarySync'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    // Zone tokens are deleted by delete_metadata_by_prefix
    assert!(zone_token.is_none(), "zone token should be deleted");
    let db_token: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'db_sync_token'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // db_sync_token is set to empty string, not deleted
    assert_eq!(db_token, "", "db_sync_token should be cleared to empty");
    let scoped_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM scoped_db_sync_tokens", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(scoped_count, 0, "scoped db tokens should be deleted");
}
#[test]
fn reset_state_without_yes_on_non_tty() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo.jpg",
        Some("/p/photo.jpg"),
        None,
        None,
    );
    drop(conn);

    let db_path = dir
        .path()
        .join(format!("{}.db", sanitize_username(username)));

    // Without --yes on a non-TTY, stdin.read_line returns empty/EOF -> "Cancelled"
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "state"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Cancelled"),
        "non-interactive should print 'Cancelled', stdout: {stdout}"
    );
    assert!(db_path.exists(), "DB should NOT be deleted without --yes");
}
#[test]
fn reset_sync_token_without_yes_on_non_tty_errors() {
    // `kei reset sync-token` ships a confirmation guard. Under non-TTY use
    // (CI, scripts, docker exec without -t), running without `--yes` errors
    // out instead of silently re-enumerating every asset on the next sync.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('sync_token:PrimarySync', 'tok-abc')",
        [],
    )
    .unwrap();
    drop(conn);

    let db_path = dir
        .path()
        .join(format!("{}.db", sanitize_username(username)));

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "sync-token"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--yes"),
        "non-tty error must mention --yes; stderr: {stderr}"
    );

    // Tokens must remain untouched.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let zone_token: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'sync_token:PrimarySync'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert_eq!(
        zone_token.as_deref(),
        Some("tok-abc"),
        "zone token must not be cleared without --yes"
    );
}
#[test]
fn reset_sync_token_with_yes_clears_under_non_tty() {
    // Mirror of the test above with `--yes`: the same non-TTY context now
    // succeeds and clears tokens. Confirms the safety guard only fires on
    // the missing-flag path, not in legitimate scripted use.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('sync_token:PrimarySync', 'tok-abc')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('db_sync_token', 'db-tok-123')",
        [],
    )
    .unwrap();
    drop(conn);

    let db_path = dir
        .path()
        .join(format!("{}.db", sanitize_username(username)));

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "sync-token", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Cleared sync tokens"),
        "stdout should report Cleared sync tokens: {stdout}"
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let zone_token: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'sync_token:PrimarySync'",
            [],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    assert!(
        zone_token.is_none(),
        "zone token must be cleared with --yes"
    );
}

#[test]
fn reset_session_no_session() {
    // With no session files at all, `reset session` reports and exits 0
    // before the confirmation/`--yes` guard fires, mirroring the no-DB
    // early return of `reset state` / `reset sync-token`.
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "session"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No local session found"));
}

#[test]
fn reset_session_removes_session_files_and_keeps_bystanders() {
    // assert_cmd pipes stdin, so this also covers `--yes` under non-TTY:
    // the guard only fires on the missing-flag path, not in scripted use.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let slug = sanitize_username(username);

    let jar = dir.path().join(&slug);
    let session = dir.path().join(format!("{slug}.session"));
    let cache = dir.path().join(format!("{slug}.cache"));
    let credential = dir.path().join(format!("{slug}.credential"));
    let db = dir.path().join(format!("{slug}.db"));
    for (path, contents) in [
        (&jar, "jar"),
        (&session, "session"),
        (&cache, "cache"),
        (&credential, "cred"),
        (&db, "db"),
    ] {
        std::fs::write(path, contents).unwrap();
    }

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "session", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Removed"),
        "stdout should report removed files: {stdout}"
    );
    assert!(
        stdout.contains("kei login"),
        "stdout should point at `kei login`: {stdout}"
    );

    assert!(!jar.exists(), "cookie jar must be removed");
    assert!(!session.exists(), "persisted session must be removed");
    assert!(!cache.exists(), "response cache must be removed");
    assert!(credential.exists(), "password store must survive the reset");
    assert!(db.exists(), "state database must survive the reset");
}

#[test]
fn reset_session_without_yes_on_non_tty_errors() {
    // `kei reset session` ships the same non-interactive guard as
    // `reset sync-token`. Under non-TTY use (CI, scripts, docker exec
    // without -t), running without `--yes` errors out instead of silently
    // doing nothing — or worse, discarding trust tokens by accident.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let slug = sanitize_username(username);

    let jar = dir.path().join(&slug);
    let session = dir.path().join(format!("{slug}.session"));
    std::fs::write(&jar, "jar").unwrap();
    std::fs::write(&session, "session").unwrap();

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "session"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--yes"),
        "non-tty error must mention --yes; stderr: {stderr}"
    );

    assert!(jar.exists(), "cookie jar must not be removed without --yes");
    assert!(
        session.exists(),
        "persisted session must not be removed without --yes"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Password source behavior
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn password_file_strips_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw.txt");
    std::fs::write(&pw_file, "secret\n").unwrap();

    // Should fail at auth (network), not at password retrieval.
    // The error message should NOT contain "empty" or "No password available".
    let out = clean_cmd()
        .args(["login", "--password-file", pw_file.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("No password available"),
        "password file with newline should work, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("empty"),
        "password should not be empty, stderr: {stderr}"
    );
}
#[test]
fn password_file_empty() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw.txt");
    std::fs::write(&pw_file, "").unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args(["login", "--password-file", pw_file.to_str().unwrap()])
        .assert()
        .code(3)
        .stderr(
            predicate::str::contains("No password available").or(predicate::str::contains("empty")),
        );
}
#[test]
fn password_file_newline_only() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw.txt");
    std::fs::write(&pw_file, "\n").unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args(["login", "--password-file", pw_file.to_str().unwrap()])
        .assert()
        .code(3)
        .stderr(
            predicate::str::contains("No password available").or(predicate::str::contains("empty")),
        );
}

// `--password-command` is rejected at startup on Windows (see Flag 8 in the
// audit); the success path this test is asserting only applies on unix.
#[cfg(unix)]
#[test]
fn password_command_success() {
    let dir = tempfile::tempdir().unwrap();

    // The password command succeeds and returns "cmdpw".
    // Auth will fail at network, not at password retrieval.
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["login", "--password-command", "echo cmdpw"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("No password available"),
        "password command should provide password, stderr: {stderr}"
    );
}
#[test]
fn password_command_failure() {
    let dir = tempfile::tempdir().unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["login", "--password-command", "false"])
        .assert()
        .code(3)
        .stderr(
            predicate::str::contains("No password was available")
                .or(predicate::str::contains("exited with status")),
        );
}

// ═══════════════════════════════════════════════════════════════════════
// Exit codes
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn exit_2_for_clap_errors() {
    // Removed durable flags are clap errors.
    clean_cmd()
        .args(["--username", "", "config", "show"])
        .assert()
        .code(2);
}
#[test]
fn exit_1_for_missing_directory_on_sync() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["sync"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set [download].directory"));
}

#[cfg(unix)]
#[test]
fn sync_unwritable_download_directory_errors_before_auth() {
    use std::os::unix::fs::PermissionsExt;

    // Root can write through 0o555 directories, so this probe is not
    // meaningful under root-owned CI containers.
    // SAFETY: libc::geteuid() is a stateless POSIX call with no
    // preconditions and no memory-safety implications.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let download_dir = dir.path().join("photos");
    let config_path = dir.path().join("config.toml");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    std::fs::set_permissions(&download_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    write_sync_config(&config_path, download_dir.to_str().unwrap());

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", &data_dir)
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--password",
            "not-used-before-auth",
            "--no-progress-bar",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "Cannot write to download directory",
        ));

    std::fs::set_permissions(&download_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        std::fs::read_dir(&data_dir).unwrap().next().is_none(),
        "unwritable download dir must fail before auth/session state is written"
    );
}

#[test]
fn exit_1_for_missing_username_on_sync() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set your iCloud username"));
}

// ═══════════════════════════════════════════════════════════════════════
// Log level behavior
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn log_level_default_info() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    // sync with username + directory will fail at auth. Check stderr for INFO.
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Default level is INFO; "Starting kei" should appear but DEBUG should not.
    assert!(
        stderr.contains("Starting kei"),
        "default log level should show INFO-level messages like 'Starting kei', stderr: {stderr}"
    );
    let has_debug = stderr.lines().any(|line| {
        let lower = line.to_lowercase();
        lower.contains(" debug ") && !line.starts_with("Error:")
    });
    assert!(
        !has_debug,
        "default log level should suppress DEBUG-level messages, stderr: {stderr}"
    );
}
#[test]
fn log_level_debug() {
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = dir.path().join("photos");
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, dl_dir.to_str().unwrap());
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args([
            "--log-level",
            "debug",
            "sync",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("DEBUG") || stderr.contains("debug"),
        "debug log level should produce DEBUG entries, stderr: {stderr}"
    );
}
#[test]
fn log_level_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    let out = clean_cmd()
        .args([
            "--log-level",
            "error",
            "sync",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // With log level error, no info/debug lines should appear.
    // The tracing subscriber uses the format "LEVEL kei::" for structured logs.
    // "Error:" comes from main's eprintln, not from tracing, so it's fine.
    let has_info = stderr.lines().any(|line| {
        let lower = line.to_lowercase();
        (lower.contains(" info ") || lower.contains(" debug ")) && !line.starts_with("Error:")
    });
    assert!(
        !has_info,
        "error log level should suppress info/debug lines, stderr: {stderr}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Help and version
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn help_flag_exits_zero() {
    clean_cmd().arg("--help").assert().success();
}
#[test]
fn version_flag_exits_zero() {
    clean_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("kei"));
}
#[test]
fn sync_help_exits_zero() {
    clean_cmd().args(["sync", "--help"]).assert().success();
}
#[test]
fn config_show_help_exits_zero() {
    clean_cmd()
        .args(["config", "show", "--help"])
        .assert()
        .success();
}

#[test]
fn config_setup_requires_interactive_terminal() {
    clean_cmd()
        .args(["config", "setup"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "The setup wizard needs an interactive terminal",
        ));
}

// ═══════════════════════════════════════════════════════════════════════
// Subcommand parsing: unknown subcommand
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn unknown_subcommand_fails() {
    clean_cmd().arg("nonexistent-command").assert().code(2);
}

// ═══════════════════════════════════════════════════════════════════════
// verify with empty DB (no downloaded assets)
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn verify_empty_db() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let _conn = create_state_db(dir.path(), username);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Verifying 0 downloaded assets"),
        "stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// status with DB but no sync runs
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn status_with_db_no_sync_runs() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Total:      1"), "stdout: {stdout}");
    assert!(stdout.contains("Pending:    1"), "stdout: {stdout}");
    // No "Last sync" lines since no sync_runs
    assert!(
        !stdout.contains("Last sync started"),
        "no sync runs, so no 'Last sync started', stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// verify with --checksums but no local_checksum stored
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn verify_checksums_no_stored_checksum_still_passes() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let file_path = dir.path().join("photo.jpg");
    std::fs::write(&file_path, "some content").unwrap();

    // No local_checksum stored -- verify --checksums should still pass
    // (skips verification when no checksum is stored)
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo.jpg",
        Some(file_path.to_str().unwrap()),
        None,
        None, // no local_checksum
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify", "--checksums"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Verified:  1"), "stdout: {stdout}");
}

// ═══════════════════════════════════════════════════════════════════════
// Domain flag
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn domain_cn_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\ndomain = \"cn\"\n",
    )
    .unwrap();
    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("cn"));
}
#[test]
fn domain_invalid_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\ndomain = \"uk\"\n",
    )
    .unwrap();
    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1);
}

// ═══════════════════════════════════════════════════════════════════════
// TOML config with domain
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn toml_domain_cn() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\ndomain = \"cn\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("cn"));
}

// ═══════════════════════════════════════════════════════════════════════
// Status --failed with no failed assets
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn status_failed_with_no_failures() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some("/p/photo1.jpg"),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--failed"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Failed:     0"), "stdout: {stdout}");
    // Should NOT print "Failed assets:" section
    assert!(
        !stdout.contains("Failed assets:"),
        "no failed assets section expected, stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Reset sync-token on empty metadata
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn reset_sync_token_empty_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let _conn = create_state_db(dir.path(), username);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "sync-token", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Cleared sync tokens"),
        "should still report clearing even with empty metadata, stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Config show outputs threads from TOML
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn config_show_reflects_threads_from_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\nthreads = 4\n",
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("threads = 4"), "stdout: {stdout}");
}

// ═══════════════════════════════════════════════════════════════════════
// Multiple verify issues at once
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn verify_mixed_present_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let present_path = dir.path().join("present.jpg");
    std::fs::write(&present_path, "exists").unwrap();

    let missing_path = dir.path().join("missing.jpg");

    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "present.jpg",
        Some(present_path.to_str().unwrap()),
        None,
        None,
    );
    insert_asset(
        &conn,
        "a2",
        "downloaded",
        "missing.jpg",
        Some(missing_path.to_str().unwrap()),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Verified:  1"), "stdout: {stdout}");
    assert!(stdout.contains("Missing:   1"), "stdout: {stdout}");
}
#[test]
fn verify_truncates_issue_listing_past_cap() {
    // Covers the 200-issue listing cap for `kei verify` on large libraries
    // where many files have gone missing. 250 missing assets should print
    // 200 MISSING lines plus a truncation tail, with the summary showing
    // the full count.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..250 {
        let id = format!("miss{i:04}");
        let filename = format!("missing_{i:04}.jpg");
        // local_path points at a file that doesn't exist on disk
        let path = dir.path().join(&filename);
        insert_asset(
            &conn,
            &id,
            "downloaded",
            &filename,
            Some(path.to_str().unwrap()),
            None,
            None,
        );
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["verify"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Missing:   250"), "stdout: {stdout}");
    assert!(
        stdout.contains("... and 50 more (listing capped at 200)"),
        "truncation tail missing; stdout: {stdout}"
    );
    // First 200 MISSING lines present, 201st+ suppressed.
    assert!(
        stdout.contains("missing_0000.jpg"),
        "first missing line absent"
    );
    assert!(
        stdout.contains("missing_0199.jpg"),
        "200th missing line absent"
    );
    assert!(
        !stdout.contains("missing_0200.jpg"),
        "201st missing line should have been suppressed; stdout: {stdout}"
    );
}
#[test]
fn reconcile_truncates_issue_listing_past_cap() {
    // Covers the 200-issue listing cap for `kei reconcile`. 250 seeded
    // missing rows produce 200 MISSING lines + a tail; summary shows
    // the full count and the `Marked failed` line confirms every row
    // was re-queued regardless of which lines printed.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..250 {
        let id = format!("rid{i:04}");
        let filename = format!("missing_{i:04}.jpg");
        let path = dir.path().join(&filename);
        // Path is under the tempdir but we never write the file, so
        // the existence check inside reconcile reports it as missing.
        insert_asset(
            &conn,
            &id,
            "downloaded",
            &filename,
            Some(path.to_str().unwrap()),
            None,
            None,
        );
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reconcile"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Missing:  250"), "stdout: {stdout}");
    assert!(
        stdout.contains("Marked failed: 250"),
        "every row should be re-queued regardless of the print cap; stdout: {stdout}"
    );
    assert!(
        stdout.contains("... and 50 more (listing capped at 200)"),
        "truncation tail missing; stdout: {stdout}"
    );
    assert!(stdout.contains("missing_0000.jpg"), "first row absent");
    assert!(stdout.contains("missing_0199.jpg"), "200th row absent");
    assert!(
        !stdout.contains("missing_0200.jpg"),
        "201st row should be suppressed; stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Dry run + retry-failed conflict
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn dry_run_and_retry_failed_conflict() {
    let dir = tempfile::tempdir().unwrap();
    // clap-level conflicts_with should reject this
    clean_cmd()
        .env("KEI_DATA_DIR", dir.path())
        .args(["sync", "--dry-run", "--retry-failed"])
        .assert()
        .code(2);
}

// ═══════════════════════════════════════════════════════════════════════
// Dry run: no state DB created
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn dry_run_creates_no_state_db() {
    let data_dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    let config_path = data_dir.path().join("config.toml");
    write_sync_config(&config_path, dl_dir.path().to_str().unwrap());

    clean_cmd()
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
        ])
        .assert()
        .failure(); // fails at auth, but that's after the dry-run DB skip point

    // No .db file should have been created in data-dir
    let db_files: Vec<_> = std::fs::read_dir(data_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "db"))
        .collect();
    assert!(
        db_files.is_empty(),
        "dry-run should not create a state DB, found: {:?}",
        db_files.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Status: --pending and --downloaded (issue #211)
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn status_pending_shows_pending_assets() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(&conn, "a1", "pending", "photo1.jpg", None, None, None);
    insert_asset(&conn, "a2", "pending", "photo2.jpg", None, None, None);
    insert_asset(
        &conn,
        "a3",
        "downloaded",
        "photo3.jpg",
        Some("/p/photo3.jpg"),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--pending"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Pending assets:"), "stdout: {stdout}");
    assert!(stdout.contains("photo1.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("photo2.jpg"), "stdout: {stdout}");
    // Downloaded asset must not appear in the pending listing
    assert!(!stdout.contains("photo3.jpg"), "stdout: {stdout}");
}
#[test]
fn status_downloaded_shows_downloaded_assets() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some("/p/photo1.jpg"),
        None,
        None,
    );
    insert_asset(&conn, "a2", "pending", "photo2.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Downloaded assets:"), "stdout: {stdout}");
    assert!(stdout.contains("photo1.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("/p/photo1.jpg"), "stdout: {stdout}");
    // Pending asset must not appear in the downloaded listing
    assert!(!stdout.contains("photo2.jpg"), "stdout: {stdout}");
}
#[test]
fn status_pending_empty_when_none_pending() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo1.jpg",
        Some("/p/photo1.jpg"),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--pending"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("Pending assets:"), "stdout: {stdout}");
}
#[test]
fn status_downloaded_with_null_local_path_surfaces_missing_marker() {
    // Covers the `<MISSING local_path>` display path in print_downloaded.
    // A downloaded row without a local_path is a state-DB invariant
    // violation; status must not silently hide it.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    // Directly insert a downloaded row with NULL local_path. insert_asset
    // helper would still pass None through, so we use it with explicit
    // Option::None for local_path.
    insert_asset(&conn, "a1", "downloaded", "broken.jpg", None, None, None);
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("<MISSING local_path>"),
        "missing-path marker not surfaced: {stdout}"
    );
    assert!(stdout.contains("broken.jpg"), "stdout: {stdout}");
}
#[test]
fn status_all_three_flags_render_all_sections() {
    // End-to-end coverage for --failed --pending --downloaded combined.
    // Locks in the three-section rendering and proves the flags are
    // orthogonal in the actual binary (not just clap parsing).
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    insert_asset(
        &conn,
        "dl1",
        "downloaded",
        "dl.jpg",
        Some("/p/dl.jpg"),
        None,
        None,
    );
    insert_asset(&conn, "pend1", "pending", "pend.jpg", None, None, None);
    insert_asset(
        &conn,
        "fail1",
        "failed",
        "fail.jpg",
        None,
        Some("timeout"),
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--failed", "--pending", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Failed assets:"), "stdout: {stdout}");
    assert!(stdout.contains("fail.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("Pending assets:"), "stdout: {stdout}");
    assert!(stdout.contains("pend.jpg"), "stdout: {stdout}");
    assert!(stdout.contains("Downloaded assets:"), "stdout: {stdout}");
    assert!(stdout.contains("dl.jpg"), "stdout: {stdout}");
}
#[test]
fn status_downloaded_paginates_past_page_size() {
    // Covers the pagination loop in run_status for --downloaded when the
    // result set exceeds page_size (100) but stays under the print cap
    // (200). 150 rows require at least two page fetches and all should
    // render (no truncation tail).
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..150 {
        let id = format!("dl{i:04}");
        let filename = format!("photo_{i:04}.jpg");
        let local = format!("/p/photo_{i:04}.jpg");
        insert_asset(
            &conn,
            &id,
            "downloaded",
            &filename,
            Some(&local),
            None,
            None,
        );
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Downloaded: 150"), "stdout: {stdout}");
    // First and last rows across the page boundary must both appear.
    assert!(stdout.contains("photo_0000.jpg"), "first row missing");
    assert!(stdout.contains("photo_0099.jpg"), "boundary row missing");
    assert!(
        stdout.contains("photo_0100.jpg"),
        "post-boundary row missing"
    );
    assert!(stdout.contains("photo_0149.jpg"), "last row missing");
    assert!(
        !stdout.contains("listing capped"),
        "no truncation tail expected when under cap; stdout: {stdout}"
    );
}
#[test]
fn status_downloaded_truncates_past_print_cap() {
    // Covers the 200-row listing cap for --downloaded on large libraries.
    // With 250 rows, the first 200 render and a tail names 50 more.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..250 {
        let id = format!("dl{i:04}");
        let filename = format!("photo_{i:04}.jpg");
        let local = format!("/p/photo_{i:04}.jpg");
        insert_asset(
            &conn,
            &id,
            "downloaded",
            &filename,
            Some(&local),
            None,
            None,
        );
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--downloaded"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Downloaded: 250"), "stdout: {stdout}");
    assert!(stdout.contains("photo_0000.jpg"), "first row missing");
    assert!(stdout.contains("photo_0199.jpg"), "200th row missing");
    assert!(
        !stdout.contains("photo_0200.jpg"),
        "201st row should have been truncated; stdout: {stdout}"
    );
    assert!(
        stdout.contains("... and 50 more (listing capped at 200)"),
        "truncation tail missing; stdout: {stdout}"
    );
}
#[test]
fn status_failed_truncates_past_print_cap() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    for i in 0..250 {
        let id = format!("fail{i:04}");
        let filename = format!("photo_{i:04}.jpg");
        insert_asset(&conn, &id, "failed", &filename, None, Some("timeout"), None);
    }
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["status", "--failed"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Failed:     250"), "stdout: {stdout}");
    assert!(
        stdout.contains("... and 50 more (listing capped at 200)"),
        "truncation tail missing; stdout: {stdout}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Config env vars in TOML (KEI_CONFIG, KEI_DATA_DIR)
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn kei_config_env_var_loads_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("env-config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"fromenv@example.com\"\n").unwrap();

    clean_cmd()
        .env("KEI_CONFIG", config_path.to_str().unwrap())
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fromenv@example.com"));
}

// ═══════════════════════════════════════════════════════════════════════
// kei reconcile: end-to-end CLI routing (no network)
// ═══════════════════════════════════════════════════════════════════════
#[test]
fn reconcile_subcommand_marks_missing_and_preserves_present() {
    let data_dir = tempfile::tempdir().unwrap();
    let photos_dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(data_dir.path(), username);

    let present_path = photos_dir.path().join("present.jpg");
    std::fs::write(&present_path, vec![0u8; 1000]).unwrap();
    let missing_path = photos_dir.path().join("gone.jpg");

    insert_asset(
        &conn,
        "PRESENT",
        "downloaded",
        "present.jpg",
        Some(present_path.to_str().unwrap()),
        None,
        None,
    );
    insert_asset(
        &conn,
        "MISSING",
        "downloaded",
        "gone.jpg",
        Some(missing_path.to_str().unwrap()),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", data_dir.path())
        .args(["reconcile"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("MISSING:") && stdout.contains("gone.jpg"),
        "missing file must be reported: {stdout}"
    );
    assert!(
        stdout.contains("Present:  1"),
        "present count must be 1: {stdout}"
    );
    assert!(
        stdout.contains("Missing:  1"),
        "missing count must be 1: {stdout}"
    );
    assert!(
        stdout.contains("Marked failed: 1"),
        "one mark_failed must have fired: {stdout}"
    );

    // Verify state transition landed in the DB.
    let db_name = format!("{}.db", sanitize_username(username));
    let conn = rusqlite::Connection::open(data_dir.path().join(db_name)).unwrap();
    let missing_status: String = conn
        .query_row("SELECT status FROM assets WHERE id = 'MISSING'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(missing_status, "failed");
    let missing_error: String = conn
        .query_row(
            "SELECT last_error FROM assets WHERE id = 'MISSING'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(missing_error, "FILE_MISSING_AT_STARTUP");
    let present_status: String = conn
        .query_row("SELECT status FROM assets WHERE id = 'PRESENT'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(present_status, "downloaded");
}

#[test]
fn reconcile_marks_truncated_file_failed() {
    let data_dir = tempfile::tempdir().unwrap();
    let photos_dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(data_dir.path(), username);

    let truncated_path = photos_dir.path().join("truncated.jpg");
    std::fs::write(&truncated_path, b"short").unwrap();
    insert_asset(
        &conn,
        "TRUNCATED",
        "downloaded",
        "truncated.jpg",
        Some(truncated_path.to_str().unwrap()),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", data_dir.path())
        .args(["reconcile"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("TRUNCATED:"), "stdout: {stdout}");
    assert!(stdout.contains("Damaged:  1"), "stdout: {stdout}");
    assert!(
        stdout.contains("Marked failed: 1"),
        "one drifted row must be re-queued: {stdout}"
    );

    let db_name = format!("{}.db", sanitize_username(username));
    let conn = rusqlite::Connection::open(data_dir.path().join(db_name)).unwrap();
    let (status, error): (String, String) = conn
        .query_row(
            "SELECT status, last_error FROM assets WHERE id = 'TRUNCATED'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(error, "FILE_TRUNCATED_AT_STARTUP");
}

#[test]
fn reconcile_dry_run_reports_but_does_not_mutate() {
    let data_dir = tempfile::tempdir().unwrap();
    let photos_dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(data_dir.path(), username);

    let missing_path = photos_dir.path().join("gone.jpg");
    insert_asset(
        &conn,
        "MISSING_DRY",
        "downloaded",
        "gone.jpg",
        Some(missing_path.to_str().unwrap()),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", data_dir.path())
        .args(["reconcile", "--dry-run"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dry run") || stdout.contains("Dry run"),
        "dry-run wording must appear: {stdout}"
    );
    assert!(
        stdout.contains("Missing:  1"),
        "missing count must still be 1 in dry-run: {stdout}"
    );
    assert!(
        !stdout.contains("Marked failed:"),
        "dry-run must not print Marked failed summary: {stdout}"
    );

    let db_name = format!("{}.db", sanitize_username(username));
    let conn = rusqlite::Connection::open(data_dir.path().join(db_name)).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM assets WHERE id = 'MISSING_DRY'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "downloaded", "dry-run must leave the DB unchanged");
}
#[test]
fn reconcile_on_empty_db_prints_guidance_and_exits_clean() {
    let data_dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", data_dir.path())
        .args(["reconcile"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("No state database") || stdout.contains("no state database"),
        "operator must see guidance when DB doesn't exist: {stdout}"
    );
}

/// Pin the per-version columns added by each schema migration so a future
/// helper-DDL refactor that drops one fails this test instead of silently
/// shipping a behavioral suite running against a thinner shape than the
/// binary writes.
#[test]
fn behavioral_helper_carries_every_migrated_column() {
    let dir = tempfile::tempdir().unwrap();
    let conn = create_state_db(dir.path(), "schema_check@example.com");

    fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .any(|name| name.is_ok_and(|n| n == column))
    }

    assert!(
        has_column(&conn, "assets", "metadata_write_failed_at"),
        "v6 column metadata_write_failed_at must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "sync_runs", "status"),
        "v7 column sync_runs.status must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "assets", "library"),
        "v8 column assets.library must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "asset_albums", "library"),
        "v9 column asset_albums.library must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "asset_people", "library"),
        "v9 column asset_people.library must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "sync_runs", "enumeration_errors"),
        "v10 column sync_runs.enumeration_errors must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "assets", "imported_size"),
        "v11 column assets.imported_size must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "assets", "imported_mtime"),
        "v11 column assets.imported_mtime must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "sync_runs", "api_total_at_start"),
        "v13 column sync_runs.api_total_at_start must exist in the behavioral helper's DDL"
    );
    assert!(
        has_column(&conn, "sync_runs", "inventory_drop_detected"),
        "v13 column sync_runs.inventory_drop_detected must exist in the behavioral helper's DDL"
    );

    let has_asset_albums: bool = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='asset_albums'")
        .unwrap()
        .exists([])
        .unwrap();
    assert!(
        has_asset_albums,
        "v5 table asset_albums must exist in the behavioral helper's DDL"
    );

    for table in [
        "album_containers",
        "album_membership_snapshots",
        "asset_album_memberships",
    ] {
        let exists: bool = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name = ?1")
            .unwrap()
            .exists([table])
            .unwrap();
        assert!(
            exists,
            "v12 table {table} must exist in the behavioral helper's DDL"
        );
    }

    let has_asset_master_mappings: bool = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name = 'asset_master_mappings'",
        )
        .unwrap()
        .exists([])
        .unwrap();
    assert!(
        has_asset_master_mappings,
        "v15 table asset_master_mappings must exist in the behavioral helper's DDL"
    );

    let has_asset_verifications: bool = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name = 'asset_verifications'",
        )
        .unwrap()
        .exists([])
        .unwrap();
    assert!(
        has_asset_verifications,
        "v16 table asset_verifications must exist in the behavioral helper's DDL"
    );

    let has_legacy_master_state_owners: bool = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name = 'legacy_master_state_owners'",
        )
        .unwrap()
        .exists([])
        .unwrap();
    assert!(
        has_legacy_master_state_owners,
        "v17 table legacy_master_state_owners must exist in the behavioral helper's DDL"
    );

    for table in ["asset_metadata_capture_revisions", "metadata_capture_state"] {
        let exists: bool = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name = ?1")
            .unwrap()
            .exists([table])
            .unwrap();
        assert!(
            exists,
            "v18 table {table} must exist in the behavioral helper's DDL"
        );
    }

    let has_owned_temp_files: bool = conn
        .prepare(
            "SELECT name FROM sqlite_master \
             WHERE type='table' AND name = 'owned_temp_files'",
        )
        .unwrap()
        .exists([])
        .unwrap();
    assert!(
        has_owned_temp_files,
        "v19 table owned_temp_files must exist in the behavioral helper's DDL"
    );
    for column in [
        "capture_repair_metadata_hash",
        "capture_repair_output_checksum",
        "capture_repair_output_size",
    ] {
        assert!(
            has_column(&conn, "assets", column),
            "v20 column assets.{column} must exist in the behavioral helper's DDL"
        );
    }
    let path_key: Vec<String> = conn
        .prepare(
            "SELECT name FROM pragma_table_info('asset_metadata_paths') WHERE pk > 0 ORDER BY pk",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(path_key, ["library", "id", "version_size", "local_path"]);
}

// ═══════════════════════════════════════════════════════════════════════
// v0.13 selection + per-category folder-structure surface
//
// Stdout (resolved config) checks drive `kei config show` from a TOML
// fixture, since that subcommand uses `SyncArgs::default()` and won't
// accept sync flags. CLI/env-flag tests drive `kei sync` and only assert
// stderr / exit code so they don't require auth.
// ═══════════════════════════════════════════════════════════════════════

/// Run `kei config show` against an inline TOML fixture and return the
/// (stdout, stderr) pair. Builds a tempdir, writes `[download].directory`
/// and the supplied `body` into it, then dumps the resolved config.
fn run_config_show(body: &str) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n{body}"),
    )
    .unwrap();
    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_config_show_error(body: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n{body}"),
    )
    .unwrap();
    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .clone();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_removed_env_config_hint(stderr: &str) {
    assert!(
        stderr.contains("found removed v0.20 env config"),
        "stale sync env vars should emit a migration hint; stderr: {stderr}"
    );
    assert!(
        stderr.contains("KEI_DOWNLOAD_DIR"),
        "stale sync env vars should name KEI_DOWNLOAD_DIR; stderr: {stderr}"
    );
}

/// Build a `kei sync` invocation pre-populated with username, fresh tempdir
/// config/data directories, and `--only-print-filenames` so the
/// run exits before auth. Returns the live `Command` so callers can append
/// flag-specific args. Tempdirs are leaked into the binary (which never
/// touches them, as these tests bail in `Config::build`).
fn sync_cmd_for_validation() -> assert_cmd::Command {
    sync_cmd_for_config_body("")
}

fn sync_cmd_for_config_body(body: &str) -> assert_cmd::Command {
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
#[test]
fn removed_legacy_album_in_cli_errors() {
    sync_cmd_for_validation()
        .args([
            "--folder-structure",
            "{album}/%Y/%m/%d",
            "--only-print-filenames",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"))
        .stderr(predicate::str::contains("--folder-structure"));
}
#[test]
fn removed_legacy_album_in_toml_errors() {
    let stderr = run_config_show_error("folder_structure = \"{album}/%B\"\n");
    assert!(
        stderr.contains("`{album}` cannot be used in --folder-structure")
            && stderr.contains("--folder-structure-albums"),
        "stderr: {stderr}"
    );
}
#[test]
fn removed_legacy_album_env_is_ignored() {
    sync_cmd_for_validation()
        .env("KEI_FOLDER_STRUCTURE", "{album}/%Y")
        .arg("--only-print-filenames")
        .assert()
        .failure()
        .stderr(predicate::str::contains("`{album}` cannot be used in --folder-structure").not());
}
#[test]
fn removed_legacy_album_errors_even_with_user_set_albums_template() {
    let stderr = run_config_show_error(
        "folder_structure = \"{album}/%Y\"\nfolder_structure_albums = \"{album}/custom\"\n",
    );
    assert!(
        stderr.contains("`{album}` cannot be used in --folder-structure")
            && stderr.contains("--folder-structure-albums"),
        "stderr: {stderr}"
    );
}
/// Every per-category selection flag composed in a single
/// invocation must validate end-to-end through the
/// `Cli -> Config -> Selection` pipeline. Per-category unit tests in
/// `selection.rs` cover each parser in isolation, but the binary-level
/// wiring (clap field name, config-resolver field name, the
/// `Cli::command` mapping can drift independently of the
/// parsers; a regression there lands green for every per-category
/// test even when the combined flag set bails at startup.
///
/// Flags exercised here:
///   --album none              → AlbumSelector::None
///   --smart-folder all        → SmartFolderSelector::All { sensitive=false }
///   --unfiled false           → Selection.unfiled = false
///   --library shared          → LibrarySelector { primary=false, shared_all=true }
///
/// The binary may exit non-zero for downstream reasons (no password
/// available, network unreachable, auth bail) — those are
/// out-of-scope. What matters is that none of the parser-level bail
/// strings ("must not be empty", "not supported", "cannot be combined")
/// reach stderr.
#[test]
fn sync_validation_accepts_full_selection_combo() {
    let out = sync_cmd_for_config_body(
        "\n[filters]\nalbums = [\"none\"]\nsmart_folders = [\"all\"]\nunfiled = false\nlibraries = [\"shared\"]\n",
    )
        .arg("--only-print-filenames")
        .assert()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // No parser-level bail strings — those would mean the combo got
    // rejected at parse time, which the per-category tests already
    // disprove for each flag in isolation.
    assert!(
        !stderr.contains("must not be empty"),
        "no parser empty-input bail expected; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("not supported"),
        "no friendly-alias bail expected; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("cannot be combined"),
        "no sentinel-mix bail expected; stderr: {stderr}"
    );
}
#[test]
fn config_show_emits_per_category_templates_from_toml() {
    let (stdout, _) = run_config_show(
        "folder_structure_albums = \"{album}/%Y/%m\"\nfolder_structure_smart_folders = \"{smart-folder}/%Y\"\n",
    );
    assert!(
        stdout.contains("folder_structure_albums = \"{album}/%Y/%m\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("folder_structure_smart_folders = \"{smart-folder}/%Y\""),
        "stdout: {stdout}"
    );
}

/// Default per-category templates stay implicit -- a future refactor that
/// starts emitting the defaults would inflate every dumped config.
#[test]
fn config_show_omits_default_per_category_templates() {
    let (stdout, _) = run_config_show("");
    assert!(
        !stdout.contains("folder_structure_albums"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("folder_structure_smart_folders"),
        "stdout: {stdout}"
    );
}
#[test]
fn sync_bails_on_album_token_in_smart_folders_template() {
    sync_cmd_for_config_body("folder_structure_smart_folders = \"{album}/%Y\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{album}"))
        .stderr(predicate::str::contains("--folder-structure-albums"));
}
#[test]
fn sync_bails_on_smart_folder_token_in_albums_template() {
    sync_cmd_for_config_body("folder_structure_albums = \"{smart-folder}/foo\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{smart-folder}"))
        .stderr(predicate::str::contains("--folder-structure-smart-folders"));
}
#[test]
fn sync_bails_on_library_token_not_first_segment() {
    sync_cmd_for_config_body("folder_structure = \"%Y/{library}\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{library}"))
        .stderr(predicate::str::contains("first path segment"));
}
#[test]
fn sync_bails_on_duplicate_library_token() {
    sync_cmd_for_config_body("folder_structure_albums = \"{library}/{library}/{album}\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{library}"))
        .stderr(predicate::str::contains("once"));
}
#[test]
fn sync_bails_on_within_album_contradiction() {
    sync_cmd_for_config_body("\n[filters]\nalbums = [\"Family\", \"!Family\"]\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("includes and excludes"))
        .stderr(predicate::str::contains("Family"));
}
#[test]
fn sync_bails_on_library_none() {
    sync_cmd_for_config_body("\n[filters]\nlibraries = [\"none\"]\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("library none"));
}
#[test]
fn config_show_emits_smart_folder_selection() {
    let (stdout, _) =
        run_config_show("\n[filters]\nsmart_folders = [\"Favorites\", \"!Hidden\"]\n");
    assert!(stdout.contains("smart_folders"), "stdout: {stdout}");
    assert!(stdout.contains("Favorites"), "stdout: {stdout}");
    assert!(stdout.contains("!Hidden"), "stdout: {stdout}");
}
#[test]
fn config_show_emits_unfiled_false_when_disabled() {
    let (stdout, _) = run_config_show("\n[filters]\nunfiled = false\n");
    assert!(stdout.contains("unfiled = false"), "stdout: {stdout}");
}

/// Default `unfiled = true` stays implicit -- locks in that defaults don't
/// inflate dumped configs.
#[test]
fn config_show_omits_unfiled_when_default_true() {
    let (stdout, _) = run_config_show("");
    assert!(!stdout.contains("unfiled = true"), "stdout: {stdout}");
}
#[test]
fn config_show_emits_libraries_when_non_default() {
    let (stdout, _) = run_config_show("\n[filters]\nlibraries = [\"all\"]\n");
    assert!(stdout.contains("libraries = [\"all\"]"), "stdout: {stdout}");
}
#[test]
fn config_show_emits_libraries_when_repeatable_named_zone() {
    // Pin the multi-zone case at the binary boundary: a zone-truncated
    // alias plus `primary` must round-trip into a libraries array that
    // contains both. A regression in `LibrarySelector::to_raw()` that
    // dropped the named zone (or collapsed multiple inputs to a single
    // sentinel) lands red here.
    let (stdout, _) =
        run_config_show("\n[filters]\nlibraries = [\"primary\", \"SharedSync-A1B2C3D4\"]\n");
    assert!(
        stdout.contains("libraries"),
        "stdout must include a libraries key:\n{stdout}"
    );
    assert!(
        stdout.contains("primary"),
        "stdout must include primary:\n{stdout}"
    );
    assert!(
        stdout.contains("SharedSync-A1B2C3D4"),
        "stdout must include the named zone:\n{stdout}"
    );
}

#[test]
fn config_show_round_trips_persistent_recent_and_dates() {
    let (stdout, _) = run_config_show(
        "\n[filters]\nrecent = 100\nrecent_scope = \"per-filter\"\nskip_created_before = \"2024-01-01\"\nskip_created_after = \"30d\"\n",
    );
    assert!(stdout.contains("recent = 100"), "stdout: {stdout}");
    assert!(
        stdout.contains("recent_scope = \"per-filter\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skip_created_before = \"2024-01-01\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skip_created_after = \"30d\""),
        "stdout: {stdout}"
    );
}

#[test]
fn config_show_round_trips_media_filter() {
    let (stdout, _) = run_config_show("\n[filters]\nmedia = [\"photos\", \"live-photos\"]\n");
    assert!(stdout.contains("media"), "stdout: {stdout}");
    assert!(stdout.contains("photos"), "stdout: {stdout}");
    assert!(stdout.contains("live-photos"), "stdout: {stdout}");
}

#[test]
fn config_show_round_trips_escaped_selection_values() {
    let (stdout, _) = run_config_show(
        "\n[filters]\nalbums = [\"=all\", \"=!Drafts\"]\nsmart_folders = [\"=none\"]\nlibraries = [\"=primary\"]\n",
    );
    assert!(stdout.contains("\"=all\""), "stdout: {stdout}");
    assert!(stdout.contains("\"=!Drafts\""), "stdout: {stdout}");
    assert!(stdout.contains("\"=none\""), "stdout: {stdout}");
    assert!(stdout.contains("\"=primary\""), "stdout: {stdout}");
}

// ── Removed v0.20 selection aliases ───────────────────────────────
#[test]
fn removed_exclude_album_cli_flag_errors() {
    sync_cmd_for_validation()
        .args(["--exclude-album", "Family", "--only-print-filenames"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--exclude-album"));
}
#[test]
fn removed_toml_filter_aliases_error() {
    for (field, body) in [
        ("album", "\n[filters]\nalbum = \"Vacation\"\n"),
        (
            "exclude_albums",
            "\n[filters]\nexclude_albums = [\"Drafts\", \"Family\"]\n",
        ),
        ("library", "\n[filters]\nlibrary = \"PrimarySync\"\n"),
        ("skip_videos", "\n[filters]\nskip_videos = true\n"),
        ("skip_photos", "\n[filters]\nskip_photos = true\n"),
    ] {
        let stderr = run_config_show_error(body);
        assert!(
            stderr.contains(&format!("unknown field `{field}`")),
            "expected unknown-field error for {field}; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("pre-v0.20 config"),
            "expected v0.20 migration hint for {field}; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("docs/v0.20-migration.md"),
            "expected migration guide URL for {field}; stderr:\n{stderr}"
        );
    }
}
#[test]
fn removed_sync_env_vars_do_not_block_non_sync_command() {
    // Regression: issue #385 - stale sync env vars set in old Docker Compose
    // files must not block non-sync subcommands like `kei reset`.
    let temp = tempfile::tempdir().unwrap();
    let mut cmd = clean_cmd();
    cmd.current_dir(temp.path());
    cmd.env("KEI_DOWNLOAD_DIR", "/photos");
    cmd.env("KEI_ALBUM", "none");
    cmd.env("KEI_LIVE_PHOTO_MODE", "image-only");
    cmd.env("KEI_FOLDER_STRUCTURE", "{:%Y/%m/%Y-%m-%d}");
    #[cfg(feature = "xmp")]
    cmd.env("KEI_EMBED_XMP", "true");
    cmd.env("ICLOUD_USERNAME", "test@example.com");
    cmd.args(["reset", "state", "--yes"]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("sync-only flag").not());
}

#[test]
fn removed_sync_env_vars_do_not_supply_sync_config() {
    // Removed sync env mirrors must not keep configuring sync after v0.20.
    // With no TOML [download].directory, this must fail at config resolution
    // even if a stale KEI_DOWNLOAD_DIR is still present in the environment.
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .env("KEI_DOWNLOAD_DIR", "/legacy/photos")
        .env("KEI_ALBUM", "Legacy Album")
        .env("KEI_THREADS", "4")
        .args(["sync", "--only-print-filenames"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Set [download].directory"),
        "stale sync env vars must not provide durable config; stderr: {stderr}"
    );
    assert_removed_env_config_hint(&stderr);
    assert!(
        stderr.contains("ignored in v0.20"),
        "stale sync env vars should explain they are ignored; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "stale sync env vars should be ignored by clap, not parsed as CLI args; stderr: {stderr}"
    );
}

#[test]
fn removed_sync_env_vars_do_not_supply_import_config() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .env("KEI_DOWNLOAD_DIR", "/legacy/photos")
        .env("KEI_ALBUM", "Legacy Album")
        .args(["import-existing", "--dry-run"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Set [download].directory"),
        "stale sync env vars must not provide import config; stderr: {stderr}"
    );
    assert_removed_env_config_hint(&stderr);
}

#[test]
fn removed_sync_env_vars_do_not_block_service_status() {
    // Regression: issue #385 - same class of bug on a different non-sync
    // subcommand (service status does not carry SyncArgs).
    let temp = tempfile::tempdir().unwrap();
    let mut cmd = clean_cmd();
    cmd.current_dir(temp.path());
    cmd.env("KEI_DOWNLOAD_DIR", "/photos");
    cmd.env("KEI_ALBUM", "none");
    cmd.env("KEI_LIVE_PHOTO_MODE", "image-only");
    cmd.env("ICLOUD_USERNAME", "test@example.com");
    cmd.args(["service", "status"]);
    cmd.assert()
        .stderr(predicate::str::contains("sync-only flag").not());
}
