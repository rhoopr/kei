//! `kei reconcile` — reconcile the state database with files on disk.
//!
//! Scans every asset marked `downloaded` in the state database and checks
//! that its recorded `local_path` still exists. Missing files are marked
//! as failed with reason `FILE_MISSING_AT_STARTUP` so the next sync
//! re-downloads them.
//!
//! This guards against:
//! - User manually deleting files from the photo directory.
//! - Partial restore from backup where DB state is newer than disk state.
//! - Mount/NAS outages that leave stale state rows pointing at vanished files.
//!
//! The reconcile pass is intentionally additive-only: it never deletes files,
//! never removes DB rows, and never modifies files on disk. The only DB
//! change is status transitions from `downloaded` -> `failed`, which the
//! normal sync path knows how to retry.

#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print a reconcile report to stdout"
)]

use std::cell::Cell;
use std::path::PathBuf;

use crate::cli;
use crate::config;
use crate::state;
use crate::state::{StateDb, VersionSizeKey};

use super::{print_truncation_tail, LISTING_CAP};

/// Stable sentinel written to `assets.last_error` so monitoring tools can
/// key on the reason a row flipped from downloaded back to failed.
const FILE_MISSING_REASON: &str = "FILE_MISSING_AT_STARTUP";

const SCAN_PAGE_SIZE: u32 = 1000;

#[derive(Debug)]
struct MissingAsset {
    id: String,
    version_size: VersionSizeKey,
    local_path: PathBuf,
}

#[derive(Debug, Default, Clone, Copy)]
struct ScanCounts {
    present: u64,
    missing: u64,
    no_path: u64,
}

/// Reads every page before any mutation so later `mark_failed` calls can't
/// shift OFFSET pagination and skip rows.
async fn scan_missing(
    db: &dyn StateDb,
    mut report_missing: impl FnMut(&MissingAsset),
    mut report_no_path: impl FnMut(&str),
) -> anyhow::Result<(ScanCounts, Vec<MissingAsset>)> {
    let mut counts = ScanCounts::default();
    let mut missing = Vec::new();

    let mut offset = 0u64;
    loop {
        let page = db.get_downloaded_page(offset, SCAN_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as u64;

        for asset in page {
            let crate::state::AssetRecord {
                id,
                version_size,
                local_path,
                ..
            } = asset;

            let Some(local_path) = local_path else {
                report_no_path(&id);
                counts.no_path += 1;
                continue;
            };

            if tokio::fs::try_exists(&local_path).await.unwrap_or(false) {
                counts.present += 1;
                continue;
            }

            let record = MissingAsset {
                id,
                version_size,
                local_path,
            };
            report_missing(&record);
            counts.missing += 1;
            missing.push(record);
        }
    }

    Ok((counts, missing))
}

pub(crate) async fn run_reconcile(
    args: cli::ReconcileArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    let summary = db.get_summary().await?;

    if args.dry_run {
        println!(
            "Reconciling {} downloaded assets (dry run — no changes will be written)...",
            summary.downloaded
        );
    } else {
        println!("Reconciling {} downloaded assets...", summary.downloaded);
    }
    println!();

    // Shared cell so both callbacks can bump the same counter under the
    // FnMut + FnMut constraint. Single-threaded; Cell is cheap here.
    let printed: Cell<usize> = Cell::new(0);

    let (counts, missing) = scan_missing(
        &db,
        |m| {
            if printed.get() < LISTING_CAP {
                println!(
                    "MISSING: {} ({}, {})",
                    m.local_path.display(),
                    m.id,
                    m.version_size.as_str(),
                );
                printed.set(printed.get() + 1);
            }
        },
        |id| {
            if printed.get() < LISTING_CAP {
                println!("NO PATH: {id} - no local path recorded");
                printed.set(printed.get() + 1);
            }
        },
    )
    .await?;

    #[allow(
        clippy::cast_possible_truncation,
        reason = "scan counts come from pagination over the state DB; well under usize::MAX on supported targets"
    )]
    let total_issues = (counts.missing + counts.no_path) as usize;
    if total_issues > printed.get() {
        println!();
        print_truncation_tail(total_issues, printed.get());
    }

    let mut marked_failed = 0u64;
    let mut mark_errors = 0u64;
    if !args.dry_run {
        for m in &missing {
            match db
                .mark_failed(&m.id, m.version_size.as_str(), FILE_MISSING_REASON)
                .await
            {
                Ok(()) => marked_failed += 1,
                Err(e) => {
                    #[allow(
                        clippy::print_stderr,
                        reason = "reconcile is a CLI subcommand that reports to the user via stdout/stderr"
                    )]
                    {
                        eprintln!(
                            "  failed to mark {}:{} as failed: {e}",
                            m.id,
                            m.version_size.as_str()
                        );
                    }
                    mark_errors += 1;
                }
            }
        }
    }

    println!();
    if mark_errors > 0 {
        // Print before "Results:" so stdout-scraping scripts don't see a
        // success-looking summary right before the non-zero exit.
        println!("FAILED: {mark_errors} state updates errored — see stderr above for details.");
        println!();
    }
    println!("Results:");
    println!("  Present:  {}", counts.present);
    println!("  Missing:  {}", counts.missing);
    if counts.no_path > 0 {
        println!("  No path:  {}", counts.no_path);
    }
    if args.dry_run {
        println!();
        println!("Dry run — no changes written. Re-run without --dry-run to mark missing assets as failed.");
    } else {
        println!("  Marked failed: {marked_failed}");
        if mark_errors > 0 {
            println!("  Mark errors:   {mark_errors}");
        }
    }

    if mark_errors > 0 {
        anyhow::bail!("reconcile partially failed: {mark_errors} state updates errored");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AssetStatus, SqliteStateDb};
    use crate::test_helpers::TestAssetRecord;

    /// Seed a `downloaded` row whose `local_path` points at `path`; the path
    /// itself is not touched on disk, so the scan treats it as missing
    /// unless the caller also writes a file there.
    async fn seed_downloaded(db: &SqliteStateDb, id: &str, path: &std::path::Path) {
        let record = TestAssetRecord::new(id)
            .checksum(&format!("ck_{id}"))
            .filename(&format!("{id}.jpg"))
            .size(100)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(id, "original", path, &format!("ck_{id}"), None)
            .await
            .unwrap();
    }

    async fn seed_missing(db: &SqliteStateDb, id: &str, path: &std::path::Path) {
        seed_downloaded(db, id, path).await;
    }

    async fn seed_present(db: &SqliteStateDb, id: &str, path: &std::path::Path) {
        std::fs::write(path, b"x").unwrap();
        seed_downloaded(db, id, path).await;
    }

    #[tokio::test]
    async fn reconcile_marks_missing_file_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        seed_missing(&db, "MISSING_1", &dir.path().join("does_not_exist.jpg")).await;
        seed_present(&db, "PRESENT_1", &dir.path().join("present.jpg")).await;

        let (counts, missing) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.present, 1);
        assert_eq!(counts.missing, 1);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id, "MISSING_1");

        for m in &missing {
            db.mark_failed(&m.id, m.version_size.as_str(), FILE_MISSING_REASON)
                .await
                .unwrap();
        }

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, "MISSING_1");
        assert_eq!(failed[0].status, AssetStatus::Failed);
        assert_eq!(
            failed[0].last_error.as_deref(),
            Some(FILE_MISSING_REASON),
            "reason should be the stable FILE_MISSING_AT_STARTUP sentinel"
        );

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed, 1);
    }

    #[tokio::test]
    async fn reconcile_dry_run_does_not_mutate_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        seed_missing(&db, "MISSING_DRY", &dir.path().join("x.jpg")).await;

        let (counts, missing) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.missing, 1);
        assert_eq!(missing.len(), 1);

        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed, 0);
    }

    #[tokio::test]
    async fn reconcile_handles_pagination_with_many_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        let total = (SCAN_PAGE_SIZE as usize) + 500;
        for i in 0..total {
            let id = format!("ROW_{i:05}");
            seed_missing(&db, &id, &dir.path().join(format!("{id}.jpg"))).await;
        }

        let (counts, missing) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.missing as usize, total);
        assert_eq!(counts.present, 0);
        assert_eq!(missing.len(), total);

        for m in &missing {
            db.mark_failed(&m.id, m.version_size.as_str(), FILE_MISSING_REASON)
                .await
                .unwrap();
        }
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 0);
        assert_eq!(summary.failed as usize, total);

        let (counts2, missing2) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts2.missing, 0);
        assert!(missing2.is_empty());
    }

    #[tokio::test]
    async fn reconcile_classifies_mixed_present_and_missing_across_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        let total = (SCAN_PAGE_SIZE as usize) + 250;
        let mut expected_missing = 0u64;
        let mut expected_present = 0u64;
        for i in 0..total {
            let id = format!("MIX_{i:05}");
            let path = dir.path().join(format!("{id}.jpg"));
            if i % 3 == 0 {
                seed_present(&db, &id, &path).await;
                expected_present += 1;
            } else {
                seed_missing(&db, &id, &path).await;
                expected_missing += 1;
            }
        }

        let (counts, missing) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.present, expected_present);
        assert_eq!(counts.missing, expected_missing);
        assert_eq!(missing.len() as u64, expected_missing);
    }

    #[test]
    fn file_missing_reason_is_stable() {
        // Wire format guarantee for any operator tooling keying on the sentinel.
        assert_eq!(FILE_MISSING_REASON, "FILE_MISSING_AT_STARTUP");
    }
}
