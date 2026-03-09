//! State-management tests that do NOT require network credentials.
//!
//! Tests that need credentials live in `state_auth.rs`.

mod common;

use predicates::prelude::*;
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
