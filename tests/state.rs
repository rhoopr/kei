//! State-management tests that do NOT require network credentials.
//!
//! Tests that need credentials live in `state_auth.rs`.

mod common;

use predicates::prelude::*;
use rusqlite::{Connection, OptionalExtension};
use tempfile::tempdir;

// ══════════════════════════════════════════════════════════════════════════
//  STATUS
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn status_no_db_prints_informational_message() {
    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "status",
            "--username",
            "fake@example.com",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn status_failed_flag_accepted() {
    let cookie_dir = tempdir().expect("failed to create tempdir");

    // With no DB, --failed still works — just shows the "no database" message
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "status",
            "--failed",
            "--username",
            "fake@example.com",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

// ══════════════════════════════════════════════════════════════════════════
//  RESET-STATE
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn reset_state_no_db_prints_message() {
    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "reset-state",
            "--yes",
            "--username",
            "fake@example.com",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

// ══════════════════════════════════════════════════════════════════════════
//  VERIFY
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn verify_no_db_prints_informational_message() {
    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "verify",
            "--username",
            "fake@example.com",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

// ══════════════════════════════════════════════════════════════════════════
//  METADATA PERSISTENCE (sync token storage)
// ══════════════════════════════════════════════════════════════════════════

/// Create a fully migrated state DB at the given path (schema v3).
fn create_state_db(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).expect("failed to open DB");
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
    .expect("failed to create schema");
    conn.pragma_update(None, "user_version", 3)
        .expect("failed to set schema version");
    conn
}

#[test]
fn metadata_persists_across_close_and_reopen() {
    let dir = tempdir().expect("failed to create tempdir");
    let db_path = dir.path().join("test.db");

    // Write metadata, close connection
    {
        let conn = create_state_db(&db_path);
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            ["sync_token:PrimarySync", "tok-abc-123"],
        )
        .expect("insert failed");
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            ["db_sync_token", "db-tok-456"],
        )
        .expect("insert failed");
    }

    // Reopen and verify
    let conn = Connection::open(&db_path).expect("reopen failed");
    let zone_token: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:PrimarySync"],
            |row| row.get(0),
        )
        .expect("zone token not found");
    assert_eq!(zone_token, "tok-abc-123");

    let db_token: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["db_sync_token"],
            |row| row.get(0),
        )
        .expect("db token not found");
    assert_eq!(db_token, "db-tok-456");
}

#[test]
fn metadata_upsert_overwrites_existing_value() {
    let dir = tempdir().expect("failed to create tempdir");
    let db_path = dir.path().join("test.db");
    let conn = create_state_db(&db_path);

    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        ["sync_token:PrimarySync", "old-token"],
    )
    .expect("insert failed");

    // Upsert (same SQL pattern as SqliteStateDb::set_metadata)
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        ["sync_token:PrimarySync", "new-token"],
    )
    .expect("upsert failed");

    let value: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:PrimarySync"],
            |row| row.get(0),
        )
        .expect("not found");
    assert_eq!(value, "new-token");
}

#[test]
fn metadata_empty_string_is_stored_not_null() {
    let dir = tempdir().expect("failed to create tempdir");
    let db_path = dir.path().join("test.db");
    let conn = create_state_db(&db_path);

    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        ["sync_token:PrimarySync", ""],
    )
    .expect("insert failed");

    let value: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:PrimarySync"],
            |row| row.get(0),
        )
        .expect("not found — empty string should be stored, not NULL");
    assert_eq!(value, "");
}

#[test]
fn metadata_missing_key_returns_no_rows() {
    let dir = tempdir().expect("failed to create tempdir");
    let db_path = dir.path().join("test.db");
    let conn = create_state_db(&db_path);

    let result: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["nonexistent"],
            |row| row.get(0),
        )
        .optional()
        .expect("query failed");
    assert!(result.is_none());
}

#[test]
fn metadata_multiple_zone_tokens_isolated() {
    let dir = tempdir().expect("failed to create tempdir");
    let db_path = dir.path().join("test.db");
    let conn = create_state_db(&db_path);

    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        ["sync_token:PrimarySync", "primary-tok"],
    )
    .expect("insert failed");
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        ["sync_token:SharedSync-ABCD", "shared-tok"],
    )
    .expect("insert failed");
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        ["db_sync_token", "db-level-tok"],
    )
    .expect("insert failed");

    // Each key returns its own value
    let primary: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:PrimarySync"],
            |row| row.get(0),
        )
        .unwrap();
    let shared: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:SharedSync-ABCD"],
            |row| row.get(0),
        )
        .unwrap();
    let db: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["db_sync_token"],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(primary, "primary-tok");
    assert_eq!(shared, "shared-tok");
    assert_eq!(db, "db-level-tok");

    // Updating one doesn't affect others
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        ["sync_token:PrimarySync", "updated-tok"],
    )
    .unwrap();

    let primary_new: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:PrimarySync"],
            |row| row.get(0),
        )
        .unwrap();
    let shared_unchanged: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:SharedSync-ABCD"],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(primary_new, "updated-tok");
    assert_eq!(shared_unchanged, "shared-tok");
}

#[test]
fn metadata_clear_token_by_setting_empty() {
    let dir = tempdir().expect("failed to create tempdir");
    let db_path = dir.path().join("test.db");
    let conn = create_state_db(&db_path);

    // Store a valid token
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
        ["sync_token:PrimarySync", "valid-tok"],
    )
    .unwrap();

    // "Clear" by setting to empty string (matches --reset-sync-token behavior)
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        ["sync_token:PrimarySync", ""],
    )
    .unwrap();

    let value: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            ["sync_token:PrimarySync"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "");
}
