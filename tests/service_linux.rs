//! Linux-specific integration tests for `kei install` / `kei uninstall`.
//!
//! Exercises the unit-file rendering pipeline end-to-end via `--dry-run`,
//! which prints the systemd unit without writing files or invoking
//! `systemctl` / `loginctl`. Faithful coverage of the daemon-reload /
//! enable / linger steps requires an active user session, so that stays
//! in the manual real-install path.

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
    // Scrub inherited environment so the test can't reach the developer's
    // real systemd user session: XDG_RUNTIME_DIR + DBUS_SESSION_BUS_ADDRESS
    // are how `systemctl --user` finds the live user-bus, so removing them
    // makes the (best-effort) disable / daemon-reload calls fail fast in
    // the tempdir instead of touching the host's running services.
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("XDG_RUNTIME_DIR");
    cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    cmd.env_remove("SUDO_USER");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", home.path().join(".config"));
    cmd
}

fn user_unit_path(home: &TempDir) -> std::path::PathBuf {
    home.path().join(".config/systemd/user/kei.service")
}

fn write_user_unit_fixture(home: &TempDir) -> std::path::PathBuf {
    let unit = user_unit_path(home);
    std::fs::create_dir_all(unit.parent().unwrap()).unwrap();
    std::fs::write(
        &unit,
        "[Unit]\nDescription=kei Media Sync Engine\n[Service]\nExecStart=/bin/true\n[Install]\nWantedBy=default.target\n",
    )
    .unwrap();
    unit
}

#[test]
fn dry_run_install_user_prints_unit_without_writing_file() {
    let home = TempDir::new().unwrap();
    let assert = cmd_with_home(&home)
        .args(["install", "--user", "--dry-run"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    let unit = user_unit_path(&home);
    assert!(
        !unit.exists(),
        "dry-run must not write unit file at {}",
        unit.display()
    );

    // Spot-check the load-bearing keys; the renderer's full shape is
    // covered by unit tests in src/service/linux.rs.
    assert!(stdout.contains("[Unit]"), "missing [Unit]:\n{stdout}");
    assert!(stdout.contains("[Service]"), "missing [Service]:\n{stdout}");
    assert!(stdout.contains("[Install]"), "missing [Install]:\n{stdout}");
    assert!(
        stdout.contains("Type=notify"),
        "missing Type=notify:\n{stdout}"
    );
    assert!(
        stdout.contains("Environment=MALLOC_ARENA_MAX=2"),
        "missing allocator arena limit:\n{stdout}"
    );
    assert!(
        stdout.contains("Description=kei Media Sync Engine"),
        "missing Description:\n{stdout}"
    );
    assert!(
        stdout.contains("WantedBy=default.target"),
        "missing WantedBy:\n{stdout}"
    );

    // ExecStart must point at an absolute path (the actual kei binary
    // canonicalized by current_executable). Verifying the prefix is
    // enough — the path itself is environment-dependent.
    assert!(
        stdout.contains("ExecStart=/") && stdout.contains(" service run --config "),
        "ExecStart not absolute or missing flags:\n{stdout}"
    );
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
    assert!(!user_unit_path(&home).exists());
}

#[test]
fn dry_run_install_user_does_not_write_to_xdg_config_home_override() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();
    let mut cmd = common::cmd();
    cmd.timeout(TIMEOUT);
    cmd.env_remove("XDG_RUNTIME_DIR");
    cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    cmd.env_remove("SUDO_USER");
    cmd.env("HOME", home.path());
    cmd.env("XDG_CONFIG_HOME", xdg.path());

    cmd.args(["install", "--user", "--dry-run"])
        .assert()
        .success();

    let xdg_unit = xdg.path().join("systemd/user/kei.service");
    let home_unit = home.path().join(".config/systemd/user/kei.service");
    assert!(
        !xdg_unit.exists(),
        "dry-run must not write unit under XDG_CONFIG_HOME at {}",
        xdg_unit.display()
    );
    assert!(
        !home_unit.exists(),
        "dry-run must not write unit under $HOME at {}",
        home_unit.display()
    );
}

#[test]
fn dry_run_install_system_prints_unit_without_requiring_root() {
    let home = TempDir::new().unwrap();
    let mut cmd = cmd_with_home(&home);
    cmd.env("USER", "kei-preview-user");

    let assert = cmd
        .args(["install", "--system", "--dry-run"])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains("User=kei-preview-user"),
        "system dry-run must show target user:\n{stdout}"
    );
    assert!(
        stdout.contains("WantedBy=multi-user.target"),
        "system dry-run must render system install target:\n{stdout}"
    );
}

#[test]
fn dry_run_install_system_rejects_root_preview_user() {
    let home = TempDir::new().unwrap();
    let mut cmd = cmd_with_home(&home);
    cmd.env("USER", "root");
    cmd.env_remove("LOGNAME");

    cmd.args(["install", "--system", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Could not determine which user the system service would run as",
        ));
}

#[test]
fn dry_run_install_system_rejects_control_character_preview_user() {
    let home = TempDir::new().unwrap();
    let mut cmd = cmd_with_home(&home);
    cmd.env("SUDO_USER", "alice\nEnvironment=KEI_INJECTED=1");
    cmd.env("USER", "root");
    cmd.env_remove("LOGNAME");

    let assert = cmd
        .args(["install", "--system", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Could not determine which user the system service would run as",
        ));
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        !stdout.contains("Environment=KEI_INJECTED"),
        "system dry-run must not render injected unit directives:\n{stdout}"
    );
}

#[test]
fn install_system_without_root_fails_clearly() {
    // CI runs as a non-root user, which is exactly the condition this
    // test wants to exercise. Skipping when EUID==0 (rare local-dev
    // case) keeps the assertion meaningful without false negatives.
    // SAFETY: stateless POSIX FFI call, no memory-safety preconditions.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipping non-root assertion: running as root");
        return;
    }

    let home = TempDir::new().unwrap();
    cmd_with_home(&home)
        .args(["install", "--system"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("must be run as root"));
}

#[test]
fn uninstall_removes_unit_file() {
    let home = TempDir::new().unwrap();
    let unit = write_user_unit_fixture(&home);

    // Bare `uninstall` (no --purge) should remove the unit file. It
    // also tries `systemctl --user disable --now`, which is a no-op /
    // failure in a tempdir-only environment — the function swallows
    // that and proceeds to delete the file.
    cmd_with_home(&home).arg("uninstall").assert().success();
    assert!(!unit.exists(), "uninstall should remove {}", unit.display());
}

#[test]
fn uninstall_with_no_unit_files_is_a_clean_no_op() {
    let home = TempDir::new().unwrap();
    cmd_with_home(&home).arg("uninstall").assert().success();
    assert!(!user_unit_path(&home).exists());
}

#[test]
fn uninstall_purge_removes_kei_state_dir_when_present() {
    let home = TempDir::new().unwrap();
    let kei_dir = home.path().join(".config/kei");
    std::fs::create_dir_all(&kei_dir).unwrap();
    std::fs::write(kei_dir.join("config.toml"), "[auth]\n").unwrap();
    std::fs::write(kei_dir.join("state.db"), b"\x00").unwrap();

    // Install a unit so uninstall has something to do too — but the
    // assertion is on the purge path.
    write_user_unit_fixture(&home);

    cmd_with_home(&home)
        .args(["uninstall", "--purge"])
        .assert()
        .success();

    assert!(
        !kei_dir.exists(),
        "--purge should remove ~/.config/kei (was at {})",
        kei_dir.display()
    );
}

#[test]
fn uninstall_purge_clears_encrypted_credential_file() {
    // --purge must wipe stored credentials, not just the on-disk state.
    // The encrypted-file backend is the only one we can exercise
    // hermetically: the keyring backend would otherwise dispatch through
    // libsecret to the dev's real OS keyring. cmd_with_home scrubs
    // DBUS_SESSION_BUS_ADDRESS specifically so that backend can't reach
    // anything live, leaving CredentialStore::delete to land its
    // file-side cleanup inside the tempdir.
    let home = TempDir::new().unwrap();
    let kei_dir = home.path().join(".config/kei");
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
        "--purge should remove the encrypted credential file"
    );
    assert!(
        !kei_dir.exists(),
        "--purge should also remove the kei state directory"
    );
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
