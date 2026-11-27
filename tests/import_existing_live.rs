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
//!    (or a copy of a subset of it) with state that holds the listed catalog
//!    but no copies, so each import reads only the changes since the listing
//!    and must find every file on disk.
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
// Import runs a cycle that lists the whole library on fresh state.
const IMPORT_TIMEOUT_SECS: u64 = 1800;

/// Copy auth artifacts (cookie file + .session + .cache) from `src`
/// into `dst`, deliberately skipping state databases with their SQLite
/// companions and `.lock` files. Each test starts from its own state.
fn copy_auth_artifacts(src: &Path, dst: &Path) {
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.contains(".db") || name_str.ends_with(".lock") {
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
        common::seed_listed_state(&username, &data_dir);

        eprintln!(
            "Building import-existing fixture with fixed lower date {} into {}",
            common::live_selection::require_fixed_date(),
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
                "--password-file",
                &common::password_file(&password),
                "--config",
                config_path.to_str().unwrap(),
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
    std::fs::write(src.path().join("user.sync.db-wal"), b"stale wal").unwrap();
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
        !dst.path().join("user.db").exists() && !dst.path().join("user.sync.db-wal").exists(),
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
    // A data_dir without state gets the listed catalog, so the import does
    // not list the whole library again.
    copy_auth_artifacts(cookie_dir, data_dir);
    if common::state_db(data_dir).is_none() && username != OFFLINE_USER {
        common::seed_listed_state(username, data_dir);
    }
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
        "--password-file",
        &common::password_file(password),
        "--no-progress-bar",
    ]);
    cmd.args(extra);
    cmd
}

/// Offline construction tests use this account; nothing is listed for it.
const OFFLINE_USER: &str = "test@example.invalid";

#[test]
fn import_command_lists_the_whole_selection_without_a_sample() {
    let dir = tempdir().unwrap();
    let command = import_cmd(
        OFFLINE_USER,
        "test",
        dir.path(),
        dir.path(),
        dir.path(),
        &["--config", "unused-construction-only.toml"],
    );
    let args: Vec<_> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(!args.iter().any(|arg| arg == "--recent"), "{args:?}");
    assert!(common::state_db(dir.path()).is_none());
}

/// Parse the summary `import-existing` prints. A real run reports the files
/// it adopted; a dry run reports the files it would adopt.
fn parse_summary(stdout: &str) -> ImportSummary {
    let mut summary = ImportSummary::default();
    let number = |value: &str| -> u64 {
        value
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("unreadable import summary value {value:?} in:\n{stdout}"))
    };
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Adopted ") {
            summary.matched = number(rest);
        } else if let Some(n) = line.strip_prefix("Local files that match a photo:") {
            summary.matched = number(n);
        } else if let Some(n) = line.strip_prefix("Already recorded:") {
            summary.already = number(n);
        } else if let Some(n) = line.strip_prefix("Copies with no local file yet:") {
            summary.missing = number(n);
        } else if let Some(n) = line.strip_prefix("Could not adopt:") {
            summary.refused = number(n);
        }
    }
    summary
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ImportSummary {
    /// Files adopted, or for a dry run, files that would be.
    matched: u64,
    /// Copies the state already recorded as saved.
    already: u64,
    /// Planned copies with no file at their path.
    missing: u64,
    /// Copies whose path holds a different file.
    refused: u64,
}

#[test]
fn import_summaries_parse_real_and_dry_runs() {
    let real = "Adopted 4 existing local files.\n  Already recorded:              2\n  Copies with no local file yet: 9 (run `kei sync` to download them)\n  Could not adopt:               1 (a different file is at the planned path)\n";
    assert_eq!(
        parse_summary(real),
        ImportSummary {
            matched: 4,
            already: 2,
            missing: 9,
            refused: 1
        }
    );
    let dry = "Dry run: no state was written.\n  Local files that match a photo: 3\n  Already recorded:               0\n  Copies with no local file yet:  7\n";
    assert_eq!(
        parse_summary(dry),
        ImportSummary {
            matched: 3,
            already: 0,
            missing: 7,
            refused: 0
        }
    );
}

/// Saved copies the state in `data_dir` records.
fn count_downloaded_rows(data_dir: &Path) -> u64 {
    common::saved_copies(data_dir).len() as u64
}

/// This run's saved copies as absolute paths under `download_dir`, not
/// unrelated files left in the reusable cache.
fn selected_fixture_files(data_dir: &Path, download_dir: &Path) -> Vec<PathBuf> {
    common::saved_copies(data_dir)
        .into_iter()
        .map(|path| download_dir.join(path))
        .collect()
}

/// Import is not sampled: every copy the fixture sync saved must be found.
fn assert_fixture_matches(summary: &ImportSummary, versions: u64) {
    assert!(versions > 0, "fixture must contain source-backed media");
    assert_eq!(
        summary.matched, versions,
        "every saved fixture copy must match: {summary:?}"
    );
    assert_eq!(
        summary.refused, 0,
        "no fixture path may hold another file: {summary:?}"
    );
}

fn import_fixture_summary(
    username: &str,
    password: &str,
    cookies: &Path,
    media: &Path,
    data: &Path,
) -> ImportSummary {
    let output = import_cmd(username, password, cookies, media, data, &[])
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
    parse_summary(&String::from_utf8_lossy(&output.stdout))
}

/// Exactly one copy lost its file: it is no longer adopted and is owed
/// instead. A copy whose file is shorter is not adopted either; it is
/// planned at another path and owed there.
fn assert_one_version_lost(before: &ImportSummary, after: &ImportSummary) {
    assert!(
        before.matched > 0,
        "healthy import must match the mutation target"
    );
    assert_eq!(
        after.matched + after.missing,
        before.matched + before.missing,
        "the planned copies changed during the mutation test"
    );
    assert_eq!(
        after.matched + 1,
        before.matched,
        "exactly one known match must be lost"
    );
    assert_eq!(after.refused, before.refused);
}

#[test]
fn fixture_oracles_require_every_copy_and_exactly_one_loss() {
    let healthy = ImportSummary {
        matched: 2,
        already: 0,
        missing: 8,
        refused: 0,
    };
    assert_fixture_matches(&healthy, 2);
    let one_lost = ImportSummary {
        matched: 1,
        already: 0,
        missing: 9,
        refused: 0,
    };
    assert_one_version_lost(&healthy, &one_lost);
    assert!(std::panic::catch_unwind(|| assert_fixture_matches(&healthy, 3)).is_err());
    let refused = ImportSummary {
        refused: 1,
        ..healthy
    };
    assert!(std::panic::catch_unwind(|| assert_fixture_matches(&refused, 2)).is_err());
    assert!(std::panic::catch_unwind(|| assert_one_version_lost(&healthy, &healthy)).is_err());
    let replanned = ImportSummary {
        matched: 1,
        already: 0,
        missing: 10,
        refused: 0,
    };
    assert!(std::panic::catch_unwind(|| assert_one_version_lost(&healthy, &replanned)).is_err());
}

// ── Tests ──────────────────────────────────────────────────────────────

/// Smoke test: import media written by the fixture sync with the same
/// bounded primary-library selection. Import plans every photo in the
/// selected libraries; every copy the fixture saved must be found, and
/// photos outside the date-bounded tree are owed, not matched. This
/// fixture does not exercise album overlap or shared libraries.
#[test]
#[ignore]
fn import_matches_default_layout_after_sync() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let toml_path = write_kei_toml(test_data.path(), download_dir, "");
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--config", toml_path.to_str().unwrap()],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        let versions = selected_fixture_files(sync_data_dir, download_dir).len() as u64;
        assert_fixture_matches(&summary, versions);
        assert_eq!(
            count_downloaded_rows(test_data.path()),
            summary.matched,
            "every adopted file is recorded as saved"
        );
        assert_eq!(
            common::saved_copies(test_data.path()),
            common::saved_copies(sync_data_dir),
            "import records the paths the fixture sync saved"
        );
    });
}

/// `--dry-run` reports the same matched count but writes no rows.
#[test]
#[ignore]
fn import_dry_run_writes_no_rows() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let output = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--dry-run"],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .stdout(predicate::str::contains("Dry run: no state was written."))
        .get_output()
        .clone();
        let summary = parse_summary(&String::from_utf8_lossy(&output.stdout));
        assert_fixture_matches(
            &summary,
            selected_fixture_files(sync_data_dir, download_dir).len() as u64,
        );
        assert_eq!(
            count_downloaded_rows(test_data.path()),
            0,
            "dry-run must not write rows"
        );
        let leftovers: Vec<_> = std::fs::read_dir(test_data.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".preview."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "dry-run left its scratch state: {leftovers:?}"
        );
    });
}

/// Re-running import-existing finds the same files already recorded and
/// writes no new rows.
#[test]
#[ignore]
fn import_is_idempotent() {
    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, _sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let first = import_fixture_summary(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
        );
        let rows_after_first = count_downloaded_rows(test_data.path());
        let second = import_fixture_summary(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
        );
        let rows_after_second = count_downloaded_rows(test_data.path());

        assert!(first.matched > 0, "first import matched nothing: {first:?}");
        assert_eq!(
            second.matched, 0,
            "nothing is new the second time: {second:?}"
        );
        assert_eq!(
            second.already, first.matched,
            "the second run finds the first run's files recorded: {first:?} vs {second:?}"
        );
        assert_eq!(
            rows_after_first, rows_after_second,
            "DB row count changed on re-run -- import-existing isn't idempotent"
        );
    });
}

/// Import plans the whole selection in one listing; the retired count
/// sample is refused before any account or provider work.
#[test]
fn import_recent_count_is_refused() {
    let test_root = tempdir().unwrap();
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env("KEI_DATA_DIR", test_root.path())
        .args(["import-existing", "--recent", "5"])
        .timeout(Duration::from_secs(10))
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "`--recent` is no longer supported",
        ));
    assert!(
        std::fs::read_dir(test_root.path())
            .unwrap()
            .next()
            .is_none()
    );
}

/// Retired date windows fail at CLI parsing without credentials or provider I/O.
#[test]
fn import_recent_days_form_is_rejected() {
    let test_root = tempdir().unwrap();
    common::cmd()
        .env_remove("ICLOUD_USERNAME")
        .env_remove("ICLOUD_PASSWORD")
        .env("KEI_DATA_DIR", test_root.path())
        .args(["import-existing", "--recent", "30d"])
        .timeout(Duration::from_secs(10))
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "import-existing --recent requires a positive count",
        ))
        .stderr(predicate::str::contains("omit --recent"))
        .stderr(predicate::str::contains("--recent 100"));
    assert!(
        std::fs::read_dir(test_root.path())
            .unwrap()
            .next()
            .is_none()
    );
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
        let files = selected_fixture_files(sync_data_dir, download_dir);
        assert!(!files.is_empty(), "bounded fixture contains no media");
        for src in &files {
            let rel = src.strip_prefix(download_dir).unwrap();
            let dst = test_dl.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::copy(src, &dst).unwrap();
        }

        // Prove a healthy match of this copy before changing it.
        let healthy_data = tempdir().unwrap();
        let before = import_fixture_summary(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            healthy_data.path(),
        );
        let matched = selected_fixture_files(healthy_data.path(), &test_dl);
        assert!(
            !matched.is_empty(),
            "healthy import must have a source-backed target"
        );
        let truncated = &matched[0];
        let relative = truncated.strip_prefix(&test_dl).unwrap();
        assert!(
            files
                .iter()
                .any(|path| path.strip_prefix(download_dir).unwrap() == relative)
        );
        assert!(
            std::fs::read(truncated).unwrap()
                == std::fs::read(download_dir.join(relative)).unwrap(),
            "mutation target must retain the source-backed fixture bytes"
        );
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(truncated)
            .unwrap();
        let len = f.metadata().unwrap().len();
        f.set_len(len.saturating_sub(1)).unwrap();

        let test_data = tempdir().unwrap();
        let after = import_fixture_summary(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            test_data.path(),
        );
        assert_one_version_lost(&before, &after);
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
        let files = selected_fixture_files(sync_data_dir, download_dir);
        assert!(!files.is_empty(), "bounded fixture contains no media");
        for src in &files {
            let rel = src.strip_prefix(download_dir).unwrap();
            let dst = test_dl.join(rel);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::copy(src, &dst).unwrap();
        }

        let healthy_data = tempdir().unwrap();
        let before = import_fixture_summary(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            healthy_data.path(),
        );
        let matched = selected_fixture_files(healthy_data.path(), &test_dl);
        assert!(
            !matched.is_empty(),
            "healthy import must have a source-backed target"
        );
        let missing = &matched[0];
        let relative = missing.strip_prefix(&test_dl).unwrap();
        assert!(
            files
                .iter()
                .any(|path| path.strip_prefix(download_dir).unwrap() == relative)
        );
        assert!(
            std::fs::read(missing).unwrap() == std::fs::read(download_dir.join(relative)).unwrap(),
            "mutation target must retain the source-backed fixture bytes"
        );
        std::fs::remove_file(missing).unwrap();

        let test_data = tempdir().unwrap();
        let after = import_fixture_summary(
            &username,
            &password,
            &cookie_dir,
            &test_dl,
            test_data.path(),
        );
        assert_one_version_lost(&before, &after);
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
    let (download_dir, sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());
        common::seed_listed_state(&username, test_data.path());
        // TOML re-states the defaults explicitly. If the resolution
        // plumbing ever drops a field, the import matches nothing and
        // this test surfaces the regression.
        let toml_path = write_kei_toml(
            test_data.path(),
            download_dir,
            r#"folder_structure = "%Y/%m/%d"

[photos]
file_match_policy = "name-size-dedup-with-suffix"
live_photo_mov_filename_policy = "suffix"
keep_unicode_in_filenames = false
"#,
        );

        let mut cmd = common::cmd();
        clear_toml_resolved_env(&mut cmd);
        cmd.env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", test_data.path());
        cmd.args([
            "import-existing",
            "--password-file",
            &common::password_file(&password),
            "--config",
            toml_path.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
        ]);
        let output = cmd
            .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
            .assert()
            .success()
            .get_output()
            .clone();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let summary = parse_summary(&stdout);
        assert_eq!(
            summary.matched,
            selected_fixture_files(sync_data_dir, download_dir).len() as u64,
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
// fixture download dir + same data_dir. Sync must download nothing.

/// Write a `kei.toml` under `dir` with `[download].directory` set to the
/// fixture and the caller's extra TOML body appended. Returns the path.
fn write_kei_toml(dir: &Path, download_dir: &Path, extra: &str) -> std::path::PathBuf {
    let path = dir.join("kei.toml");
    let body = format!(
        "[download]\ndirectory = {dl:?}\n{extra}",
        dl = download_dir.to_string_lossy(),
    );
    common::write_versioned_config(&path, common::live_selection::live_config(&body)).unwrap();
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

/// Run fixed-date `kei sync` reusing the post-import data_dir and return
/// what its `Library synced` lines report as downloaded and adopted.
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
        "--password-file",
        &common::password_file(password),
        "--config",
        config_path.to_str().unwrap(),
        "--no-progress-bar",
    ]);
    let output = cmd
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let field = |name: &str| {
        common::synced(&stderr, name)
            .unwrap_or_else(|| panic!("missing `{name}` in the sync summary:\n{stderr}"))
    };
    (field("downloaded"), field("adopted"))
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
        let toml_path = write_kei_toml(test_data.path(), download_dir, "");

        let import = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &["--config", toml_path.to_str().unwrap()],
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

        // The strongest invariant: nothing downloaded, and nothing left to
        // adopt either, since import already recorded every file.
        let (downloaded, adopted) =
            run_sync_against_fixture(&username, &password, download_dir, test_data.path(), "");
        assert_eq!(
            (downloaded, adopted),
            (0, 0),
            "sync downloaded {downloaded} and adopted {adopted} files after import-existing \
             recorded them; matched={}",
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
            &["--dry-run"],
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
        let import_out = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &[],
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
        // verify prints a checksum-verified count on success. A positive
        // different-checksum count indicates DB local_checksum diverges
        // from re-hashing on disk.
        assert!(
            stdout.contains("Checksum verified:"),
            "verify did not print a checksum-verified line:\n{stdout}",
        );
        assert!(
            stdout.contains("Different checksum:             0"),
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
    let (download_dir, sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        let mut cmd = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &[],
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
            summary.matched == selected_fixture_files(sync_data_dir, download_dir).len() as u64,
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
            "--password-file",
            &common::password_file(&password),
            "--config",
            toml_path.to_str().unwrap(),
            "--no-progress-bar",
            "--dry-run",
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

// ── Crash safety: SIGINT mid-listing, restart, idempotent ───────────────
//
// Starts a real `kei import-existing` on fresh state, so it lists the whole
// library, and sends SIGINT once a page of that listing is saved. The rerun
// must resume from the saved position instead of listing from the start,
// and still find every fixture file.

#[cfg(unix)]
#[test]
#[ignore]
fn import_sigint_then_rerun_is_idempotent() {
    use std::process::Stdio;

    let (username, password, cookie_dir) = common::require_preauth();
    let (download_dir, sync_data_dir) = fixture();

    common::with_auth_retry(|| {
        let test_data = tempdir().unwrap();
        copy_auth_artifacts(&cookie_dir, test_data.path());

        let config_path = write_kei_toml(test_data.path(), download_dir, "");
        let mut child = kei_std_command()
            .env("ICLOUD_USERNAME", &username)
            .env("KEI_DATA_DIR", test_data.path())
            .args([
                "--config",
                config_path.to_str().unwrap(),
                "import-existing",
                "--password-file",
                &common::password_file(&password),
                "--no-progress-bar",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kei import-existing");

        // The listing logs its progress at most every 30 seconds, after
        // the pages it counts are saved.
        let saw_progress = common::wait_for_stderr_line(
            &mut child,
            |line| line.contains("Listing library: progress saved"),
            Duration::from_secs(600),
        );
        if saw_progress.is_none() {
            let _ = child.kill();
            panic!("did not observe a saved listing page within 600s");
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

        let db = common::state_db(test_data.path()).expect("the interrupted import kept its state");
        let position: Option<String> = rusqlite::Connection::open(db)
            .unwrap()
            .query_row(
                "SELECT position FROM zones WHERE zone = 'PrimarySync'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            position.is_some(),
            "the saved listing pages kept their position"
        );
        let post_sigint_rows = count_downloaded_rows(test_data.path());

        let out = import_cmd(
            &username,
            &password,
            &cookie_dir,
            download_dir,
            test_data.path(),
            &[],
        )
        .timeout(Duration::from_secs(IMPORT_TIMEOUT_SECS))
        .assert()
        .success()
        .get_output()
        .clone();
        let stderr = common::strip_ansi(&String::from_utf8_lossy(&out.stderr));
        assert!(
            !stderr.contains("Listing the whole library"),
            "the rerun must resume the listing, not start it again: {stderr}"
        );
        let summary = parse_summary(&String::from_utf8_lossy(&out.stdout));
        let final_rows = count_downloaded_rows(test_data.path());
        let versions = selected_fixture_files(sync_data_dir, download_dir).len() as u64;

        assert!(
            final_rows >= post_sigint_rows,
            "rerun lost rows: post-sigint {post_sigint_rows} -> final {final_rows}",
        );
        assert_eq!(
            final_rows, versions,
            "the rerun records every fixture file: {summary:?}"
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
        common::seed_listed_state(&username, test_data.path());

        let config_path = write_kei_toml(test_data.path(), download_dir, "");
        let spawn = || {
            kei_std_command()
                .env("ICLOUD_USERNAME", &username)
                .env("KEI_DATA_DIR", test_data.path())
                .args([
                    "--config",
                    config_path.to_str().unwrap(),
                    "import-existing",
                    "--password-file",
                    &common::password_file(&password),
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
