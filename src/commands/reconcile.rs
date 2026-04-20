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

use std::path::PathBuf;

use crate::cli;
use crate::config;
use crate::state;
use crate::state::StateDb;

/// Error message written to `assets.last_error` when reconcile detects a
/// missing file. Stable across versions so monitoring tools can key on it.
const FILE_MISSING_REASON: &str = "FILE_MISSING_AT_STARTUP";

/// Page size for the scan pass. `get_downloaded_page` is paginated to cap
/// DB memory; the scan does not mutate the `downloaded` result set, so
/// OFFSET pagination is safe here.
const SCAN_PAGE_SIZE: u32 = 1000;

/// One missing-file record collected during the scan pass.
#[derive(Debug, Clone)]
struct MissingAsset {
    id: String,
    version_size: String,
    local_path: PathBuf,
}

/// Aggregate counts from a reconcile scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ScanCounts {
    present: u64,
    missing: u64,
    no_path: u64,
}

/// Scan all `downloaded` assets and return the subset whose recorded
/// `local_path` no longer exists on disk. Pure read: mutates neither the
/// DB nor any files. Decoupled from `run_reconcile` so tests can drive it
/// against arbitrary state without reimplementing the loop.
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

        for asset in &page {
            let Some(local_path) = &asset.local_path else {
                report_no_path(&asset.id);
                counts.no_path += 1;
                continue;
            };

            let exists = tokio::fs::try_exists(local_path).await.unwrap_or(false);
            if exists {
                counts.present += 1;
                continue;
            }

            let record = MissingAsset {
                id: asset.id.clone(),
                version_size: asset.version_size.as_str().to_string(),
                local_path: local_path.clone(),
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

    // Scan pass: collect every missing row. This is read-only, so OFFSET
    // pagination on `WHERE status='downloaded'` is safe.
    let (counts, missing) = scan_missing(
        &db,
        |m| {
            println!(
                "MISSING: {} ({}, {})",
                m.local_path.display(),
                m.id,
                m.version_size,
            );
        },
        |id| {
            println!("NO PATH: {id} - no local path recorded");
        },
    )
    .await?;

    // Mutation pass: mark each missing row as failed. Executed after the
    // scan completes so flipping rows out of the `downloaded` set can't
    // cause pagination to skip still-downloaded rows.
    let mut marked_failed = 0u64;
    let mut mark_errors = 0u64;
    if !args.dry_run {
        for m in &missing {
            match db
                .mark_failed(&m.id, &m.version_size, FILE_MISSING_REASON)
                .await
            {
                Ok(()) => marked_failed += 1,
                Err(e) => {
                    eprintln!(
                        "  failed to mark {}:{} as failed: {e}",
                        m.id, m.version_size
                    );
                    mark_errors += 1;
                }
            }
        }
    }

    println!();
    if mark_errors > 0 {
        // Surface the failure before the summary so scripts reading
        // stdout don't see a "Results:" block and assume success.
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

    /// Build a downloaded asset whose `local_path` does not exist on disk.
    async fn seed_missing(db: &SqliteStateDb, id: &str, path: &std::path::Path) {
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

    #[tokio::test]
    async fn reconcile_marks_missing_file_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Missing file
        seed_missing(&db, "MISSING_1", &dir.path().join("does_not_exist.jpg")).await;

        // Present file
        let record2 = TestAssetRecord::new("PRESENT_1")
            .checksum("cksum_2")
            .filename("present.jpg")
            .size(100)
            .build();
        db.upsert_seen(&record2).await.unwrap();
        let present_path = dir.path().join("present.jpg");
        std::fs::write(&present_path, b"data").unwrap();
        db.mark_downloaded("PRESENT_1", "original", &present_path, "cksum_2", None)
            .await
            .unwrap();

        let (counts, missing) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.present, 1);
        assert_eq!(counts.missing, 1);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].id, "MISSING_1");

        // Apply the mutation pass.
        for m in &missing {
            db.mark_failed(&m.id, &m.version_size, FILE_MISSING_REASON)
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

        // scan_missing is always read-only. A dry-run in run_reconcile just
        // skips the mutation loop that follows.
        let (counts, missing) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.missing, 1);
        assert_eq!(missing.len(), 1);

        // Nothing mutated the DB.
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed, 0);
    }

    /// Regression for the offset-vs-mutation bug: with > SCAN_PAGE_SIZE rows
    /// and a high miss ratio, the original implementation paginated the
    /// `downloaded` result set while simultaneously flipping rows out of it,
    /// causing later pages to skip rows that were still downloaded. The
    /// two-phase (scan, then mutate) design keeps the scan read-only so
    /// every missing row is collected before any mark_failed fires.
    #[tokio::test]
    async fn reconcile_handles_pagination_with_many_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Seed >SCAN_PAGE_SIZE rows, all with non-existent local_paths.
        let total = (SCAN_PAGE_SIZE as usize) + 500; // 1500 rows
        for i in 0..total {
            let id = format!("ROW_{i:05}");
            seed_missing(&db, &id, &dir.path().join(format!("{id}.jpg"))).await;
        }

        // Scan pass should find every missing row, regardless of page
        // boundary interactions.
        let (counts, missing) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.missing as usize, total);
        assert_eq!(counts.present, 0);
        assert_eq!(missing.len(), total);

        // Mutation pass: mark them all failed, then verify every original
        // row ended up in the failed state — no silent skips.
        for m in &missing {
            db.mark_failed(&m.id, &m.version_size, FILE_MISSING_REASON)
                .await
                .unwrap();
        }
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 0);
        assert_eq!(summary.failed as usize, total);

        // Second reconcile pass against the resulting DB must see zero
        // downloaded rows and therefore zero missing files.
        let (counts2, missing2) = scan_missing(&db, |_: &MissingAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts2.missing, 0);
        assert!(missing2.is_empty());
    }

    /// Even with a mix of present and missing files across page
    /// boundaries, the scan classifies each row correctly.
    #[tokio::test]
    async fn reconcile_classifies_mixed_present_and_missing_across_pages() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        let total = (SCAN_PAGE_SIZE as usize) + 250; // 1250 rows
        let mut expected_missing = 0u64;
        let mut expected_present = 0u64;
        for i in 0..total {
            let id = format!("MIX_{i:05}");
            let path = dir.path().join(format!("{id}.jpg"));
            // Every third row is "present" on disk; the rest are missing.
            if i % 3 == 0 {
                std::fs::write(&path, b"x").unwrap();
                let rec = TestAssetRecord::new(&id)
                    .checksum(&format!("ck_{id}"))
                    .filename(&format!("{id}.jpg"))
                    .size(1)
                    .build();
                db.upsert_seen(&rec).await.unwrap();
                db.mark_downloaded(&id, "original", &path, &format!("ck_{id}"), None)
                    .await
                    .unwrap();
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
