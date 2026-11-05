//! Cross-platform CLI parsing tests for `kei service {run,status}`.
//!
//! Platform-specific status behavior lives in `tests/service_linux.rs`
//! and analogous suites for macOS and Windows.

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
fn top_level_help_keeps_service_and_drops_install_commands() {
    cmd().arg("--help").assert().success().stdout(
        predicate::str::contains("service")
            .and(predicate::str::contains("install").not())
            .and(predicate::str::contains("uninstall").not()),
    );
}

#[test]
fn removed_install_commands_do_not_parse() {
    for command in ["install", "uninstall"] {
        cmd()
            .arg(command)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unrecognized subcommand"));
    }
}
