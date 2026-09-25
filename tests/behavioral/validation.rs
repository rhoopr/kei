//! Command arguments, help, exit codes, and pre-authentication validation.

use super::{clean_cmd, sync_cmd_for_config_body, sync_cmd_for_validation, write_sync_config};
#[cfg(debug_assertions)]
use crate::common;
use predicates::prelude::predicate;

#[test]
fn repair_help_explains_one_shot_scope_and_datetime_requirement() {
    for command in [vec!["sync", "--help"], vec!["service", "run", "--help"]] {
        let output = clean_cmd().args(command).output().expect("run help");
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert_eq!(help.matches("Use with sync, not service run.").count(), 2);
        assert_eq!(
            help.matches("A configured watch interval is ignored")
                .count(),
            2
        );
        assert!(help.contains("metadata.set_exif_datetime = true"));
    }
}

#[cfg(debug_assertions)]
#[test]
fn maintenance_refresh_example_accepts_complete_sweep() {
    let guide = include_str!("../../docs/backup-maintenance.md").replace("\r\n", "\n");
    // Exercise the documented command with both Unix and Windows checkout text.
    for line_ending in ["\n", "\r\n"] {
        let guide = guide.replace('\n', line_ending);
        let filters = guide
            .split_once("```toml")
            .expect("maintenance guide has a TOML example")
            .1
            .split_once("```")
            .expect("TOML example has a closing fence")
            .0;
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let report_path = dir.path().join("report.json");
        std::fs::write(
            &config_path,
            format!(
                "[auth]\nusername = \"offline-refresh@example.com\"\n[download]\ndirectory = {}\n{}\n[report]\njson = {}\n",
                common::toml_string(&dir.path().join("photos").to_string_lossy()),
                filters,
                common::toml_string(&report_path.to_string_lossy()),
            ),
        )
        .unwrap();
        clean_cmd()
            .env("KEI_UNSTABLE_FAKE_SYNC_REPORT_FOR_TESTS", "1")
            .args(["sync", "--refresh-metadata", "--config"])
            .arg(&config_path)
            .assert()
            .success();
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        assert_eq!(report["version"], "3");
        assert_eq!(report["options"]["library"], "all");
    }
}

#[test]
fn login_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["login"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn list_albums_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["list", "albums"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn password_set_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["password", "set"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn password_clear_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["password", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn password_backend_requires_username() {
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["password", "backend"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn sync_requires_username() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn service_run_uses_sync_worker_path_and_requires_username() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args([
            "service",
            "run",
            "--config",
            config_path.to_str().unwrap(),
            "--no-progress-bar",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("starting kei service worker"))
        .stderr(predicate::str::contains(
            "service mode: applied default watch interval",
        ))
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn sync_requires_directory() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["sync"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set [download].directory"));
}

#[test]
fn import_existing_requires_directory() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["import-existing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Set [download].directory"));
}

#[test]
fn import_existing_rejects_nonexistent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[download]\ndirectory = \"/does/not/exist/anywhere\"\n",
    )
    .unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["--config", config.to_str().unwrap(), "import-existing"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Could not read download directory /does/not/exist/anywhere",
        ));
}

#[test]
fn exit_2_for_clap_errors() {
    // Removed durable flags are clap errors.
    clean_cmd()
        .args(["--username", "", "config", "show"])
        .assert()
        .code(2);
}

#[test]
fn exit_1_for_missing_directory_on_sync() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["sync"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set [download].directory"));
}

#[cfg(unix)]
#[test]
fn sync_unwritable_download_directory_errors_before_auth() {
    use std::os::unix::fs::PermissionsExt;

    // Root can write through 0o555 directories, so this probe is not
    // meaningful under root-owned CI containers.
    // SAFETY: libc::geteuid() is a stateless POSIX call with no
    // preconditions and no memory-safety implications.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    let download_dir = dir.path().join("photos");
    let config_path = dir.path().join("config.toml");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::create_dir_all(&download_dir).unwrap();
    std::fs::set_permissions(&download_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    write_sync_config(&config_path, download_dir.to_str().unwrap());

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", &data_dir)
        .args([
            "sync",
            "--config",
            config_path.to_str().unwrap(),
            "--password",
            "not-used-before-auth",
            "--no-progress-bar",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "Cannot write to download directory",
        ));

    std::fs::set_permissions(&download_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        std::fs::read_dir(&data_dir).unwrap().next().is_none(),
        "unwritable download dir must fail before auth/session state is written"
    );
}

#[test]
fn exit_1_for_missing_username_on_sync() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Set your iCloud username"));
}

#[test]
fn help_flag_exits_zero() {
    clean_cmd().arg("--help").assert().success();
}

#[test]
fn version_flag_exits_zero() {
    clean_cmd()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("kei"));
}

#[test]
fn sync_help_exits_zero() {
    clean_cmd().args(["sync", "--help"]).assert().success();
}

#[test]
fn unknown_subcommand_fails() {
    clean_cmd().arg("nonexistent-command").assert().code(2);
}

#[test]
fn domain_cn_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\ndomain = \"cn\"\n",
    )
    .unwrap();
    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("cn"));
}

#[test]
fn domain_invalid_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\ndomain = \"uk\"\n",
    )
    .unwrap();
    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1);
}

#[test]
fn dry_run_and_retry_failed_conflict() {
    let dir = tempfile::tempdir().unwrap();
    // clap-level conflicts_with should reject this
    clean_cmd()
        .env("KEI_DATA_DIR", dir.path())
        .args(["sync", "--dry-run", "--retry-failed"])
        .assert()
        .code(2);
}

#[test]
fn removed_legacy_album_in_cli_errors() {
    sync_cmd_for_validation()
        .args([
            "--folder-structure",
            "{album}/%Y/%m/%d",
            "--only-print-filenames",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"))
        .stderr(predicate::str::contains("--folder-structure"));
}

/// Every per-category selection flag composed in a single
/// invocation must validate end-to-end through the
/// `Cli -> Config -> Selection` pipeline. Per-category unit tests in
/// `selection.rs` cover each parser in isolation, but the binary-level
/// wiring (clap field name, config-resolver field name, the
/// `Cli::command` mapping can drift independently of the
/// parsers; a regression there lands green for every per-category
/// test even when the combined flag set bails at startup.
///
/// Flags exercised here:
///   --album none              → AlbumSelector::None
///   --smart-folder all        → SmartFolderSelector::All { sensitive=false }
///   --unfiled false           → Selection.unfiled = false
///   --library shared          → LibrarySelector { primary=false, shared_all=true }
///
/// The binary may exit non-zero for downstream reasons (no password
/// available, network unreachable, auth bail) — those are
/// out-of-scope. What matters is that none of the parser-level bail
/// strings ("must not be empty", "not supported", "cannot be combined")
/// reach stderr.
#[test]
fn sync_validation_accepts_full_selection_combo() {
    let out = sync_cmd_for_config_body(
        "\n[filters]\nalbums = [\"none\"]\nsmart_folders = [\"all\"]\nunfiled = false\nlibraries = [\"shared\"]\n",
    )
        .arg("--only-print-filenames")
        .assert()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // No parser-level bail strings — those would mean the combo got
    // rejected at parse time, which the per-category tests already
    // disprove for each flag in isolation.
    assert!(
        !stderr.contains("must not be empty"),
        "no parser empty-input bail expected; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("not supported"),
        "no friendly-alias bail expected; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("cannot be combined"),
        "no sentinel-mix bail expected; stderr: {stderr}"
    );
}

#[test]
fn sync_bails_on_album_token_in_smart_folders_template() {
    sync_cmd_for_config_body("folder_structure_smart_folders = \"{album}/%Y\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{album}"))
        .stderr(predicate::str::contains("--folder-structure-albums"));
}

#[test]
fn sync_bails_on_smart_folder_token_in_albums_template() {
    sync_cmd_for_config_body("folder_structure_albums = \"{smart-folder}/foo\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{smart-folder}"))
        .stderr(predicate::str::contains("--folder-structure-smart-folders"));
}

#[test]
fn sync_bails_on_library_token_not_first_segment() {
    sync_cmd_for_config_body("folder_structure = \"%Y/{library}\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{library}"))
        .stderr(predicate::str::contains("first path segment"));
}

#[test]
fn sync_bails_on_duplicate_library_token() {
    sync_cmd_for_config_body("folder_structure_albums = \"{library}/{library}/{album}\"\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("{library}"))
        .stderr(predicate::str::contains("once"));
}

#[test]
fn sync_bails_on_within_album_contradiction() {
    sync_cmd_for_config_body("\n[filters]\nalbums = [\"Family\", \"!Family\"]\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("includes and excludes"))
        .stderr(predicate::str::contains("Family"));
}

#[test]
fn sync_bails_on_library_none() {
    sync_cmd_for_config_body("\n[filters]\nlibraries = [\"none\"]\n")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("library none"));
}

// ── Removed v0.20 selection aliases ───────────────────────────────
#[test]
fn removed_exclude_album_cli_flag_errors() {
    sync_cmd_for_validation()
        .args(["--exclude-album", "Family", "--only-print-filenames"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--exclude-album"));
}
