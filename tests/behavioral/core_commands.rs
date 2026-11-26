//! Offline commands on sync-core state: status, verify, reconcile, reset and
//! manifest. State is created the way users get it, by upgrading a
//! previous-engine file, and then given rows directly.

use super::{clean_cmd, create_state_db, download_config_args, insert_asset, sanitize_username};

const USER: &str = "core-commands@example.com";

/// Upgrade an empty previous-engine file with `status` and return the
/// sync-core state file.
fn core_state(dir: &std::path::Path) -> std::path::PathBuf {
    drop(create_state_db(dir, USER));
    kei(dir, &["status"]).assert().success();
    let path = dir.join(format!("{}.sync.db", sanitize_username(USER)));
    assert!(path.exists(), "status should upgrade the previous state");
    path
}

fn kei(dir: &std::path::Path, args: &[&str]) -> assert_cmd::Command {
    let mut cmd = clean_cmd();
    cmd.env("ICLOUD_USERNAME", USER)
        .env("KEI_DATA_DIR", dir)
        .args(download_config_args(dir))
        .args(args);
    cmd
}

fn add_file(
    state: &std::path::Path,
    path: &str,
    state_name: &str,
    sha256: Option<&str>,
    error: Option<&str>,
) {
    let conn = rusqlite::Connection::open(state).unwrap();
    conn.execute(
        "INSERT INTO files (zone, asset, rendition, album, path, size, checksum, sha256, state, last_error)
         VALUES ('PrimarySync', ?1, 'original', '', ?1, 4, 'AQID', ?2, ?3, ?4)",
        rusqlite::params![path, sha256, state_name, error],
    )
    .unwrap();
}

fn file_state(state: &std::path::Path, path: &str) -> (String, Option<String>) {
    rusqlite::Connection::open(state)
        .unwrap()
        .query_row(
            "SELECT state, sha256 FROM files WHERE path = ?1",
            [path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

fn stdout(cmd: &mut assert_cmd::Command) -> String {
    String::from_utf8(cmd.assert().success().get_output().stdout.clone()).unwrap()
}

#[test]
fn status_reports_backup_counts_and_lists_each_section() {
    let dir = tempfile::tempdir().unwrap();
    let state = core_state(dir.path());
    add_file(&state, "a.jpg", "done", Some(&sha256_hex(b"aaaa")), None);
    add_file(&state, "b.jpg", "pending", None, None);
    add_file(
        &state,
        "c.jpg",
        "failed",
        None,
        Some("iCloud refused the download URL twice"),
    );
    add_file(
        &state,
        "d.jpg",
        "modified",
        Some(&sha256_hex(b"dddd")),
        None,
    );
    let conn = rusqlite::Connection::open(&state).unwrap();
    conn.execute_batch(
        "INSERT INTO zones (zone, position, listed) VALUES ('PrimarySync', 'p9', 1);
         INSERT INTO runs (started_at, finished_at, outcome, downloaded, failed, pending)
             VALUES (1700000000, 1700000060, 'incomplete', 1, 1, 1);",
    )
    .unwrap();

    let out = stdout(&mut kei(
        dir.path(),
        &["status", "--failed", "--pending", "--downloaded"],
    ));
    for expected in [
        "Saved:            1",
        "Owed:             1",
        "Failed:           1",
        "Locally modified: 1",
        "PrimarySync: caught up",
        "Last sync finished:",
        "incomplete; 1 downloaded, 1 failed",
        "Backup record: last sync status is incomplete; 1 failed copy remains; 1 owed copy remains",
        "  c.jpg - iCloud refused the download URL twice",
        "Pending assets:\n  b.jpg",
        "Downloaded assets:\n  a.jpg",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }
}

#[test]
fn status_counts_copies_carried_over_from_the_previous_state() {
    let dir = tempfile::tempdir().unwrap();
    let media = dir.path().join("kept.jpg");
    std::fs::write(&media, b"kept").unwrap();
    let conn = create_state_db(dir.path(), USER);
    insert_asset(
        &conn,
        "a1",
        "downloaded",
        "kept.jpg",
        Some(media.to_str().unwrap()),
        None,
        Some(&sha256_hex(b"kept")),
    );
    drop(conn);
    let legacy = dir.path().join(format!("{}.db", sanitize_username(USER)));
    let before = std::fs::read(&legacy).unwrap();

    let out = stdout(&mut kei(dir.path(), &["status", "--downloaded"]));
    assert!(out.contains("Carried over:     1"), "{out}");
    assert!(out.contains("  kept.jpg (carried over)"), "{out}");
    assert_eq!(
        std::fs::read(&legacy).unwrap(),
        before,
        "the previous state stays untouched"
    );

    let out = stdout(&mut kei(dir.path(), &["verify", "--checksums"]));
    assert!(out.contains("Checksum verified:              1"), "{out}");
}

#[test]
fn reconcile_owes_missing_copies_again_and_dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let state = core_state(dir.path());
    std::fs::write(dir.path().join("present.jpg"), b"here").unwrap();
    add_file(
        &state,
        "present.jpg",
        "done",
        Some(&sha256_hex(b"here")),
        None,
    );
    add_file(&state, "gone.jpg", "done", Some(&sha256_hex(b"gone")), None);

    let out = stdout(&mut kei(dir.path(), &["reconcile", "--dry-run"]));
    assert!(
        out.contains("MISSING:") && out.contains("gone.jpg"),
        "{out}"
    );
    assert!(out.contains("Dry run: 1 missing files"), "{out}");
    assert_eq!(file_state(&state, "gone.jpg").0, "done");

    let out = stdout(&mut kei(dir.path(), &["reconcile"]));
    assert!(
        out.contains("1 missing files will be downloaded again"),
        "{out}"
    );
    assert_eq!(file_state(&state, "gone.jpg"), ("pending".to_owned(), None));
    assert_eq!(file_state(&state, "present.jpg").0, "done");
}

#[test]
fn reconcile_confirms_damage_only_for_the_inspected_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let state = core_state(dir.path());
    let media = dir.path().join("damaged.jpg");
    std::fs::write(&media, b"bad!").unwrap();
    add_file(
        &state,
        "damaged.jpg",
        "done",
        Some(&sha256_hex(b"good")),
        None,
    );
    let path = media.to_str().unwrap();

    let out = stdout(&mut kei(dir.path(), &["reconcile", "--inspect-path", path]));
    let token = format!("{}:4", sha256_hex(b"bad!"));
    assert!(out.contains(&format!("Fingerprint: {token}")), "{out}");

    kei(
        dir.path(),
        &[
            "reconcile",
            "--inspect-path",
            path,
            "--confirm-damage",
            "stale:4",
        ],
    )
    .assert()
    .failure()
    .stderr(predicates::str::contains("File changed since inspection"));
    assert_eq!(file_state(&state, "damaged.jpg").0, "done");

    stdout(&mut kei(
        dir.path(),
        &[
            "reconcile",
            "--inspect-path",
            path,
            "--confirm-damage",
            &token,
        ],
    ));
    assert_eq!(
        file_state(&state, "damaged.jpg"),
        ("pending".to_owned(), Some(sha256_hex(b"bad!")))
    );
    assert_eq!(
        std::fs::read(&media).unwrap(),
        b"bad!",
        "reconcile never touches the file"
    );
}

#[test]
fn reset_sync_token_forgets_positions_and_keeps_copies() {
    let dir = tempfile::tempdir().unwrap();
    let state = core_state(dir.path());
    add_file(&state, "a.jpg", "done", None, None);
    rusqlite::Connection::open(&state)
        .unwrap()
        .execute_batch(
            "INSERT INTO zones (zone, position, listed) VALUES ('PrimarySync', 'p9', 1);",
        )
        .unwrap();

    let out = stdout(&mut kei(dir.path(), &["reset", "sync-token", "--yes"]));
    assert!(out.contains("Cleared sync tokens (1 zone token)"), "{out}");
    let position: Option<String> = rusqlite::Connection::open(&state)
        .unwrap()
        .query_row("SELECT position FROM zones", [], |row| row.get(0))
        .unwrap();
    assert_eq!(position, None);
    assert_eq!(file_state(&state, "a.jpg").0, "done");
}

#[test]
fn reset_state_deletes_previous_and_current_state() {
    let dir = tempfile::tempdir().unwrap();
    let state = core_state(dir.path());
    let legacy = dir.path().join(format!("{}.db", sanitize_username(USER)));
    assert!(legacy.exists() && state.exists());
    let out = stdout(&mut kei(dir.path(), &["reset", "state", "--yes"]));
    assert!(out.contains("State database deleted."), "{out}");
    assert!(!legacy.exists() && !state.exists());
}

#[test]
fn manifest_exports_every_copy() {
    let dir = tempfile::tempdir().unwrap();
    let state = core_state(dir.path());
    add_file(
        &state,
        "Trip, 2024/a.jpg",
        "done",
        Some(&sha256_hex(b"aaaa")),
        None,
    );
    let json: serde_json::Value =
        serde_json::from_str(&stdout(&mut kei(dir.path(), &["manifest"]))).unwrap();
    assert_eq!(json[0]["local_path"], "Trip, 2024/a.jpg");
    assert_eq!(json[0]["status"], "done");
    assert_eq!(json[0]["selected"], true);
    let csv = stdout(&mut kei(dir.path(), &["manifest", "--format", "csv"]));
    assert!(csv.contains("\"Trip, 2024/a.jpg\""), "{csv}");
}
