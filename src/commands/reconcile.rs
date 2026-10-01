//! `kei reconcile` - reconcile the state database with files on disk.
//!
//! Scans every asset marked `downloaded` in the state database and checks
//! that its recorded `local_path` still exists and is not shorter than the
//! provider-reported size. Drifted files are marked as failed so the next sync
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
use std::sync::Arc;

use crate::cli;
use crate::config;
use crate::download::file::{LocalFileSizeExpectation, local_file_size_matches_state};
use crate::state;
use crate::state::{DownloadStateStore, ReportStateStore, VersionSizeKey};

use super::{LISTING_CAP, print_truncation_tail};

/// Stable sentinels written to `assets.last_error` so monitoring tools can
/// key on the reason a row flipped from downloaded back to failed.
pub(crate) const FILE_MISSING_REASON: &str = "FILE_MISSING_AT_STARTUP";
pub(crate) const FILE_TRUNCATED_REASON: &str = "FILE_TRUNCATED_AT_STARTUP";

const SCAN_PAGE_SIZE: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalDriftKind {
    Missing,
    Truncated {
        actual_size: u64,
        expected_size: u64,
    },
}

impl LocalDriftKind {
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Missing => FILE_MISSING_REASON,
            Self::Truncated { .. } => FILE_TRUNCATED_REASON,
        }
    }
}

#[derive(Debug)]
pub(crate) struct LocalDriftAsset {
    pub(crate) library: Arc<str>,
    pub(crate) id: Box<str>,
    pub(crate) version_size: VersionSizeKey,
    pub(crate) local_path: PathBuf,
    pub(crate) kind: LocalDriftKind,
}

#[derive(Debug)]
pub(crate) struct LocalDriftUpdate {
    pub(crate) library: Arc<str>,
    pub(crate) id: Box<str>,
    pub(crate) version_size: VersionSizeKey,
    pub(crate) kind: LocalDriftKind,
}

impl From<LocalDriftAsset> for LocalDriftUpdate {
    fn from(asset: LocalDriftAsset) -> Self {
        Self {
            library: asset.library,
            id: asset.id,
            version_size: asset.version_size,
            kind: asset.kind,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ScanCounts {
    pub(crate) present: u64,
    pub(crate) missing: u64,
    pub(crate) damaged: u64,
    pub(crate) no_path: u64,
}

pub(crate) async fn classify_local_drift(
    asset: state::AssetRecord,
) -> anyhow::Result<(Option<LocalDriftAsset>, bool)> {
    let state::AssetRecord {
        library,
        id,
        version_size,
        local_path,
        size_bytes,
        local_checksum,
        download_checksum,
        ..
    } = asset;

    let Some(local_path) = local_path else {
        return Ok((None, true));
    };

    let metadata = match tokio::fs::metadata(&local_path).await {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                Some(LocalDriftAsset {
                    library,
                    id,
                    version_size,
                    local_path,
                    kind: LocalDriftKind::Missing,
                }),
                false,
            ));
        }
        Err(e) => return Err(e.into()),
    };

    let actual_size = metadata.len();
    if !local_file_size_matches_state(
        &local_path,
        actual_size,
        LocalFileSizeExpectation::AtLeastProvider(size_bytes),
        local_checksum.as_deref(),
        download_checksum.as_deref(),
    )
    .await?
    {
        return Ok((
            Some(LocalDriftAsset {
                library,
                id,
                version_size,
                local_path,
                kind: LocalDriftKind::Truncated {
                    actual_size,
                    expected_size: size_bytes,
                },
            }),
            false,
        ));
    }

    Ok((None, false))
}

/// Reads every page before any mutation so later `mark_failed` calls can't
/// shift OFFSET pagination and skip rows.
pub(crate) async fn scan_local_drift<D>(
    db: &D,
    mut report_drift: impl FnMut(&LocalDriftAsset),
    mut report_no_path: impl FnMut(&str),
) -> anyhow::Result<(ScanCounts, Vec<LocalDriftUpdate>)>
where
    D: ReportStateStore + ?Sized,
{
    let mut counts = ScanCounts::default();
    let mut drift_updates = Vec::new();

    let mut offset = 0u64;
    loop {
        let page = db.get_downloaded_page(offset, SCAN_PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as u64;

        for asset in page {
            let no_path_id = asset.local_path.is_none().then(|| asset.id.clone());
            let (record, no_path) = classify_local_drift(asset).await?;
            if no_path {
                if let Some(id) = no_path_id {
                    report_no_path(&id);
                }
                counts.no_path += 1;
                continue;
            }
            let Some(record) = record else {
                counts.present += 1;
                continue;
            };
            match record.kind {
                LocalDriftKind::Missing => counts.missing += 1,
                LocalDriftKind::Truncated { .. } => counts.damaged += 1,
            }
            report_drift(&record);
            drift_updates.push(LocalDriftUpdate::from(record));
        }
    }

    Ok((counts, drift_updates))
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
            "Reconciling {} downloaded assets (dry run - no changes will be written)...",
            summary.downloaded
        );
    } else {
        println!("Reconciling {} downloaded assets...", summary.downloaded);
    }
    println!();

    // Shared cell so both callbacks can bump the same counter under the
    // FnMut + FnMut constraint. Single-threaded; Cell is cheap here.
    let printed: Cell<usize> = Cell::new(0);

    let (counts, drifted) = scan_local_drift(
        &db,
        |m| {
            if printed.get() < LISTING_CAP {
                match m.kind {
                    LocalDriftKind::Missing => println!(
                        "MISSING: {} ({}, {})",
                        m.local_path.display(),
                        m.id,
                        m.version_size.as_str(),
                    ),
                    LocalDriftKind::Truncated {
                        actual_size,
                        expected_size,
                    } => println!(
                        "TRUNCATED: {} ({}, {}) - {actual_size} < {expected_size} bytes",
                        m.local_path.display(),
                        m.id,
                        m.version_size.as_str(),
                    ),
                }
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
    let total_issues = (counts.missing + counts.damaged + counts.no_path) as usize;
    if total_issues > printed.get() {
        println!();
        print_truncation_tail(total_issues, printed.get());
    }

    let protected = db.get_protected_legacy_ids().await?;
    let mut protected_drift = 0u64;
    let mut marked_failed = 0u64;
    let mut mark_errors = 0u64;
    if !args.dry_run {
        for m in &drifted {
            if protected.contains(&(m.library.to_string(), m.id.to_string())) {
                protected_drift += 1;
                continue;
            }
            match db
                .mark_failed(&m.library, &m.id, m.version_size.as_str(), m.kind.reason())
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
        println!("FAILED: {mark_errors} state updates errored - see stderr above for details.");
        println!();
    }
    println!("Results:");
    println!("  Present:  {}", counts.present);
    println!("  Missing:  {}", counts.missing);
    if counts.damaged > 0 {
        println!("  Damaged:  {}", counts.damaged);
    }
    if counts.no_path > 0 {
        println!("  No path:  {}", counts.no_path);
    }
    if args.dry_run {
        println!();
        println!(
            "Dry run - no changes written. Re-run without --dry-run to mark drifted assets as failed."
        );
    } else {
        println!("  Marked failed: {marked_failed}");
        if protected_drift > 0 {
            println!(
                "  Protected legacy records needing inspection: {protected_drift}; original receipts retained"
            );
        }
        if mark_errors > 0 {
            println!("  Mark errors:   {mark_errors}");
        }
    }

    if mark_errors > 0 {
        anyhow::bail!(
            "Reconcile found {mark_errors} drifted files that could not be marked failed in the state database."
        );
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
        db.mark_downloaded(
            "PrimarySync",
            id,
            "original",
            path,
            &format!("ck_{id}"),
            None,
        )
        .await
        .unwrap();
    }

    async fn seed_missing(db: &SqliteStateDb, id: &str, path: &std::path::Path) {
        seed_downloaded(db, id, path).await;
    }

    async fn seed_present(db: &SqliteStateDb, id: &str, path: &std::path::Path) {
        std::fs::write(path, vec![0u8; 100]).unwrap();
        seed_downloaded(db, id, path).await;
    }

    #[tokio::test]
    async fn reconcile_marks_missing_file_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        seed_missing(&db, "MISSING_1", &dir.path().join("does_not_exist.jpg")).await;
        seed_present(&db, "PRESENT_1", &dir.path().join("present.jpg")).await;

        let (counts, missing) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.present, 1);
        assert_eq!(counts.missing, 1);
        assert_eq!(missing.len(), 1);
        assert_eq!(&*missing[0].id, "MISSING_1");

        for m in &missing {
            db.mark_failed(
                &m.library,
                &m.id,
                m.version_size.as_str(),
                FILE_MISSING_REASON,
            )
            .await
            .unwrap();
        }

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(&*failed[0].id, "MISSING_1");
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

        let (counts, missing) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
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

        let (counts, missing) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.missing as usize, total);
        assert_eq!(counts.present, 0);
        assert_eq!(missing.len(), total);

        for m in &missing {
            db.mark_failed(
                &m.library,
                &m.id,
                m.version_size.as_str(),
                FILE_MISSING_REASON,
            )
            .await
            .unwrap();
        }
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 0);
        assert_eq!(summary.failed as usize, total);

        let (counts2, missing2) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
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

        let (counts, missing) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
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

    #[tokio::test]
    async fn reconcile_marks_truncated_file_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let path = dir.path().join("truncated.jpg");
        std::fs::write(&path, b"short").unwrap();
        seed_downloaded(&db, "TRUNCATED_1", &path).await;

        let (counts, drift) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
            .await
            .unwrap();
        assert_eq!(counts.present, 0);
        assert_eq!(counts.missing, 0);
        assert_eq!(counts.damaged, 1);
        assert_eq!(drift.len(), 1);
        assert_eq!(&*drift[0].id, "TRUNCATED_1");
        assert!(matches!(
            drift[0].kind,
            LocalDriftKind::Truncated {
                actual_size: 5,
                expected_size: 100
            }
        ));

        for m in &drift {
            db.mark_failed(&m.library, &m.id, m.version_size.as_str(), m.kind.reason())
                .await
                .unwrap();
        }

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].last_error.as_deref(), Some(FILE_TRUNCATED_REASON));
    }

    #[tokio::test]
    async fn reconcile_accepts_smaller_intact_metadata_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let path = dir.path().join("metadata-rewritten.jpg");
        std::fs::write(&path, b"post-write bytes").unwrap();
        let local_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let record = TestAssetRecord::new("METADATA_REWRITTEN").size(100).build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "METADATA_REWRITTEN",
            "original",
            &path,
            &local_checksum,
            Some("pre-metadata-download-checksum"),
        )
        .await
        .unwrap();

        let (counts, drift) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
            .await
            .unwrap();

        assert_eq!(counts.present, 1);
        assert_eq!(counts.damaged, 0);
        assert!(drift.is_empty());
    }

    #[tokio::test]
    async fn reconcile_detects_changed_metadata_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let path = dir.path().join("changed-after-metadata.jpg");
        std::fs::write(&path, b"intact post-write bytes").unwrap();
        let local_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let record = TestAssetRecord::new("CHANGED_AFTER_METADATA")
            .size(100)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CHANGED_AFTER_METADATA",
            "original",
            &path,
            &local_checksum,
            Some("pre-metadata-download-checksum"),
        )
        .await
        .unwrap();
        std::fs::write(&path, b"changed").unwrap();

        let (counts, drift) = scan_local_drift(&db, |_: &LocalDriftAsset| {}, |_: &str| {})
            .await
            .unwrap();

        assert_eq!(counts.present, 0);
        assert_eq!(counts.damaged, 1);
        assert!(matches!(
            drift.as_slice(),
            [LocalDriftUpdate {
                kind: LocalDriftKind::Truncated {
                    actual_size: 7,
                    expected_size: 100,
                },
                ..
            }]
        ));
    }

    #[test]
    fn file_truncated_reason_is_stable() {
        assert_eq!(FILE_TRUNCATED_REASON, "FILE_TRUNCATED_AT_STARTUP");
    }
}
