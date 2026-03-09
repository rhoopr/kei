//! State-management tests that require network credentials.
//!
//! Exercises status, reset-state, verify, import-existing, and retry-failed
//! against real iCloud data. Requires pre-authentication via
//! `cargo test --test setup_auth -- --ignored`.

mod common;

use predicates::prelude::*;
use std::path::Path;

const TIMEOUT_SYNC: u64 = 180;
const TIMEOUT_CMD: u64 = 30;

// ── Command builders ──────────────────────────────────────────────────

fn sync_cmd(
    username: &str,
    password: &str,
    cookie_dir: &Path,
    dir: &Path,
    recent: u32,
) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "sync",
        "--recent",
        &recent.to_string(),
        "--username",
        username,
        "--password",
        password,
        "--cookie-directory",
        cookie_dir.to_str().unwrap(),
        "--directory",
        dir.to_str().unwrap(),
        "--no-progress-bar",
    ]);
    cmd
}

fn status_cmd(username: &str, cookie_dir: &Path) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "status",
        "--username",
        username,
        "--cookie-directory",
        cookie_dir.to_str().unwrap(),
    ]);
    cmd
}

fn reset_state_cmd(username: &str, cookie_dir: &Path) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "reset-state",
        "--username",
        username,
        "--cookie-directory",
        cookie_dir.to_str().unwrap(),
    ]);
    cmd
}

fn verify_cmd(username: &str, cookie_dir: &Path) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "verify",
        "--username",
        username,
        "--cookie-directory",
        cookie_dir.to_str().unwrap(),
    ]);
    cmd
}

fn import_cmd(
    username: &str,
    password: &str,
    cookie_dir: &Path,
    dir: &Path,
) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "import-existing",
        "--username",
        username,
        "--password",
        password,
        "--cookie-directory",
        cookie_dir.to_str().unwrap(),
        "--directory",
        dir.to_str().unwrap(),
    ]);
    cmd
}

fn retry_failed_cmd(
    username: &str,
    password: &str,
    cookie_dir: &Path,
    dir: &Path,
) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "retry-failed",
        "--username",
        username,
        "--password",
        password,
        "--cookie-directory",
        cookie_dir.to_str().unwrap(),
        "--directory",
        dir.to_str().unwrap(),
        "--no-progress-bar",
    ]);
    cmd
}

fn db_file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "db")
        })
        .count()
}

// ══════════════════════════════════════════════════════════════════════════
//  STATUS
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn status_after_sync_shows_counts() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 2)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        status_cmd(&username, &cookie_dir)
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success()
            .stdout(
                predicate::str::contains("State Database:")
                    .and(predicate::str::contains("Assets:"))
                    .and(predicate::str::contains("Total:"))
                    .and(predicate::str::contains("Downloaded:"))
                    .and(predicate::str::contains("Pending:"))
                    .and(predicate::str::contains("Failed:"))
                    .and(predicate::str::contains("Last sync started:")),
            );
    });
}

// ══════════════════════════════════════════════════════════════════════════
//  RESET-STATE
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn reset_state_deletes_db_after_sync() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        assert!(
            db_file_count(cookie_dir.as_path()) > 0,
            "expected .db file after sync"
        );

        reset_state_cmd(&username, &cookie_dir)
            .arg("--yes")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success()
            .stdout(predicate::str::contains("State database deleted"));

        assert_eq!(
            db_file_count(cookie_dir.as_path()),
            0,
            "DB file should be deleted after reset-state"
        );
    });
}

#[test]
fn reset_state_without_yes_does_not_delete() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        let count_before = db_file_count(cookie_dir.as_path());
        assert!(count_before > 0, "expected .db file after sync");

        // No --yes and no stdin — should not delete
        // (stdin is /dev/null in subprocess, so read_line returns empty → "N")
        reset_state_cmd(&username, &cookie_dir)
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success()
            .stdout(predicate::str::contains("Cancelled"));

        assert_eq!(
            db_file_count(cookie_dir.as_path()),
            count_before,
            "DB should not be deleted without --yes"
        );
    });
}

// ══════════════════════════════════════════════════════════════════════════
//  VERIFY
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn verify_after_sync_reports_results() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        // Clear stale DB entries from prior test runs (may not exist yet)
        let _ = reset_state_cmd(&username, &cookie_dir)
            .arg("--yes")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert();

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 2)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        verify_cmd(&username, &cookie_dir)
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success()
            .stdout(
                predicate::str::contains("Verifying")
                    .and(predicate::str::contains("Results:"))
                    .and(predicate::str::contains("Verified:")),
            );
    });
}

#[test]
fn verify_checksums_after_sync() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        // Clear stale DB entries from prior test runs (may not exist yet)
        let _ = reset_state_cmd(&username, &cookie_dir)
            .arg("--yes")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert();

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        verify_cmd(&username, &cookie_dir)
            .arg("--checksums")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success()
            .stdout(predicate::str::contains("Verified:"));
    });
}

#[test]
fn verify_detects_missing_files() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        // Clear stale DB entries from prior test runs (may not exist yet)
        let _ = reset_state_cmd(&username, &cookie_dir)
            .arg("--yes")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert();

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        for entry in common::walkdir(download_dir.path()) {
            std::fs::remove_file(&entry).ok();
        }

        verify_cmd(&username, &cookie_dir)
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .failure()
            .stdout(
                predicate::str::contains("MISSING")
                    .and(predicate::str::contains("Missing:"))
                    .and(predicate::str::contains("Results:")),
            );
    });
}

#[test]
fn verify_checksums_detects_corruption() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        // Clear stale DB entries from prior test runs (may not exist yet)
        let _ = reset_state_cmd(&username, &cookie_dir)
            .arg("--yes")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert();

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .args(["--skip-videos"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(!files.is_empty(), "need at least one file to corrupt");
        std::fs::write(&files[0], b"CORRUPTED DATA").expect("corrupt file");

        verify_cmd(&username, &cookie_dir)
            .arg("--checksums")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .failure()
            .stdout(
                predicate::str::contains("CORRUPTED").and(predicate::str::contains("Corrupted:")),
            );
    });
}

// ══════════════════════════════════════════════════════════════════════════
//  IMPORT-EXISTING
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn import_existing_with_nonexistent_directory_fails() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "import-existing",
                "--username",
                &username,
                "--password",
                &password,
                "--cookie-directory",
                cookie_dir.to_str().unwrap(),
                "--directory",
                "/nonexistent/path/that/does/not/exist",
            ])
            .timeout(std::time::Duration::from_secs(60))
            .assert()
            .failure()
            .stderr(predicate::str::contains("does not exist"));
    });
}

#[test]
fn import_existing_matches_synced_files() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 2)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(!files.is_empty(), "expected files from sync");

        reset_state_cmd(&username, &cookie_dir)
            .arg("--yes")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success();

        import_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--recent", "5"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success()
            .stdout(predicate::str::contains("Import complete:"));

        status_cmd(&username, &cookie_dir)
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success()
            .stdout(predicate::str::contains("Downloaded:"));
    });
}

#[test]
fn import_existing_empty_directory_reports_zero_matches() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        import_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--recent", "5"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success()
            .stdout(
                predicate::str::contains("Import complete:")
                    .and(predicate::str::contains("Total assets scanned:"))
                    .and(predicate::str::contains("Files matched:"))
                    .and(predicate::str::contains("Unmatched versions:")),
            );
    });
}

#[test]
fn import_existing_custom_folder_structure() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .args(["--folder-structure", "%Y"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(!files.is_empty(), "expected files from sync");

        reset_state_cmd(&username, &cookie_dir)
            .arg("--yes")
            .timeout(std::time::Duration::from_secs(TIMEOUT_CMD))
            .assert()
            .success();

        import_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--folder-structure", "%Y", "--recent", "5"])
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success()
            .stdout(predicate::str::contains("Import complete:"));
    });
}

// ══════════════════════════════════════════════════════════════════════════
//  RETRY-FAILED
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn retry_failed_after_successful_sync_is_noop() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        retry_failed_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(std::time::Duration::from_secs(60))
            .assert()
            .success();
    });
}

#[test]
fn retry_failed_with_no_db_succeeds() {
    let Some((username, password, cookie_dir)) = common::require_preauth() else {
        return;
    };

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        retry_failed_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();
    });
}
