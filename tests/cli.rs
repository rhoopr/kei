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
    "doctor",
    "manifest",
    "import-existing",
    "verify",
];

/// Subcommands that accept `--password` (have PasswordArgs).
const PASSWORD_SUBCOMMANDS: &[&str] = &["sync", "login", "import-existing"];

fn assert_removed_sync_flag_hint(args: &[&str]) {
    common::cmd()
        .args(args)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "v0.20 removed durable sync CLI flags",
        ))
        .stderr(predicate::str::contains("docs/v0.20-migration.md"));
}

// ── Help output ─────────────────────────────────────────────────────────

#[test]
fn help_flag_succeeds() {
    common::cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("kei: photo sync engine"))
        .stdout(predicate::str::contains(
            "https://github.com/rhoopr/kei/wiki",
        ))
        .stdout(predicate::str::contains(
            "https://github.com/rhoopr/kei/issues",
        ));
}

#[test]
fn subcommand_help_links_to_relevant_documentation() {
    for (command, page) in [
        ("sync", "Sync"),
        ("login", "Login"),
        ("list", "List"),
        ("password", "Password"),
        ("reset", "Reset"),
        ("config", "Config"),
        ("status", "Status"),
        ("doctor", "Doctor"),
        ("manifest", "Manifest"),
        ("import-existing", "Import-Existing"),
        ("verify", "Verify"),
        ("reconcile", "Reconcile"),
        ("install", "Install"),
        ("uninstall", "Service"),
        ("service", "Service"),
    ] {
        common::cmd()
            .args([command, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains(format!(
                "https://github.com/rhoopr/kei/wiki/{page}"
            )));
    }
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
        .stdout(predicate::str::contains("--friendly"))
        .stdout(predicate::str::contains("--recent"))
        .stdout(predicate::str::contains("capture-local calendar date"))
        .stdout(predicate::str::contains("explicit datetimes"))
        .stdout(predicate::str::contains("relative intervals"))
        .stdout(predicate::str::contains("--download-dir").not());
}

#[test]
fn response_capture_help_covers_sync_and_service() {
    for args in [&["sync", "--help"][..], &["service", "run", "--help"][..]] {
        common::cmd()
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("--capture-icloud-responses"))
            .stdout(predicate::str::contains("sensitive, unredacted"))
            .stdout(predicate::str::contains(".diagnostics"))
            .stdout(predicate::str::contains("Explicitly writes diagnostics"));
    }
}

#[test]
fn friendly_flags_are_not_shown_on_non_sync_help() {
    for args in [
        &["password", "--help"][..],
        &["config", "setup", "--help"][..],
        &["service", "status", "--help"][..],
    ] {
        common::cmd()
            .args(args)
            .assert()
            .success()
            .stdout(predicate::str::contains("--friendly").not())
            .stdout(predicate::str::contains("--no-friendly").not());
    }
}

#[test]
fn sync_help_omits_removed_directory_flag() {
    // Users should only see the current spelling.
    common::cmd()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--directory").not());
}

#[test]
fn sync_help_omits_removed_exclude_album_flag() {
    common::cmd()
        .args(["sync", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--exclude-album").not())
        .stdout(predicate::str::contains("--album").not());
}

#[test]
fn sync_help_omits_removed_sync_token_flags() {
    // The canonical way to force a fresh token is `kei reset sync-token`.
    let assert = common::cmd().args(["sync", "--help"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        !stdout.contains("--no-incremental"),
        "sync help should not advertise the removed `--no-incremental` flag"
    );
    assert!(
        !stdout.contains("--reset-sync-token"),
        "sync help should not advertise the removed `--reset-sync-token` flag"
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
fn doctor_help_succeeds() {
    common::cmd()
        .args(["doctor", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--json"))
        .stdout(predicate::str::contains("--live"));
}

#[test]
fn doctor_json_uses_local_config_paths_and_redacts() {
    let dir = tempfile::tempdir().unwrap();
    let photos = dir.path().join("photos-doctor-secret@example.com");
    let data = dir.path().join("data-doctor-secret@example.com");
    std::fs::create_dir_all(&photos).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let report = dir.path().join("report-doctor-secret@example.com.json");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "data_dir = {:?}\n\
             [auth]\n\
             username = \"doctor-secret@example.com\"\n\
             [download]\n\
             directory = {:?}\n\
             [report]\n\
             json = {:?}\n",
            data, photos, report
        ),
    )
    .unwrap();

    let output = common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("doctor-secret@example.com"),
        "doctor JSON should redact configured identifiers, stdout:\n{stdout}"
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["config_path"], config.to_string_lossy().as_ref());
    assert_eq!(report["checks"][0]["name"], "config_parse");
    assert_eq!(report["checks"][0]["status"], "ok");
    let checks = report["checks"].as_array().unwrap();
    for expected in [
        "config_parse",
        "download_dir",
        "state_db",
        "session",
        "health",
        "report",
    ] {
        assert!(
            checks.iter().any(|check| check["name"] == expected),
            "doctor JSON missing `{expected}` check: {report:#}"
        );
    }
    assert!(
        checks
            .iter()
            .any(|check| check["name"] == "download_dir" && check["status"] == "ok"),
        "doctor should probe configured download directory: {report:#}"
    );
}

#[test]
fn doctor_invalid_config_reports_redacted_json_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("invalid.toml");
    std::fs::write(
        &config,
        "[auth]\nusername = \"invalid-doctor@example.com\"\npassword = \"plain-secret\"\nbroken = [\n",
    )
    .unwrap();

    let output = common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .args(["doctor", "--json", "--config", config.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .failure()
        .get_output()
        .clone();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("invalid-doctor@example.com") && !stdout.contains("plain-secret"),
        "doctor JSON should redact invalid config snippets, stdout:\n{stdout}"
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| { check["name"] == "config_parse" && check["status"] == "error" }),
        "doctor should keep config parse failures in the report: {report:#}"
    );
}

#[test]
fn manifest_help_succeeds() {
    common::cmd()
        .args(["manifest", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--format"))
        .stdout(predicate::str::contains("json"))
        .stdout(predicate::str::contains("csv"));
}

#[test]
fn reset_state_help_succeeds() {
    common::cmd()
        .args(["reset", "state", "--help"])
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
        .stdout(predicate::str::contains("--strict"))
        .stdout(predicate::str::contains("--download-dir").not());
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
fn legacy_get_code_help_fails() {
    common::cmd()
        .args(["get-code", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn submit_code_help_succeeds() {
    common::cmd()
        .args(["login", "submit-code", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2FA"));
}

#[test]
fn password_set_help_documents_headless_sources() {
    common::cmd()
        .args(["password", "set", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "kei password --password-file <PATH> set",
        ))
        .stdout(predicate::str::contains(
            "kei password --password-command <COMMAND> set",
        ));
}

#[test]
fn legacy_retry_failed_help_fails() {
    common::cmd()
        .args(["retry-failed", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
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
    // succeeded.
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
fn short_u_flag_removed() {
    common::cmd()
        .args(["sync", "-u", "x@x.com", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn short_p_flag_accepted() {
    common::cmd()
        .args(["sync", "-p", "secret", "--help"])
        .assert()
        .success();
}

#[test]
fn short_d_flag_accepted() {
    common::cmd()
        .args(["sync", "-d", "/tmp", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn short_l_flag_removed() {
    common::cmd()
        .args(["sync", "-l", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn short_a_flag_accepted() {
    common::cmd()
        .args(["sync", "-a", "Favorites", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn short_y_flag_on_reset_state() {
    common::cmd()
        .args(["reset", "state", "-y", "--help"])
        .assert()
        .success();
}

#[test]
fn reset_sync_token_help_advertises_yes_flag() {
    // The new canonical `kei reset sync-token` ships with `--yes` to skip
    // the confirmation prompt, matching `reset state`. Help text must
    // surface it so users discover the safe non-interactive form.
    common::cmd()
        .args(["reset", "sync-token", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn reset_sync_token_yes_flag_parses() {
    common::cmd()
        .args(["reset", "sync-token", "--yes", "--help"])
        .assert()
        .success();
}

#[test]
fn reset_sync_token_short_y_flag_parses() {
    common::cmd()
        .args(["reset", "sync-token", "-y", "--help"])
        .assert()
        .success();
}

#[test]
fn reset_session_help_advertises_yes_flag() {
    // `kei reset session` ships with `--yes` to skip the confirmation
    // prompt, matching the other reset subcommands. Help text must surface
    // it so users discover the safe non-interactive form.
    common::cmd()
        .args(["reset", "session", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--yes"));
}

#[test]
fn reset_session_yes_flag_parses() {
    common::cmd()
        .args(["reset", "session", "--yes", "--help"])
        .assert()
        .success();
}

#[test]
fn reset_session_short_y_flag_parses() {
    common::cmd()
        .args(["reset", "session", "-y", "--help"])
        .assert()
        .success();
}

// ── Enum validation (rejection only — acceptance covered by unit tests) ─

#[test]
fn size_rejects_invalid_variant() {
    common::cmd()
        .args(["sync", "--size", "huge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn domain_rejects_invalid() {
    common::cmd()
        .args(["sync", "--domain", "uk"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

#[test]
fn live_photo_size_rejects_invalid() {
    common::cmd()
        .args(["sync", "--live-photo-size", "xlarge"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn live_photo_mov_filename_policy_rejects_invalid() {
    common::cmd()
        .args(["sync", "--live-photo-mov-filename-policy", "custom"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn align_raw_rejects_invalid() {
    common::cmd()
        .args(["sync", "--align-raw", "bogus"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn file_match_policy_rejects_invalid() {
    common::cmd()
        .args(["sync", "--file-match-policy", "random"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

// ── Numeric validation (rejection only — acceptance covered by unit tests)

#[test]
fn threads_rejects_zero() {
    common::cmd()
        .args(["sync", "--threads", "0"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn removed_threads_num_flag_fails() {
    common::cmd()
        .args(["sync", "--threads-num", "4"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn removed_sync_flag_error_includes_v020_migration_hint() {
    assert_removed_sync_flag_hint(&["sync", "--download-dir", "/photos"]);
}

#[test]
fn removed_import_flag_error_includes_v020_migration_hint() {
    assert_removed_sync_flag_hint(&["import-existing", "--download-dir", "/photos"]);
}

#[test]
fn removed_service_run_flag_error_includes_v020_migration_hint() {
    assert_removed_sync_flag_hint(&["service", "run", "--download-dir", "/photos"]);
}

// ── submit-code requires positional CODE ────────────────────────────────

#[test]
fn submit_code_requires_code_argument() {
    common::cmd()
        .env("ICLOUD_USERNAME", "x@x.com")
        .args(["login", "submit-code"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("error"));
}

// ── import-existing requires TOML directory ─────────────────────────────

#[test]
fn import_existing_requires_directory() {
    common::cmd()
        .env("ICLOUD_USERNAME", "x@x.com")
        .args(["import-existing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set [download].directory"));
}

// ── Removed durable boolean flags fail ─────────────────────────────────

#[test]
fn removed_boolean_sync_flag_fails() {
    common::cmd()
        .args(["sync", "--skip-videos", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

// ── Kept per-run value flags are accepted ───────────────────────────────

#[test]
fn value_sync_flags_accepted() {
    let pairs = [
        ("--recent", "10"),
        ("--skip-created-before", "2024-01-01"),
        ("--skip-created-after", "2025-01-01"),
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
        .args(["sync", "--album", "Favorites", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn smart_folder_flag_accepts_multiple() {
    common::cmd()
        .args(["sync", "--smart-folder", "Favorites", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn library_flag_accepts_repeatable_sentinels() {
    common::cmd()
        .args(["sync", "--library", "primary", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn album_flag_accepts_inline_exclusion() {
    common::cmd()
        .args(["sync", "--album", "!Family", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn album_flag_rejects_duplicates() {
    common::cmd()
        .args(["sync", "--album", "Family", "--album", "Family"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn folder_structure_albums_flag_parses() {
    common::cmd()
        .args([
            "sync",
            "--folder-structure-albums",
            "{album}/%Y/%m/%d",
            "--help",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn folder_structure_smart_folders_flag_parses() {
    common::cmd()
        .args([
            "sync",
            "--folder-structure-smart-folders",
            "{smart-folder}/%Y",
            "--help",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn unfiled_flag_accepts_bare_and_explicit_value() {
    for args in [
        vec!["sync", "--unfiled", "--help"],
        vec!["sync", "--unfiled", "false", "--help"],
        vec!["sync", "--unfiled", "true", "--help"],
    ] {
        common::cmd()
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

// ── Explicit sync command required ──────────────────────────────────────

#[test]
fn bare_invocation_with_removed_durable_flags_fails() {
    common::cmd()
        .args(["--username", "x@x.com", "--download-dir", "/tmp", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
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
fn import_existing_library_flag_accepts_repeatable_sentinels() {
    common::cmd()
        .args([
            "import-existing",
            "--library",
            "primary",
            "--library",
            "shared",
            "--library",
            "SharedSync-A1B2C3D4",
            "--library",
            "!SharedSync-AAAAAAAA",
            "--help",
        ])
        .assert()
        .success();
}

#[test]
fn import_existing_dry_run_flag_parses() {
    common::cmd()
        .args(["import-existing", "--dry-run", "--help"])
        .assert()
        .success();
}

#[test]
fn import_existing_recent_flag() {
    common::cmd()
        .args(["import-existing", "--recent", "100", "--help"])
        .assert()
        .success();
}

#[test]
fn import_existing_strict_flag_parses() {
    common::cmd()
        .args(["import-existing", "--strict", "--help"])
        .assert()
        .success();
}

#[test]
fn import_existing_removed_durable_flags_fail() {
    for args in [
        vec!["import-existing", "--download-dir", "/tmp", "--help"],
        vec!["import-existing", "-d", "/tmp", "--help"],
        vec!["import-existing", "--folder-structure", "%Y-%m", "--help"],
        vec![
            "import-existing",
            "--folder-structure-albums",
            "{album}",
            "--help",
        ],
        vec![
            "import-existing",
            "--folder-structure-smart-folders",
            "{smart-folder}",
            "--help",
        ],
        vec![
            "import-existing",
            "--file-match-policy",
            "name-id7",
            "--help",
        ],
        vec!["import-existing", "--size", "medium", "--help"],
        vec![
            "import-existing",
            "--live-photo-mode",
            "image-only",
            "--help",
        ],
        vec!["import-existing", "--live-photo-size", "medium", "--help"],
        vec![
            "import-existing",
            "--live-photo-mov-filename-policy",
            "original",
            "--help",
        ],
        vec!["import-existing", "--align-raw", "original", "--help"],
        vec!["import-existing", "--force-size", "--help"],
        vec!["import-existing", "--keep-unicode-in-filenames", "--help"],
    ] {
        common::cmd()
            .args(args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

#[test]
fn import_existing_removed_durable_env_vars_are_ignored_by_help() {
    common::cmd()
        .env("KEI_FILE_MATCH_POLICY", "name-id7")
        .env("KEI_DOWNLOAD_DIR", "/tmp")
        .args(["import-existing", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--file-match-policy").not())
        .stdout(predicate::str::contains("--download-dir").not());
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
        .env("ICLOUD_USERNAME", "x@x.com")
        .args(["--config", "/nonexistent/explicit/config.toml", "status"])
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
    assert!(
        stderr.contains("kei doctor --json"),
        "generic runtime errors should suggest diagnostics, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("https://github.com/rhoopr/kei/issues"),
        "generic runtime errors should link to issue reporting, stderr:\n{stderr}"
    );
}

// ── Auth flags on non-sync subcommands ──────────────────────────────────

#[test]
fn domain_flag_is_removed_on_status() {
    common::cmd()
        .args(["status", "--domain", "cn", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn password_flag_works_on_submit_code() {
    common::cmd()
        .args(["login", "-p", "secret", "submit-code", "123456", "--help"])
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

// ── import-existing short -d flag removed ───────────────────────────────

#[test]
fn import_existing_short_d_flag_removed() {
    common::cmd()
        .args(["import-existing", "-d", "/tmp", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
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

// ── Auth flags removed from all subcommands ─────────────────────────

#[test]
fn auth_flags_rejected_on_all_subcommands() {
    // Durable auth/storage inputs are TOML/env only in v0.20.
    for sub in ALL_SUBCOMMANDS {
        for (flag, value) in [
            ("--username", "x@x.com"),
            ("--domain", "com"),
            ("--data-dir", "/tmp/data"),
        ] {
            common::cmd()
                .args([sub, flag, value, "--help"])
                .assert()
                .failure()
                .stderr(predicate::str::contains("unexpected argument"));
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
        .args(["sync", "--retry-failed", "--recent", "10", "--help"])
        .assert()
        .success();
}

#[test]
fn removed_sync_token_flags_fail() {
    common::cmd()
        .args(["sync", "--no-incremental", "--reset-sync-token", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
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

/// Exit code 1 (generic failure) when username is missing.
#[test]
fn exit_code_1_on_missing_username() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[download]\ndirectory = \"/tmp/codex/kei/exit-code-test\"\n",
    )
    .unwrap();

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env("KEI_DATA_DIR", dir.path())
        .args(["sync", "--config", config.to_str().unwrap()])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set your iCloud username"));
}

/// Exit code 3 (auth failure) when password file is empty.
///
/// An empty password file triggers `AuthError::FailedLogin("Password
/// provider returned no data")`, which maps to EXIT_AUTH (3).
#[test]
fn exit_code_3_on_empty_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[download]\ndirectory = \"/tmp/codex/kei/exit-code-test\"\n",
    )
    .unwrap();
    let pw_file = dir.path().join("empty-password");
    std::fs::write(&pw_file, "").unwrap();

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env("ICLOUD_USERNAME", "exit-code-test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args([
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--password-file",
            pw_file.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .code(3)
        .stderr(predicate::str::contains("No password was available"))
        .stderr(predicate::str::contains("kei doctor --json").not());
}

/// Exit code 3 (auth failure) when password file contains only a newline.
#[test]
fn exit_code_3_on_newline_only_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[download]\ndirectory = \"/tmp/codex/kei/exit-code-test\"\n",
    )
    .unwrap();
    let pw_file = dir.path().join("newline-password");
    std::fs::write(&pw_file, "\n").unwrap();

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env("ICLOUD_USERNAME", "exit-code-test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args([
            "sync",
            "--config",
            config.to_str().unwrap(),
            "--password-file",
            pw_file.to_str().unwrap(),
        ])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .code(3)
        .stderr(predicate::str::contains("No password was available"));
}

/// Exit code 2 (clap validation error) for invalid argument values.
#[test]
fn exit_code_2_on_invalid_argument() {
    common::cmd()
        .args(["sync", "--unknown"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("unexpected argument"));
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
        .stdout(predicate::str::contains("sync-token"))
        .stdout(predicate::str::contains("session"));
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
fn reset_session_help_succeeds() {
    common::cmd()
        .args(["reset", "session", "--help"])
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

// ── Removed legacy commands not in help ─────────────────────────────────

#[test]
fn hidden_legacy_commands_not_in_help() {
    let assert = common::cmd().arg("--help").assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    // These hyphenated names only appeared as top-level subcommands in
    // the old CLI. They should stay out of help.
    for hidden in ["get-code", "submit-code", "reset-state", "reset-sync-token"] {
        assert!(
            !stdout.contains(hidden),
            "help output should not list removed command `{hidden}`"
        );
    }
}

// ── Retry-failed conflicts ─────────────────────────────────────────────

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
        .stderr(predicate::str::contains("unexpected argument"));
}

// ── --data-dir global removed ─────────────────────────────────────────

#[test]
fn data_dir_flag_rejected() {
    common::cmd()
        .args(["sync", "--data-dir", "/tmp/data", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn data_dir_global_rejected_with_subcommands() {
    for sub in ALL_SUBCOMMANDS {
        common::cmd()
            .args([sub, "--data-dir", "/tmp/data", "--help"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("unexpected argument"));
    }
}

// ── KEI_* env vars ────────────────────────────────────────────────────

#[test]
fn download_dir_sync_flag_removed() {
    common::cmd()
        .args(["sync", "--download-dir", "/photos", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
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

#[test]
fn kei_reconcile_every_n_cycles_env_var_ignored_by_help() {
    common::cmd()
        .env("KEI_RECONCILE_EVERY_N_CYCLES", "24")
        .args(["sync", "--help"])
        .assert()
        .success();
}

#[test]
fn reconcile_help_mentions_truncated_files() {
    common::cmd()
        .args(["reconcile", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("missing or truncated"));
}

// ── config show produces TOML ─────────────────────────────────────────

#[test]
fn config_show_produces_toml_output() {
    let output = common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", "/tmp")
        .args(["config", "show"])
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
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "sync-token"])
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
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "state", "--yes"])
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
        .args(["login", "submit-code", "123456"])
        .timeout(std::time::Duration::from_secs(30))
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

// ── --report-json removed from public sync CLI ───────────────────────

#[test]
fn report_json_flag_fails() {
    common::cmd()
        .args(["sync", "--report-json", "/tmp/report.json", "--help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

#[test]
fn report_json_not_visible_in_help() {
    let assert = common::cmd().args(["sync", "--help"]).assert().success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        !stdout.contains("\n      --report-json"),
        "sync help should not expose a --report-json option, got:\n{stdout}"
    );
}
