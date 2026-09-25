//! Credential sources, session reset, and foreground or service two-factor behavior.

use super::{clean_cmd, sanitize_username, write_fake_two_factor_config};
use crate::common;
use predicates::prelude::{PredicateBooleanExt, predicate};
use std::time::Duration;

#[test]
fn auth_retry_detects_cloudkit_401_as_stale_session() {
    let msg = "import scan aborted: fetcher returned error: HTTP 401 for https://p121-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/private/records/query";
    assert!(
        common::is_retryable_auth_failure(msg),
        "CloudKit 401 should refresh auth and retry the live command"
    );
}

#[test]
fn password_set_headless_accepts_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let password_file = dir.path().join("icloud-password");
    let data_dir = dir.path().join("data");
    let secret = "file-source-secret";
    std::fs::write(&password_file, format!("{secret}\n")).unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", &data_dir)
        .args([
            "password",
            "--password-file",
            password_file.to_str().unwrap(),
            "set",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Password stored in"))
        .stdout(predicate::str::contains(secret).not())
        .stderr(predicate::str::contains(secret).not());
}

#[test]
fn password_set_headless_without_source_fails_before_prompt() {
    let dir = tempfile::tempdir().unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "set"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("stdin is not a terminal"))
        .stderr(predicate::str::contains(
            "kei password --password-file <PATH> set",
        ))
        .stderr(predicate::str::contains(
            "kei password --password-command <COMMAND> set",
        ));
}

fn assert_foreground_two_factor_failure(command: &str) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_fake_two_factor_config(dir.path(), "test@example.com");

    clean_cmd()
        .env("KEI_DATA_DIR", dir.path().join("data"))
        .env("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS", "1")
        .args([command, "--config", config_path.to_str().unwrap()])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("kei login get-code"))
        .stderr(predicate::str::contains("kei login submit-code <CODE>"));
}

#[test]
fn login_two_factor_required_exits_with_auth_code() {
    assert_foreground_two_factor_failure("login");
}

#[test]
fn one_shot_sync_two_factor_required_exits_with_auth_code() {
    assert_foreground_two_factor_failure("sync");
}

#[test]
fn service_two_factor_required_keeps_waiting_for_submitted_code() {
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let config_path = write_fake_two_factor_config(dir.path(), username);
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(
        data_dir.join(format!("{}.session", sanitize_username(username))),
        "{}\n",
    )
    .unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kei"))
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env_remove("KEI_CONFIG")
        .env("KEI_DATA_DIR", &data_dir)
        .env("KEI_UNSTABLE_FAKE_TWO_FACTOR_REQUIRED_FOR_TESTS", "1")
        .args(["service", "run", "--config", config_path.to_str().unwrap()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    std::thread::sleep(Duration::from_secs(1));
    if let Some(status) = child.try_wait().unwrap() {
        let output = child.wait_with_output().unwrap();
        panic!(
            "service exited instead of waiting for 2FA: {status}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    child.kill().unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Waiting for 2FA code submission"),
        "service did not enter durable 2FA wait:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// `password backend` against a fresh cookie dir prints the credential
/// backend name. The backend choice is platform-dependent (OS keyring
/// when available, encrypted file fallback), so we only assert the
/// output is non-empty and exit is clean.
#[test]
fn password_backend_prints_backend_name() {
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "backend"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "password backend must print the backend name, got empty stdout"
    );
}

/// `password clear` against a cookie dir with no stored credential
/// surfaces a clear error rather than silently succeeding. Locks in the
/// "not idempotent" contract so nobody changes the behaviour without
/// noticing the operator-visible impact.
#[test]
fn password_clear_on_empty_store_errors() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No stored credential"));
}

#[test]
fn password_backend_shows_a_backend_name() {
    let dir = tempfile::tempdir().unwrap();
    // Output is one of: "encrypted-file", "keyring", or "none"
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "backend"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("encrypted-file")
                .or(predicate::str::contains("keyring"))
                .or(predicate::str::contains("none")),
        );
}

#[test]
fn password_clear_without_stored_credential_errors() {
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "clear"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No stored credential"));
}

#[test]
fn password_backend_with_empty_data_dir_reports_none() {
    // Fresh data dir with no keyring entry (keyring may still report for the
    // username if it was set outside this test), so we use an unlikely
    // username to minimize false positives.
    let dir = tempfile::tempdir().unwrap();
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "unlikely-empty-store@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["password", "backend"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("none") || stdout.contains("keyring"),
        "expected 'none' (or 'keyring' if system keyring returns stale entry), got: {stdout}"
    );
}

#[test]
fn reset_session_no_session() {
    // With no session files at all, `reset session` reports and exits 0
    // before the confirmation/`--yes` guard fires, mirroring the no-DB
    // early return of `reset state` / `reset sync-token`.
    let dir = tempfile::tempdir().unwrap();
    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "session"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No local session found"));
}

#[test]
fn reset_session_removes_session_files_and_keeps_bystanders() {
    // assert_cmd pipes stdin, so this also covers `--yes` under non-TTY:
    // the guard only fires on the missing-flag path, not in scripted use.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let slug = sanitize_username(username);

    let jar = dir.path().join(&slug);
    let session = dir.path().join(format!("{slug}.session"));
    let cache = dir.path().join(format!("{slug}.cache"));
    let credential = dir.path().join(format!("{slug}.credential"));
    let db = dir.path().join(format!("{slug}.db"));
    for (path, contents) in [
        (&jar, "jar"),
        (&session, "session"),
        (&cache, "cache"),
        (&credential, "cred"),
        (&db, "db"),
    ] {
        std::fs::write(path, contents).unwrap();
    }

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "session", "--yes"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Removed"),
        "stdout should report removed files: {stdout}"
    );
    assert!(
        stdout.contains("kei login"),
        "stdout should point at `kei login`: {stdout}"
    );

    assert!(!jar.exists(), "cookie jar must be removed");
    assert!(!session.exists(), "persisted session must be removed");
    assert!(!cache.exists(), "response cache must be removed");
    assert!(credential.exists(), "password store must survive the reset");
    assert!(db.exists(), "state database must survive the reset");
}

#[test]
fn reset_session_without_yes_on_non_tty_errors() {
    // `kei reset session` ships the same non-interactive guard as
    // `reset sync-token`. Under non-TTY use (CI, scripts, docker exec
    // without -t), running without `--yes` errors out instead of silently
    // doing nothing — or worse, discarding trust tokens by accident.
    let dir = tempfile::tempdir().unwrap();
    let username = "test@example.com";
    let slug = sanitize_username(username);

    let jar = dir.path().join(&slug);
    let session = dir.path().join(format!("{slug}.session"));
    std::fs::write(&jar, "jar").unwrap();
    std::fs::write(&session, "session").unwrap();

    let out = clean_cmd()
        .env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", dir.path())
        .args(["reset", "session"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--yes"),
        "non-tty error must mention --yes; stderr: {stderr}"
    );

    assert!(jar.exists(), "cookie jar must not be removed without --yes");
    assert!(
        session.exists(),
        "persisted session must not be removed without --yes"
    );
}

#[test]
fn password_file_strips_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw.txt");
    std::fs::write(&pw_file, "secret\n").unwrap();

    // Should fail at auth (network), not at password retrieval.
    // The error message should NOT contain "empty" or "No password available".
    let out = clean_cmd()
        .args(["login", "--password-file", pw_file.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("No password available"),
        "password file with newline should work, stderr: {stderr}"
    );
    assert!(
        !stderr.contains("empty"),
        "password should not be empty, stderr: {stderr}"
    );
}

#[test]
fn password_file_empty() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw.txt");
    std::fs::write(&pw_file, "").unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args(["login", "--password-file", pw_file.to_str().unwrap()])
        .assert()
        .code(3)
        .stderr(
            predicate::str::contains("No password available").or(predicate::str::contains("empty")),
        );
}

#[test]
fn password_file_newline_only() {
    let dir = tempfile::tempdir().unwrap();
    let pw_file = dir.path().join("pw.txt");
    std::fs::write(&pw_file, "\n").unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .args(["login", "--password-file", pw_file.to_str().unwrap()])
        .assert()
        .code(3)
        .stderr(
            predicate::str::contains("No password available").or(predicate::str::contains("empty")),
        );
}

// `--password-command` is rejected at startup on Windows (see Flag 8 in the
// audit); the success path this test is asserting only applies on unix.
#[cfg(unix)]
#[test]
fn password_command_success() {
    let dir = tempfile::tempdir().unwrap();

    // The password command succeeds and returns "cmdpw".
    // Auth will fail at network, not at password retrieval.
    let out = clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["login", "--password-command", "echo cmdpw"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("No password available"),
        "password command should provide password, stderr: {stderr}"
    );
}

#[test]
fn password_command_failure() {
    let dir = tempfile::tempdir().unwrap();

    clean_cmd()
        .env("ICLOUD_USERNAME", "test@example.com")
        .env("KEI_DATA_DIR", dir.path())
        .args(["login", "--password-command", "false"])
        .assert()
        .code(3)
        .stderr(
            predicate::str::contains("No password was available")
                .or(predicate::str::contains("exited with status")),
        );
}

#[cfg(unix)]
#[test]
fn startup_scrubs_password_before_dispatching_password_command() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[auth]\nusername = 'startup@example.test'\n").unwrap();
    // Empty output fails before storing credentials. The other branch reports
    // a different failure if the child inherits the captured environment secret.
    clean_cmd()
        .env("ICLOUD_PASSWORD", "startup-environment-secret")
        .env("KEI_DATA_DIR", dir.path().join("data"))
        .args([
            "password",
            "--config",
            config_path.to_str().unwrap(),
            "--password-command",
            "test -z \"${ICLOUD_PASSWORD+x}\"",
            "set",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("did not print a password"))
        .stdout(predicate::str::contains("startup-environment-secret").not())
        .stderr(predicate::str::contains("startup-environment-secret").not());
}
