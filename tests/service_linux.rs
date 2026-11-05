//! Linux status integration tests for `kei service status`.
//!
//! The status probe reads unit-file existence and, when a unit exists,
//! queries systemd. These tests use an isolated HOME with no unit file.

#![cfg(target_os = "linux")]
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
    // Keep the probe on the temp directory and away from any real user bus.
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("XDG_RUNTIME_DIR");
    cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    cmd.env_remove("SUDO_USER");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd
}

#[test]
fn service_status_with_no_unit_reports_not_installed() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Service: not installed"));
}
