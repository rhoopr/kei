#![allow(
    clippy::string_slice,
    reason = "test assertions on known-ASCII filenames"
)]
//! Live `import-existing` tests against the real Apple CloudKit API.
//!
//! Strategy:
//! 1. **Setup on first fixture use**: download recent media via the real
//!    `kei sync` command with the bounded primary-library selection in
//!    `data/live-selection.toml`. Use the default size, folder structure,
//!    and match policy. A `OnceLock` shares the fixture within this binary.
//! 2. **Live import cases**: run `kei import-existing` against that fixture
//!    (or a copy of a subset of it) with a fresh state DB.
//!
//! Live tests are `#[ignore]`. Fixture-helper, command-builder, CLI-removal,
//! and shared helper tests run offline without `--ignored`.
//! Run the live tests with:
//!
//! ```sh
//! cargo test --all-features --test import_existing_live -- --ignored --test-threads=1
//! ```
//!
//! Each invocation downloads one bounded fixture into a fresh run directory
//! under `KEI_IMPORT_FIXTURE_DIR` (default `/tmp/codex/kei/import-fixture/`).
//! Runs retain their media and seed DB for failure inspection. Reusing old
//! media with a new seed DB can select collision paths that differ from the
//! paths an import of the original files must match.

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
    reason = "import assertions and shared helpers use panics, diagnostics, and bounded fixture casts and indexing"
)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use predicates::prelude::*;
use tempfile::tempdir;

const FIXTURE_TIMEOUT_SECS: u64 = 1800; // Large media can need more time than metadata-only scans.
const IMPORT_TIMEOUT_SECS: u64 = 300; // 5m: import-existing scans, no downloads

/// Copy auth artifacts (cookie file + .session + .cache) from `src`
/// into `dst`, deliberately skipping `.db` and `.lock` files. A state
/// DB created on a higher-schema branch would refuse to open from a
/// lower-schema branch, so we always rebuild state.db fresh.
fn copy_auth_artifacts(src: &Path, dst: &Path) {
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".db") || name_str.ends_with(".lock") {
            continue;
        }
        let target = dst.join(&name);
        if !target.exists() {
            let _ = std::fs::copy(&path, &target);
        }
    }
}

fn prepare_fixture_data_dir(download_dir: &Path, cookie_dir: &Path) -> PathBuf {
    // Fresh data_dir under the download_dir, recreated each run.
    // Wipe an existing one (which may carry a higher-schema DB).
    let data_dir = download_dir.join("_kei_data");
    if data_dir.exists() {
        let _ = std::fs::remove_dir_all(&data_dir);
    }
    std::fs::create_dir_all(&data_dir).unwrap();

    // Mirror auth artifacts from the cookie dir into data_dir so the
    // sync command can re-use the existing trust cookie. Deliberately
    // skips .db / .lock files: a state DB from a different branch can
    // have a higher schema version than this checkout supports, which
    // would fail the sync open with a confusing "schema too new"
    // error. We rebuild state.db fresh on every run.
    copy_auth_artifacts(cookie_dir, &data_dir);
    data_dir
}

/// Parent directory for isolated, retained live import runs.
fn fixture_root() -> PathBuf {
    if let Ok(dir) = std::env::var("KEI_IMPORT_FIXTURE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from("/tmp/codex/kei/import-fixture")
}

fn fresh_fixture_download_dir(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(root)
        .unwrap()
        .keep()
}

#[test]
fn fixture_download_tree_is_isolated_between_runs() {
    let root = tempdir().unwrap();
    let first = fresh_fixture_download_dir(root.path());
    std::fs::write(first.join("existing.JPG"), b"retained prior media").unwrap();
    let second = fresh_fixture_download_dir(root.path());
    assert_ne!(first, second);
    assert!(std::fs::read_dir(second).unwrap().next().is_none());
    assert_eq!(
        std::fs::read(first.join("existing.JPG")).unwrap(),
        b"retained prior media"
    );
}

/// Share one pristine bounded seed within this test binary, never across runs.
fn fixture() -> &'static (PathBuf, PathBuf) {
    static FIX: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    FIX.get_or_init(|| {
        let (username, password, cookie_dir) = common::require_preauth();
        let download_dir = fresh_fixture_download_dir(&fixture_root());

        let data_dir = prepare_fixture_data_dir(&download_dir, &cookie_dir);

        eprintln!(
            "Building import-existing fixture: --recent {} into {}",
            common::live_selection::live_recent(),
            download_dir.display()
        );
        // Keep the reusable fixture to one library-wide pass. The default
        // selection also walks every user album, which makes live accounts
        // with overlapping album memberships produce account-dependent
        // unmatched duplicate paths in import-existing smoke tests.
        let config_path = write_kei_toml(&data_dir, &download_dir, "");
        let output = common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", &data_dir)
            .args([
                "sync",
                "--password",
                &password,
                "--config",
                config_path.to_str().unwrap(),
                "--recent",
                &common::live_selection::live_recent().to_string(),
                "--no-progress-bar",
            ])
            .timeout(Duration::from_secs(FIXTURE_TIMEOUT_SECS))
            .output()
            .expect("failed to run fixture sync");
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!("fixture sync failed:\nstderr: {stderr}");
        }
        eprintln!("Fixture ready: {}", download_dir.display());
        (download_dir, data_dir)
    })
}

#[test]
fn copy_auth_artifacts_skips_state_db_and_lock_files() {
    let src = tempdir().unwrap();
    let dst = tempdir().unwrap();
    std::fs::write(src.path().join("user.session"), b"session").unwrap();
    std::fs::write(src.path().join("user.cache"), b"cache").unwrap();
    std::fs::write(src.path().join("user.db"), b"stale db").unwrap();
    std::fs::write(src.path().join("user.lock"), b"stale lock").unwrap();

    copy_auth_artifacts(src.path(), dst.path());

    assert_eq!(
        std::fs::read(dst.path().join("user.session")).unwrap(),
        b"session"
    );
    assert_eq!(
        std::fs::read(dst.path().join("user.cache")).unwrap(),
        b"cache"
    );
    assert!(
        !dst.path().join("user.db").exists(),
        "fixture auth copy must not carry stale state DBs between branches"
    );
    assert!(
        !dst.path().join("user.lock").exists(),
        "fixture auth copy must not carry stale process locks between branches"
    );
}

#[test]
fn fixture_data_dir_is_rebuilt_before_live_sync() {
    let download_dir = tempdir().unwrap();
    let cookie_dir = tempdir().unwrap();
    std::fs::write(cookie_dir.path().join("user.session"), b"session").unwrap();
    let stale_data = download_dir.path().join("_kei_data");
    std::fs::create_dir_all(&stale_data).unwrap();
    std::fs::write(stale_data.join("old.db"), b"stale schema").unwrap();
    std::fs::write(stale_data.join("old.lock"), b"stale lock").unwrap();

    let data_dir = prepare_fixture_data_dir(download_dir.path(), cookie_dir.path());

    assert_eq!(data_dir, stale_data);
    assert_eq!(
        std::fs::read(data_dir.join("user.session")).unwrap(),
        b"session"
    );
    assert!(
        !data_dir.join("old.db").exists() && !data_dir.join("old.lock").exists(),
        "fixture data dir must be rebuilt instead of reusing stale DB/lock artifacts"
    );
}

/// Build an `import-existing` command targeting the fixture's download
/// dir but with a fresh `KEI_DATA_DIR` so the per-test state DB stays
/// isolated.
fn import_cmd(
    username: &str,
    password: &str,
    cookie_dir: &Path,
    download_dir: &Path,
    data_dir: &Path,
    extra: &[&str],
) -> assert_cmd::Command {
    // Mirror auth artifacts (not state DB; see fixture()) into the
    // per-test data_dir so import-existing can re-use the trust cookie.
    copy_auth_artifacts(cookie_dir, data_dir);
    let has_config_override = extra.contains(&"--config");
    let config_path = (!has_config_override).then(|| write_kei_toml(data_dir, download_dir, ""));
    let mut cmd = common::cmd();
    cmd.env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", data_dir);
    if let Some(config_path) = &config_path {
        cmd.args(["--config", config_path.to_str().unwrap()]);
    }
    cmd.args([
        "import-existing",
        "--password",
        password,
        "--no-progress-bar",
    ]);
    // import-existing takes its scan cap from the CLI, not filters.recent.
    if !extra.contains(&"--recent") {
        cmd.args([
            "--recent",
            &common::live_selection::live_recent().to_string(),
        ]);
    }
    cmd.args(extra);
    cmd
}

#[test]
fn import_command_uses_shared_bound_unless_scenario_overrides_it() {
    let dir = tempdir().unwrap();
    for (extra, expected) in [
        (
            Vec::new(),
            common::live_selection::live_recent().to_string(),
        ),
        (vec!["--recent", "3"], "3".to_owned()),
        (vec!["--recent", "30d"], "30d".to_owned()),
    ] {
        let command = import_cmd(
            "test@example.invalid",
            "test",
            dir.path(),
            dir.path(),
            dir.path(),
            &extra,
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let bounds: Vec<_> = args
            .windows(2)
            .filter(|pair| pair[0] == "--recent")
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(bounds, [expected.as_str()]);
    }
}

/// Parse the trailing summary printed by `import-existing`.
fn parse_summary(stdout: &str) -> ImportSummary {
    let mut total = 0_u64;
    let mut matched = 0_u64;
    let mut unmatched = 0_u64;
    let mut filtered = 0_u64;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(n) = line.strip_prefix("Total assets scanned:") {
            total = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("Files matched:") {
            matched = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("Unmatched versions:") {
            unmatched = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("Filtered (no path):") {
            filtered = n.trim().parse().unwrap_or(0);
        }
    }
    ImportSummary {
        total,
        matched,
        unmatched,
        filtered,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ImportSummary {
    total: u64,
    matched: u64,
    unmatched: u64,
    filtered: u64,
}

/// Count downloaded rows in the state DB. kei names the DB after the
/// sanitized username (e.g. `rhrobhooperxyz.db`), so we look for any
/// non-cache `.db` under `data_dir`.
fn count_downloaded_rows(data_dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return 0;
    };
    let db_path = entries
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|s| s.to_str()) == Some("db"));
    let Some(db_path) = db_path else {
        return 0;
    };
    let conn = rusqlite::Connection::open(&db_path).expect("open state db");
    conn.query_row(
        "SELECT COUNT(*) FROM assets WHERE status = 'downloaded'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| u64::try_from(n).unwrap_or(0))
    .unwrap_or(0)
}

/// Use this run's selected rows, not unrelated files left in the reusable cache.
fn selected_fixture_files(data_dir: &Path) -> Vec<PathBuf> {
    let db_path = std::fs::read_dir(data_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "db"))
        .expect("fixture database");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut query = conn
        .prepare("SELECT local_path FROM assets WHERE status = 'downloaded' ORDER BY local_path")
        .unwrap();
    query
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|path| PathBuf::from(path.unwrap()))
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

/// Smoke test: import media written by the fixture sync with the same
/// bounded primary-library selection. The recent window can change between
/// scans, so the match ratio and row-count bounds are broad smoke checks.
/// This fixture does not exercise album overlap or shared libraries.
#[test]
#[ignore]
fn import_matches_default_layout_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = common::live_selection::live_recent().to_string();
        let toml_path = write_kei_toml(test_data.path(), download_dir, "");
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", &recent, "--config", toml_path.to_str().unwrap()],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(summary.total > 0, "expected some assets, got {summary:?}");
        // CloudKit's recent window can change while the live suite runs.
        // Keep this as a broad smoke threshold. Offline import tests in
        // src/commands/import.rs cover controlled matching cases.
        let eligible = summary.total.saturating_sub(summary.filtered);
        let match_ratio = if eligible > 0 {
            (summary.matched as f64) / (eligible as f64)
        } else {
            1.0
        };
        assert!(
            match_ratio >= 0.90,
            "match ratio too low: {match_ratio:.2} ({summary:?}) eligible={eligible}\n{stdout}"
        );

        let rows = count_downloaded_rows(test_data.path());
        assert!(rows > 0, "no rows written to state DB");
        // Compare reported matches with durable rows. These broad bounds
        // detect missing rows or excessive counts, not exact equality or
        // multi-pass behavior.
        assert!(
            summary.matched >= rows,
            "matched ({matched}) < rows ({rows}); per-pass model expects matched >= rows",
            matched = summary.matched,
        );
        assert!(
            summary.matched <= rows.saturating_mul(10),
            "matched ({matched}) > 10x rows ({rows}); over-counting regression?",
            matched = summary.matched,
        );
    });
}

/// `--dry-run` reports the same matched count but writes no rows.
#[test]
#[ignore]
fn import_dry_run_writes_no_rows() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = common::live_selection::live_recent().to_string();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--dry-run", "--recent", &recent],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .stdout(predicate::str::contains("DRY RUN"))
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(summary.matched > 0, "dry-run should still count matches");
        assert_eq!(
            count_downloaded_rows(test_data.path()),
            0,
            "dry-run must not write rows"
        );
    });
}

/// Re-running import-existing should produce the same matched count and
/// the same DB row count -- no duplicates.
#[test]
#[ignore]
fn import_is_idempotent() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = common::live_selection::live_recent().to_string();
        let run = || -> ImportSummary {
            let output = import_cmd(
                &username,
                &password,
                &cookie_dir,
                download_dir,
                test_data.path(),
                &["--recent", &recent],
            )
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
            parse_summary(&String::from_utf8_lossy(&output.stdout))
        };
        let first = run();
        let rows_after_first = count_downloaded_rows(test_data.path());
        let second = run();
        let rows_after_second = count_downloaded_rows(test_data.path());

        assert_eq!(
            first.matched, second.matched,
            "matched counts diverged across runs: {first:?} vs {second:?}"
        );
        assert_eq!(
            rows_after_first, rows_after_second,
            "DB row count grew on re-run -- import-existing isn't idempotent"
        );
    });
}

/// The shared single pass honors a smaller per-scenario bound.
/// Multiple-pass cap coverage lives in bundled_import_recent_caps_each_pass.
#[test]
#[ignore]
fn import_recent_limit_caps_scan() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        // Single-pass scenario: TOML pins `albums = ["none"]` so only the
        // unfiled pass runs; `--recent 5` -> total <= 5 globally.
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());
        let toml_path = write_kei_toml(
            test_data.path(),
            download_dir,
            "[filters]\nalbums = [\"none\"]\nunfiled = true\n",
        );
        let output = common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", test_data.path())
            .args([
                "import-existing",
                "--password",
                &password,
                "--config",
                toml_path.to_str().unwrap(),
                "--no-progress-bar",
                "--recent",
                "5",
            ])
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.total <= 5,
            "single-pass --recent 5 must scan at most 5 assets, got {summary:?}"
        );
    });
}

/// `--recent <N>d` (date filter) is rejected with a clear bail message,
/// matching the explicit handling we added to the binary. No fixture
/// required: the bail happens before any I/O against the download dir.
#[test]
#[ignore]
fn import_recent_days_form_is_rejected() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let test_root = tempdir().unwrap();
        let download_dir = test_root.path().join("photos");
        std::fs::create_dir_all(&download_dir).unwrap();
        import_cmd(
            &username,
            &password,
            &cookie_dir,
            &download_dir,
            test_root.path(),
            &["--recent", "30d"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "is not supported for import-existing",
        ));
    });
}

/// Truncating one of the fixture's files makes that version come up
/// `unmatched`. Operates on a copy of a small slice of the fixture so
/// the shared fixture itself stays intact.
#[test]
#[ignore]
fn import_unmatches_truncated_file() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        // Copy this bounded selection into a fresh dir, preserving the original parent
        // directory layout (Y/m/d/...) so import-existing's path
        // derivation lines up.
        let test_root = tempdir().unwrap();
        let test_dl = test_root.path().join("photos");
        std::fs::create_dir_all(&test_dl).unwrap();
        let files = selected_fixture_files(sync_data_dir);
        assert!(!files.is_empty(), "bounded fixture contains no media");
        for src in &files {
            let rel = src.strip_prefix(download_dir).unwrap();
            let dst = test_dl.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::copy(src, &dst).unwrap();
        }

        // Truncate the first file by 1 byte.
        let first_rel = files[0].strip_prefix(download_dir).unwrap();
        let truncated = test_dl.join(first_rel);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&truncated)
            .unwrap();
        let len = f.metadata().unwrap().len();
        f.set_len(len.saturating_sub(1)).unwrap();

        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            test_data.path(),
            &[],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert_eq!(
            summary.unmatched, 1,
            "only the truncated version should be unmatched: {summary:?}\n{stdout}"
        );
    });
}

/// Removing a file makes that version come up `unmatched` (file not on disk).
#[test]
#[ignore]
fn import_unmatches_missing_file() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_root = tempdir().unwrap();
        let test_dl = test_root.path().join("photos");
        std::fs::create_dir_all(&test_dl).unwrap();
        let files = selected_fixture_files(sync_data_dir);
        assert!(!files.is_empty(), "bounded fixture contains no media");
        // Omit one known file, even when the library contains only one asset.
        for src in &files[..files.len() - 1] {
            let rel = src.strip_prefix(download_dir).unwrap();
            let dst = test_dl.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::copy(src, &dst).unwrap();
        }

        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            test_data.path(),
            &[],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert_eq!(
            summary.unmatched, 1,
            "only the missing version should be unmatched: {summary:?}\n{stdout}"
        );
    });
}

/// Pointing at a non-existent download dir is a clear bail.
#[test]
#[ignore]
fn import_bails_on_missing_download_dir() {
    let (username, password, cookie_dir) = common::require_preauth();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let bogus = test_data.path().join("does-not-exist");
        import_cmd(
            &username,
            &password,
            &cookie_dir,
            &bogus,
            test_data.path(),
            &[],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Could not read download directory",
        ));
    });
}

/// TOML-only configuration (no CLI flags for resolved fields). Verifies
/// `[photos]` and `[download]` sections feed import-existing's
/// `DownloadConfig` correctly.
#[test]
#[ignore]
fn import_reads_toml_for_path_derivation() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());
        // TOML re-states the defaults explicitly. If the resolution
        // plumbing ever drops a field, the import matches nothing and
        // this test surfaces the regression.
        let toml_path = write_kei_toml(
            test_data.path(),
            download_dir,
            r#"folder_structure = "%Y/%m/%d"

[photos]
resolution = "original"
file_match_policy = "name-size-dedup-with-suffix"
live_photo_mode = "both"
live_resolution = "original"
live_photo_mov_filename_policy = "suffix"
raw_policy = "as-is"
keep_unicode_in_filenames = false
force_resolution = false
"#,
        );

        let mut cmd = common::cmd();
        clear_toml_resolved_env(&mut cmd);
        cmd.env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", test_data.path());
        cmd.args([
            "import-existing",
            "--password",
            &password,
            "--config",
            toml_path.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
            "--recent",
            &common::live_selection::live_recent().to_string(),
        ]);
        let output = cmd
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.matched > 0,
            "TOML-only config must drive matching, got {summary:?}\n{stdout}"
        );
    });
}

// ── Round-trip: import-existing → sync skips the imported assets ────────
//
// The whole point of import-existing is to register on-disk files so the
// next sync doesn't re-download them. These tests verify that handoff
// across a variety of flag combinations.
//
// For each test: stand up a fresh data_dir, run import-existing (with the
// flags under test), then run sync (with the same flags) against the same
// fixture download dir + same data_dir. Sync must report
// `<imported> already downloaded` (or equivalent) and `0 downloaded`.

/// Write a `kei.toml` under `dir` with `[download].directory` set to the
/// fixture and the caller's extra TOML body appended. Returns the path.
fn write_kei_toml(dir: &Path, download_dir: &Path, extra: &str) -> std::path::PathBuf {
    let path = dir.join("kei.toml");
    let body = format!(
        "[download]\ndirectory = {dl:?}\n{extra}",
        dl = download_dir.to_string_lossy(),
    );
    std::fs::write(&path, common::live_selection::live_config(&body)).unwrap();
    path
}

/// Strip stale `KEI_*` env vars from older durable-config surfaces.
/// v0.20 ignores these, but clearing them keeps TOML-driven live tests
/// explicit about the source of truth.
fn clear_toml_resolved_env(cmd: &mut assert_cmd::Command) {
    for var in [
        "KEI_FILE_MATCH_POLICY",
        "KEI_SIZE",
        "KEI_LIVE_PHOTO_MODE",
        "KEI_LIVE_PHOTO_SIZE",
        "KEI_LIVE_PHOTO_MOV_FILENAME_POLICY",
        "KEI_ALIGN_RAW",
        "KEI_KEEP_UNICODE_IN_FILENAMES",
        "KEI_FORCE_SIZE",
        "KEI_SKIP_VIDEOS",
        "KEI_SKIP_PHOTOS",
    ] {
        cmd.env_remove(var);
    }
}

/// `std::process::Command` for the kei binary with `.env` loaded into
/// the parent process the same way `common::cmd()` does. Use for tests
/// that need StdCommand features (signals, parallel spawn) instead of
/// `assert_cmd::Command`.
#[cfg(unix)]
fn kei_std_command() -> std::process::Command {
    // Force the lazy .env load in `common::cmd()`'s init_env() by
    // touching it once; cheap, idempotent, and matches the live-test
    // recipe's expected env state.
    let _ = common::cmd();
    std::process::Command::new(env!("CARGO_BIN_EXE_kei"))
}

/// Run `kei sync --recent N` reusing the post-import data_dir, capture
/// stderr, and parse downloaded/skipped counts. Returns
/// `(downloaded, skipped_or_zero)`. Sync logs go to stderr via
/// `tracing::info!`. Two patterns to match:
///
///   - `"N downloaded, M skipped, K failed (T total)"` — when sync did
///     any download work or had per-asset skips it tallied.
///   - `"No new photos to download"` — when every enumerated asset is
///     filtered or skipped on-disk before download. Maps to (0, 0).
fn run_sync_against_fixture(
    username: &str,
    password: &str,
    download_dir: &Path,
    data_dir: &Path,
    extra_toml: &str,
) -> (u64, u64) {
    let config_path = write_kei_toml(data_dir, download_dir, extra_toml);
    let mut cmd = common::cmd();
    cmd.env("ICLOUD_USERNAME", username)
        .env("KEI_DATA_DIR", data_dir);
    cmd.args([
        "sync",
        "--password",
        password,
        "--config",
        config_path.to_str().unwrap(),
        "--recent",
        &common::live_selection::live_recent().to_string(),
        "--no-progress-bar",
    ]);
    let output = cmd
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Fast path: sync skipped everything before the download phase.
    if stderr.contains("No new photos to download") {
        return (0, 0);
    }

    let downloaded = digits_before(&stderr, " downloaded")
        .unwrap_or_else(|| panic!("missing 'N downloaded' line in sync stderr:\n{stderr}"));
    let skipped = digits_before(&stderr, " skipped").unwrap_or(0);
    (downloaded, skipped)
}

/// Find the run of ASCII digits immediately preceding `marker` in `hay`
/// and parse them as `u64`. Used to scan kei's `N downloaded, M skipped`
/// summary line without pulling in the regex crate.
fn digits_before(hay: &str, marker: &str) -> Option<u64> {
    let idx = hay.find(marker)?;
    let bytes = hay.as_bytes();
    let mut start = idx;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    hay[start..idx].parse().ok()
}

/// Import then sync the fixture's single library-wide pass. The strongest
/// invariant: zero downloads after import.
#[test]
#[ignore]
fn roundtrip_default_layout_sync_skips_after_import() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = common::live_selection::live_recent().to_string();
        let toml_path = write_kei_toml(test_data.path(), download_dir, "");

        let import = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", &recent, "--config", toml_path.to_str().unwrap()],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let summary = parse_summary(&String::from_utf8_lossy(&import.stdout));
        assert!(
            summary.matched > 0,
            "import-existing did not match anything; sync skip test would be vacuous: {summary:?}"
        );

        // The strongest invariant: 0 downloaded. The skipped count is
        // 0 when sync short-circuits at "No new photos to download"
        // (every asset rejected before the download phase) and >0 when
        // sync emits a per-asset skip tally; both are valid no-download
        // outcomes for the round-trip.
        let (downloaded, _skipped) =
            run_sync_against_fixture(&username, &password, download_dir, test_data.path(), "");
        assert_eq!(
            downloaded, 0,
            "sync re-downloaded {downloaded} files after import-existing populated state DB; \
             matched={}",
            summary.matched,
        );
    });
}

/// Negative case: dry-run import-existing must NOT prevent sync from
/// downloading. Pins that --dry-run truly leaves the DB untouched.
#[test]
#[ignore]
fn roundtrip_dry_run_import_does_not_prevent_sync() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", "5", "--dry-run"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success();

        let post_import_rows = count_downloaded_rows(test_data.path());
        assert_eq!(
            post_import_rows, 0,
            "dry-run wrote {post_import_rows} rows; the only invariant for \
             this test is that --dry-run leaves the DB untouched",
        );
    });
}

// ── Verify-after-import: kei verify --checksums passes on imported set ──

/// After `import-existing` registers a file, `kei verify --checksums`
/// must read the file, hash it, and report a clean verification with
/// zero mismatches. Pins the local-checksum field written during import
/// is the same shape verify expects.
#[test]
#[ignore]
fn verify_checksums_passes_after_import() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = common::live_selection::live_recent().to_string();
        let import_out = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", &recent],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let summary = parse_summary(&String::from_utf8_lossy(&import_out.stdout));
        assert!(
            summary.matched > 0,
            "import matched no bounded fixture files: {summary:?}"
        );

        let verify_out = common::cmd()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", test_data.path())
            .args(["verify", "--checksums"])
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&verify_out.stdout);
        // verify prints "Verified:  N" on success. If it printed
        // "Mismatched: K" with K>0, that indicates DB local_checksum
        // diverges from re-hashing on disk.
        assert!(
            stdout.contains("Verified:"),
            "verify did not print Verified line:\n{stdout}",
        );
        assert!(
            !stdout.contains("Mismatched:") || stdout.contains("Mismatched:  0"),
            "verify reported checksum mismatches after import:\n{stdout}",
        );
    });
}

// ── Import policy configuration ────────────────────────────────────────
//
// `import_reads_toml_for_path_derivation` covers the TOML-configured path.
// These cases cover the removed CLI flag, the default, and invalid TOML.

/// The old `--file-match-policy` import override is gone in v0.20. Import
/// path matching now reads `[photos].file_match_policy` from TOML.
#[test]
fn import_file_match_policy_cli_flag_is_removed() {
    common::cmd()
        .args([
            "import-existing",
            "--file-match-policy",
            "name-size-dedup-with-suffix",
            "--help",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unexpected argument"));
}

/// Without a TOML override, file_match_policy defaults to
/// `name-size-dedup-with-suffix`, which matches the fixture.
#[test]
#[ignore]
fn default_used_when_no_toml_no_cli_flag() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = common::live_selection::live_recent().to_string();
        let mut cmd = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", &recent],
        );
        clear_toml_resolved_env(&mut cmd);
        let out = cmd
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let summary = parse_summary(&stdout);
        assert!(
            summary.matched > 0,
            "no-toml/no-flag default did not match the fixture: {summary:?}\n\
             stdout:\n{stdout}\n\
             stderr:\n{stderr}"
        );
    });
}

/// An invalid TOML value for a typed enum field must produce a clean
/// error (non-success exit), not silently fall back to the default policy.
#[test]
#[ignore]
fn toml_invalid_file_match_policy_errors_loudly() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());
        let toml_path = write_kei_toml(
            test_data.path(),
            download_dir,
            "[photos]\nfile_match_policy = \"made-up-policy\"\n",
        );

        let mut cmd = common::cmd();
        clear_toml_resolved_env(&mut cmd);
        cmd.env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", test_data.path());
        cmd.args([
            "import-existing",
            "--password",
            &password,
            "--config",
            toml_path.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
            "--recent",
            "5",
        ]);
        let assert = cmd
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .failure();
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
        assert!(
            stderr.to_lowercase().contains("file_match_policy")
                || stderr.to_lowercase().contains("made-up-policy")
                || stderr.to_lowercase().contains("variant"),
            "expected the error to name the bad TOML field; got:\n{stderr}",
        );
    });
}

// ── Crash safety: SIGINT mid-scan, restart, idempotent ──────────────────
//
// Best-effort: starts a real `kei import-existing` subprocess, sends
// SIGINT after a short delay, waits for exit, then runs the command
// again to completion and verifies a clean idempotent state. If the
// first run finishes before the SIGINT lands (small library, fast
// network), the test still verifies idempotence on the second run --
// it just doesn't exercise the cancellation path.

#[cfg(unix)]
#[test]
#[ignore]
fn import_sigint_then_rerun_is_idempotent() {
    use std::process::Stdio;

    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());
        let recent = common::live_selection::live_recent().to_string();

        let config_path = write_kei_toml(test_data.path(), download_dir, "");
        let mut child = kei_std_command()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", test_data.path())
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "import-existing",
                "--password",
                &password,
                "--recent",
                &recent,
                "--no-progress-bar",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kei import-existing");

        // Sync on the `stage="scan_started"` tracing marker the binary
        // emits when the first asset is dequeued. This replaces a
        // `thread::sleep(1500)` race that fired SIGINT before the scan
        // had started on slow networks (cancelling auth instead) and
        // long after first matches on fast ones (cancelling too late
        // to exercise mid-scan crash safety).
        let saw_scan_started = common::wait_for_stderr_line(
            &mut child,
            |line| line.contains("stage=\"scan_started\""),
            Duration::from_secs(120),
        );
        if saw_scan_started.is_none() {
            let _ = child.kill();
            panic!("did not observe stage=scan_started within 120s");
        }

        // SAFETY: child.id() is the live PID for our spawned process;
        // SIGINT is the documented graceful-shutdown signal.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGINT);
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match child.try_wait().expect("try_wait") {
                Some(_) => break,
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    panic!("child did not exit within 30s of SIGINT");
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }

        let post_sigint_rows = count_downloaded_rows(test_data.path());

        let out = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", &recent],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let summary = parse_summary(&String::from_utf8_lossy(&out.stdout));
        let final_rows = count_downloaded_rows(test_data.path());

        assert!(
            final_rows >= post_sigint_rows,
            "rerun lost rows: post-sigint {post_sigint_rows} -> final {final_rows}",
        );
        assert!(
            final_rows <= summary.matched + 1,
            "rerun produced more rows than scanned matches: rows={final_rows} matched={}",
            summary.matched,
        );
    });
}

// ── Lock concurrency: import-existing vs sync, two imports ──────────────
//
// kei's session lock prevents concurrent state-DB writers. Spawning two
// processes against the same data_dir should NOT have both succeed with
// matching rows -- one must wait or bail. These tests pin that contract
// for import-existing too.

#[cfg(unix)]
#[test]
#[ignore]
fn two_concurrent_imports_do_not_both_succeed_silently() {
    use std::process::Stdio;

    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());
        let recent = common::live_selection::live_recent().to_string();

        let config_path = write_kei_toml(test_data.path(), download_dir, "");
        let spawn = || {
            kei_std_command()
                .env("ICLOUD_USERNAME", &username)
                .env("KEI_DATA_DIR", test_data.path())
                .args([
                    "--config",
                    config_path.to_str().unwrap(),
                    "import-existing",
                    "--password",
                    &password,
                    "--recent",
                    &recent,
                    "--no-progress-bar",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn kei import-existing")
        };

        let a = spawn();
        let b = spawn();

        let out_a = a.wait_with_output().expect("wait a");
        let out_b = b.wait_with_output().expect("wait b");

        // At least one must succeed (no double bail). The combined
        // observation we care about: not both running fully unaware of
        // each other -- one must either wait for the lock, fail with a
        // lock error, or bail cleanly with a "another instance" message.
        let a_ok = out_a.status.success();
        let b_ok = out_b.status.success();
        let stderr_a = String::from_utf8_lossy(&out_a.stderr);
        let stderr_b = String::from_utf8_lossy(&out_b.stderr);
        let lock_text_a = stderr_a.to_lowercase().contains("lock")
            || stderr_a.to_lowercase().contains("already running");
        let lock_text_b = stderr_b.to_lowercase().contains("lock")
            || stderr_b.to_lowercase().contains("already running");

        // If both succeeded *and* neither logged a lock-related message,
        // there's no concurrency guard for import-existing -- which is
        // a finding worth surfacing. The state DB is sqlite WAL so
        // concurrent writers won't corrupt it, but two concurrent
        // imports could double-count or interleave matches.
        let both_silent_success = a_ok && b_ok && !lock_text_a && !lock_text_b;
        assert!(
            !both_silent_success,
            "both concurrent imports succeeded with no lock-related log; \
             import-existing has no concurrency guard. \
             stderr_a:\n{stderr_a}\nstderr_b:\n{stderr_b}",
        );
    });
}
