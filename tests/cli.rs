//! Pure CLI-parsing tests — no network, no credentials required.
//!
//! Validates that every subcommand, flag, and enum value is accepted or
//! rejected by the argument parser as expected.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod common;

use predicates::prelude::*;

/// Visible subcommands shown in `--help`.
const ALL_SUBCOMMANDS: &[&str] = &[
    "sync",
    "login",
    "list",
    "password",
    "reset",
    "config",
    "status",
    "import-existing",
    "verify",
];

/// Subcommands that accept `--password` (have PasswordArgs).
const PASSWORD_SUBCOMMANDS: &[&str] = &["sync", "login", "import-existing"];

// ── Help output ─────────────────────────────────────────────────────────

#[test]
fn help_flag_succeeds() {
    common::cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("kei: photo sync engine"));
}

#[test]
fn help_lists_all_subcommands() {
    assert!(
        !ALL_SUBCOMMANDS.is_empty(),
        "ALL_SUBCOMMANDS must not be empty"
    );
    let assert = common::cmd().arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    for sub in ALL_SUBCOMMANDS {
        assert!(
            stdout.contains(sub),
            "help output missing subcommand `{sub}`"
        );
    }
}

#[test]
fn sync_help_succeeds() {
    common::cmd()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--directory"));
}

#[test]
fn sync_help_lists_sync_token_flags() {
    let assert = common::cmd().args(["sync", "--help"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        stdout.contains("--no-incremental"),
        "sync help missing --no-incremental"
    );
    // --reset-sync-token is now a hidden compat flag (use `kei reset sync-token`)
}

#[test]
fn status_help_succeeds() {
    common::cmd()
        .args(["status", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--failed"));
}

#[test]
fn reset_state_help_succeeds() {
    common::cmd()
        .args(["reset-state", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn import_existing_help_succeeds() {
    common::cmd()
        .args(["import-existing", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--directory"));
}

#[test]
fn verify_help_succeeds() {
    common::cmd()
        .args(["verify", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--checksums"));
}

#[test]
fn get_code_help_succeeds() {
    // get-code is a hidden legacy alias for `login get-code`
    common::cmd()
        .args(["get-code", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("login get-code"));
}

#[test]
fn submit_code_help_succeeds() {
    common::cmd()
        .args(["submit-code", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2FA"));
}

#[test]
fn retry_failed_help_succeeds() {
    common::cmd()
        .args(["retry-failed", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--directory"));
}

// ── Invalid subcommand / unknown flags ──────────────────────────────────

#[test]
fn unknown_subcommand_fails() {
    common::cmd()
        .arg("frobnicate")
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn unknown_flag_fails() {
    common::cmd()
        .args(["--nonexistent-flag"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn unknown_flag_on_subcommand_fails() {
    common::cmd()
        .args(["sync", "--bogus-flag"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn unknown_flag_on_status_fails() {
    common::cmd()
        .args(["status", "--bogus-flag"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

// ── Global flags ────────────────────────────────────────────────────────

#[test]
fn log_level_debug_accepted() {
    // Just parsing — the binary will fail at runtime without creds, but
    // the exit code for "bad args" is 2, not 1. A non-2 exit means parsing
    // succeeded. We use --auth-only to short-circuit into auth, which will
    // fail gracefully without credentials.
    common::cmd()
        .args(["--log-level", "debug", "--help"])
        .assert()
        .success();
}

#[test]
fn log_level_invalid_rejected() {
    common::cmd()
        .args(["--log-level", "verbose"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn config_flag_accepted() {
    common::cmd()
        .args(["--config", "/nonexistent/config.toml", "--help"])
        .assert()
        .success();
}

// ── Short flag aliases ──────────────────────────────────────────────────

#[test]
fn short_u_flag_accepted() {
    common::cmd()
        .args(["sync", "-u", "x@x.com", "--help"])
        .assert()
        .success();
}

#[test]
fn short_p_flag_accepted() {
    common::cmd()
        .args(["sync", "-u", "x@x.com", "-p", "secret", "--help"])
        .assert()
        .success();
}

#[test]
fn short_d_flag_accepted() {
    common::cmd()
        .args(["sync", "-d", "/tmp", "--help"])
        .assert()
        .success();
}

#[test]
fn short_l_flag_accepted() {
    common::cmd()
        .args(["sync", "-l", "--help"])
        .assert()
        .success();
}

#[test]
fn short_a_flag_accepted() {
    common::cmd()
        .args(["sync", "-a", "Favorites", "--help"])
        .assert()
        .success();
}

#[test]
fn short_y_flag_on_reset_state() {
    common::cmd()
        .args(["reset-state", "-y", "--help"])
        .assert()
        .success();
}

// ── Enum validation (rejection only — acceptance covered by unit tests) ─

#[test]
fn size_rejects_invalid_variant() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--size", "huge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn domain_rejects_invalid() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--domain", "uk"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn live_photo_size_rejects_invalid() {
    common::cmd()
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--live-photo-size",
            "xlarge",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn live_photo_mov_filename_policy_rejects_invalid() {
    common::cmd()
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--live-photo-mov-filename-policy",
            "custom",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn align_raw_rejects_invalid() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--align-raw", "bogus"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn file_match_policy_rejects_invalid() {
    common::cmd()
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--file-match-policy",
            "random",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

// ── Numeric validation (rejection only — acceptance covered by unit tests)

#[test]
fn threads_num_rejects_zero() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--threads-num", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

// ── submit-code requires positional CODE ────────────────────────────────

#[test]
fn submit_code_requires_code_argument() {
    common::cmd()
        .args(["submit-code", "--username", "x@x.com"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn submit_code_accepts_code_argument() {
    common::cmd()
        .args(["submit-code", "--help"])
        .assert()
        .success();
}

// ── import-existing requires --directory ─────────────────────────────────

#[test]
fn import_existing_requires_directory() {
    common::cmd()
        .args(["import-existing", "--username", "x@x.com"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--directory is required"));
}

// ── Boolean flags are accepted ──────────────────────────────────────────

#[test]
fn boolean_sync_flags_accepted() {
    let mut flags = vec![
        "--auth-only",
        "--list-albums",
        "--list-libraries",
        "--skip-videos",
        "--skip-photos",
        "--skip-live-photos",
        "--force-size",
        "--dry-run",
        "--no-progress-bar",
        "--keep-unicode-in-filenames",
        "--notify-systemd",
        "--no-incremental",
        "--reset-sync-token",
    ];
    if cfg!(feature = "xmp") {
        flags.push("--set-exif-datetime");
    }
    for flag in flags {
        common::cmd()
            .args(["sync", flag, "--help"])
            .assert()
            .success();
    }
}

// ── Value flags are accepted ────────────────────────────────────────────

#[test]
fn value_sync_flags_accepted() {
    let pairs = [
        ("--directory", "/tmp"),
        ("--folder-structure", "%Y-%m"),
        ("--recent", "10"),
        ("--threads-num", "4"),
        ("--watch-with-interval", "3600"),
        ("--max-retries", "5"),
        ("--retry-delay", "10"),
        ("--temp-suffix", ".downloading"),
        ("--skip-created-before", "2024-01-01"),
        ("--skip-created-after", "2025-01-01"),
        ("--pid-file", "/tmp/test.pid"),
        ("--notification-script", "/tmp/notify.sh"),
        ("--library", "SharedSync-ABC"),
        ("--cookie-directory", "/tmp/cookies"),
    ];
    for (flag, value) in pairs {
        common::cmd()
            .args(["sync", flag, value, "--help"])
            .assert()
            .success();
    }
}

#[test]
fn album_flag_accepts_multiple() {
    common::cmd()
        .args([
            "sync",
            "--album",
            "Favorites",
            "--album",
            "Vacation",
            "--help",
        ])
        .assert()
        .success();
}

// ── Default command (no subcommand = sync) ──────────────────────────────

#[test]
fn bare_invocation_with_username_and_directory_parses() {
    // With --help to avoid actually running
    common::cmd()
        .args(["--username", "x@x.com", "--directory", "/photos", "--help"])
        .assert()
        .success();
}

// ── Global flags work with all subcommands ──────────────────────────────

#[test]
fn config_global_flag_works_with_all_subcommands() {
    for sub in ALL_SUBCOMMANDS {
        common::cmd()
            .args([sub, "--config", "/custom/config.toml", "--help"])
            .assert()
            .success();
    }
}

#[test]
fn log_level_global_flag_works_with_all_subcommands() {
    for sub in ALL_SUBCOMMANDS {
        common::cmd()
            .args([sub, "--log-level", "warn", "--help"])
            .assert()
            .success();
    }
}

// ── import-existing subcommand-specific flags ───────────────────────────

#[test]
fn import_existing_folder_structure_flag() {
    common::cmd()
        .args([
            "import-existing",
            "--directory",
            "/tmp",
            "--folder-structure",
            "%Y-%m",
            "--help",
        ])
        .assert()
        .success();
}

#[test]
fn import_existing_recent_flag() {
    common::cmd()
        .args([
            "import-existing",
            "--directory",
            "/tmp",
            "--recent",
            "100",
            "--help",
        ])
        .assert()
        .success();
}

// ── Env var credential passthrough ──────────────────────────────────────

#[test]
fn username_from_env_var() {
    // The binary reads ICLOUD_USERNAME from the environment. Verify parsing
    // succeeds when the env var is set instead of --username.
    common::cmd()
        .env("ICLOUD_USERNAME", "envuser@example.com")
        .args(["sync", "--help"])
        .assert()
        .success();
}

#[test]
fn password_from_env_var() {
    common::cmd()
        .env("ICLOUD_PASSWORD", "env-secret")
        .args(["sync", "--help"])
        .assert()
        .success();
}

// ── --config with explicit nonexistent path ─────────────────────────────

#[test]
fn config_explicit_nonexistent_path_fails_at_runtime() {
    // When the user explicitly sets --config to a path that doesn't exist
    // (not the default), the binary should fail at runtime.
    let output = common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "--config",
            "/nonexistent/explicit/config.toml",
            "status",
            "--username",
            "x@x.com",
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .failure()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config") || stderr.contains("nonexistent"),
        "error should mention the config file, stderr:\n{stderr}"
    );
}

// ── Auth flags on non-sync subcommands ──────────────────────────────────

#[test]
fn domain_flag_works_on_status() {
    common::cmd()
        .args(["status", "--domain", "cn", "--help"])
        .assert()
        .success();
}

#[test]
fn cookie_directory_flag_works_on_verify() {
    common::cmd()
        .args(["verify", "--cookie-directory", "/tmp/cookies", "--help"])
        .assert()
        .success();
}

#[test]
fn password_flag_works_on_submit_code() {
    common::cmd()
        .args(["submit-code", "-p", "secret", "123456", "--help"])
        .assert()
        .success();
}

// ── Global flags before subcommand ──────────────────────────────────────

#[test]
fn log_level_before_subcommand() {
    common::cmd()
        .args(["--log-level", "error", "sync", "--help"])
        .assert()
        .success();
}

// ── --only-print-filenames ──────────────────────────────────────────────

#[test]
fn only_print_filenames_visible_in_help() {
    common::cmd()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--only-print-filenames"));
}

// ── import-existing short -d flag ───────────────────────────────────────

#[test]
fn import_existing_short_d_flag() {
    common::cmd()
        .args(["import-existing", "-d", "/tmp", "--help"])
        .assert()
        .success();
}

// ── Unknown flags on all subcommands ────────────────────────────────────

#[test]
fn unknown_flag_on_all_subcommands_fails() {
    for sub in ALL_SUBCOMMANDS {
        common::cmd()
            .args([sub, "--bogus-flag"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("error"));
    }
}

// ── Auth flags accepted on all subcommands ──────────────────────────

#[test]
fn auth_flags_accepted_on_all_subcommands() {
    // Global flags (--username, --domain, --data-dir) work on all subcommands
    for sub in ALL_SUBCOMMANDS {
        for (flag, value) in [
            ("--username", "x@x.com"),
            ("--domain", "com"),
            ("--data-dir", "/tmp/data"),
        ] {
            common::cmd()
                .args([sub, flag, value, "--help"])
                .assert()
                .success();
        }
    }
    // --password only accepted on commands with PasswordArgs
    for sub in PASSWORD_SUBCOMMANDS {
        common::cmd()
            .args([sub, "--password", "secret", "--help"])
            .assert()
            .success();
    }
}

// ── retry-failed shares sync flags ──────────────────────────────────

#[test]
fn retry_failed_accepts_sync_flags() {
    common::cmd()
        .args([
            "retry-failed",
            "--directory",
            "/tmp",
            "--recent",
            "10",
            "--skip-videos",
            "--threads-num",
            "2",
            "--help",
        ])
        .assert()
        .success();
}

#[test]
fn retry_failed_accepts_sync_token_flags() {
    common::cmd()
        .args([
            "retry-failed",
            "--no-incremental",
            "--reset-sync-token",
            "--help",
        ])
        .assert()
        .success();
}

#[test]
fn no_incremental_and_reset_sync_token_together() {
    common::cmd()
        .args(["sync", "--no-incremental", "--reset-sync-token", "--help"])
        .assert()
        .success();
}

// ── Version flag ────────────────────────────────────────────────────────

#[test]
fn version_flag() {
    common::cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

// ── import-existing --no-progress-bar ───────────────────────────────────

#[test]
fn import_existing_accepts_no_progress_bar() {
    common::cmd()
        .args(["import-existing", "--no-progress-bar", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-progress-bar"));
}

// ── exit codes ────────────────────────────────────────────────────────

/// Exit code 0 for --help.
#[test]
fn exit_code_0_on_help() {
    common::cmd().arg("--help").assert().code(0);
}

/// Exit code 0 for --version.
#[test]
fn exit_code_0_on_version() {
    common::cmd().arg("--version").assert().code(0);
}

/// Exit code 1 (generic failure) when --username is missing.
#[test]
fn exit_code_1_on_missing_username() {
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "sync",
            "--directory",
            "/tmp/claude/exit-code-test",
            "--cookie-directory",
            "/tmp/claude/exit-code-cookies",
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--username is required"));
}

/// Exit code 3 (auth failure) when password file is empty.
///
/// An empty password file triggers `AuthError::FailedLogin("Password
/// provider returned no data")`, which maps to EXIT_AUTH (3).
#[test]
fn exit_code_3_on_empty_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("empty-password");
    std::fs::write(&pw_file, "").unwrap();

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "sync",
            "--username",
            "exit-code-test@example.com",
            "--directory",
            "/tmp/claude/exit-code-test",
            "--cookie-directory",
            dir.path().to_str().unwrap(),
            "--password-file",
            pw_file.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .code(3)
        .stderr(predicate::str::contains("No password available"));
}

/// Exit code 3 (auth failure) when password file contains only a newline.
#[test]
fn exit_code_3_on_newline_only_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("newline-password");
    std::fs::write(&pw_file, "\n").unwrap();

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "sync",
            "--username",
            "exit-code-test@example.com",
            "--directory",
            "/tmp/claude/exit-code-test",
            "--cookie-directory",
            dir.path().to_str().unwrap(),
            "--password-file",
            pw_file.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .code(3)
        .stderr(predicate::str::contains("No password available"));
}

/// Exit code 2 (clap validation error) for invalid argument values.
#[test]
fn exit_code_2_on_invalid_argument() {
    common::cmd()
        .args(["sync", "--username", ""])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("value must not be empty"));
}

// ── New subcommand help ────────────────────────────────────────────────

#[test]
fn login_help_succeeds() {
    common::cmd()
        .args(["login", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("get-code"))
        .stdout(predicate::str::contains("submit-code"));
}

#[test]
fn login_get_code_help_succeeds() {
    common::cmd()
        .args(["login", "get-code", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2FA"));
}

#[test]
fn login_submit_code_help_succeeds() {
    common::cmd()
        .args(["login", "submit-code", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("CODE"));
}

#[test]
fn login_submit_code_requires_code() {
    common::cmd()
        .args(["login", "submit-code"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn list_help_succeeds() {
    common::cmd()
        .args(["list", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("albums"))
        .stdout(predicate::str::contains("libraries"));
}

#[test]
fn list_albums_help_succeeds() {
    common::cmd()
        .args(["list", "albums", "--help"])
        .assert()
        .success();
}

#[test]
fn list_libraries_help_succeeds() {
    common::cmd()
        .args(["list", "libraries", "--help"])
        .assert()
        .success();
}

#[test]
fn password_help_succeeds() {
    common::cmd()
        .args(["password", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("set"))
        .stdout(predicate::str::contains("clear"))
        .stdout(predicate::str::contains("backend"));
}

#[test]
fn reset_help_succeeds() {
    common::cmd()
        .args(["reset", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("state"))
        .stdout(predicate::str::contains("sync-token"));
}

#[test]
fn reset_state_new_help_succeeds() {
    common::cmd()
        .args(["reset", "state", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn reset_sync_token_help_succeeds() {
    common::cmd()
        .args(["reset", "sync-token", "--help"])
        .assert()
        .success();
}

#[test]
fn config_help_succeeds() {
    common::cmd()
        .args(["config", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("show"))
        .stdout(predicate::str::contains("setup"));
}

#[test]
fn config_show_help_succeeds() {
    common::cmd()
        .args(["config", "show", "--help"])
        .assert()
        .success();
}

#[test]
fn config_setup_help_succeeds() {
    common::cmd()
        .args(["config", "setup", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--output"));
}

#[test]
fn sync_retry_failed_help_succeeds() {
    common::cmd()
        .args(["sync", "--retry-failed", "--help"])
        .assert()
        .success();
}

// ── Hidden legacy commands not in help ─────────────────────────────────

#[test]
fn hidden_legacy_commands_not_in_help() {
    let assert = common::cmd().arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    // These hyphenated names only appeared as top-level subcommands in
    // the old CLI. They should now be hidden.
    // Note: "retry-failed" excluded because --retry-failed is a visible
    // flag on sync (flattened at top level).
    for hidden in ["get-code", "submit-code", "reset-state", "reset-sync-token"] {
        assert!(
            !stdout.contains(hidden),
            "help output should not list hidden command `{hidden}`"
        );
    }
}

// ── Deprecation warnings ──────────────────────────────────────────────

#[test]
fn legacy_credential_backend_prints_deprecation_warning() {
    let output = common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args(["credential", "backend", "--username", "x@x.com"])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deprecated"),
        "legacy command should print deprecation warning, stderr:\n{stderr}"
    );
}

#[test]
fn legacy_reset_state_prints_deprecation_warning() {
    let output = common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args(["reset-state", "--username", "x@x.com", "--yes"])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("deprecated"),
        "legacy command should print deprecation warning, stderr:\n{stderr}"
    );
}

// ── --retry-failed conflicts ──────────────────────────────────────────

#[test]
fn retry_failed_conflicts_with_dry_run() {
    common::cmd()
        .args(["sync", "--retry-failed", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn retry_failed_conflicts_with_watch() {
    common::cmd()
        .args(["sync", "--retry-failed", "--watch-with-interval", "300"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

// ── --data-dir global ─────────────────────────────────────────────────

#[test]
fn data_dir_flag_accepted() {
    common::cmd()
        .args(["sync", "--data-dir", "/tmp/data", "--help"])
        .assert()
        .success();
}

#[test]
fn data_dir_global_works_with_subcommands() {
    for sub in ALL_SUBCOMMANDS {
        common::cmd()
            .args([sub, "--data-dir", "/tmp/data", "--help"])
            .assert()
            .success();
    }
}

// ── KEI_* env vars ────────────────────────────────────────────────────

#[test]
fn kei_directory_env_var_accepted() {
    common::cmd()
        .env("KEI_DIRECTORY", "/photos")
        .args(["sync", "--help"])
        .assert()
        .success();
}

#[test]
fn kei_data_dir_env_var_accepted() {
    common::cmd()
        .env("KEI_DATA_DIR", "/data")
        .args(["sync", "--help"])
        .assert()
        .success();
}

#[test]
fn kei_log_level_env_var_accepted() {
    common::cmd()
        .env("KEI_LOG_LEVEL", "debug")
        .args(["sync", "--help"])
        .assert()
        .success();
}

// ── config show produces TOML ─────────────────────────────────────────

#[test]
fn config_show_produces_toml_output() {
    let output = common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "config",
            "show",
            "--username",
            "test@example.com",
            "--data-dir",
            "/tmp",
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[auth]") && stdout.contains("test@example.com"),
        "config show should produce TOML with username, got:\n{stdout}"
    );
}

// ── reset with no DB prints message ───────────────────────────────────

#[test]
fn reset_sync_token_no_db_prints_message() {
    let dir = tempfile::tempdir().unwrap();
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "reset",
            "sync-token",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn reset_state_no_db_prints_message() {
    let dir = tempfile::tempdir().unwrap();
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args([
            "reset",
            "state",
            "--yes",
            "--username",
            "test@example.com",
            "--data-dir",
            dir.path().to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

// ── submit-code validation ─────────────────────────────────────────────

#[test]
fn submit_code_fails_without_username() {
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args(["submit-code", "123456"])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .failure()
        .stderr(predicate::str::contains("error").or(predicate::str::contains("required")));
}

// ── --report-json ────────────────────────────────────────────────────

#[test]
fn report_json_flag_accepted() {
    common::cmd()
        .args(["sync", "--report-json", "/tmp/report.json", "--help"])
        .assert()
        .success();
}

#[test]
fn report_json_visible_in_help() {
    common::cmd()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--report-json"));
}
