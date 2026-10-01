//! Cross-platform CLI parsing tests for `kei install`, `kei uninstall`,
//! and `kei service {run,status}`.
//!
//! Tests in this file must hold on every target. Platform-specific
//! behavior (actually writing a unit file or invoking systemctl) lives
//! in `tests/service_linux.rs` and analogous suites for the other
//! platforms. The "not yet implemented" assertions are gated to
//! targets without a backend so they auto-deactivate as each platform
//! ships.

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

const TIMEOUT: Duration = Duration::from_secs(10);

fn cmd() -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    cmd
}

// ── Help output ─────────────────────────────────────────────────────────

#[test]
fn install_help_lists_user_and_system_flags() {
    cmd()
        .args(["install", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--user").and(predicate::str::contains("--system")));
}

#[test]
fn uninstall_help_lists_purge_flag() {
    cmd()
        .args(["uninstall", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--purge"));
}

#[test]
fn service_help_lists_run_and_status() {
    cmd()
        .args(["service", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("run").and(predicate::str::contains("status")));
}

#[test]
fn service_run_help_inherits_runtime_sync_flags() {
    // `kei service run` shares the reduced runtime SyncArgs. Durable
    // settings are TOML-only and should not be exposed here.
    cmd()
        .args(["service", "run", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--recent"))
        .stdout(predicate::str::contains("--download-dir").not())
        .stdout(predicate::str::contains("--threads").not());
}

#[test]
fn service_status_help_renders_without_panic() {
    // `Status` is a unit variant with no flags of its own. The assertion
    // is just "clap renders help and exits 0" -- defends against an
    // accidental enum-shape change that would break help generation.
    cmd()
        .args(["service", "status", "--help"])
        .assert()
        .success();
}

#[test]
fn top_level_help_lists_install_uninstall_service() {
    cmd().arg("--help").assert().success().stdout(
        predicate::str::contains("install")
            .and(predicate::str::contains("uninstall"))
            .and(predicate::str::contains("service")),
    );
}

// ── Argument parsing ────────────────────────────────────────────────────

#[test]
fn install_user_and_system_are_mutually_exclusive() {
    cmd()
        .args(["install", "--user", "--system"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot be used with"));
}

// ── Stub error contract (non-Linux only) ────────────────────────────────
//
// On targets where the platform backend has shipped, these tests
// auto-deactivate via cfg. They guard the macOS / Windows stubs until
// those backends land.

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[test]
fn install_returns_clean_not_implemented_error() {
    cmd()
        .arg("install")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[test]
fn uninstall_returns_clean_not_implemented_error() {
    cmd()
        .arg("uninstall")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
#[test]
fn service_status_returns_clean_not_implemented_error() {
    cmd()
        .args(["service", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not yet implemented"));
}
