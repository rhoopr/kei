//! Seeded SQLite verification, reset, reconciliation, and non-mutating dry runs.

use super::{
    HELPER_SCHEMA_VERSION, clean_cmd, create_state_db, insert_asset, sanitize_username,
    write_sync_config,
};
use predicates::prelude::predicate;
use rusqlite::OptionalExtension;

/// Pin the helper schema version against the binary's
/// production constant. The binary writes a fresh DB at
/// `state::schema::SCHEMA_VERSION` (currently 25). The shared helper
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
    // update the DDL in `create_state_db` in support to match the new
    // shape. The fresh-DB DDL emitted by a real binary run can be
    // dumped via `sqlite3 <db> '.schema'` for reference.
    const PRODUCTION_SCHEMA_VERSION: i32 = 25;
    assert_eq!(
        HELPER_SCHEMA_VERSION, PRODUCTION_SCHEMA_VERSION,
        "behavioral.rs::create_state_db schema is out of sync with \
         src/state/schema.rs::SCHEMA_VERSION (helper={HELPER_SCHEMA_VERSION}, \
         production={PRODUCTION_SCHEMA_VERSION}). Bump both, plus the DDL \
         block in create_state_db, then update this test."
    );
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
