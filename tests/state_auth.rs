//! State-management tests that require network credentials (live iCloud API).
//!
//! Exercises status, reset state, verify, import-existing, and retry-failed
//! against real iCloud data. All tests are `#[ignore]` — run with:
//!
//! ```sh
//! cargo test --test state_auth -- --ignored --test-threads=1
//! ```

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
use std::path::Path;
use std::time::Duration;
use tempfile::tempdir;

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
        "--data-dir",
        cookie_dir.to_str().unwrap(),
        "--directory",
        dir.to_str().unwrap(),
        "--no-progress-bar",
        "--no-incremental",
    ]);
    cmd
}

fn status_cmd(username: &str, cookie_dir: &Path) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "status",
        "--username",
        username,
        "--data-dir",
        cookie_dir.to_str().unwrap(),
    ]);
    cmd
}

fn reset_state_cmd(username: &str, cookie_dir: &Path) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.args([
        "reset",
        "state",
        "--username",
        username,
        "--data-dir",
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
        "--data-dir",
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
        "--data-dir",
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
        "sync",
        "--retry-failed",
        "--username",
        username,
        "--password",
        password,
        "--data-dir",
        cookie_dir.to_str().unwrap(),
        "--directory",
        dir.to_str().unwrap(),
        "--no-progress-bar",
        "--log-level",
        "info",
    ]);
    cmd
}

fn db_file_count(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
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
#[ignore]
fn status_after_sync_shows_counts() {
    let (username, password, cookie_dir) = common::require_preauth();

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
//  RESET STATE
// ══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore]
fn reset_state_deletes_db_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn reset_state_without_yes_does_not_delete() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn verify_after_sync_reports_results() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn verify_checksums_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn verify_detects_missing_files() {
    let (username, password, cookie_dir) = common::require_preauth();

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

        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "need files to delete for missing-file test"
        );
        for entry in files {
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
#[ignore]
fn verify_checksums_detects_corruption() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn import_existing_with_nonexistent_directory_fails() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "import-existing",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
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
#[ignore]
fn import_existing_matches_synced_files() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn import_existing_empty_directory_reports_zero_matches() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn import_existing_custom_folder_structure() {
    let (username, password, cookie_dir) = common::require_preauth();

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
#[ignore]
fn retry_failed_after_successful_sync_is_noop() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 1)
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        let assertion = retry_failed_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(std::time::Duration::from_secs(60))
            .assert()
            .success();

        let stderr = String::from_utf8_lossy(&assertion.get_output().stderr);
        assert!(
            stderr.contains("No failed assets to retry"),
            "retry-failed after successful sync should report no failures, stderr:\n{stderr}"
        );
    });
}

#[test]
#[ignore]
fn retry_failed_with_no_db_succeeds() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempfile::tempdir().expect("failed to create download dir");

        let output = retry_failed_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success()
            .get_output()
            .clone();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("No failed assets to retry"),
            "retry-failed with no DB should report nothing to retry, stderr:\n{stderr}"
        );
    });
}

// ══════════════════════════════════════════════════════════════════════════
//  DRY-RUN SIDE EFFECTS
// ══════════════════════════════════════════════════════════════════════════

/// Verify that --dry-run does NOT create a state DB or store sync tokens.
/// A dry run must be side-effect-free so that a subsequent real sync
/// still performs full enumeration and downloads all photos.
#[test]
#[ignore]
fn dry_run_does_not_create_state_db() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let isolated_cookies = tempfile::tempdir().expect("tempdir for isolated cookies");

        // Copy session/cookie files from shared cookie dir so we can auth
        // without contaminating the shared state DB. Skip .db files (state
        // databases) since we're testing that dry-run doesn't create one.
        for entry in std::fs::read_dir(&cookie_dir).expect("read cookie dir") {
            let entry = entry.expect("dir entry");
            let src = entry.path();
            if src.is_file()
                && !src
                    .extension()
                    .is_some_and(|ext| ext == "db" || ext == "db-wal" || ext == "db-shm")
            {
                let dest = isolated_cookies.path().join(entry.file_name());
                std::fs::copy(&src, &dest).expect("copy cookie file");
            }
        }

        let download_dir = tempfile::tempdir().expect("tempdir for downloads");

        // Run a dry-run sync
        sync_cmd(
            &username,
            &password,
            isolated_cookies.path(),
            download_dir.path(),
            2,
        )
        .args(["--dry-run"])
        .timeout(std::time::Duration::from_secs(TIMEOUT_SYNC))
        .assert()
        .success();

        // Verify no .db file was created
        let db_files: Vec<_> = std::fs::read_dir(isolated_cookies.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext == "db")
            })
            .collect();

        assert!(
            db_files.is_empty(),
            "--dry-run should not create a state DB, found: {:?}",
            db_files.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    });
}

// ══════════════════════════════════════════════════════════════════════════
//  RESET SYNC-TOKEN
// ══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore]
fn reset_sync_token_forces_full_enumeration() {
    let (username, password, cookie_dir) = common::require_preauth();
    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        // First sync to populate sync tokens
        sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 2)
            .timeout(Duration::from_secs(TIMEOUT_SYNC))
            .assert()
            .success();

        // Reset sync tokens
        common::cmd()
            .args([
                "reset",
                "sync-token",
                "--username",
                &username,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
            ])
            .timeout(Duration::from_secs(10))
            .assert()
            .success()
            .stdout(predicate::str::contains("Cleared sync tokens"));

        // Second sync should do full enumeration (no stored token)
        let output = sync_cmd(&username, &password, &cookie_dir, download_dir.path(), 2)
            .args(["--log-level", "debug"])
            .timeout(Duration::from_secs(TIMEOUT_SYNC))
            .output()
            .unwrap();
        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("full enumeration") || stderr.contains("No sync token found"),
            "after reset, sync should do full enumeration, stderr: {stderr}"
        );
    });
}

// ══════════════════════════════════════════════════════════════════════════
//  CONFIG SHOW
// ══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore]
fn config_show_after_sync() {
    let (username, _password, cookie_dir) = common::require_preauth();
    // config show doesn't need auth, just needs username resolution
    common::cmd()
        .args([
            "config",
            "show",
            "--username",
            &username,
            "--data-dir",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains(&username))
        .stdout(predicate::str::contains("[auth]"));
}

// ══════════════════════════════════════════════════════════════════════════
//  LOGIN
// ══════════════════════════════════════════════════════════════════════════

#[test]
#[ignore]
fn login_with_existing_session() {
    let (username, password, cookie_dir) = common::require_preauth();
    common::with_auth_retry(|| {
        common::cmd()
            .args([
                "login",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                cookie_dir.to_str().unwrap(),
            ])
            .timeout(Duration::from_secs(60))
            .assert()
            .success();
    });
}
