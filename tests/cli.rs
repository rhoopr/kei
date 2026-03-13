//! Pure CLI-parsing tests — no network, no credentials required.
//!
//! Validates that every subcommand, flag, and enum value is accepted or
//! rejected by the argument parser as expected.

mod common;

use predicates::prelude::*;

const ALL_SUBCOMMANDS: &[&str] = &[
    "sync",
    "status",
    "reset-state",
    "verify",
    "retry-failed",
    "submit-code",
    "import-existing",
];

// ── Help output ─────────────────────────────────────────────────────────

#[test]
fn help_flag_succeeds() {
    common::cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Download iCloud photos and videos",
        ));
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
    assert!(
        stdout.contains("--reset-sync-token"),
        "sync help missing --reset-sync-token"
    );
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

// ── Enum validation ─────────────────────────────────────────────────────

#[test]
fn size_accepts_all_valid_variants() {
    for variant in ["original", "medium", "thumb", "adjusted", "alternative"] {
        common::cmd()
            .args(["sync", "--size", variant, "--help"])
            .assert()
            .success();
    }
}

#[test]
fn size_rejects_invalid_variant() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--size", "huge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn domain_accepts_com_and_cn() {
    for variant in ["com", "cn"] {
        common::cmd()
            .args(["sync", "--domain", variant, "--help"])
            .assert()
            .success();
    }
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
fn live_photo_size_accepts_valid() {
    for variant in ["original", "medium", "thumb"] {
        common::cmd()
            .args(["sync", "--live-photo-size", variant, "--help"])
            .assert()
            .success();
    }
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
fn live_photo_mov_filename_policy_accepts_valid() {
    for variant in ["suffix", "original"] {
        common::cmd()
            .args([
                "sync",
                "--live-photo-mov-filename-policy",
                variant,
                "--help",
            ])
            .assert()
            .success();
    }
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
fn align_raw_accepts_valid() {
    for variant in ["as-is", "original", "alternative"] {
        common::cmd()
            .args(["sync", "--align-raw", variant, "--help"])
            .assert()
            .success();
    }
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
fn file_match_policy_accepts_valid() {
    for variant in ["name-size-dedup-with-suffix", "name-id7"] {
        common::cmd()
            .args(["sync", "--file-match-policy", variant, "--help"])
            .assert()
            .success();
    }
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

// ── Threads-num validation ──────────────────────────────────────────────

#[test]
fn threads_num_rejects_zero() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--threads-num", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn threads_num_rejects_negative() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--threads-num", "-1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn threads_num_accepts_positive() {
    common::cmd()
        .args(["sync", "--threads-num", "5", "--help"])
        .assert()
        .success();
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
        .stderr(predicate::str::contains("error"));
}

// ── Boolean flags are accepted ──────────────────────────────────────────

#[test]
fn boolean_sync_flags_accepted() {
    for flag in [
        "--auth-only",
        "--list-albums",
        "--list-libraries",
        "--skip-videos",
        "--skip-photos",
        "--skip-live-photos",
        "--force-size",
        "--set-exif-datetime",
        "--dry-run",
        "--no-progress-bar",
        "--keep-unicode-in-filenames",
        "--notify-systemd",
        "--no-incremental",
        "--reset-sync-token",
    ] {
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

// ── Hidden flags ────────────────────────────────────────────────────────

#[test]
fn only_print_filenames_hidden_flag_accepted() {
    common::cmd()
        .args(["sync", "--only-print-filenames", "--help"])
        .assert()
        .success();
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
    for sub in ALL_SUBCOMMANDS {
        for (flag, value) in [
            ("--username", "x@x.com"),
            ("--password", "secret"),
            ("--domain", "com"),
            ("--cookie-directory", "/tmp/cookies"),
        ] {
            common::cmd()
                .args([sub, flag, value, "--help"])
                .assert()
                .success();
        }
    }
}

// ── Log level all variants ──────────────────────────────────────────

#[test]
fn log_level_all_variants_accepted() {
    for variant in ["debug", "info", "warn", "error"] {
        common::cmd()
            .args(["--log-level", variant, "--help"])
            .assert()
            .success();
    }
}

// ── Numeric flag validation ─────────────────────────────────────────

#[test]
fn threads_num_rejects_non_numeric() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--threads-num", "abc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn recent_rejects_non_numeric() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--recent", "abc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn watch_with_interval_rejects_non_numeric() {
    common::cmd()
        .args([
            "sync",
            "--username",
            "x@x.com",
            "--watch-with-interval",
            "abc",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn max_retries_rejects_non_numeric() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--max-retries", "abc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn retry_delay_rejects_non_numeric() {
    common::cmd()
        .args(["sync", "--username", "x@x.com", "--retry-delay", "abc"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
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
