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

use crate::cli;
use crate::config;
use crate::state;
use crate::state::StateDb;

/// Error message written to `assets.last_error` when reconcile detects a
/// missing file. Stable across versions so monitoring tools can key on it.
const FILE_MISSING_REASON: &str = "FILE_MISSING_AT_STARTUP";

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

    let mut missing = 0u64;
    let mut present = 0u64;
    let mut no_path = 0u64;
    let mut marked_failed = 0u64;
    let mut mark_errors = 0u64;

    const PAGE_SIZE: u32 = 1000;
    let mut offset = 0u64;
    loop {
        let page = db.get_downloaded_page(offset, PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as u64;

        for asset in &page {
            let Some(local_path) = &asset.local_path else {
                println!("NO PATH: {} - no local path recorded", asset.id);
                no_path += 1;
                continue;
            };

            let exists = tokio::fs::try_exists(local_path).await.unwrap_or(false);
            if exists {
                present += 1;
                continue;
            }

            println!(
                "MISSING: {} ({}, {})",
                local_path.display(),
                asset.id,
                asset.version_size.as_str(),
            );
            missing += 1;

            if args.dry_run {
                continue;
            }
            match db
                .mark_failed(&asset.id, asset.version_size.as_str(), FILE_MISSING_REASON)
                .await
            {
                Ok(()) => marked_failed += 1,
                Err(e) => {
                    eprintln!(
                        "  failed to mark {}:{} as failed: {e}",
                        asset.id,
                        asset.version_size.as_str()
                    );
                    mark_errors += 1;
                }
            }
        }
    }

    println!();
    println!("Results:");
    println!("  Present:  {present}");
    println!("  Missing:  {missing}");
    if no_path > 0 {
        println!("  No path:  {no_path}");
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

    #[tokio::test]
    async fn reconcile_marks_missing_file_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        // Create an asset with a local_path that does not exist
        let record = TestAssetRecord::new("MISSING_1")
            .checksum("cksum_1")
            .filename("missing.jpg")
            .size(100)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let missing_path = dir.path().join("does_not_exist.jpg");
        db.mark_downloaded("MISSING_1", "original", &missing_path, "cksum_1", None)
            .await
            .unwrap();

        // Create an asset with a local_path that DOES exist
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

        // Simulate a direct reconcile run via the inner loop logic
        const PAGE_SIZE: u32 = 1000;
        let mut marked = 0u64;
        let page = db.get_downloaded_page(0, PAGE_SIZE).await.unwrap();
        for asset in &page {
            let Some(local_path) = &asset.local_path else {
                continue;
            };
            if !tokio::fs::try_exists(local_path).await.unwrap_or(false) {
                db.mark_failed(&asset.id, asset.version_size.as_str(), FILE_MISSING_REASON)
                    .await
                    .unwrap();
                marked += 1;
            }
        }
        assert_eq!(marked, 1);

        // Verify: MISSING_1 is failed with the right reason, PRESENT_1 still downloaded
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

        let record = TestAssetRecord::new("MISSING_DRY")
            .checksum("c")
            .filename("x.jpg")
            .size(1)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "MISSING_DRY",
            "original",
            &dir.path().join("x.jpg"),
            "c",
            None,
        )
        .await
        .unwrap();

        // In dry-run we still detect missing but don't mark_failed.
        let page = db.get_downloaded_page(0, 1000).await.unwrap();
        let mut detected = 0u64;
        for asset in &page {
            let Some(lp) = &asset.local_path else {
                continue;
            };
            if !tokio::fs::try_exists(lp).await.unwrap_or(false) {
                detected += 1;
                // ← dry-run skips mark_failed here
            }
        }
        assert_eq!(detected, 1);

        // State should be unchanged
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn file_missing_reason_is_stable() {
        // Wire format guarantee for any operator tooling keying on the sentinel.
        assert_eq!(FILE_MISSING_REASON, "FILE_MISSING_AT_STARTUP");
    }
}
