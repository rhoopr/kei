//! Configuration commands, precedence, persistence, and logging resolution.

use super::{clean_cmd, sync_cmd_for_validation, write_fake_two_factor_config, write_sync_config};
use crate::common;
use predicates::prelude::{PredicateBooleanExt, predicate};

#[test]
fn config_show_outputs_valid_toml() {
    let out = clean_cmd()
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should be parseable TOML
    assert!(
        toml::from_str::<toml::Value>(&stdout).is_ok(),
        "config show should produce valid TOML, got:\n{stdout}"
    );
}

#[test]
fn config_show_contains_username() {
    clean_cmd()
        .env("ICLOUD_USERNAME", "myuser@icloud.com")
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("myuser@icloud.com"));
}

#[test]
fn config_show_reflects_directory_from_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/my/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "cli@example.com")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("/my/photos"));
}

#[test]
fn config_show_rejects_toml_with_password() {
    // `[auth] password` is banned; `config show` should fail loudly with
    // the migration message rather than silently dropping the field.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword = \"super_secret_value\"\n",
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("super_secret_value"),
        "password must not appear in stdout even on rejection, got:\n{stdout}"
    );
}

#[test]
fn config_show_reflects_toml_values() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[auth]
username = "toml@example.com"

[download]
directory = "/toml/photos"
threads = 4
"#,
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("toml@example.com"), "stdout: {stdout}");
    assert!(stdout.contains("/toml/photos"), "stdout: {stdout}");
    assert!(
        stdout.contains("4"),
        "threads should be 4, stdout: {stdout}"
    );
}

#[test]
fn config_show_preserves_top_level_toml_values() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let data_dir = dir.path().join("data");
    let data_dir_string = data_dir.to_string_lossy();
    std::fs::write(
        &config_path,
        format!(
            r#"
data_dir = {}
log_level = "debug"

[auth]
username = "toml@example.com"

[download]
directory = "/toml/photos"
"#,
            common::toml_string(&data_dir_string)
        ),
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: toml::Value = toml::from_str(&stdout).unwrap();
    assert_eq!(
        parsed.get("data_dir").and_then(toml::Value::as_str),
        Some(data_dir_string.as_ref()),
        "stdout: {stdout}"
    );
    assert_eq!(
        parsed.get("log_level").and_then(toml::Value::as_str),
        Some("debug"),
        "stdout: {stdout}"
    );
}

#[test]
fn config_show_omits_derived_top_level_values_when_unset() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let download_dir = dir.path().join("photos");
    std::fs::write(
        &config_path,
        format!(
            r#"
[auth]
username = "toml@example.com"

[download]
directory = {}
"#,
            common::toml_string(&download_dir.to_string_lossy())
        ),
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: toml::Value = toml::from_str(&stdout).unwrap();
    assert!(
        parsed.get("data_dir").is_none(),
        "config show must not serialize derived data_dir when TOML omitted it; stdout: {stdout}"
    );
    assert!(
        parsed.get("log_level").is_none(),
        "config show must not serialize default log_level when TOML omitted it; stdout: {stdout}"
    );
}

#[test]
fn config_show_emits_unfiled_false_when_explicit() {
    // The cli.rs help-shadow test for --unfiled only verifies clap parses;
    // it does not pin the resolved value all the way through Config::build
    // → Selection → to_toml. A clap-default flip or selector regression
    // that swallowed the explicit `false` would land green
    // there. `to_toml()` only emits `unfiled` when the resolved value
    // differs from the `true` default, so an explicit `false` is the case
    // we can observe directly in `kei config show` output.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[auth]
username = "x@x.com"

[filters]
unfiled = false
"#,
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: toml::Value = toml::from_str(&stdout).expect("config show must emit valid TOML");
    let unfiled = parsed
        .get("filters")
        .and_then(|f| f.get("unfiled"))
        .and_then(toml::Value::as_bool);
    assert_eq!(
        unfiled,
        Some(false),
        "config show must round-trip explicit `unfiled = false`; got:\n{stdout}"
    );
}

#[test]
fn config_show_cli_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[auth]
username = "toml@example.com"
"#,
    )
    .unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "cli@example.com")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("cli@example.com"));
}

#[test]
fn kei_data_dir_env_resolves_in_status() {
    // KEI_DATA_DIR env var should be used for the data directory
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path().to_str().unwrap())
        .args(["status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No state database found"));
}

#[test]
fn icloud_username_env_resolves_in_config_show() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@icloud.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("env@icloud.com"));
}

#[test]
fn icloud_username_env_resolves_without_cli_flag() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@icloud.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("env@icloud.com"));
}

#[test]
fn first_run_auto_config_creates_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    // sync will fail at auth, but auto-config fires before auth.
    // Use --config pointing at non-existent file in existing directory.
    clean_cmd()
        .env("ICLOUD_USERNAME", "auto@example.com")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure(); // fails at auth, but config file should have been created

    assert!(
        config_path.exists(),
        "auto-config should create config file at {}",
        config_path.display()
    );
    let content = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        content.contains("auto@example.com"),
        "auto-config should contain username, got:\n{content}"
    );
}

#[test]
fn first_run_auto_config_does_not_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# existing config\n").unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success();

    let content = std::fs::read_to_string(&config_path).unwrap();
    assert_eq!(
        content, "# existing config\n",
        "auto-config must not overwrite existing file"
    );
}

#[test]
fn legacy_icloudpd_rs_paths_are_not_auto_copied_on_startup() {
    let home = tempfile::tempdir().unwrap();
    let old_config = home.path().join(".config/icloudpd-rs/config.toml");
    let old_cookie_dir = home.path().join(".icloudpd-rs");
    let config_path = home.path().join("current-config.toml");

    std::fs::create_dir_all(old_config.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&old_cookie_dir).unwrap();
    std::fs::write(&old_config, "[auth]\nusername = \"legacy@example.com\"\n").unwrap();
    std::fs::write(old_cookie_dir.join("session.json"), "{}").unwrap();
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"current@example.com\"\n[download]\ndirectory = \"/tmp/codex/kei/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .env("HOME", home.path())
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("current@example.com"))
        .stdout(predicate::str::contains("legacy@example.com").not());

    assert!(
        !home.path().join(".config/kei/config.toml").exists(),
        "startup must not copy legacy icloudpd-rs config into kei paths"
    );
    assert!(
        !home
            .path()
            .join(".config/kei/cookies/session.json")
            .exists(),
        "startup must not copy legacy icloudpd-rs session files into kei paths"
    );
}

#[test]
fn config_malformed_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "this is not valid toml {{{").unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("parse").or(predicate::str::contains("expected")));
}

#[test]
fn config_unknown_toml_field() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nbogus = true\n").unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown field"));
}

#[test]
fn config_empty_username_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"\"\n\n[download]\ndirectory = \"/photos\"\n",
    )
    .unwrap();

    // config show calls Config::build which checks for empty username
    // only when a username source is present in TOML. Since TOML sets
    // username = "", the build path validates it.
    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cannot be empty"));
}

#[test]
fn config_toml_password_field_rejected() {
    // `[auth] password` is not accepted, empty or otherwise.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword = \"\"\n\n[download]\ndirectory = \"/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("`[auth].password`"))
        .stderr(predicate::str::contains("kei password set"));
}

// On Windows, `--password-command` / `[auth] password_command` is rejected
// at config::build before the "pick one" check runs, so the assertion on
// "pick one" doesn't hold. Unix covers the path this test is guarding.
#[cfg(unix)]
#[test]
fn config_multiple_password_sources_in_toml() {
    // Both `password_file` and `password_command` set in the same TOML is
    // still rejected with "pick one".
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\npassword_file = \"/tmp/pw\"\npassword_command = \"echo hi\"\n\n[download]\ndirectory = \"/photos\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("Pick one"));
}

#[test]
fn config_strftime_folder_structure_accepted() {
    // Full strftime support: %B (month name), %q, etc. are no longer rejected.
    // The process may fail auth, but it should NOT fail config validation.
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y/%B/%d\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        // Should get past config validation (no "unrecognized format token" error).
        // Fails on auth, not on config.
        .stderr(predicate::str::contains("unrecognized format token").not());
}

#[test]
fn config_valid_folder_structure_ymd() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y/%m/%d\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("%Y/%m/%d"));
}

#[test]
fn config_valid_folder_structure_ym() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y-%m\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("%Y-%m"));
}

#[test]
fn config_valid_folder_structure_ymdh() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"%Y/%m/%d/%H\"\n",
    )
    .unwrap();

    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("%Y/%m/%d/%H"));
}

#[test]
fn config_folder_structure_none() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nfolder_structure = \"none\"\n",
    )
    .unwrap();

    // "none" is a special value that should be accepted (no error)
    clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("none"));
}

#[test]
fn config_watch_interval_below_60_in_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n\n[watch]\ninterval = 30\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "watch interval must be in 60..=86400 seconds, got 30",
        ));
}

#[test]
fn config_retry_delay_toml_key_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n\n[download.retry]\ndelay = 5\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown field `delay`"));
}

#[test]
fn config_threads_num_toml_key_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\nthreads_num = 4\n",
    )
    .unwrap();

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("unknown field `threads_num`"));
}

#[test]
fn config_resolution_toml_only() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"tomluser@example.com\"\n\n[download]\ndirectory = \"/toml/dir\"\n",
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("tomluser@example.com"), "stdout: {stdout}");
    assert!(stdout.contains("/toml/dir"), "stdout: {stdout}");
}

#[test]
fn config_resolution_toml_username_used() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"toml@example.com\"\n").unwrap();

    clean_cmd()
        .env_remove("ICLOUD_USERNAME")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("toml@example.com"));
}

#[test]
fn config_resolution_env_overrides_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"toml@example.com\"\n").unwrap();

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "env@example.com")
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Env should override TOML
    assert!(
        stdout.contains("env@example.com"),
        "env should override TOML, stdout: {stdout}"
    );
}

#[test]
fn config_resolution_env_username_used_without_toml() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "env@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("env@example.com"));
}

#[test]
fn config_resolution_default_values() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Default threads = 10 using the canonical TOML spelling.
    assert!(
        stdout.contains("threads = 10"),
        "default threads should be 10, stdout: {stdout}"
    );
    assert!(
        !stdout.contains("threads_num"),
        "serialized config should use the new `threads` key, not `threads_num`: {stdout}"
    );
    // Default folder_structure = "%Y/%m/%d"
    assert!(
        stdout.contains("%Y/%m/%d"),
        "default folder_structure should be %Y/%m/%d, stdout: {stdout}"
    );
}

#[test]
fn config_show_does_not_read_password_file_contents() {
    // `config show` may echo the `password_file` path back to the user, but
    // it must never open the file and leak its contents. This guards against
    // accidental eager resolution in future refactors of the config pipeline.
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw");
    std::fs::write(&pw_file, "my_secret_pw\n").unwrap();
    let config_path = dir.path().join("config.toml");
    // Use TOML literal strings (single quotes) for the path so Windows
    // paths like `C:\Users\...` don't get interpreted as `\U...` escapes.
    std::fs::write(
        &config_path,
        format!(
            "[auth]\nusername = \"x@x.com\"\npassword_file = '{}'\n",
            pw_file.display()
        ),
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("my_secret_pw"),
        "config show must not dereference password_file, stdout: {stdout}"
    );
    // The path itself is expected to appear (it's a config value, not a secret).
    assert!(
        stdout.contains(&pw_file.display().to_string()),
        "password_file path should be echoed back, stdout: {stdout}"
    );
}

#[test]
fn auto_config_suppressed_by_env() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    // KEI_NO_AUTO_CONFIG=1 should prevent creation of the config file
    clean_cmd()
        .env("KEI_NO_AUTO_CONFIG", "1")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure(); // fails at auth

    assert!(
        !config_path.exists(),
        "KEI_NO_AUTO_CONFIG=1 should suppress config file creation"
    );
}

#[test]
#[cfg(unix)]
fn auto_config_has_0600_perms() {
    use std::os::unix::fs::MetadataExt;

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");

    clean_cmd()
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure(); // fails at auth

    assert!(config_path.exists(), "config file should be created");
    let mode = std::fs::metadata(&config_path).unwrap().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "auto-config file should have 0600 permissions, got {:o}",
        mode
    );
}

#[test]
fn startup_logging_precedence_reaches_sync_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_fake_two_factor_config(dir.path(), "test@example.com");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, format!("log_level = 'error'\n{config}")).unwrap();

    for (flags, rust_log, shows_start) in [
        (vec![], None, false),
        (vec!["--verbose"], None, true),
        (vec!["--verbose", "--log-level", "error"], None, false),
        (vec![], Some("kei=info"), true),
    ] {
        let mut cmd = clean_cmd();
        cmd.env_remove("RUST_LOG")
            .env("KEI_DATA_DIR", dir.path().join("data"))
            .env("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS", "1")
            .args(["sync", "--config", config_path.to_str().unwrap()])
            .args(&flags);
        if let Some(filter) = rust_log {
            cmd.env("RUST_LOG", filter);
        }
        let output = cmd.assert().code(3).get_output().clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            stderr.contains("Starting kei"),
            shows_start,
            "{flags:?}: {stderr}"
        );
        assert!(stderr.contains("kei login get-code"), "{stderr}");
    }
}

#[test]
fn log_level_default_info() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    // sync with username + directory will fail at auth. Check stderr for INFO.
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args(["sync", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Default level is INFO; "Starting kei" should appear but DEBUG should not.
    assert!(
        stderr.contains("Starting kei"),
        "default log level should show INFO-level messages like 'Starting kei', stderr: {stderr}"
    );
    let has_debug = stderr.lines().any(|line| {
        let lower = line.to_lowercase();
        lower.contains(" debug ") && !line.starts_with("Error:")
    });
    assert!(
        !has_debug,
        "default log level should suppress DEBUG-level messages, stderr: {stderr}"
    );
}

#[test]
fn log_level_debug() {
    let dir = tempfile::tempdir().unwrap();
    let dl_dir = dir.path().join("photos");
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, dl_dir.to_str().unwrap());
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args([
            "--log-level",
            "debug",
            "sync",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("DEBUG") || stderr.contains("debug"),
        "debug log level should produce DEBUG entries, stderr: {stderr}"
    );
}

#[test]
fn log_level_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    write_sync_config(&config_path, "/photos");
    let out = clean_cmd()
        .args([
            "--log-level",
            "error",
            "sync",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // With log level error, no info/debug lines should appear.
    // The tracing subscriber uses the format "LEVEL kei::" for structured logs.
    // "Error:" comes from main's eprintln, not from tracing, so it's fine.
    let has_info = stderr.lines().any(|line| {
        let lower = line.to_lowercase();
        (lower.contains(" info ") || lower.contains(" debug ")) && !line.starts_with("Error:")
    });
    assert!(
        !has_info,
        "error log level should suppress info/debug lines, stderr: {stderr}"
    );
}

#[test]
fn config_show_help_exits_zero() {
    clean_cmd()
        .args(["config", "show", "--help"])
        .assert()
        .success();
}

#[test]
fn config_setup_requires_interactive_terminal() {
    clean_cmd()
        .args(["config", "setup"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "The setup wizard needs an interactive terminal",
        ));
}

#[test]
fn toml_domain_cn() {
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
fn config_show_reflects_threads_from_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "[auth]\nusername = \"x@x.com\"\n\n[download]\nthreads = 4\n",
    )
    .unwrap();

    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("threads = 4"), "stdout: {stdout}");
}

#[test]
fn kei_config_env_var_loads_toml() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("env-config.toml");
    std::fs::write(&config_path, "[auth]\nusername = \"fromenv@example.com\"\n").unwrap();

    clean_cmd()
        .env("KEI_CONFIG", config_path.to_str().unwrap())
        .env("KEI_DATA_DIR", dir.path())
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("fromenv@example.com"));
}

// ═══════════════════════════════════════════════════════════════════════
// v0.13 selection + per-category folder-structure surface
//
// Stdout (resolved config) checks drive `kei config show` from a TOML
// fixture, since that subcommand uses `SyncArgs::default()` and won't
// accept sync flags. CLI/env-flag tests drive `kei sync` and only assert
// stderr / exit code so they don't require auth.
// ═══════════════════════════════════════════════════════════════════════

/// Run `kei config show` against an inline TOML fixture and return the
/// (stdout, stderr) pair. Builds a tempdir, writes `[download].directory`
/// and the supplied `body` into it, then dumps the resolved config.
fn run_config_show(body: &str) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n{body}"),
    )
    .unwrap();
    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn run_config_show_error(body: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("[auth]\nusername = \"x@x.com\"\n\n[download]\ndirectory = \"/photos\"\n{body}"),
    )
    .unwrap();
    let out = clean_cmd()
        .args(["config", "show", "--config", config_path.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .clone();
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_removed_env_config_hint(stderr: &str) {
    assert!(
        stderr.contains("found removed v0.20 env config"),
        "stale sync env vars should emit a migration hint; stderr: {stderr}"
    );
    assert!(
        stderr.contains("KEI_DOWNLOAD_DIR"),
        "stale sync env vars should name KEI_DOWNLOAD_DIR; stderr: {stderr}"
    );
}

#[test]
fn removed_legacy_album_in_toml_errors() {
    let stderr = run_config_show_error("folder_structure = \"{album}/%B\"\n");
    assert!(
        stderr.contains("`{album}` cannot be used in --folder-structure")
            && stderr.contains("--folder-structure-albums"),
        "stderr: {stderr}"
    );
}

#[test]
fn removed_legacy_album_env_is_ignored() {
    sync_cmd_for_validation()
        .env("KEI_FOLDER_STRUCTURE", "{album}/%Y")
        .arg("--only-print-filenames")
        .assert()
        .failure()
        .stderr(predicate::str::contains("`{album}` cannot be used in --folder-structure").not());
}

#[test]
fn removed_legacy_album_errors_even_with_user_set_albums_template() {
    let stderr = run_config_show_error(
        "folder_structure = \"{album}/%Y\"\nfolder_structure_albums = \"{album}/custom\"\n",
    );
    assert!(
        stderr.contains("`{album}` cannot be used in --folder-structure")
            && stderr.contains("--folder-structure-albums"),
        "stderr: {stderr}"
    );
}

#[test]
fn config_show_emits_per_category_templates_from_toml() {
    let (stdout, _) = run_config_show(
        "folder_structure_albums = \"{album}/%Y/%m\"\nfolder_structure_smart_folders = \"{smart-folder}/%Y\"\n",
    );
    assert!(
        stdout.contains("folder_structure_albums = \"{album}/%Y/%m\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("folder_structure_smart_folders = \"{smart-folder}/%Y\""),
        "stdout: {stdout}"
    );
}

/// Default per-category templates stay implicit -- a future refactor that
/// starts emitting the defaults would inflate every dumped config.
#[test]
fn config_show_omits_default_per_category_templates() {
    let (stdout, _) = run_config_show("");
    assert!(
        !stdout.contains("folder_structure_albums"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("folder_structure_smart_folders"),
        "stdout: {stdout}"
    );
}

#[test]
fn config_show_emits_smart_folder_selection() {
    let (stdout, _) =
        run_config_show("\n[filters]\nsmart_folders = [\"Favorites\", \"!Hidden\"]\n");
    assert!(stdout.contains("smart_folders"), "stdout: {stdout}");
    assert!(stdout.contains("Favorites"), "stdout: {stdout}");
    assert!(stdout.contains("!Hidden"), "stdout: {stdout}");
}

#[test]
fn config_show_emits_unfiled_false_when_disabled() {
    let (stdout, _) = run_config_show("\n[filters]\nunfiled = false\n");
    assert!(stdout.contains("unfiled = false"), "stdout: {stdout}");
}

/// Default `unfiled = true` stays implicit -- locks in that defaults don't
/// inflate dumped configs.
#[test]
fn config_show_omits_unfiled_when_default_true() {
    let (stdout, _) = run_config_show("");
    assert!(!stdout.contains("unfiled = true"), "stdout: {stdout}");
}

#[test]
fn config_show_emits_libraries_when_non_default() {
    let (stdout, _) = run_config_show("\n[filters]\nlibraries = [\"all\"]\n");
    assert!(stdout.contains("libraries = [\"all\"]"), "stdout: {stdout}");
}

#[test]
fn config_show_emits_libraries_when_repeatable_named_zone() {
    // Pin the multi-zone case at the binary boundary: a zone-truncated
    // alias plus `primary` must round-trip into a libraries array that
    // contains both. A regression in `LibrarySelector::to_raw()` that
    // dropped the named zone (or collapsed multiple inputs to a single
    // sentinel) lands red here.
    let (stdout, _) =
        run_config_show("\n[filters]\nlibraries = [\"primary\", \"SharedSync-A1B2C3D4\"]\n");
    assert!(
        stdout.contains("libraries"),
        "stdout must include a libraries key:\n{stdout}"
    );
    assert!(
        stdout.contains("primary"),
        "stdout must include primary:\n{stdout}"
    );
    assert!(
        stdout.contains("SharedSync-A1B2C3D4"),
        "stdout must include the named zone:\n{stdout}"
    );
}

#[test]
fn config_show_round_trips_persistent_recent_and_dates() {
    let (stdout, _) = run_config_show(
        "\n[filters]\nrecent = 100\nrecent_scope = \"per-filter\"\nskip_created_before = \"2024-01-01\"\nskip_created_after = \"30d\"\n",
    );
    assert!(stdout.contains("recent = 100"), "stdout: {stdout}");
    assert!(
        stdout.contains("recent_scope = \"per-filter\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skip_created_before = \"2024-01-01\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("skip_created_after = \"30d\""),
        "stdout: {stdout}"
    );
}

#[test]
fn config_show_round_trips_media_filter() {
    let (stdout, _) = run_config_show("\n[filters]\nmedia = [\"photos\", \"live-photos\"]\n");
    assert!(stdout.contains("media"), "stdout: {stdout}");
    assert!(stdout.contains("photos"), "stdout: {stdout}");
    assert!(stdout.contains("live-photos"), "stdout: {stdout}");
}

#[test]
fn config_show_round_trips_escaped_selection_values() {
    let (stdout, _) = run_config_show(
        "\n[filters]\nalbums = [\"=all\", \"=!Drafts\"]\nsmart_folders = [\"=none\"]\nlibraries = [\"=primary\"]\n",
    );
    assert!(stdout.contains("\"=all\""), "stdout: {stdout}");
    assert!(stdout.contains("\"=!Drafts\""), "stdout: {stdout}");
    assert!(stdout.contains("\"=none\""), "stdout: {stdout}");
    assert!(stdout.contains("\"=primary\""), "stdout: {stdout}");
}

#[test]
fn removed_toml_filter_aliases_error() {
    for (field, body) in [
        ("album", "\n[filters]\nalbum = \"Vacation\"\n"),
        (
            "exclude_albums",
            "\n[filters]\nexclude_albums = [\"Drafts\", \"Family\"]\n",
        ),
        ("library", "\n[filters]\nlibrary = \"PrimarySync\"\n"),
        ("skip_videos", "\n[filters]\nskip_videos = true\n"),
        ("skip_photos", "\n[filters]\nskip_photos = true\n"),
    ] {
        let stderr = run_config_show_error(body);
        assert!(
            stderr.contains(&format!("unknown field `{field}`")),
            "expected unknown-field error for {field}; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("pre-v0.20 config"),
            "expected v0.20 migration hint for {field}; stderr:\n{stderr}"
        );
        assert!(
            stderr.contains("docs/v0.20-migration.md"),
            "expected migration guide URL for {field}; stderr:\n{stderr}"
        );
    }
}

#[test]
fn removed_sync_env_vars_do_not_block_non_sync_command() {
    // Regression: issue #385 - stale sync env vars set in old Docker Compose
    // files must not block non-sync subcommands like `kei reset`.
    let temp = tempfile::tempdir().unwrap();
    let mut cmd = clean_cmd();
    cmd.current_dir(temp.path());
    cmd.env("KEI_DOWNLOAD_DIR", "/photos");
    cmd.env("KEI_ALBUM", "none");
    cmd.env("KEI_LIVE_PHOTO_MODE", "image-only");
    cmd.env("KEI_FOLDER_STRUCTURE", "{:%Y/%m/%Y-%m-%d}");
    #[cfg(feature = "xmp")]
    cmd.env("KEI_EMBED_XMP", "true");
    cmd.env("ICLOUD_USERNAME", "test@example.com");
    cmd.args(["reset", "state", "--yes"]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("sync-only flag").not());
}

#[test]
fn removed_sync_env_vars_do_not_supply_sync_config() {
    // Removed sync env mirrors must not keep configuring sync after v0.20.
    // With no TOML [download].directory, this must fail at config resolution
    // even if a stale KEI_DOWNLOAD_DIR is still present in the environment.
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .env("KEI_DOWNLOAD_DIR", "/legacy/photos")
        .env("KEI_ALBUM", "Legacy Album")
        .env("KEI_THREADS", "4")
        .args(["sync", "--only-print-filenames"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Set [download].directory"),
        "stale sync env vars must not provide durable config; stderr: {stderr}"
    );
    assert_removed_env_config_hint(&stderr);
    assert!(
        stderr.contains("ignored in v0.20"),
        "stale sync env vars should explain they are ignored; stderr: {stderr}"
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "stale sync env vars should be ignored by clap, not parsed as CLI args; stderr: {stderr}"
    );
}

#[test]
fn removed_sync_env_vars_do_not_supply_import_config() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .env("KEI_DOWNLOAD_DIR", "/legacy/photos")
        .env("KEI_ALBUM", "Legacy Album")
        .args(["import-existing", "--dry-run"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Set [download].directory"),
        "stale sync env vars must not provide import config; stderr: {stderr}"
    );
    assert_removed_env_config_hint(&stderr);
}

#[test]
fn removed_sync_env_vars_do_not_block_service_status() {
    // Regression: issue #385 - same class of bug on a different non-sync
    // subcommand (service status does not carry SyncArgs).
    let temp = tempfile::tempdir().unwrap();
    let mut cmd = clean_cmd();
    cmd.current_dir(temp.path());
    cmd.env("KEI_DOWNLOAD_DIR", "/photos");
    cmd.env("KEI_ALBUM", "none");
    cmd.env("KEI_LIVE_PHOTO_MODE", "image-only");
    cmd.env("ICLOUD_USERNAME", "test@example.com");
    cmd.args(["service", "status"]);
    cmd.assert()
        .stderr(predicate::str::contains("sync-only flag").not());
}
