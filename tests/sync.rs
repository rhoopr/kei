#![allow(
    clippy::string_slice,
    reason = "test assertions on known-ASCII filenames"
)]
//! Sync tests with behavioral assertions (live iCloud API).
//!
//! Uses the bounded selection in `data/live-selection.toml`. No named album
//! or particular media format is required. Exact content lives in fixture tests.
//!
//! Live tests are `#[ignore]` and require iCloud credentials. Shared helper
//! tests run offline without `--ignored`. Run the live tests with:
//!
//! ```sh
//! cargo test --all-features --test sync -- --ignored --test-threads=1
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing,
    reason = "live assertions and shared helpers use panics, diagnostics, and bounded fixture casts and indexing"
)]

mod common;

use predicates::prelude::*;
use std::time::Duration;
use tempfile::tempdir;

const TIMEOUT_SECS: u64 = 180;
const TIMEOUT_META: u64 = 90;

#[derive(Debug, Default)]
struct SyncToml<'a> {
    download: &'a str,
    filters: &'a str,
    photos: &'a str,
    metadata: &'a str,
    watch: &'a str,
    server: &'a str,
    notifications: &'a str,
    report: &'a str,
}

/// Build a sync command with the shared bounded primary-library selection.
fn library_cmd(
    username: &str,
    password: &str,
    cookie_dir: &std::path::Path,
    download_dir: &std::path::Path,
) -> assert_cmd::Command {
    library_cmd_with_toml(
        username,
        password,
        cookie_dir,
        download_dir,
        SyncToml::default(),
    )
}

fn library_cmd_with_toml(
    username: &str,
    password: &str,
    cookie_dir: &std::path::Path,
    download_dir: &std::path::Path,
    toml: SyncToml<'_>,
) -> assert_cmd::Command {
    let config_path = library_config(cookie_dir, download_dir, toml);
    let mut cmd = common::cmd();
    cmd.env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", cookie_dir);
    cmd.args([
        "sync",
        "--password",
        password,
        "--config",
        config_path.to_str().unwrap(),
        "--no-progress-bar",
    ]);
    cmd
}

fn library_config(
    data_dir: &std::path::Path,
    download_dir: &std::path::Path,
    toml: SyncToml<'_>,
) -> std::path::PathBuf {
    let mut body = format!(
        "[download]\ndirectory = {}\n{}[filters]\n",
        common::toml_string(&download_dir.to_string_lossy()),
        toml.download,
    );
    body.push_str(toml.filters);
    for (section, content) in [
        ("photos", toml.photos),
        ("metadata", toml.metadata),
        ("watch", toml.watch),
        ("server", toml.server),
        ("notifications", toml.notifications),
        ("report", toml.report),
    ] {
        if !content.is_empty() {
            body.push_str(&format!("[{section}]\n{content}"));
        }
    }
    common::live_selection::write_live_config(data_dir, "sync-live", &body)
}

fn config_for_download_dir(
    data_dir: &std::path::Path,
    download_dir: &std::path::Path,
) -> std::path::PathBuf {
    let body = format!(
        "[download]\ndirectory = {}\n",
        common::toml_string(&download_dir.to_string_lossy())
    );
    common::live_selection::write_live_config(data_dir, "sync-live", &body)
}

fn reset_sync_tokens(cookie_dir: &std::path::Path) {
    common::cmd()
        .env("KEI_DATA_DIR", cookie_dir)
        .args(["reset", "sync-token", "--yes"])
        .timeout(Duration::from_secs(10))
        .assert()
        .success();
}

// ── Metadata (no downloads) ─────────────────────────────────────────────

#[test]
#[ignore]
fn list_albums_prints_album_names() {
    let (username, _password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args(["list", "albums"])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .success()
            .stdout(predicate::str::contains("Library:"));
    });
}

#[test]
#[ignore]
fn list_libraries_prints_output() {
    let (username, _password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args(["list", "libraries"])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .success()
            .stdout(predicate::str::contains("Libraries:"));
    });
}

// ── Core download ───────────────────────────────────────────────────────

/// Downloads the bounded primary-library selection without format assumptions.
#[test]
#[ignore]
fn sync_bounded_library_downloads_media() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(!files.is_empty(), "bounded selection downloaded no media");

        // All files should be non-empty
        for f in &files {
            let size = std::fs::metadata(f).unwrap().len();
            assert!(size > 0, "file should be non-empty: {}", f.display());
        }
    });
}

/// Dry-run should list assets but not write any files to disk.
#[test]
#[ignore]
fn sync_dry_run_downloads_nothing() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--dry-run"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            files.is_empty(),
            "dry-run should download nothing, found: {files:?}"
        );
    });
}

/// Running sync twice should not re-download or modify any files.
#[test]
#[ignore]
fn sync_idempotent_second_run_noop() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        // First sync
        library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files_first = common::walkdir(download_dir.path());
        assert!(!files_first.is_empty(), "first sync should download files");

        let mtimes_before: Vec<_> = files_first
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
            .collect();

        // Second sync — should be a no-op
        library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files_second = common::walkdir(download_dir.path());
        assert_eq!(
            files_first.len(),
            files_second.len(),
            "second sync should not create additional files"
        );

        let mtimes_after: Vec<_> = files_second
            .iter()
            .map(|p| std::fs::metadata(p).unwrap().modified().unwrap())
            .collect();
        assert_eq!(
            mtimes_before, mtimes_after,
            "files should not be re-written on second sync"
        );
    });
}

// ── Media filters ───────────────────────────────────────────────────────

/// A media filter that selects only live photos plus a live-photo mode that
/// drops them is rejected at startup rather than silently completing with zero
/// downloads.
#[test]
#[ignore]
fn sync_skip_all_media_rejected_at_startup() {
    let (username, password, cookie_dir) = common::require_preauth();

    let download_dir = tempdir().expect("tempdir");

    library_cmd_with_toml(
        &username,
        &password,
        &cookie_dir,
        download_dir.path(),
        SyncToml {
            filters: "media = [\"live-photos\"]\n",
            photos: "live_photo_mode = \"skip\"\n",
            ..SyncToml::default()
        },
    )
    .timeout(Duration::from_secs(TIMEOUT_META))
    .assert()
    .failure()
    .stderr(predicate::str::contains("would download nothing"));
}

// ── Misc flags ──────────────────────────────────────────────────────────

/// --temp-suffix .downloading should leave no temp files after a successful sync.
#[test]
#[ignore]
fn sync_temp_suffix_leaves_no_remnants() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        library_cmd_with_toml(
            &username,
            &password,
            &cookie_dir,
            download_dir.path(),
            SyncToml {
                download: "temp_suffix = \".downloading\"\n",
                ..SyncToml::default()
            },
        )
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .assert()
        .success();

        let all_files = common::walkdir(download_dir.path());
        assert!(
            !all_files.is_empty(),
            "should download files with --temp-suffix"
        );
        let temp_files: Vec<_> = all_files
            .iter()
            .filter(|p| p.to_str().unwrap_or("").ends_with(".downloading"))
            .collect();
        assert!(
            temp_files.is_empty(),
            "no .downloading temp files should remain: {temp_files:?}"
        );
    });
}

/// --threads value should appear as concurrency=N in log output.
#[test]
#[ignore]
fn sync_threads_reflected_in_log() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        let assertion = library_cmd_with_toml(
            &username,
            &password,
            &cookie_dir,
            download_dir.path(),
            SyncToml {
                download: "threads = 1\n",
                ..SyncToml::default()
            },
        )
        .args(["--log-level", "info"])
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .assert()
        .success();

        let stderr = String::from_utf8_lossy(&assertion.get_output().stderr);
        let clean = common::strip_ansi(&stderr);
        assert!(
            clean.contains("concurrency=1"),
            "log should reflect --threads 1, stderr:\n{clean}"
        );
    });
}

/// --only-print-filenames emits at least one filename to stdout and
/// writes nothing to disk.
#[test]
#[ignore]
fn sync_only_print_filenames_emits_names_without_downloading() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        // --dry-run makes the test state-independent: kei emits a
        // filename for every album member regardless of what the state
        // DB already considers downloaded.
        let out = library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--only-print-filenames", "--dry-run"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();

        let stdout = String::from_utf8_lossy(&out.stdout);
        let non_log_lines: Vec<&str> = stdout
            .lines()
            .filter(|l| {
                !l.is_empty()
                    && !l.contains("INFO ")
                    && !l.contains("WARN ")
                    && !l.contains("ERROR ")
            })
            .collect();
        assert!(
            !non_log_lines.is_empty(),
            "--only-print-filenames must emit at least one filename, stdout was:\n{stdout}"
        );

        let files = common::walkdir(download_dir.path());
        assert!(
            files.is_empty(),
            "--only-print-filenames must not write files, found: {files:?}"
        );
    });
}

/// [notifications].script should be called with KEI_EVENT set.
#[test]
#[ignore]
fn sync_notification_script_fires_event() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let script_dir = tempdir().expect("tempdir");
        let marker = script_dir.path().join("notified.txt");

        let script_path = script_dir.path().join("notify.sh");
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\necho \"$KEI_EVENT\" > {}\n", marker.display()),
        )
        .expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        library_cmd_with_toml(
            &username,
            &password,
            &cookie_dir,
            download_dir.path(),
            SyncToml {
                notifications: &format!(
                    "script = {}\n",
                    common::toml_string(&script_path.to_string_lossy())
                ),
                ..SyncToml::default()
            },
        )
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .assert()
        .success();

        assert!(
            marker.exists(),
            "notification script should create marker file"
        );
        let content = std::fs::read_to_string(&marker).expect("read marker");
        assert!(
            content.trim() == "sync_complete" || content.trim() == "sync_failed",
            "marker file should contain a known event name, got: {:?}",
            content.trim()
        );
    });
}

/// --pid-file should be created during sync and removed after completion.
#[test]
#[ignore]
fn sync_pid_file_cleaned_up_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let pid_dir = tempdir().expect("tempdir");
        let pid_file = pid_dir.path().join("test.pid");

        library_cmd_with_toml(
            &username,
            &password,
            &cookie_dir,
            download_dir.path(),
            SyncToml {
                watch: &format!(
                    "pid_file = {}\n",
                    common::toml_string(&pid_file.to_string_lossy())
                ),
                ..SyncToml::default()
            },
        )
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .assert()
        .success();

        assert!(
            !pid_file.exists(),
            "PID file should be removed after sync completes"
        );

        // Verify sync actually ran (downloaded files)
        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "sync with --pid-file should still download files"
        );
    });
}

// ── Explicit sync invocation ────────────────────────────────────────────

/// The explicit `sync` subcommand should run the sync worker.
#[test]
#[ignore]
fn sync_explicit_invocation_works() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let config_path = library_config(&cookie_dir, download_dir.path(), SyncToml::default());

        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args([
                "sync",
                "--password",
                &password,
                "--config",
                config_path.to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        assert!(
            !files.is_empty(),
            "sync invocation should download bounded media, got {}",
            files.len()
        );
        for f in &files {
            let size = std::fs::metadata(f).unwrap().len();
            assert!(size > 0, "file should be non-empty: {}", f.display());
        }
    });
}

// ── Error paths (no network) ────────────────────────────────────────────

#[test]
#[ignore]
fn sync_without_directory_fails() {
    let (username, password, cookie_dir) = common::require_preauth();
    let config_path = common::live_selection::write_live_config(&cookie_dir, "sync-live-empty", "");

    common::cmd()
        .env("ICLOUD_USERNAME", &username)
        .env("KEI_DATA_DIR", &cookie_dir)
        .args([
            "sync",
            "--password",
            &password,
            "--config",
            config_path.to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(Duration::from_secs(TIMEOUT_META))
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("directory").or(predicate::str::contains("--download-dir")),
        );
}

// ── Error paths (auth required) ─────────────────────────────────────────

#[test]
#[ignore]
fn sync_nonexistent_album_fails() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let config_path = library_config(
            &cookie_dir,
            download_dir.path(),
            SyncToml {
                filters: "albums = [\"ThisAlbumDefinitelyDoesNotExist999\"]\nunfiled = false\n",
                ..SyncToml::default()
            },
        );

        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args([
                "sync",
                "--password",
                &password,
                "--config",
                config_path.to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .failure()
            .stderr(predicate::str::contains("not found"));
    });
}

#[test]
#[ignore]
fn sync_nonexistent_library_fails() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let body = format!(
            "[download]\ndirectory = {}\n[filters]\nlibraries = [\"NonExistentLibrary-ZZZZZ\"]\n",
            common::toml_string(&download_dir.path().to_string_lossy())
        );
        let config_path =
            common::live_selection::write_live_config(&cookie_dir, "sync-live", &body);

        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args([
                "sync",
                "--password",
                &password,
                "--config",
                config_path.to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(TIMEOUT_META))
            .assert()
            .failure()
            .stderr(
                predicate::str::contains("error")
                    .or(predicate::str::contains("Error"))
                    .or(predicate::str::contains("ERROR")),
            );
    });
}

// ── New subcommand tests ───────────────────────────────────────────────

#[test]
#[ignore]
fn login_authenticates_successfully() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args(["login", "--password", &password])
            .timeout(Duration::from_secs(60))
            .assert()
            .success();
    });
}

#[test]
#[ignore]
fn list_albums_new_syntax() {
    let (username, _password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args(["list", "albums"])
            .timeout(Duration::from_secs(60))
            .assert()
            .success()
            .stdout(predicate::str::contains("Library:"));
    });
}

#[test]
#[ignore]
fn sync_retry_failed_flag() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let config_path = config_for_download_dir(&cookie_dir, download_dir.path());

        // sync --retry-failed with no prior failures should succeed (noop)
        common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args([
                "sync",
                "--retry-failed",
                "--password",
                &password,
                "--config",
                config_path.to_str().unwrap(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();
    });
}

#[test]
#[ignore]
fn sync_bounded_second_run_preserves_checkpoint_and_skips_download() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        // First sync: full enumeration
        library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let first_count = common::walkdir(download_dir.path()).len();
        assert!(first_count > 0, "first sync should download files");

        let db_path = std::fs::read_dir(cookie_dir.as_path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "db"))
            .expect("first sync creates the state DB");
        let db = rusqlite::Connection::open(db_path).unwrap();
        let token = || {
            use rusqlite::OptionalExtension;
            db.query_row(
                "SELECT value FROM metadata WHERE key = 'sync_token:PrimarySync'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .unwrap()
        };
        let first_token = token();

        // A bounded partial inventory must not manufacture an incremental token.
        let output = library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .args(["--log-level", "debug"])
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .output()
            .unwrap();

        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        if first_token.is_some() {
            assert!(stderr.contains("sync_mode=\"incremental\""), "{stderr}");
        } else {
            assert!(
                stderr.contains("recent_limited_full_enumeration"),
                "{stderr}"
            );
            assert!(token().is_none(), "partial inventory must not checkpoint");
        }
        assert!(
            stderr.contains("No new photos to download")
                || stderr.contains("All incremental assets already downloaded")
                || stderr.contains("0 downloaded"),
            "second run must report no new downloads: {stderr}"
        );
        assert_eq!(common::walkdir(download_dir.path()).len(), first_count);
    });
}

// ── Watch mode, report JSON, multi-album ────────────────────────────────

/// Verify `--watch-with-interval` drives multiple sync cycles within one run.
///
/// Runs at the minimum allowed interval (60 s, enforced by the CLI parser),
/// streams stderr line-by-line on a background thread, and exits as soon as
/// the second `Waiting before next cycle` marker is observed. Total wall
/// time is bounded by a hard 150 s deadline so a stuck watch loop fails the
/// test rather than hanging the suite.
///
/// Earlier revisions used `thread::sleep(135s)` then matched on a captured
/// stderr blob; that pattern silently regressed if the interval was honored
/// but the marker text changed (or vice versa) and burned the full window
/// even on success. The streaming reader catches both without adding cost.
#[test]
#[ignore]
fn sync_watch_runs_multiple_cycles() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        use std::sync::mpsc;
        use std::thread;
        use std::time::Instant;

        let download_dir = tempdir().expect("tempdir");
        let config_path = library_config(
            &cookie_dir,
            download_dir.path(),
            SyncToml {
                watch: "interval = 60\n",
                // Watch mode starts the HTTP health/metrics server. Do not
                // compete for the production default 9090 during the live
                // suite; a locally running kei service or another smoke test
                // may legitimately own it.
                server: "bind = \"127.0.0.1\"\nport = 0\n",
                ..SyncToml::default()
            },
        );
        let bin = env!("CARGO_BIN_EXE_kei");
        let mut child = Command::new(bin)
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &cookie_dir)
            .args([
                "sync",
                "--password",
                &password,
                "--config",
                config_path.to_str().unwrap(),
                "--no-progress-bar",
                "--log-level",
                "info",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kei");

        // Stream stderr on a worker so the test can react as soon as the
        // second cycle marker appears, instead of waiting for a fixed sleep.
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel::<String>();
        let reader_handle = thread::spawn(move || {
            let mut buffered = BufReader::new(stderr);
            let mut accum = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                match buffered.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        accum.push_str(&line);
                        // Best effort: send each line. Drop on send failure
                        // (test thread already exited, e.g. assertion fired).
                        let _ = tx.send(line.clone());
                    }
                    Err(_) => break,
                }
            }
            accum
        });

        // Watch the channel for 2 marker hits, bounded by 150 s wall time.
        // 60 s * 2 cycles + buffer for one cycle of download work.
        let deadline = Instant::now() + Duration::from_secs(150);
        let mut markers = 0_usize;
        let mut snippet = String::new();
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(line) => {
                    snippet.push_str(&line);
                    if common::strip_ansi(&line).contains("Waiting before next cycle") {
                        markers += 1;
                        if markers >= 2 {
                            break;
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }

        let _ = child.kill();
        let _ = child.wait();
        // The reader thread will see EOF after kill and join cleanly.
        let full_stderr = reader_handle.join().unwrap_or_default();

        assert!(
            markers >= 2,
            "watch should drive at least 2 cycles, got {markers}. \
             snippet: {}\n--- full stderr (first 2000 chars) ---\n{}",
            common::strip_ansi(&snippet)
                .chars()
                .take(800)
                .collect::<String>(),
            common::strip_ansi(&full_stderr)
                .chars()
                .take(2000)
                .collect::<String>()
        );
    });
}

/// Verify `--report-json` writes a parseable report with the documented schema.
#[test]
#[ignore]
fn sync_report_json_writes_valid_schema() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");
        let report_dir = tempdir().expect("tempdir");
        let report_path = report_dir.path().join("report.json");

        library_cmd_with_toml(
            &username,
            &password,
            &cookie_dir,
            download_dir.path(),
            SyncToml {
                report: &format!(
                    "json = {}\n",
                    common::toml_string(&report_path.to_string_lossy())
                ),
                ..SyncToml::default()
            },
        )
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .assert()
        .success();

        let body = std::fs::read_to_string(&report_path).expect("report file");
        let json: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(json["version"], "3", "schema version");
        assert!(json["kei_version"].is_string(), "kei_version present");
        assert!(json["timestamp"].is_string(), "timestamp present");
        let status = json["status"].as_str().expect("status string");
        assert!(
            matches!(status, "success" | "partial_failure" | "session_expired"),
            "unexpected status: {status}"
        );
        assert!(json["options"].is_object(), "options object");
        assert_eq!(json["options"]["username"], username.as_str());
        assert!(json["stats"].is_object(), "stats object");
    });
}

// ── Download integrity ──────────────────────────────────────────────────

/// Data-sacred invariant: if the user (or `rm -rf` accident) deletes a synced
/// file, a full reconciliation must restore it. A silent skip here would mean
/// kei "loses" the file permanently.
#[test]
#[ignore]
fn sync_recovers_deleted_file() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let before = common::walkdir(download_dir.path());
        assert!(!before.is_empty(), "expected media after first sync");

        // Select any media format, retain its exact bytes, then delete it.
        let victim = before.first().expect("at least one media file").clone();
        let original_bytes = std::fs::read(&victim).unwrap();
        let expected_size = original_bytes.len() as u64;
        std::fs::remove_file(&victim).expect("delete victim");
        assert!(!victim.exists(), "victim deleted");

        // Re-sync with a full enumeration so the filter can notice the
        // missing file. A normal incremental sync only receives new iCloud
        // deltas, so local disk drift must be tested with the cursor reset.
        reset_sync_tokens(&cookie_dir);

        // Full enumeration can notice the missing file.
        // Captured output is included in the assertion below so an intermittent
        // skip-where-recovery-was-expected leaves a usable trail (data-sacred
        // invariant: a silent skip here means kei "loses" the file).
        let assert = library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .env("RUST_LOG", "kei=debug")
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

        assert!(
            victim.exists(),
            "deleted file should be re-downloaded: {}\n--- post-sync walkdir ---\n{:#?}\n--- kei stderr ---\n{stderr}",
            victim.display(),
            common::walkdir(download_dir.path()),
        );
        let after_size = std::fs::metadata(&victim).unwrap().len();
        assert_eq!(
            after_size, expected_size,
            "recovered file should match original size"
        );
        assert_eq!(std::fs::read(&victim).unwrap(), original_bytes);
    });
}

/// Data-sacred invariant: a truncated file left on disk (e.g. from a crashed
/// write) must not mask the real photo during full reconciliation. The default
/// `name-size-dedup-with-suffix` policy preserves the existing file untouched
/// and downloads the real photo alongside with a size suffix in the filename.
/// Either way, the correctly-sized photo bytes must end up on disk.
#[test]
#[ignore]
fn sync_truncated_file_does_not_cause_data_loss() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let download_dir = tempdir().expect("tempdir");

        library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();

        let files = common::walkdir(download_dir.path());
        let victim = files.first().expect("media file").clone();
        let expected_size = std::fs::metadata(&victim).unwrap().len();
        let original_bytes = std::fs::read(&victim).unwrap();
        let parent = victim.parent().unwrap().to_path_buf();

        // Truncate to zero bytes -- simulates a crashed write leaving an empty file.
        std::fs::File::create(&victim)
            .expect("truncate")
            .set_len(0)
            .expect("set_len 0");
        assert_eq!(std::fs::metadata(&victim).unwrap().len(), 0);

        // Re-sync with a full enumeration so path planning compares the
        // truncated local file with iCloud's expected size. A normal
        // incremental sync only receives new iCloud deltas.
        reset_sync_tokens(&cookie_dir);

        // Captured output is included in the assertion below so an
        // intermittent skip-where-recovery-was-expected leaves a usable
        // trail (data-sacred invariant: zero-byte file must not mask the
        // real photo).
        let assert = library_cmd(&username, &password, &cookie_dir, download_dir.path())
            .env("RUST_LOG", "kei=debug")
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .assert()
            .success();
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();

        // The correctly-sized photo must exist somewhere under the same folder
        // (either overwriting the zero-byte file or as a size-suffixed sibling).
        let candidates: Vec<_> = common::walkdir(&parent)
            .into_iter()
            .filter(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) == expected_size)
            .collect();
        assert!(
            !candidates.is_empty(),
            "after re-sync, the correctly-sized photo must be on disk somewhere in {parent:?} (expected {expected_size} bytes)\n--- post-sync walkdir ---\n{:#?}\n--- kei stderr ---\n{stderr}",
            common::walkdir(&parent),
        );
        let recovered = std::fs::read(&candidates[0]).unwrap();
        assert_eq!(
            recovered, original_bytes,
            "recovered photo content must match the original"
        );
    });
}

// ── Bad credentials (LAST -- hits auth from scratch, burns rate limit) ──

#[test]
#[ignore]
fn zz_bad_credentials_fails() {
    let cookie_dir = tempdir().expect("tempdir");
    let download_dir = tempdir().expect("tempdir");
    let config_path = config_for_download_dir(cookie_dir.path(), download_dir.path());

    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env("ICLOUD_USERNAME", "nonexistent-xyz@icloud.com")
        .env("KEI_DATA_DIR", cookie_dir.path())
        .args([
            "sync",
            "--password",
            "wrong-password",
            "--config",
            config_path.to_str().unwrap(),
            "--no-progress-bar",
        ])
        .timeout(Duration::from_secs(60))
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("error")
                .or(predicate::str::contains("Error"))
                .or(predicate::str::contains("ERROR")),
        );
}

// ── Helpers ─────────────────────────────────────────────────────────────
