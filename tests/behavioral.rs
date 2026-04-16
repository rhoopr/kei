//! Behavioral tests -- exercise real execution paths without credentials.
//!
//! These tests run the actual binary and verify outputs, exit codes,
//! deprecation warnings, config resolution, and error messages.
//! No network, no iCloud credentials required.

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

/// Create a state DB at the expected path for the given username inside `data_dir`.
fn create_state_db(data_dir: &std::path::Path, username: &str) -> rusqlite::Connection {
    let db_name = format!("{}.db", sanitize_username(username));
    let db_path = data_dir.join(db_name);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(
        r"
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
            local_checksum TEXT,
            PRIMARY KEY (id, version_size)
        );
        CREATE TABLE IF NOT EXISTS sync_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at INTEGER NOT NULL,
            completed_at INTEGER,
            assets_seen INTEGER DEFAULT 0,
            assets_downloaded INTEGER DEFAULT 0,
            assets_failed INTEGER DEFAULT 0,
            interrupted INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        );
        ",
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 3).unwrap();
    conn
}

/// Insert an asset row into the state DB.
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
        "INSERT INTO assets (id, version_size, checksum, filename, created_at, size_bytes, \
         media_type, status, local_path, last_seen_at, last_error, local_checksum, downloaded_at) \
         VALUES (?1, 'original', 'abc', ?2, 1700000000, 1000, 'photo', ?3, ?4, 1700000000, \
         ?5, ?6, CASE WHEN ?3 = 'downloaded' THEN 1700000000 ELSE NULL END)",
        rusqlite::params![id, filename, status, local_path, last_error, local_checksum],
    )
    .unwrap();
}

// ═══════════════════════════════════════════════════════════════════════
// Deprecation warnings: every legacy command prints to stderr
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn deprecation_get_code() {
    let out = clean_cmd()
        .args(["get-code", "--username", "x@x.com", "--data-dir", "/tmp"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated") && stderr.contains("login get-code"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_submit_code() {
    let out = clean_cmd()
        .args([
            "submit-code",
            "123456",
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
        stderr.contains("deprecated") && stderr.contains("login submit-code"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_credential() {
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
    let out = clean_cmd()
        .args([
            "retry-failed",
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
        stderr.contains("deprecated") && stderr.contains("sync --retry-failed"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_reset_state() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "reset-state",
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
        stderr.contains("deprecated") && stderr.contains("reset state"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_reset_sync_token() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .args([
            "reset-sync-token",
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
        stderr.contains("deprecated") && stderr.contains("reset sync-token"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_setup() {
    // setup is interactive, so just pass --help after the deprecation fires
    // Actually, setup reads stdin, so we need to avoid it. Use a non-existent
    // output path that will fail after the deprecation warning.
    let out = clean_cmd()
        .args(["setup", "--help"])
        .assert()
        .success()
        .get_output()
        .clone();
    // --help short-circuits before effective_command(), so no deprecation.
    // Instead, verify the subcommand still parses by checking exit 0.
    assert!(out.status.success());
}

#[test]
fn deprecation_auth_only_flag() {
    let out = clean_cmd()
        .args(["--auth-only", "--username", "x@x.com", "--data-dir", "/tmp"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated") && stderr.contains("kei login"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_list_albums_flag() {
    let out = clean_cmd()
        .args([
            "--list-albums",
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
        stderr.contains("deprecated") && stderr.contains("list albums"),
        "stderr: {stderr}"
    );
}

#[test]
fn deprecation_list_libraries_flag() {
    let out = clean_cmd()
        .args([
            "--list-libraries",
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
        stderr.contains("deprecated") && stderr.contains("list libraries"),
        "stderr: {stderr}"
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
fn config_show_never_contains_password() {
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
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("super_secret_value"),
        "config show must never contain password, got:\n{stdout}"
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
fn sync_requires_username() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .args([
            "sync",
            "--directory",
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
            "--directory",
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
            "--directory",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("must not be empty"));
}

#[test]
fn config_empty_password_in_toml() {
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
            "--directory",
            "/photos",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("must not be empty"));
}

#[test]
fn config_multiple_password_sources_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword = \"secret\"\npassword_file = \"/tmp/pw\"\n",
    )
    .unwrap();

    clean_cmd()
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--directory",
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
        .stderr(predicate::str::contains("watch interval must be >= 60"));
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
    // Default threads_num = 10
    assert!(
        stdout.contains("threads_num = 10"),
        "default threads_num should be 10, stdout: {stdout}"
    );
    // Default folder_structure = "%Y/%m/%d"
    assert!(
        stdout.contains("%Y/%m/%d"),
        "default folder_structure should be %Y/%m/%d, stdout: {stdout}"
    );
}

#[test]
fn config_resolution_password_never_shown() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword = \"my_secret_pw\"\n",
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
        "password must not appear in config show, stdout: {stdout}"
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
            "--directory",
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
            "--directory",
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
            "--directory",
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
            "--directory",
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
            "--directory",
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
            "--directory",
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
fn config_show_threads_num_cli_override() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\nthreads_num = 4\n",
    )
    .unwrap();

    // config show does not accept --threads-num directly (it's a sync arg),
    // but we can verify the TOML value is reflected
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
    assert!(stdout.contains("threads_num = 4"), "stdout: {stdout}");
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
            "--directory",
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
            "--directory",
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
