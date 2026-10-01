//! Integration coverage for the `Service:` section in `kei status`.
//!
//! These tests run on every host the rest of the suite runs on (linux,
//! macOS, Windows) because the Service line is the first line of
//! `kei status` output regardless of platform. The platform-native
//! detail (`systemd user`, `launchd user`, `windows scm`,
//! `running in container (...)`) is determined at runtime by the
//! per-platform `service_state()` and is not asserted here -- the
//! per-platform integration suites (`service_linux.rs` /
//! `service_macos.rs` / `service_windows.rs`) cover that.
//!
//! What we do guarantee here:
//!
//! - `kei status` against a host with no kei service registered
//!   (the test harness's CI runner) prints a `Service:` line and exits
//!   successfully.
//! - The Service line is emitted even when no state DB exists, so
//!   `kei install` followed by `kei status` works on a fresh host.

// `mod common` pulls in tests/common/mod.rs which uses eprintln!, unwrap,
// expect, and panic; the file-level allow propagates into that module so
// the shared test harness keeps compiling without per-call attributes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "service assertions and shared test helpers fail with panics and print failure diagnostics"
)]

mod common;

use std::time::Duration;

use predicates::prelude::*;
use tempfile::TempDir;

const TIMEOUT: Duration = Duration::from_secs(20);

/// Returns a `kei status` command bound to a fresh tempdir, with the
/// username pinned to a placeholder so the resolver doesn't fail.
/// `kei status` only uses the username to derive the per-account state
/// DB path -- no network call is made -- so any non-empty value works.
fn status_cmd(tmp: &TempDir) -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT)
        .env("ICLOUD_USERNAME", "service-status-test@example.invalid")
        .env("KEI_DATA_DIR", tmp.path())
        .arg("status");
    cmd
}

#[test]
fn status_prints_service_line_with_no_state_db() {
    // Fresh tempdir = no state.db. Status must still succeed and lead
    // with a Service: line, since the user just ran `kei install` and
    // is checking whether the service registered.
    let tmp = TempDir::new().expect("tempdir");
    status_cmd(&tmp)
        .assert()
        .success()
        .stdout(predicate::str::contains("Service:"));
}

#[test]
fn status_emits_service_line_before_assets_section() {
    // The Service line must come before the State Database / Assets
    // block; an out-of-order emission would mean the section was wired
    // into the wrong branch of run_status.
    let tmp = TempDir::new().expect("tempdir");
    let assert = status_cmd(&tmp).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    let service_idx = stdout
        .find("Service:")
        .expect("Service: line must be present in status output");
    // No state DB on a fresh tempdir, so the next section is the
    // "No state database found" notice rather than the Assets block.
    let next_idx = stdout
        .find("No state database found")
        .or_else(|| stdout.find("State Database:"))
        .expect("status must surface either a missing-DB notice or the State Database line");
    assert!(
        service_idx < next_idx,
        "Service line ({service_idx}) must come before the DB section ({next_idx}); got:\n{stdout}",
    );
}
