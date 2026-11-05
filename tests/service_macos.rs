//! macOS status integration tests for `kei service status`.
//!
//! The status probe reads LaunchAgent existence and, when a plist exists,
//! queries launchd. These tests use an isolated HOME with no plist.

#![cfg(target_os = "macos")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "service assertions and shared test helpers fail with panics and print failure diagnostics"
)]

mod common;

use predicates::prelude::*;
use std::time::Duration;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(20);

fn cmd_with_home(home: &TempDir) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    // Keep the probe on the temp directory and away from a real GUI domain.
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("XPC_SERVICE_NAME");
    cmd.env_remove("XPC_FLAGS");
    cmd.env("HOME", home.path());
    cmd
}

#[test]
fn service_status_with_no_plist_reports_not_installed() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Service: not installed"));
}
