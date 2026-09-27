//! macOS-specific integration tests for `kei install` / `kei uninstall`.
//!
//! Exercises the plist rendering pipeline end-to-end via `--dry-run`,
//! which prints the launchd property list without writing files or
//! invoking `launchctl`. Faithful coverage of the bootstrap / bootout /
//! load-fallback path requires a live launchd GUI domain, so that stays
//! in the manual real-install path.

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
    // Scrub inherited environment so the test can't reach the developer's
    // real launchd GUI domain. `XPC_*` and the bootstrap port are how
    // launchctl finds the live domain; clearing them makes the (best-effort)
    // bootout / bootstrap calls fail fast in the tempdir instead of
    // touching the host's running services.
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("XPC_SERVICE_NAME");
    cmd.env_remove("XPC_FLAGS");
    cmd.env("HOME", home.path());
    cmd
}

fn user_plist_path(home: &TempDir) -> std::path::PathBuf {
    home.path()
        .join("Library/LaunchAgents/com.rhoopr.kei.plist")
}

fn user_log_dir(home: &TempDir) -> std::path::PathBuf {
    home.path().join("Library/Logs/kei")
}

fn kei_state_dir(home: &TempDir) -> std::path::PathBuf {
    home.path().join(".config/kei")
}

fn write_user_plist_fixture(home: &TempDir) -> std::path::PathBuf {
    let plist = user_plist_path(home);
    std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::fs::write(
        &plist,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.rhoopr.kei</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/kei</string>
    <string>service</string>
    <string>run</string>
  </array>
</dict>
</plist>
"#,
    )
    .unwrap();
    plist
}

#[test]
fn dry_run_install_user_prints_plist_without_writing_file() {
    let home = TempDir::new().unwrap();
    let assert = cmd_with_home(&home)
        .args(["install", "--user", "--dry-run"])
        .assert()
        .success();
    let output = assert.get_output();

    let plist_path = user_plist_path(&home);
    assert!(
        !plist_path.exists(),
        "dry-run must not write plist at {}",
        plist_path.display(),
    );
    assert!(
        !user_log_dir(&home).exists(),
        "dry-run must not create {}",
        user_log_dir(&home).display(),
    );

    // Spot-check via the plist crate so we're asserting the parsed
    // structure rather than substring-matching XML. The renderer's
    // detailed shape is covered by unit tests in src/service/macos.rs.
    let dict: plist::Dictionary =
        plist::from_bytes(&output.stdout).expect("plist must parse as a dictionary");
    assert_eq!(
        dict.get("Label").and_then(|v| v.as_string()),
        Some("com.rhoopr.kei"),
    );
    assert_eq!(
        dict.get("RunAtLoad").and_then(|v| v.as_boolean()),
        Some(true),
    );

    let args = dict
        .get("ProgramArguments")
        .and_then(|v| v.as_array())
        .expect("ProgramArguments must be an array");
    let strings: Vec<&str> = args.iter().filter_map(|v| v.as_string()).collect();
    assert!(
        strings.first().map(|s| s.starts_with('/')).unwrap_or(false),
        "ProgramArguments[0] must be absolute, got {strings:?}",
    );
    assert!(
        strings.contains(&"service") && strings.contains(&"run") && strings.contains(&"--config"),
        "ProgramArguments must carry `service run --config`: {strings:?}",
    );

    assert!(!output.stdout.is_empty(), "dry-run must print plist XML");
}

#[test]
fn dry_run_install_user_is_repeatable_without_writing_file() {
    let home = TempDir::new().unwrap();
    for _ in 0..2 {
        cmd_with_home(&home)
            .args(["install", "--user", "--dry-run"])
            .assert()
            .success();
    }
    assert!(!user_plist_path(&home).exists());
    assert!(!user_log_dir(&home).exists());
}

#[test]
fn install_system_is_rejected_with_clear_message() {
    // macOS deliberately does not ship a LaunchDaemon path.
    // The user-facing error must point operators at `--user` rather than
    // silently downgrading.
    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["install", "--system", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("per-user only"));
}

#[test]
fn uninstall_removes_plist() {
    let home = TempDir::new().unwrap();
    let plist_path = write_user_plist_fixture(&home);

    // Bare `uninstall` (no --purge) should remove the plist file. It
    // also tries `launchctl bootout`, which will fail in a
    // tempdir-only environment without an active GUI domain — the
    // function swallows that and proceeds to delete the file.
    cmd_with_home(&home).arg("uninstall").assert().success();
    assert!(
        !plist_path.exists(),
        "uninstall should remove {}",
        plist_path.display(),
    );
}

#[test]
fn uninstall_with_no_plist_is_a_clean_no_op() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home).arg("uninstall").assert().success();
    assert!(!user_plist_path(&home).exists());
}

#[test]
fn uninstall_purge_removes_kei_state_dir_when_present() {
    let home = TempDir::new().unwrap();
    let kei_dir = kei_state_dir(&home);
    std::fs::create_dir_all(&kei_dir).unwrap();
    std::fs::write(kei_dir.join("config.toml"), "[auth]\n").unwrap();
    std::fs::write(kei_dir.join("state.db"), b"\x00").unwrap();

    // Install a plist so uninstall has something to do too — but the
    // assertion is on the purge path.
    write_user_plist_fixture(&home);

    cmd_with_home(&home)
        .args(["uninstall", "--purge"])
        .assert()
        .success();

    assert!(
        !kei_dir.exists(),
        "--purge should remove ~/.config/kei (was at {})",
        kei_dir.display(),
    );
}

#[test]
fn uninstall_purge_clears_encrypted_credential_file() {
    // --purge must wipe stored credentials, not just the on-disk state.
    // The encrypted-file backend is the only one we can exercise
    // hermetically: the keychain backend would otherwise dispatch
    // through the macOS Security framework to the dev's real keychain.
    let home = TempDir::new().unwrap();
    let kei_dir = kei_state_dir(&home);
    std::fs::create_dir_all(&kei_dir).unwrap();
    std::fs::write(
        kei_dir.join("config.toml"),
        "[auth]\nusername = \"kei-purge-test@example.invalid\"\n",
    )
    .unwrap();
    let cred_file = kei_dir.join("credentials.enc");
    std::fs::write(&cred_file, b"opaque-ciphertext-bytes").unwrap();

    cmd_with_home(&home)
        .args(["uninstall", "--purge"])
        .assert()
        .success();

    assert!(
        !cred_file.exists(),
        "--purge should remove the encrypted credential file",
    );
    assert!(
        !kei_dir.exists(),
        "--purge should also remove the kei state directory",
    );
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
