//! Auth error path tests that make live HTTPS calls to Apple.
//!
//! Separated from cli.rs (offline-only) so coverage runs stay
//! deterministic and don't depend on Apple endpoint availability.
//!
//! Run with: `cargo test --test auth`

mod common;

use predicates::prelude::*;

#[test]
fn bad_credentials_fails() {
    let cookie_dir = tempfile::tempdir().expect("tempdir");
    let download_dir = tempfile::tempdir().expect("tempdir");

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "sync",
            "--username",
            "nonexistent-xyz@icloud.com",
            "--password",
            "wrong-password",
            "--cookie-directory",
            cookie_dir.path().to_str().unwrap(),
            "--directory",
            download_dir.path().to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(std::time::Duration::from_secs(60))
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("error")
                .or(predicate::str::contains("Error"))
                .or(predicate::str::contains("ERROR")),
        );
}
