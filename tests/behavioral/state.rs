//! Seeded SQLite verification, reset, reconciliation, and non-mutating dry runs.

use super::{
    HELPER_SCHEMA_VERSION, clean_cmd, create_state_db, insert_asset, sanitize_username,
    write_sync_config,
};
use predicates::prelude::{PredicateBooleanExt, predicate};
use rusqlite::OptionalExtension;

/// Pin the helper schema version against the binary's
/// production constant. The binary writes a fresh DB at
/// `state::schema::SCHEMA_VERSION`. The shared helper
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
    const PRODUCTION_SCHEMA_VERSION: i32 = 49;
    let production_schema = include_str!("../../src/state/schema.rs");
    assert!(
        production_schema.contains(&format!(
            "pub(crate) const SCHEMA_VERSION: i32 = {PRODUCTION_SCHEMA_VERSION};"
        )),
        "behavioral production-version pin must match the actual schema owner"
    );
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
        .args(super::download_config_args(dir.path()))
        .args(["verify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn reset_state_no_db() {
    let dir = tempfile::tempdir().unwrap();
    for args in [vec!["reset", "state"], vec!["reset", "state", "--yes"]] {
        clean_cmd()
            .env("ICLOUD_USERNAME", "test@example.com")
            .env("KEI_DATA_DIR", dir.path())
            .args(super::download_config_args(dir.path()))
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("No state database found"));
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn reset_sync_token_no_db() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
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
        .args(super::download_config_args(dir.path()))
        .args(["verify"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Present, not checksum-verified: 1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Missing:                        0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Checksums were not compared"),
        "stdout: {stdout}"
    );
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
        .args(super::download_config_args(dir.path()))
        .args(["verify"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MISSING"), "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("reconcile --inspect-path PATH"),
        "repair hint should name the existing command: {stderr}"
    );
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
        .args(super::download_config_args(dir.path()))
        .args(["verify", "--checksums"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Checksum verified:              1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Present, no checksum recorded:  0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Different checksum:             0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Unreadable:                     0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Missing:                        0"),
        "stdout: {stdout}"
    );
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
        .args(super::download_config_args(dir.path()))
        .args(["verify", "--checksums"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Different checksum:             1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Unreadable:                     0"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("Checksum verified:              1"),
        "a different file must not be reported as checksum verified: {stdout}"
    );
}

/// Identify the changed copy without claiming that a local edit is damage.
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
        .args(super::download_config_args(dir.path()))
        .args(["verify", "--checksums"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("DIFFERENT:") && line.contains(asset_id)),
        "expected asset id {asset_id} on the DIFFERENT line, stdout: {stdout}"
    );
}

#[test]
fn verify_checksums_reports_unreadable_separately_from_different() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let different_path = dir.path().join("different.jpg");
    std::fs::write(&different_path, b"edited bytes").unwrap();
    let unreadable_path = dir.path().join("unreadable.jpg");
    std::fs::create_dir(&unreadable_path).unwrap();

    insert_asset(
        &conn,
        "DIFFERENT",
        "downloaded",
        "different.jpg",
        Some(different_path.to_str().unwrap()),
        None,
        Some("0000000000000000000000000000000000000000000000000000000000000000"),
    );
    insert_asset(
        &conn,
        "UNREADABLE",
        "downloaded",
        "unreadable.jpg",
        Some(unreadable_path.to_str().unwrap()),
        None,
        Some("1111111111111111111111111111111111111111111111111111111111111111"),
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
        .args(["verify", "--checksums"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("DIFFERENT:") && line.contains("DIFFERENT")),
        "stdout: {stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("UNREADABLE:") && line.contains("UNREADABLE")),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Different checksum:             1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Unreadable:                     1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Checksum verified:              0"),
        "stdout: {stdout}"
    );
}

#[test]
fn verify_reports_non_file_as_unreadable_without_checksums() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let directory_path = dir.path().join("not-media.jpg");
    std::fs::create_dir(&directory_path).unwrap();
    insert_asset(
        &conn,
        "NON_FILE",
        "downloaded",
        "not-media.jpg",
        Some(directory_path.to_str().unwrap()),
        None,
        None,
    );
    drop(conn);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
        .args(["verify"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("UNREADABLE:") && line.contains("NON_FILE")),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Unreadable:                     1"),
        "stdout: {stdout}"
    );
}

#[test]
fn reset_state_deletes_db() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);
    let media = dir.path().join("photo.jpg");
    std::fs::write(&media, b"synthetic media").unwrap();
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "photo.jpg",
        Some(media.to_str().unwrap()),
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
        .args(super::download_config_args(dir.path()))
        .args(["reset", "state", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(!db_path.exists(), "DB file should be deleted after reset");
    assert_eq!(std::fs::read(&media).unwrap(), b"synthetic media");
    assert!(
        stdout.contains("deleted"),
        "should print 'deleted', stdout: {stdout}"
    );
    clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
        .args(["reset", "state", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
    assert_eq!(std::fs::read(&media).unwrap(), b"synthetic media");
}
#[test]
fn reset_state_without_yes_on_non_tty() {
    assert_reset_state_requires_yes("");
}

#[test]
fn reset_state_piped_confirmation_requires_yes() {
    assert_reset_state_requires_yes("y\n");
}

fn assert_reset_state_requires_yes(input: &str) {
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
    let before = std::fs::read(&db_path).unwrap();

    // Both EOF and a piped confirmation must refuse, including on retry.
    for _ in 0..2 {
        clean_cmd()
            .env("ICLOUD_USERNAME", username)
            .env("KEI_DATA_DIR", dir.path())
            .args(super::download_config_args(dir.path()))
            .args(["reset", "state"])
            .write_stdin(input)
            .assert()
            .failure()
            .stderr(predicate::str::contains("--yes"))
            .stderr(predicate::str::contains("database was not deleted"))
            .stdout(predicate::str::contains("Are you sure?").not())
            .stdout(predicate::str::contains("Cancelled").not());
        assert_eq!(std::fs::read(&db_path).unwrap(), before);
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let status: String = conn
            .query_row("SELECT status FROM assets WHERE id = 'a1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "downloaded");
    }
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
        .args(super::download_config_args(dir.path()))
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
fn verify_empty_db() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let _conn = create_state_db(dir.path(), username);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
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
fn verify_checksums_reports_missing_baseline_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let conn = create_state_db(dir.path(), username);

    let file_path = dir.path().join("photo.jpg");
    std::fs::write(&file_path, "some content").unwrap();

    // No local_checksum stored. Presence alone is not checksum verification,
    // and a missing baseline must not be treated as corruption.
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
        .args(super::download_config_args(dir.path()))
        .args(["verify", "--checksums"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Checksum verified:              0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Present, no checksum recorded:  1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Different checksum:             0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Unreadable:                     0"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("present but unchecked, not verified"),
        "stdout: {stdout}"
    );
}

#[test]
fn reset_sync_token_empty_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let _conn = create_state_db(dir.path(), username);

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(super::download_config_args(dir.path()))
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
        .args(super::download_config_args(dir.path()))
        .args(["verify"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Present, not checksum-verified: 1"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("Missing:                        1"),
        "stdout: {stdout}"
    );
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
        .args(super::download_config_args(dir.path()))
        .args(["verify"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Missing:                        250"),
        "stdout: {stdout}"
    );
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
fn reconcile_on_empty_db_prints_guidance_and_exits_clean() {
    let data_dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", data_dir.path())
        .args(super::download_config_args(data_dir.path()))
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

    // Exact provider-schema equivalence is checked against a database built by
    // the real migration chain in `src/state/schema.rs`:
    // `behavioral_helper_matches_real_migrated_provider_schema`. That test
    // compares columns, primary keys, foreign keys, checks, indexes and rowid
    // mode. Keep the explicit behavioral-side column pins below.

    fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .any(|name| name.is_ok_and(|n| n == column))
    }

    for (table, column, migration) in [
        ("provider_active_destinations", "current_publication", "v39"),
        ("provider_active_destinations", "preserved_local", "v39"),
        ("album_containers", "directory_binding", "v40"),
        (
            "provider_active_destinations",
            "embedded_metadata_receipt",
            "v41",
        ),
        ("asset_metadata_paths", "embedded_metadata_receipt", "v41"),
        ("provider_active_destinations", "confirmed_repair", "v42"),
        ("provider_active_destinations", "writer_output_proof", "v47"),
        ("provider_selection_rank_pages", "has_relation", "v48"),
        ("provider_active_destinations", "metadata_successor", "v49"),
    ] {
        assert!(
            has_column(&conn, table, column),
            "{migration} column {table}.{column} must exist in the behavioral helper's DDL"
        );
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
    for sql in [
        "SELECT id,account_key,provider_key,format,scope,config_hash,manifest,charged_bytes FROM provider_selection_generations LIMIT 0",
        "SELECT generation,page_id,ordinal,body_hash FROM provider_selection_sources LIMIT 0",
        "SELECT generation,pass_key,child,outcome,reason FROM provider_selection_decisions LIMIT 0",
        "SELECT generation,pass_key,child,version_size,path,checksum,size_bytes,metadata_hash FROM provider_selection_destinations LIMIT 0",
        "SELECT id,account_key,provider_key,scope,request_cursor,successor,more_coming,body_hash,body,charged_bytes,observed_at FROM provider_shadow_pages LIMIT 0",
        "SELECT page_id,ordinal,record_name,record_type,deleted FROM provider_shadow_records LIMIT 0",
        "SELECT scope,page_id FROM provider_shadow_receipts LIMIT 0",
        "SELECT page_id,ordinal,record_name,record_type,deleted,kind FROM provider_catalog_records LIMIT 0",
        "SELECT page_id,ordinal,field_path,target_record_name,target_zone_name,target_zone_owner FROM provider_catalog_references LIMIT 0",
        "SELECT page_id,ordinal,field_path,reason FROM provider_catalog_debt LIMIT 0",
        "SELECT page_id,projector_version,body_hash,record_count,reference_count,debt_count,charged_bytes FROM provider_catalog_pages LIMIT 0",
        "SELECT page_id,ordinal,config_hash,confirmation_hash,confirmation,state,reason,scope,source_hash,master_record_name,charged_bytes FROM provider_work_receipts LIMIT 0",
        "SELECT page_id,ordinal,config_hash,confirmation_hash,version_size,library,asset_id,checksum,size_bytes,filename,created_at,added_at,metadata_hash FROM provider_work_obligations LIMIT 0",
        "SELECT scope,config_hash,page_id,ordinal FROM provider_work_scan LIMIT 0",
    ] {
        conn.prepare(sql).unwrap();
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

#[test]
fn behavioral_preservation_tables_and_guards_are_present() {
    let dir = tempfile::tempdir().unwrap();
    let conn = create_state_db(dir.path(), "fixture@example.com");
    for table in [
        "unattributed_legacy",
        "unattributed_legacy_paths",
        "unattributed_legacy_proofs",
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "{table}");
    }
    let guards:i64=conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'unattributed_%'",[],|row|row.get(0)).unwrap();
    assert_eq!(guards, 30);
}
#[test]
fn retired_legacy_trust_plan_and_apply_refuse_before_archive_or_auth_writes() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let media = root.path().join("photos");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&media).unwrap();
    let username = "retired-trust@example.invalid";
    let conn = create_state_db(&data, username);
    let photo = media.join("existing.jpg");
    std::fs::write(&photo, b"retained synthetic media").unwrap();
    std::fs::write(media.join("existing.jpg.xmp"), b"retained custom metadata").unwrap();
    insert_asset(
        &conn,
        "existing",
        "downloaded",
        "existing.jpg",
        Some(photo.to_str().unwrap()),
        None,
        Some("old-local-proof"),
    );
    conn.execute("INSERT INTO metadata VALUES('trusted_legacy_file:v1:historical:receipt','immutable historical declaration')",[]).unwrap();
    conn.execute(
        "INSERT INTO metadata VALUES('sync_token:PrimarySync','old-cursor')",
        [],
    )
    .unwrap();
    drop(conn);
    let config = root.path().join("config.toml");
    crate::common::write_versioned_config(&config,format!("[auth]\nusername = {username:?}\npassword = 'synthetic-not-a-real-password'\n[download]\ndirectory = {:?}\n",media.to_str().unwrap())).unwrap();
    let plan = root.path().join("plan.json");
    let snapshot = || {
        crate::common::walkdir(root.path())
            .into_iter()
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
                (path, bytes, modified)
            })
            .collect::<Vec<_>>()
    };
    let before = snapshot();
    for args in [
        vec![
            "plan",
            "--library",
            "PrimarySync",
            "--output",
            plan.to_str().unwrap(),
        ],
        vec!["apply", "--plan", plan.to_str().unwrap()],
    ] {
        let output = clean_cmd()
            .env("KEI_DATA_DIR", &data)
            .env("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS", "1")
            .arg("--config")
            .arg(&config)
            .args(["reconcile", "trust-legacy-files"])
            .args(args)
            .assert()
            .failure()
            .get_output()
            .clone();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("Legacy file trust is no longer supported. Run kei sync. Existing trust records and media were not changed."),"{error}");
        assert!(!error.contains("Authenticating"), "{error}");
        assert!(
            !error.contains("Offline 2FA-required test seam enabled"),
            "{error}"
        );
        assert!(output.stdout.is_empty());
        assert_eq!(snapshot(), before);
        assert!(!plan.exists());
    }
}
