//! State-management tests that require network credentials.
//!
//! Exercises status, reset-state, verify, import-existing, and retry-failed
//! against real iCloud data. Requires pre-authentication via
//! `cargo test --test setup_auth -- --ignored`.

mod common;

use predicates::prelude::*;

// ══════════════════════════════════════════════════════════════════════════
//  STATUS
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn status_after_sync_shows_counts() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Run a small sync to populate the DB
    common::cmd()
        .args([
            "sync",
            "--recent",
            "2",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success();

    // Now run status and check all output fields
    common::cmd()
        .args([
            "status",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
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
}

// ══════════════════════════════════════════════════════════════════════════
//  RESET-STATE
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn reset_state_deletes_db_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync to create a DB
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify DB exists
    let db_files: Vec<_> = std::fs::read_dir(cookie_dir.as_path())
        .expect("read cookie dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "db")
        })
        .collect();
    assert!(!db_files.is_empty(), "expected .db file after sync");

    // Reset state with --yes
    common::cmd()
        .args([
            "reset-state",
            "--yes",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("State database deleted"));

    // Verify DB is gone
    let db_files_after: Vec<_> = std::fs::read_dir(cookie_dir.as_path())
        .expect("read cookie dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "db")
        })
        .collect();
    assert!(
        db_files_after.is_empty(),
        "DB file should be deleted after reset-state"
    );
}

#[test]
fn reset_state_without_yes_does_not_delete() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync to create a DB
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Count DB files before
    let db_count_before = std::fs::read_dir(cookie_dir.as_path())
        .expect("read cookie dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "db")
        })
        .count();
    assert!(db_count_before > 0, "expected .db file after sync");

    // Reset state WITHOUT --yes and no stdin — should not delete
    // (stdin is /dev/null in subprocess, so read_line returns empty → "N")
    common::cmd()
        .args([
            "reset-state",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("Cancelled"));

    // DB should still exist
    let db_count_after = std::fs::read_dir(cookie_dir.as_path())
        .expect("read cookie dir")
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "db")
        })
        .count();
    assert_eq!(
        db_count_before, db_count_after,
        "DB should not be deleted without --yes"
    );
}

// ══════════════════════════════════════════════════════════════════════════
//  VERIFY
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn verify_after_sync_reports_results() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync a couple files
    common::cmd()
        .args([
            "sync",
            "--recent",
            "2",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success();

    // Verify — files are still on disk so should all pass
    common::cmd()
        .args([
            "verify",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Verifying")
                .and(predicate::str::contains("Results:"))
                .and(predicate::str::contains("Verified:")),
        );
}

#[test]
fn verify_checksums_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Verify with checksums
    common::cmd()
        .args([
            "verify",
            "--checksums",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .success()
        .stdout(predicate::str::contains("Verified:"));
}

#[test]
fn verify_detects_missing_files() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Delete all downloaded files but keep the DB
    for entry in walkdir(download_dir.path()) {
        std::fs::remove_file(&entry).ok();
    }

    // Verify should now detect missing files and exit with code 1
    common::cmd()
        .args([
            "verify",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .failure()
        .stdout(
            predicate::str::contains("MISSING")
                .and(predicate::str::contains("Missing:"))
                .and(predicate::str::contains("Results:")),
        );
}

#[test]
fn verify_checksums_detects_corruption() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--skip-videos",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // Corrupt a downloaded file by overwriting it
    let files = walkdir(download_dir.path());
    assert!(!files.is_empty(), "need at least one file to corrupt");
    std::fs::write(&files[0], b"CORRUPTED DATA").expect("corrupt file");

    // Verify --checksums should detect the corruption
    common::cmd()
        .args([
            "verify",
            "--checksums",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .failure()
        .stdout(predicate::str::contains("CORRUPTED").and(predicate::str::contains("Corrupted:")));
}

// ══════════════════════════════════════════════════════════════════════════
//  IMPORT-EXISTING
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn import_existing_with_nonexistent_directory_fails() {
    let (username, password, cookie_dir) = common::require_preauth();

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
}

#[test]
fn import_existing_matches_synced_files() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync a few files first
    common::cmd()
        .args([
            "sync",
            "--recent",
            "2",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(180))
        .assert()
        .success();

    let files = walkdir(download_dir.path());
    assert!(!files.is_empty(), "expected files from sync");

    // Delete the state DB but keep the downloaded files
    common::cmd()
        .args([
            "reset-state",
            "--yes",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success();

    // Now import-existing should match those files
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
            download_dir.path().to_str().unwrap(),
            "--recent",
            "5",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success()
        .stdout(predicate::str::contains("Import complete:"));

    // Status should now show downloaded assets
    common::cmd()
        .args([
            "status",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("Downloaded:"));
}

#[test]
fn import_existing_empty_directory_reports_zero_matches() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

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
            download_dir.path().to_str().unwrap(),
            "--recent",
            "5",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Import complete:")
                .and(predicate::str::contains("Total assets scanned:"))
                .and(predicate::str::contains("Files matched:"))
                .and(predicate::str::contains("Unmatched versions:")),
        );
}

#[test]
fn import_existing_custom_folder_structure() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync with a custom folder structure
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--folder-structure",
            "%Y",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    let files = walkdir(download_dir.path());
    assert!(!files.is_empty(), "expected files from sync");

    // Delete the state DB
    common::cmd()
        .args([
            "reset-state",
            "--yes",
            "--username",
            &username,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success();

    // Import with matching folder structure
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
            download_dir.path().to_str().unwrap(),
            "--folder-structure",
            "%Y",
            "--recent",
            "5",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success()
        .stdout(predicate::str::contains("Import complete:"));
}

// ══════════════════════════════════════════════════════════════════════════
//  RETRY-FAILED
// ══════════════════════════════════════════════════════════════════════════

#[test]
fn retry_failed_after_successful_sync_is_noop() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // Sync successfully
    common::cmd()
        .args([
            "sync",
            "--recent",
            "1",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();

    // retry-failed should be a no-op since nothing failed
    common::cmd()
        .args([
            "retry-failed",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();
}

#[test]
fn retry_failed_with_no_db_succeeds() {
    let (username, password, cookie_dir) = common::require_preauth();
    let download_dir = tempfile::tempdir().expect("failed to create download dir");

    // retry-failed with no prior sync — DB will be created fresh with
    // zero failed assets, so it should report "No failed assets to retry"
    common::cmd()
        .args([
            "retry-failed",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(120))
        .assert()
        .success();
}

// ── Helper ──────────────────────────────────────────────────────────────

/// Recursively collect all regular files under `dir`.
fn walkdir(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files
}
