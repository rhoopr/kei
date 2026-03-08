//! Authentication tests — require valid iCloud credentials.
//!
//! These tests exercise `sync --auth-only` and `submit-code` against Apple's
//! real authentication servers. They are skipped when `ICLOUD_USERNAME` and
//! `ICLOUD_PASSWORD` are not set in the environment (or `.env`).

mod common;

use predicates::prelude::*;
use tempfile::tempdir;

// ── sync --auth-only ────────────────────────────────────────────────────

#[test]
fn auth_only_succeeds_with_valid_credentials() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .args([
            "sync",
            "--auth-only",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();

    // Session files should have been written to cookie dir
    let entries: Vec<_> = std::fs::read_dir(cookie_dir.path())
        .expect("read cookie dir")
        .collect();
    assert!(
        !entries.is_empty(),
        "cookie directory should contain session files"
    );
}

#[test]
fn auth_only_fails_with_bad_password() {
    let (username, _password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .args([
            "sync",
            "--auth-only",
            "--username",
            &username,
            "--password",
            "definitely-wrong-password-12345",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn auth_only_fails_with_bad_username() {
    let cookie_dir = tempdir().expect("failed to create tempdir");

    common::cmd()
        .args([
            "sync",
            "--auth-only",
            "--username",
            "nonexistent-user-abc123@icloud.com",
            "--password",
            "some-password",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

// ── submit-code ─────────────────────────────────────────────────────────

#[test]
fn submit_code_fails_with_invalid_code() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    // Submit a bogus code — should fail because it's not a valid 2FA code
    // (or because no 2FA is pending)
    common::cmd()
        .args([
            "submit-code",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "000000",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

#[test]
fn submit_code_fails_without_username() {
    // Clear env vars so the binary can't fall back on them
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args(["submit-code", "123456"])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

// ── auth-only via env vars ──────────────────────────────────────────────

#[test]
fn auth_only_via_env_vars() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    // Pass credentials only via env vars, no --username/--password flags
    common::cmd()
        .env("ICLOUD_USERNAME", &username)
        .env("ICLOUD_PASSWORD", &password)
        .args([
            "sync",
            "--auth-only",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();
}

// ── submit-code on already-authenticated session ────────────────────────

#[test]
fn submit_code_on_authenticated_session() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    // First authenticate normally
    common::cmd()
        .args([
            "sync",
            "--auth-only",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success();

    // Now submit-code on the already-authenticated session
    // Should print "Session is already authenticated." or succeed
    common::cmd()
        .args([
            "submit-code",
            "--username",
            &username,
            "--password",
            &password,
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "000000",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .success()
        .stdout(predicate::str::contains("already authenticated"));
}

// ── auth-only with --domain cn ──────────────────────────────────────────

#[test]
fn auth_only_with_wrong_domain_fails() {
    let (username, password) = match common::creds_or_skip() {
        Some(c) => c,
        None => {
            eprintln!("SKIP: no credentials");
            return;
        }
    };

    let cookie_dir = tempdir().expect("failed to create tempdir");

    // Use --domain cn with a non-Chinese iCloud account — should fail
    common::cmd()
        .args([
            "sync",
            "--auth-only",
            "--username",
            &username,
            "--password",
            &password,
            "--domain",
            "cn",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .failure();
}
