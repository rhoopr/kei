//! Windows status integration tests for `kei service status`.
//!
//! The status probe queries an existing SCM registration. Gated to
//! Windows so the file is a no-op on linux/macOS hosts.

#![cfg(target_os = "windows")]
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

const TIMEOUT: Duration = Duration::from_secs(30);

fn cmd() -> assert_cmd::Command {
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    cmd
}

#[test]
fn status_on_host_without_kei_service_reports_not_installed_or_scm_unavailable() {
    // CI windows-latest runs as Administrator so SCM is reachable; the
    // status output for a host with no kei registered is "not
    // installed". Local non-elevated runs see "SCM unavailable" -- both
    // are valid for this test, since the assertion is "kei service
    // status returns 0 and emits a Service: line", not which verdict.
    cmd()
        .args(["service", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Service:"));
}
