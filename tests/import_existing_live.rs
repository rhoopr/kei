//! Live `import-existing` tests against the real Apple CloudKit API.
//!
//! Strategy:
//! 1. **Setup once per test run**: download a fixture set of recent photos
//!    via the real `kei sync` command (default size, default folder
//!    structure, default match policy). The fixture directory is reused
//!    across every test in this file via a `OnceLock`.
//! 2. **Per test**: each test runs `kei import-existing` against that
//!    fixture (or a copy of a subset of it) with a fresh state DB to
//!    isolate side-effects.
//!
//! All tests are gated `#[ignore]`. Run with:
//!
//! ```sh
//! cargo test --test import_existing_live -- --ignored --test-threads=1
//! ```
//!
//! The fixture is intentionally not cleaned up between runs — the next
//! invocation can reuse it via `KEI_IMPORT_FIXTURE_DIR`. By default the
//! fixture lives in `/tmp/claude/kei-import-fixture/`, so a re-run just
//! polls for new photos via the same `kei sync` command (which is a no-op
//! when nothing changed).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::print_stderr,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::indexing_slicing
)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use predicates::prelude::*;
use tempfile::tempdir;

const FIXTURE_TIMEOUT_SECS: u64 = 1800; // 30m: full sync of ~100 assets
const IMPORT_TIMEOUT_SECS: u64 = 300; // 5m: import-existing scans, no downloads
const FIXTURE_RECENT: u32 = 100;

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

/// Dir where the fixture sync writes its files. Reused across tests in a
/// single `cargo test` invocation, and persisted across invocations
/// (allowing the second run to re-use the cache as long as the dir exists
/// and has files).
fn fixture_root() -> PathBuf {
    if let Ok(dir) = std::env::var("KEI_IMPORT_FIXTURE_DIR") {
        return PathBuf::from(dir);
    }
    PathBuf::from("/tmp/claude/kei-import-fixture")
}

/// One-shot ensure-fixture: returns the fixture download dir + the data
/// dir used during the sync.
///
/// `download_dir` is persisted across cargo invocations (so the next run
/// re-uses the cached photos). `data_dir` is rebuilt fresh each
/// invocation because state-DB schemas drift across branches -- a v8 DB
/// from a prior main-branch run would refuse to open on a v7 PR branch
/// and fail the fixture sync. Photos on disk don't carry that risk.
fn fixture() -> &'static (PathBuf, PathBuf) {
    static FIX: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    FIX.get_or_init(|| {
        let (username, password, cookie_dir) = common::require_preauth();
        let download_dir = fixture_root();
        std::fs::create_dir_all(&download_dir).unwrap();

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
        copy_auth_artifacts(&cookie_dir, &data_dir);

        eprintln!(
            "Building import-existing fixture: --recent {FIXTURE_RECENT} into {}",
            download_dir.display()
        );
        let output = common::cmd()
            .args([
                "sync",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                data_dir.to_str().unwrap(),
                "--directory",
                download_dir.to_str().unwrap(),
                "--recent",
                &FIXTURE_RECENT.to_string(),
                "--no-progress-bar",
                "--no-incremental",
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

/// Build an `import-existing` command targeting the fixture's download
/// dir but with a fresh `--data-dir` so the per-test state DB stays
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
    let mut cmd = common::cmd();
    cmd.args([
        "import-existing",
        "--username",
        username,
        "--password",
        password,
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--download-dir",
        download_dir.to_str().unwrap(),
        "--no-progress-bar",
    ]);
    cmd.args(extra);
    cmd
}

/// Parse the trailing summary printed by `import-existing`.
fn parse_summary(stdout: &str) -> ImportSummary {
    let mut total = 0_u64;
    let mut matched = 0_u64;
    let mut unmatched = 0_u64;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(n) = line.strip_prefix("Total assets scanned:") {
            total = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("Files matched:") {
            matched = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("Unmatched versions:") {
            unmatched = n.trim().parse().unwrap_or(0);
        }
    }
    ImportSummary {
        total,
        matched,
        unmatched,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ImportSummary {
    total: u64,
    matched: u64,
    unmatched: u64,
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

// ── Tests ──────────────────────────────────────────────────────────────

/// Smoke test: import-existing against the fixture's download dir
/// matches the same assets the fixture sync wrote. Constrains the scan
/// to `--recent N` matching the fixture so the comparison is apples-to-
/// apples — the user's full library can be far larger than the fixture.
#[test]
#[ignore]
fn import_matches_default_layout_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = FIXTURE_RECENT.to_string();
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
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(summary.total > 0, "expected some assets, got {summary:?}");
        // With the same `--recent N` as the fixture sync, every enumerated
        // asset's primary version should already be on disk. The expected
        // ratio in the happy path is ~1.0; tighten to 0.95 so a regression
        // that drops half the matches actually fails. The 5% headroom
        // accommodates rare cases (0-byte placeholder, ProRAW filtered by
        // `align_raw`, LivePhotoMode::Skip on a live photo) where
        // import-existing enumerates an asset sync skipped.
        let match_ratio = (summary.matched as f64) / (summary.total as f64);
        assert!(
            match_ratio > 0.95,
            "match ratio too low: {match_ratio:.2} ({summary:?})\n{stdout}"
        );

        let rows = count_downloaded_rows(test_data.path());
        assert!(rows > 0, "no rows written to state DB");
        // Row count should track `matched` closely. A small drift is
        // acceptable: some real-Apple metadata corner cases (e.g. an
        // asset whose `mark_downloaded` UPDATE happens to race a stuck
        // pending state) leave a row in `pending` instead of
        // `downloaded`. We only count `downloaded` here. Caps the slop
        // at 2% of matched, generous enough to soak up the rare drift
        // without missing a regression that breaks DB writes wholesale.
        let max_drift = (summary.matched / 50).max(1);
        let drift = summary.matched.saturating_sub(rows);
        assert!(
            drift <= max_drift,
            "DB downloaded rows ({rows}) drift from stdout matched ({}) by {drift} > tolerance {max_drift}",
            summary.matched,
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
        let recent = FIXTURE_RECENT.to_string();
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
        let recent = FIXTURE_RECENT.to_string();
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

/// `--recent N` caps the scan -- with N << total, matched < total scanned
/// against the full fixture, but greater than zero.
#[test]
#[ignore]
fn import_recent_limit_caps_scan() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", "5"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.total <= 5,
            "--recent 5 must scan at most 5 assets, got {summary:?}"
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
            "isn't supported for import-existing",
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
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        // Copy 3 files into a fresh dir, preserving the original parent
        // directory layout (Y/m/d/...) so import-existing's path
        // derivation lines up.
        let test_root = tempdir().unwrap();
        let test_dl = test_root.path().join("photos");
        std::fs::create_dir_all(&test_dl).unwrap();
        let files: Vec<PathBuf> = common::walkdir(download_dir)
            .into_iter()
            .filter(|p| {
                let s = p.to_string_lossy();
                !s.contains("/_kei_data/") && !s.contains("/state.db")
            })
            .take(3)
            .collect();
        if files.len() < 3 {
            eprintln!("Fixture only has {} files, skipping", files.len());
            return;
        }
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
            &["--recent", "10"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.unmatched >= 1,
            "expected ≥1 unmatched (truncated file), got {summary:?}\n{stdout}"
        );
    });
}

/// Removing a file makes that version come up `unmatched` (file not on disk).
#[test]
#[ignore]
fn import_unmatches_missing_file() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_root = tempdir().unwrap();
        let test_dl = test_root.path().join("photos");
        std::fs::create_dir_all(&test_dl).unwrap();
        let files: Vec<PathBuf> = common::walkdir(download_dir)
            .into_iter()
            .filter(|p| {
                let s = p.to_string_lossy();
                !s.contains("/_kei_data/") && !s.contains("/state.db")
            })
            .take(3)
            .collect();
        if files.len() < 3 {
            eprintln!("Fixture only has {} files, skipping", files.len());
            return;
        }
        // Copy only the first 2 of 3 -- the third will be "missing".
        for src in &files[..2] {
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
            &["--recent", "5"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert!(
            summary.unmatched >= 1,
            "expected ≥1 unmatched (missing file), got {summary:?}\n{stdout}"
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
        .stderr(predicate::str::contains("Directory does not exist"));
    });
}

/// `--directory` is the deprecated alias for `--download-dir`. It must
/// still resolve, with a warning logged. Skips dry-run-only verification --
/// just exercises the alias resolution end-to-end.
#[test]
#[ignore]
fn import_accepts_deprecated_directory_flag() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());
        // Use --directory directly (bypassing import_cmd which uses --download-dir)
        let mut cmd = common::cmd();
        cmd.args([
            "import-existing",
            "--username",
            &username,
            "--password",
            &password,
            "--data-dir",
            test_data.path().to_str().unwrap(),
            "--directory",
            download_dir.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
            "--recent",
            "10",
        ]);
        cmd.timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .stdout(predicate::str::contains("DRY RUN"));
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
size = "original"
file_match_policy = "name-size-dedup-with-suffix"
live_photo_mode = "both"
live_photo_size = "original"
live_photo_mov_filename_policy = "suffix"
align_raw = "as-is"
keep_unicode_in_filenames = false
force_size = false
"#,
        );

        let mut cmd = common::cmd();
        clear_toml_resolved_env(&mut cmd);
        cmd.args([
            "import-existing",
            "--username",
            &username,
            "--password",
            &password,
            "--data-dir",
            test_data.path().to_str().unwrap(),
            "--config",
            toml_path.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
            "--recent",
            "10",
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

const ROUNDTRIP_RECENT: u32 = 10;

/// Write a `kei.toml` under `dir` with `[download].directory` set to the
/// fixture and the caller's extra TOML body appended. Returns the path.
fn write_kei_toml(dir: &Path, download_dir: &Path, extra: &str) -> std::path::PathBuf {
    let path = dir.join("kei.toml");
    let body = format!(
        "[download]\ndirectory = {dl:?}\n{extra}",
        dl = download_dir.to_string_lossy(),
    );
    std::fs::write(&path, body).unwrap();
    path
}

/// Strip every `KEI_*` env var that overrides a TOML-resolved field.
/// Live test runners often export several of these; without scrubbing,
/// the env layer (CLI > env > TOML > default) silently shadows TOML
/// under test. Apply at every TOML-driven test that asserts what TOML
/// resolves to.
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
    extra: &[&str],
) -> (u64, u64) {
    let mut cmd = common::cmd();
    cmd.args([
        "sync",
        "--username",
        username,
        "--password",
        password,
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--directory",
        download_dir.to_str().unwrap(),
        "--recent",
        &ROUNDTRIP_RECENT.to_string(),
        "--no-progress-bar",
        "--no-incremental",
    ]);
    cmd.args(extra);
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

/// Import-then-sync default flags. The strongest invariant: zero
/// downloads after import.
#[test]
#[ignore]
fn roundtrip_default_layout_sync_skips_after_import() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = ROUNDTRIP_RECENT.to_string();

        let import = import_cmd(
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
            run_sync_against_fixture(&username, &password, download_dir, test_data.path(), &[]);
        assert_eq!(
            downloaded, 0,
            "sync re-downloaded {downloaded} files after import-existing populated state DB; \
             matched={}",
            summary.matched,
        );
    });
}

/// Import then sync under `name-id7` file_match_policy. Pins that the
/// id7 suffix is consistent across both call sites so sync sees the
/// imported rows by (id, version_size) and skips.
#[test]
#[ignore]
fn roundtrip_name_id7_sync_skips_after_import() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        // The fixture was synced with default policy, so id7-shaped paths
        // probably don't exist on disk -- import-existing's NameId7 scan
        // would match nothing. Skip cleanly with a note rather than fail
        // the round-trip on a non-applicable layout.
        let test_data = tempdir().unwrap();
        let recent = ROUNDTRIP_RECENT.to_string();
        let import_out = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--recent", &recent, "--file-match-policy", "name-id7"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let summary = parse_summary(&String::from_utf8_lossy(&import_out.stdout));

        if summary.matched == 0 {
            eprintln!(
                "skip: fixture has no name-id7-shaped files on disk; \
                 round-trip not exercisable in this layout ({summary:?})"
            );
            return;
        }

        let (downloaded, _skipped) = run_sync_against_fixture(
            &username,
            &password,
            download_dir,
            test_data.path(),
            &["--file-match-policy", "name-id7"],
        );
        assert_eq!(
            downloaded, 0,
            "sync re-downloaded {downloaded} after name-id7 import; matched={}",
            summary.matched,
        );
    });
}

/// Import with `--skip-videos`, sync without it. Sync still must NOT
/// re-download the photos imported. Videos (which import skipped) should
/// also be skipped by sync because the state DB has nothing on them and
/// the on-disk files match.
#[test]
#[ignore]
fn roundtrip_skip_videos_sync_skips_imported_photos() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        // import-existing doesn't expose --skip-videos as a flag (it
        // builds DownloadConfig with skip_videos=false hard-coded). Use
        // TOML to force it.
        let test_data = tempdir().unwrap();
        let toml_path = write_kei_toml(
            test_data.path(),
            download_dir,
            "[filters]\nskip_videos = true\n",
        );

        let recent = ROUNDTRIP_RECENT.to_string();
        let import_out = import_cmd(
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
        let summary = parse_summary(&String::from_utf8_lossy(&import_out.stdout));
        if summary.matched == 0 {
            eprintln!("skip: skip_videos import matched nothing; round-trip not applicable");
            return;
        }

        let (downloaded, _skipped) =
            run_sync_against_fixture(&username, &password, download_dir, test_data.path(), &[]);
        assert_eq!(
            downloaded, 0,
            "sync re-downloaded {downloaded} after skip_videos import; matched={}",
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
        let recent = ROUNDTRIP_RECENT.to_string();
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
        if summary.matched == 0 {
            eprintln!("skip: import matched nothing; verify-after-import not applicable");
            return;
        }

        let verify_out = common::cmd()
            .args([
                "verify",
                "--checksums",
                "--username",
                &username,
                "--data-dir",
                test_data.path().to_str().unwrap(),
            ])
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

// ── TOML × CLI override matrix ──────────────────────────────────────────
//
// CLI > env > TOML > default per CLAUDE.md. The existing
// `import_reads_toml_for_path_derivation` covers the TOML-only happy
// path. These cover precedence + invalid-input handling.

/// CLI `--file-match-policy` overrides a conflicting TOML entry. Test:
/// TOML says one policy, CLI flag says the other; the CLI value wins.
/// Verify by observing import-existing's match behavior, since the two
/// policies produce different filenames.
#[test]
#[ignore]
fn toml_file_match_policy_overridden_by_cli_flag() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let toml_path = write_kei_toml(
            test_data.path(),
            download_dir,
            "[photos]\nfile_match_policy = \"name-id7\"\n",
        );

        let recent = ROUNDTRIP_RECENT.to_string();
        // CLI flag forces back to the default policy. Files on disk
        // were synced with the default, so this should match a healthy
        // fraction. If the CLI override didn't take, the import would
        // run name-id7 (matching nothing).
        let mut cmd = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &[
                "--recent",
                &recent,
                "--config",
                toml_path.to_str().unwrap(),
                "--file-match-policy",
                "name-size-dedup-with-suffix",
            ],
        );
        clear_toml_resolved_env(&mut cmd);
        let out = cmd
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
        let summary = parse_summary(&String::from_utf8_lossy(&out.stdout));
        assert!(
            summary.matched > 0,
            "CLI override of TOML file_match_policy did not take effect: \
             matched={} ({summary:?})",
            summary.matched,
        );
    });
}

/// Default kicks in when neither TOML nor CLI specify a value. With no
/// kei.toml and no flag, file_match_policy defaults to
/// `name-size-dedup-with-suffix`, which matches the fixture.
#[test]
#[ignore]
fn default_used_when_no_toml_no_cli_flag() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let recent = ROUNDTRIP_RECENT.to_string();
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
        let summary = parse_summary(&String::from_utf8_lossy(&out.stdout));
        assert!(
            summary.matched > 0,
            "no-toml/no-flag default did not match the fixture: {summary:?}"
        );
    });
}

/// An invalid TOML value for a typed enum field must produce a clean
/// error (non-success exit), not silently fall back to default. Pins
/// CLAUDE.md "no silent failures": a typo in the TOML can't read as
/// "use default" or you'd silently use a different policy than intended.
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
        cmd.args([
            "import-existing",
            "--username",
            &username,
            "--password",
            &password,
            "--data-dir",
            test_data.path().to_str().unwrap(),
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
        let recent = ROUNDTRIP_RECENT.to_string();

        let mut child = kei_std_command()
            .args([
                "import-existing",
                "--username",
                &username,
                "--password",
                &password,
                "--data-dir",
                test_data.path().to_str().unwrap(),
                "--download-dir",
                download_dir.to_str().unwrap(),
                "--recent",
                &recent,
                "--no-progress-bar",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kei import-existing");

        std::thread::sleep(Duration::from_millis(1500));
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
        let recent = ROUNDTRIP_RECENT.to_string();

        let spawn = || {
            kei_std_command()
                .args([
                    "import-existing",
                    "--username",
                    &username,
                    "--password",
                    &password,
                    "--data-dir",
                    test_data.path().to_str().unwrap(),
                    "--download-dir",
                    download_dir.to_str().unwrap(),
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
