//! Behavioral tests -- exercise real execution paths without credentials.
//!
//! These tests run the actual binary and verify outputs, exit codes,
//! deprecation warnings, config resolution, and error messages.
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
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(10);

/// Helper: run kei with env scrubbed and a temp data-dir so it never
/// touches real config/cookies.
fn clean_cmd() -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env_remove("KEI_CONFIG")
        .env_remove("KEI_DATA_DIR")
        .env_remove("KEI_DIRECTORY")
        .env_remove("KEI_DOWNLOAD_DIR")
        .env_remove("KEI_DOMAIN")
        .env_remove("KEI_LOG_LEVEL")
        .env_remove("KEI_NO_AUTO_CONFIG")
        .timeout(TIMEOUT);
    cmd
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
/// helper is updated to match — preventing silent drift between the
/// helper's "fresh DB" shape and what the binary expects.
const HELPER_SCHEMA_VERSION: i32 = 8;

/// Create a state DB at the expected path for the given username inside
/// `data_dir`. Mirrors the v8 schema from `src/state/schema.rs` (the
/// latest as of this writing) so the binary's migrate() loop is a no-op
/// when it opens these DBs — i.e. tests run against the same shape
/// production code writes on a fresh install. Bump `HELPER_SCHEMA_VERSION`
/// and the DDL below together whenever schema.rs changes; the
/// `behavioral_helper_schema_matches_production` meta test enforces it.
fn create_state_db(data_dir: &std::path::Path, username: &str) -> rusqlite::Connection {
    let db_name = format!("{}.db", sanitize_username(username));
    let db_path = data_dir.join(db_name);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r"
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
            PRIMARY KEY (library, id, version_size)
        );
        CREATE INDEX IF NOT EXISTS idx_assets_status ON assets(status);
        CREATE INDEX IF NOT EXISTS idx_assets_local_path ON assets(local_path);
        CREATE INDEX IF NOT EXISTS idx_assets_checksum ON assets(checksum);
        CREATE INDEX IF NOT EXISTS idx_assets_metadata_hash
            ON assets (metadata_hash) WHERE status = 'downloaded';
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
        CREATE TABLE IF NOT EXISTS sync_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at INTEGER NOT NULL,
            completed_at INTEGER,
            assets_seen INTEGER DEFAULT 0,
            assets_downloaded INTEGER DEFAULT 0,
            assets_failed INTEGER DEFAULT 0,
            interrupted INTEGER DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'running'
        );
        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
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
/// `state::schema::SCHEMA_VERSION` (currently 8). The helper above
/// claims to "Mirror the latest schema" and must therefore land on the
/// same version — otherwise existing tests rely on the binary's
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
    const PRODUCTION_SCHEMA_VERSION: i32 = 8;
    assert_eq!(
        HELPER_SCHEMA_VERSION, PRODUCTION_SCHEMA_VERSION,
        "behavioral.rs::create_state_db schema is out of sync with \
         src/state/schema.rs::SCHEMA_VERSION (helper={HELPER_SCHEMA_VERSION}, \
         production={PRODUCTION_SCHEMA_VERSION}). Bump both, plus the DDL \
         block in create_state_db, then update this test."
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Deprecation warnings: every legacy command prints to stderr
// ═══════════════════════════════════════════════════════════════════════

/// Assert a legacy invocation prints a deprecation warning naming the new command.
fn assert_deprecated(args: &[&str], should_succeed: bool, hint: &str) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();
    let with_data_dir: Vec<&str> = args
        .iter()
        .map(|a| if *a == "__DATA_DIR__" { data_dir } else { *a })
        .collect();
    let assert = clean_cmd().args(&with_data_dir).assert();
    let assert = if should_succeed {
        assert.success()
    } else {
        assert.failure()
    };
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("deprecated") && stderr.contains(hint),
        "args={args:?} hint={hint:?} stderr={stderr}"
    );
}

#[test]
fn deprecation_get_code() {
    assert_deprecated(
        &["get-code", "--username", "x@x.com", "--data-dir", "/tmp"],
        false,
        "login get-code",
    );
}

#[test]
fn deprecation_submit_code() {
    assert_deprecated(
        &[
            "submit-code",
            "123456",
            "--username",
            "x@x.com",
            "--data-dir",
            "/tmp",
        ],
        false,
        "login submit-code",
    );
}

#[test]
fn deprecation_credential() {
    // `credential` subcommand may exit success or failure depending on backend;
    // only the deprecation warning matters here.
    let out = clean_cmd()
        .args([
            "credential",
            "backend",
            "--username",
            "x@x.com",
            "--data-dir",
            "/tmp",
        ])
        .assert()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated") && stderr.contains("kei password"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_retry_failed() {
    assert_deprecated(
        &[
            "retry-failed",
            "--username",
            "x@x.com",
            "--data-dir",
            "/tmp",
        ],
        false,
        "sync --retry-failed",
    );
}

#[test]
fn deprecation_reset_state() {
    assert_deprecated(
        &[
            "reset-state",
            "--yes",
            "--username",
            "x@x.com",
            "--data-dir",
            "__DATA_DIR__",
        ],
        true,
        "reset state",
    );
}

#[test]
fn deprecation_reset_sync_token() {
    assert_deprecated(
        &[
            "reset-sync-token",
            "--username",
            "x@x.com",
            "--data-dir",
            "__DATA_DIR__",
        ],
        true,
        "reset sync-token",
    );
}

#[test]
fn deprecation_setup() {
    // --help short-circuits before effective_command(), so no deprecation warning.
    // Just confirm the subcommand still parses.
    clean_cmd().args(["setup", "--help"]).assert().success();
}

#[test]
fn deprecation_auth_only_flag() {
    assert_deprecated(
        &["--auth-only", "--username", "x@x.com", "--data-dir", "/tmp"],
        false,
        "kei login",
    );
}

#[test]
fn deprecation_list_albums_flag() {
    assert_deprecated(
        &[
            "--list-albums",
            "--username",
            "x@x.com",
            "--data-dir",
            "/tmp",
        ],
        false,
        "list albums",
    );
}

#[test]
fn deprecation_reset_sync_token_flag_on_sync() {
    // Covers the `kei sync --reset-sync-token ...` path specifically,
    // distinct from the legacy top-level `kei reset-sync-token` subcommand.
    // The flag is hidden, still parses, still clears the token, but must
    // now warn and name v0.20.0 as the removal target.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    let assert = clean_cmd()
        .args([
            "sync",
            "--reset-sync-token",
            "--username",
            "x@x.com",
            "--download-dir",
            dl_dir.path().to_str().unwrap(),
            "--data-dir",
            data_dir,
            "--dry-run", // still goes through run_sync; fails at password resolution
        ])
        .assert()
        .failure();
    // Regardless of the password-resolution failure that follows, the
    // --reset-sync-token warning fires earlier at flag-read time.
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("--reset-sync-token") && stderr.contains("kei reset sync-token"),
        "expected reset-sync-token flag deprecation warning; stderr={stderr}"
    );
}

#[test]
fn deprecation_warning_names_v0_20_0_removal() {
    // Guards the shared `deprecation_warning()` helper — every flag-rename
    // warning in the codebase flows through it, so asserting one path
    // carries v0.20.0 protects all of them from regressing back to a
    // vague "a future release" wording.
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_str().unwrap();
    let assert = clean_cmd()
        .args([
            "--list-libraries",
            "--username",
            "x@x.com",
            "--data-dir",
            data_dir,
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("v0.20.0"),
        "deprecation warning must name v0.20.0 as removal target; stderr={stderr}"
    );
}

#[test]
fn deprecation_list_libraries_flag() {
    assert_deprecated(
        &[
            "--list-libraries",
            "--username",
            "x@x.com",
            "--data-dir",
            "/tmp",
        ],
        false,
        "list libraries",
    );
}

// ═══════════════════════════════════════════════════════════════════════
// New commands: NO deprecation warnings
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn no_deprecation_login() {
    let out = clean_cmd()
        .args(["login", "--username", "x@x.com", "--data-dir", "/tmp"])
        .assert()
        .failure() // fails at auth, not at parsing
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("deprecated"),
        "new command should not print deprecation, stderr: {stderr}"
    );
}

#[test]
fn no_deprecation_list_albums() {
    let out = clean_cmd()
        .args([
            "list",
            "albums",
            "--username",
            "x@x.com",
            "--data-dir",
            "/tmp",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("deprecated"),
        "new command should not print deprecation, stderr: {stderr}"
    );
}

#[test]
fn no_deprecation_password_backend() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "password",
            "backend",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("deprecated"),
        "new command should not print deprecation, stderr: {stderr}"
    );
}

#[test]
fn no_deprecation_reset_state() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "reset",
            "state",
            "--yes",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("deprecated"),
        "new command should not print deprecation, stderr: {stderr}"
    );
}

#[test]
fn no_deprecation_reset_sync_token() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "reset",
            "sync-token",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("deprecated"),
        "new command should not print deprecation, stderr: {stderr}"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// config show: resolved config output
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn config_show_outputs_valid_toml() {
    let out = clean_cmd()
        .args([
            "config",
            "show",
            "--username",
            "test@example.com",
            "--data-dir",
            "/tmp",
        ])
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
        .args([
            "config",
            "show",
            "--username",
            "myuser@icloud.com",
            "--data-dir",
            "/tmp",
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
threads_num = 4
"#,
    )
    .unwrap();

    let out = clean_cmd()
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("toml@example.com"), "stdout: {stdout}");
    assert!(stdout.contains("/toml/photos"), "stdout: {stdout}");
    assert!(
        stdout.contains("4"),
        "threads_num should be 4, stdout: {stdout}"
    );
}

#[test]
fn config_show_emits_unfiled_false_when_explicit() {
    // The cli.rs help-shadow test for --unfiled only verifies clap parses;
    // it does not pin the resolved value all the way through Config::build
    // → Selection → to_toml. A clap-default flip (or a derive_selection
    // regression) that swallowed the explicit `false` would land green
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--username",
            "cli@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args(["login", "--data-dir", "/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--username is required"));
}

#[test]
fn list_albums_requires_username() {
    clean_cmd()
        .args(["list", "albums", "--data-dir", "/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--username is required"));
}

#[test]
fn password_set_requires_username() {
    clean_cmd()
        .args(["password", "set", "--data-dir", "/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--username is required"));
}

#[test]
fn password_clear_requires_username() {
    clean_cmd()
        .args(["password", "clear", "--data-dir", "/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--username is required"));
}

#[test]
fn password_backend_requires_username() {
    clean_cmd()
        .args(["password", "backend", "--data-dir", "/tmp"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--username is required"));
}

/// `password backend` against a fresh cookie dir prints the credential
/// backend name. The backend choice is platform-dependent (OS keyring
/// when available, encrypted file fallback), so we only assert the
/// output is non-empty and exit is clean.
#[test]
fn password_backend_prints_backend_name() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "password",
            "backend",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "password",
            "clear",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No stored credential"));
}

#[test]
fn sync_requires_username() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--username is required"));
}

#[test]
fn sync_requires_directory() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--directory is required"));
}

#[test]
fn import_existing_requires_directory() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "import-existing",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "--download-dir is required for import-existing",
        ));
}

#[test]
fn import_existing_rejects_nonexistent_directory() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "import-existing",
            "--username",
            "x@x.com",
            "--download-dir",
            "/does/not/exist/anywhere",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Directory does not exist"));
}

// ═══════════════════════════════════════════════════════════════════════
// No-DB paths: commands that hit the DB but none exists
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn status_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "status",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn verify_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "verify",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn reset_state_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "reset",
            "state",
            "--yes",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn reset_sync_token_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "reset",
            "sync-token",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

// Legacy reset-state also works with no DB
#[test]
fn legacy_reset_state_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "reset-state",
            "--yes",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "password",
            "backend",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "password",
            "clear",
            "--username",
            "nobody@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "password",
            "backend",
            "--username",
            "kei-behavioral-test-nonexistent@example.invalid",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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

// Legacy credential backend produces same output as new
#[test]
fn legacy_credential_backend_same_as_new() {
    let dir = tempfile::tempdir().unwrap();
    let base = [
        "--username",
        "test@example.com",
        "--data-dir",
        dir.path().to_str().unwrap(),
    ];

    let old = clean_cmd()
        .args(["credential", "backend"])
        .args(base)
        .output()
        .unwrap();
    let new = clean_cmd()
        .args(["password", "backend"])
        .args(base)
        .output()
        .unwrap();

    assert_eq!(old.stdout, new.stdout, "same stdout");
    // Old should have deprecation warning, new should not
    let old_stderr = String::from_utf8_lossy(&old.stderr);
    let new_stderr = String::from_utf8_lossy(&new.stderr);
    assert!(
        old_stderr.contains("deprecated"),
        "old stderr: {old_stderr}"
    );
    assert!(
        !new_stderr.contains("deprecated"),
        "new stderr: {new_stderr}"
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
        .env("KEI_DATA_DIR", dir.path().to_str().unwrap())
        .args(["status", "--username", "x@x.com"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn icloud_username_env_resolves_in_config_show() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@icloud.com")
        .args(["config", "show", "--data-dir", dir.path().to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("env@icloud.com"));
}

#[test]
fn cli_flag_overrides_env_var() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@icloud.com")
        .args([
            "config",
            "show",
            "--username",
            "cli@icloud.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("cli@icloud.com"));
}

// ═══════════════════════════════════════════════════════════════════════
// --data-dir vs --cookie-directory behavior
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn cookie_directory_prints_deprecation() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "status",
            "--username",
            "x@x.com",
            "--cookie-directory",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--cookie-directory") && stderr.contains("deprecated"),
        "should warn about deprecated --cookie-directory, stderr: {stderr}"
    );
}

#[test]
fn data_dir_no_deprecation() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "status",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("deprecated"),
        "--data-dir should not warn, stderr: {stderr}"
    );
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
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--username",
            "auto@example.com",
            "--download-dir",
            "/auto/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
    assert!(
        content.contains("/auto/photos"),
        "auto-config should contain directory, got:\n{content}"
    );
}

#[test]
fn first_run_auto_config_does_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# existing config\n").unwrap();

    clean_cmd()
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--username",
            "new@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();

    let content = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(
        content, "# existing config\n",
        "auto-config must not overwrite existing file"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Old -> new behavioral equivalence
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn legacy_and_new_credential_backend_same_output() {
    let dir = tempfile::tempdir().unwrap();
    let args_base = [
        "--username",
        "x@x.com",
        "--data-dir",
        dir.path().to_str().unwrap(),
    ];

    let old = clean_cmd()
        .args(["credential", "backend"])
        .args(args_base)
        .output()
        .unwrap();
    let new = clean_cmd()
        .args(["password", "backend"])
        .args(args_base)
        .output()
        .unwrap();

    assert_eq!(
        old.stdout, new.stdout,
        "credential backend and password backend should produce same stdout"
    );
}

#[test]
fn legacy_and_new_reset_state_same_behavior() {
    // Both should print "No state database found" (path differs, so
    // compare the prefix instead of exact bytes).
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();

    let old = clean_cmd()
        .args([
            "reset-state",
            "--yes",
            "--username",
            "x@x.com",
            "--data-dir",
            dir1.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let new = clean_cmd()
        .args([
            "reset",
            "state",
            "--yes",
            "--username",
            "x@x.com",
            "--data-dir",
            dir2.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    let old_out = String::from_utf8_lossy(&old.stdout);
    let new_out = String::from_utf8_lossy(&new.stdout);
    assert!(
        old_out.contains("No state database found"),
        "old: {old_out}"
    );
    assert!(
        new_out.contains("No state database found"),
        "new: {new_out}"
    );
    assert_eq!(old.status, new.status, "exit codes should match");
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown field"));
}

#[test]
fn config_empty_username_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"\"\n").unwrap();

    // config show calls Config::build which checks for empty username
    // only when a username source is present in TOML. Since TOML sets
    // username = "", the build path validates it.
    clean_cmd()
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("must not be empty"));
}

#[test]
fn config_toml_password_field_rejected() {
    // `[auth] password` is no longer accepted, empty or otherwise; kei must
    // exit with a migration message pointing at the supported alternatives.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword = \"\"\n",
    )
    .unwrap();

    clean_cmd()
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("`[auth] password`"))
        .stderr(predicate::str::contains("no longer supported"))
        .stderr(predicate::str::contains("kei password set"));
}

// On Windows, `--password-command` / `[auth] password_command` is rejected
// at config::build before the "pick one" check runs, so the assertion on
// "pick one" doesn't hold. Unix covers the path this test is guarding.
#[cfg(unix)]
#[test]
fn config_multiple_password_sources_in_toml() {
    // Both `password_file` and `password_command` set in the same TOML is
    // still rejected with "pick one" (the `password` variant is rejected
    // upstream by the stronger deprecation check).
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword_file = \"/tmp/pw\"\npassword_command = \"echo hi\"\n",
    )
    .unwrap();

    clean_cmd()
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("pick one"));
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
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "watch interval must be in 60..=86400 seconds, got 30",
        ));
}

#[test]
fn config_retry_delay_zero_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n\n[download.retry]\ndelay = 0\n",
    )
    .unwrap();

    clean_cmd()
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("retry delay"));
}

#[test]
fn config_threads_num_zero_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nthreads_num = 0\n",
    )
    .unwrap();

    clean_cmd()
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("threads_num"));
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tomluser@example.com"), "stdout: {stdout}");
    assert!(stdout.contains("/toml/dir"), "stdout: {stdout}");
}

#[test]
fn config_resolution_cli_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"toml@example.com\"\n").unwrap();

    clean_cmd()
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--username",
            "cli@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("cli@example.com"));
}

#[test]
fn config_resolution_env_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"toml@example.com\"\n").unwrap();

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "env@example.com")
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
fn config_resolution_cli_overrides_env() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@example.com")
        .args([
            "config",
            "show",
            "--username",
            "cli@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("cli@example.com"));
}

#[test]
fn config_resolution_default_values() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "config",
            "show",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Default threads = 10 (new canonical spelling; the `threads_num` TOML
    // key is deprecated but the serialized default uses the new name).
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--username",
            "suppress@example.com",
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--username",
            "perms@example.com",
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--failed",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "verify",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "verify",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "verify",
            "--checksums",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "verify",
            "--checksums",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CORRUPTED"), "stdout: {stdout}");
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
        .args([
            "reset",
            "state",
            "--yes",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
    drop(conn);

    let out = clean_cmd()
        .args([
            "reset",
            "sync-token",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "reset",
            "state",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "login",
            "--username",
            "x@x.com",
            "--password-file",
            pw_file.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "login",
            "--username",
            "x@x.com",
            "--password-file",
            pw_file.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "login",
            "--username",
            "x@x.com",
            "--password-file",
            pw_file.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "login",
            "--username",
            "x@x.com",
            "--password-command",
            "echo cmdpw",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "login",
            "--username",
            "x@x.com",
            "--password-command",
            "false",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(3)
        .stderr(
            predicate::str::contains("No password available")
                .or(predicate::str::contains("exited with status")),
        );
}

// ═══════════════════════════════════════════════════════════════════════
// Exit codes
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn exit_2_for_clap_errors() {
    // Pass --username with an empty string -- clap's value_parser rejects it
    clean_cmd()
        .args(["--username", "", "config", "show"])
        .assert()
        .code(2);
}

#[test]
fn exit_1_for_missing_directory_on_sync() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--directory is required"));
}

#[test]
fn exit_1_for_missing_username_on_sync() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--username is required"));
}

// ═══════════════════════════════════════════════════════════════════════
// Log level behavior
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn log_level_default_info() {
    let dir = tempfile::tempdir().unwrap();
    // sync with username + directory will fail at auth. Check stderr for INFO.
    let out = clean_cmd()
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
    let out = clean_cmd()
        .args([
            "--log-level",
            "debug",
            "sync",
            "--username",
            "x@x.com",
            "--download-dir",
            dl_dir.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
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
    let out = clean_cmd()
        .args([
            "--log-level",
            "error",
            "sync",
            "--username",
            "x@x.com",
            "--download-dir",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
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
        .args([
            "verify",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "verify",
            "--checksums",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
    clean_cmd()
        .args([
            "config",
            "show",
            "--username",
            "x@x.com",
            "--domain",
            "cn",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("cn"));
}

#[test]
fn domain_invalid_rejected() {
    clean_cmd()
        .args([
            "config",
            "show",
            "--username",
            "x@x.com",
            "--domain",
            "invalid",
            "--data-dir",
            "/tmp",
        ])
        .assert()
        .code(2);
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--failed",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "reset",
            "sync-token",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
// Config show outputs threads_num from CLI override
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("threads = 4"), "stdout: {stdout}");
}

#[test]
fn config_show_legacy_threads_num_emits_deprecation() {
    // The old `threads_num` TOML key still parses but must warn on load and
    // round-trip into the new `threads` key on the way out.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\nthreads_num = 7\n",
    )
    .unwrap();

    let assert = clean_cmd()
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let output = assert.get_output().clone();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`[download] threads_num`"), "{stderr}");
    assert!(stderr.contains("v0.20.0"), "{stderr}");
    assert!(stdout.contains("threads = 7"), "{stdout}");
}

#[test]
fn config_show_reflects_report_json_from_toml() {
    // [report] json = "..." is the canonical TOML home for --report-json.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[report]\njson = \"/tmp/kei-run.json\"\n",
    )
    .unwrap();

    let out = clean_cmd()
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[report]"), "stdout: {stdout}");
    assert!(
        stdout.contains("json = \"/tmp/kei-run.json\""),
        "stdout: {stdout}"
    );
}

#[test]
fn legacy_threads_num_cli_flag_warns() {
    // The hidden `--threads-num` still parses for backward compat but must
    // tell the user to migrate to `--threads` and name the v0.20.0 removal.
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--username",
            "legacy-threads@example.com",
            "--download-dir",
            dl_dir.path().to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--threads-num",
            "3",
            "--only-print-filenames",
        ])
        .assert()
        .stderr(predicate::str::contains("--threads-num"))
        .stderr(predicate::str::contains("--threads"))
        .stderr(predicate::str::contains("v0.20.0"));
}

#[test]
fn legacy_retry_delay_cli_flag_warns() {
    // `--retry-delay` still accepts a value but must tell the user it's
    // deprecated and point at the new smart-default behavior.
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--username",
            "legacy-retry-delay@example.com",
            "--download-dir",
            dl_dir.path().to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--retry-delay",
            "10",
            "--only-print-filenames",
        ])
        .assert()
        .stderr(predicate::str::contains("--retry-delay"))
        .stderr(predicate::str::contains("--max-retries"))
        .stderr(predicate::str::contains("v0.20.0"));
}

#[test]
fn both_threads_forms_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--username",
            "double-threads@example.com",
            "--download-dir",
            dl_dir.path().to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--threads",
            "4",
            "--threads-num",
            "8",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("both"))
        .stderr(predicate::str::contains("pick one"));
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
        .args([
            "verify",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "verify",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "reconcile",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--download-dir",
            "/photos",
            "--dry-run",
            "--retry-failed",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(2);
}

// ═══════════════════════════════════════════════════════════════════════
// --directory deprecation
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn legacy_directory_flag_emits_deprecation_warning() {
    // `--directory` still works but must tell the user to switch to
    // `--download-dir` and note the v0.20.0 removal target. We pair it with
    // `--only-print-filenames` so the command exits before hitting auth.
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();

    clean_cmd()
        .args([
            "sync",
            "--username",
            "legacy-dir@example.com",
            "--directory",
            dl_dir.path().to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--only-print-filenames",
        ])
        .assert()
        .stderr(predicate::str::contains("--directory"))
        .stderr(predicate::str::contains("--download-dir"))
        .stderr(predicate::str::contains("v0.20.0"));
}

#[test]
fn both_directory_and_download_dir_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();

    clean_cmd()
        .args([
            "sync",
            "--username",
            "double-dir@example.com",
            "--directory",
            dl_dir.path().to_str().unwrap(),
            "--download-dir",
            dl_dir.path().to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("both"))
        .stderr(predicate::str::contains("pick one"));
}

// ═══════════════════════════════════════════════════════════════════════
// Dry run: no state DB created
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn dry_run_creates_no_state_db() {
    let data_dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();

    clean_cmd()
        .args([
            "sync",
            "--username",
            "drytest@example.com",
            "--download-dir",
            dl_dir.path().to_str().unwrap(),
            "--data-dir",
            data_dir.path().to_str().unwrap(),
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
        .args([
            "status",
            "--pending",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--downloaded",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--pending",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--downloaded",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--failed",
            "--pending",
            "--downloaded",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--downloaded",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--downloaded",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args([
            "status",
            "--failed",
            "--username",
            username,
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
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
        .args(["config", "show", "--data-dir", dir.path().to_str().unwrap()])
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
    std::fs::write(&present_path, b"x").unwrap();
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
        .args([
            "reconcile",
            "--username",
            username,
            "--data-dir",
            data_dir.path().to_str().unwrap(),
        ])
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
        .args([
            "reconcile",
            "--dry-run",
            "--username",
            username,
            "--data-dir",
            data_dir.path().to_str().unwrap(),
        ])
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
        .args([
            "reconcile",
            "--username",
            username,
            "--data-dir",
            data_dir.path().to_str().unwrap(),
        ])
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
        .args([
            "config",
            "show",
            "--config",
            config_path.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Build a `kei sync` invocation pre-populated with username, fresh tempdir
/// `--download-dir` / `--data-dir` pair, and `--only-print-filenames` so the
/// run exits before auth. Returns the live `Command` so callers can append
/// flag-specific args. Tempdirs are leaked into the binary (which never
/// touches them, as these tests bail in `Config::build`).
fn sync_cmd_for_validation() -> assert_cmd::Command {
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = tempfile::tempdir().unwrap();
    let mut cmd = clean_cmd();
    cmd.args([
        "sync",
        "--username",
        "x@x.com",
        "--download-dir",
        dl_dir.path().to_str().unwrap(),
        "--data-dir",
        dir.path().to_str().unwrap(),
    ]);
    // Tempdirs leak intentionally: tests bail before sync touches them, and
    // OS-level tmpfs cleanup handles the directories at process exit.
    std::mem::forget(dir);
    std::mem::forget(dl_dir);
    cmd
}

#[test]
fn migration_legacy_album_in_cli_warns() {
    sync_cmd_for_validation()
        .args([
            "--folder-structure",
            "{album}/%Y/%m/%d",
            "--only-print-filenames",
        ])
        .assert()
        .stderr(predicate::str::contains(
            "`{album}` in `--folder-structure`",
        ))
        .stderr(predicate::str::contains("v0.20.0"))
        .stderr(predicate::str::contains("--folder-structure-albums"));
}

#[test]
fn migration_legacy_album_in_toml_warns_and_lifts() {
    let (stdout, stderr) = run_config_show("folder_structure = \"{album}/%B\"\n");
    assert!(
        stderr.contains("`{album}` in `--folder-structure`") && stderr.contains("v0.20.0"),
        "stderr: {stderr}"
    );
    assert!(
        stdout.contains("folder_structure_albums = \"{album}/%B\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("folder_structure = \"%B\""),
        "stdout: {stdout}"
    );
}

/// Locks in that all three input surfaces (CLI / TOML / env) flow through
/// the same migration helper.
#[test]
fn migration_legacy_album_in_env_warns() {
    sync_cmd_for_validation()
        .env("KEI_FOLDER_STRUCTURE", "{album}/%Y")
        .arg("--only-print-filenames")
        .assert()
        .stderr(predicate::str::contains(
            "`{album}` in `--folder-structure`",
        ))
        .stderr(predicate::str::contains("v0.20.0"));
}

/// User's albums template wins; the legacy `{album}` segment in the base
/// template still gets stripped (so no leftover token reaches the renderer)
/// and the warning still fires.
#[test]
fn migration_legacy_album_preserves_user_set_albums_template() {
    let (stdout, stderr) = run_config_show(
        "folder_structure = \"{album}/%Y\"\nfolder_structure_albums = \"{album}/custom\"\n",
    );
    assert!(
        stderr.contains("`{album}` in `--folder-structure`"),
        "stderr: {stderr}"
    );
    assert!(
        stdout.contains("folder_structure_albums = \"{album}/custom\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("folder_structure = \"%Y\""),
        "stdout: {stdout}"
    );
}

#[test]
fn migration_no_warning_when_no_album_token() {
    sync_cmd_for_validation()
        .args(["--folder-structure", "%Y/%m/%d", "--only-print-filenames"])
        .assert()
        .stderr(predicate::str::contains("`{album}` in `--folder-structure`").not());
}

/// `--smart-folder Favorites` no longer prints the pre-PR6 "not yet wired
/// into the sync pipeline" disclaimer. The flag executes end-to-end via
/// `Selection -> resolve_passes -> AlbumPlan`; a stale warning at startup
/// would mislead users into thinking their config is a no-op.
#[test]
fn smart_folder_flag_does_not_print_unwired_warning() {
    sync_cmd_for_validation()
        .args(["--smart-folder", "Favorites", "--only-print-filenames"])
        .assert()
        .stderr(predicate::str::contains("not yet wired").not())
        .stderr(predicate::str::contains("not download smart folders").not());
}

/// `--unfiled false` no longer prints the pre-PR6 "not yet wired" disclaimer.
/// The flag flows into `Selection.unfiled` and gates both the unfiled pass
/// and the cross-album exclusion-set pre-fetch in `resolve_passes`.
#[test]
fn unfiled_flag_does_not_print_unwired_warning() {
    sync_cmd_for_validation()
        .args(["--unfiled", "false", "--only-print-filenames"])
        .assert()
        .stderr(predicate::str::contains("not yet wired").not())
        .stderr(predicate::str::contains("legacy unfiled-pass rules").not());
}

/// Every per-category selection flag composed in a single
/// invocation must validate end-to-end through the
/// `Cli -> Config -> Selection` pipeline. Per-category unit tests in
/// `selection.rs` cover each parser in isolation, but the binary-level
/// wiring (clap field name, config-resolver field name, the
/// `effective_command()` mapping) can drift independently of the
/// parsers; a regression there lands green for every per-category
/// test even when the combined flag set bails or warns at startup.
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
/// or stale "not yet wired" disclaimers reach stderr.
#[test]
fn sync_validation_accepts_full_selection_combo() {
    let out = sync_cmd_for_validation()
        .args([
            "--album",
            "none",
            "--smart-folder",
            "all",
            "--unfiled",
            "false",
            "--library",
            "shared",
            "--only-print-filenames",
        ])
        .assert()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // No stale "not yet wired" disclaimers from the pre-PR6 era.
    assert!(
        !stderr.contains("not yet wired"),
        "selection combo must not surface a 'not yet wired' warning; stderr: {stderr}"
    );
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
    sync_cmd_for_validation()
        .args(["--folder-structure-smart-folders", "{album}/%Y"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{album}"))
        .stderr(predicate::str::contains("--folder-structure-albums"));
}

#[test]
fn sync_bails_on_smart_folder_token_in_albums_template() {
    sync_cmd_for_validation()
        .args(["--folder-structure-albums", "{smart-folder}/foo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{smart-folder}"))
        .stderr(predicate::str::contains("--folder-structure-smart-folders"));
}

#[test]
fn sync_bails_on_library_token_not_first_segment() {
    sync_cmd_for_validation()
        .args(["--folder-structure", "%Y/{library}"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{library}"))
        .stderr(predicate::str::contains("first path segment"));
}

#[test]
fn sync_bails_on_duplicate_library_token() {
    sync_cmd_for_validation()
        .args(["--folder-structure-albums", "{library}/{library}/{album}"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{library}"))
        .stderr(predicate::str::contains("once"));
}

#[test]
fn sync_bails_on_within_album_contradiction() {
    sync_cmd_for_validation()
        .args(["--album", "Family", "--album", "!Family"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("include and exclude"))
        .stderr(predicate::str::contains("Family"));
}

#[test]
fn sync_bails_on_library_none() {
    sync_cmd_for_validation()
        .args(["--library", "none"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--library none"));
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
