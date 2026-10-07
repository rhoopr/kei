//! State finalization for completed or failed download tasks.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::state::{DownloadStateStore, MetadataRewriteStore, VersionSizeKey};

use super::filter::DownloadTask;

pub(super) trait DownloadFinalizationStore:
    DownloadStateStore + MetadataRewriteStore
{
}

impl<T> DownloadFinalizationStore for T where T: DownloadStateStore + MetadataRewriteStore + ?Sized {}

/// A successful download whose state write to SQLite failed on first attempt.
/// Accumulated during download loops and retried in a final flush.
#[derive(Debug)]
pub(super) struct PendingStateWrite {
    pub(super) library: Arc<str>,
    pub(super) asset_id: Arc<str>,
    pub(super) version_size: VersionSizeKey,
    pub(super) download_path: PathBuf,
    pub(super) local_checksum: String,
    pub(super) download_checksum: Option<String>,
    pub(super) mark_capture_repair: bool,
    pub(super) retained: Option<Arc<super::file::RetainedPendingFile>>,
    pub(super) requires_retained: bool,
}

/// Maximum retry attempts for deferred state writes.
pub(super) const STATE_WRITE_MAX_RETRIES: u32 = 6;
const _: () = assert!(STATE_WRITE_MAX_RETRIES <= 32, "shift overflow in backoff");

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct StateWriteFlush {
    pub(super) attempted: usize,
    pub(super) failures: usize,
}

/// Minimum pending-queue size at which a 100% flush failure rate is treated
/// as "state DB unwritable" rather than a transient lock race.
pub(super) const STATE_DB_UNWRITABLE_THRESHOLD: usize = 5;

#[derive(Debug)]
pub(super) enum DownloadedFinalization {
    Persisted,
    Deferred {
        write: PendingStateWrite,
        error: Box<crate::state::error::StateError>,
    },
}

#[cfg(test)]
pub(in crate::download) mod finalization_probe;

/// Persist success state for a task that has already landed safely on disk.
/// A failed metadata write records a retry marker; retiring markers is left to
/// the rewrite drain. On failure, the caller receives a deferred write record
/// for bounded retry.
#[cfg(test)]
pub(super) async fn finalize_downloaded<D>(
    db: &D,
    library: &Arc<str>,
    task: &DownloadTask,
    local_checksum: String,
    download_checksum: Option<String>,
    exif_ok: bool,
    mark_capture_repair: bool,
) -> DownloadedFinalization
where
    D: DownloadFinalizationStore + ?Sized,
{
    finalize_downloaded_with_proof(
        db,
        library,
        task,
        local_checksum,
        download_checksum,
        exif_ok,
        mark_capture_repair,
        None,
    )
    .await
}

pub(super) async fn finalize_downloaded_with_proof<D>(
    db: &D,
    library: &Arc<str>,
    task: &DownloadTask,
    local_checksum: String,
    download_checksum: Option<String>,
    exif_ok: bool,
    mark_capture_repair: bool,
    retained: Option<Arc<super::file::RetainedPendingFile>>,
) -> DownloadedFinalization
where
    D: DownloadFinalizationStore + ?Sized,
{
    #[cfg(all(test, target_os = "linux"))]
    crate::test_helpers::process_death_point("published");
    let write = PendingStateWrite {
        library: Arc::clone(library),
        asset_id: task.asset_id.clone(),
        version_size: task.version_size,
        download_path: task.download_path.clone(),
        local_checksum,
        download_checksum,
        mark_capture_repair,
        retained,
        requires_retained: task.pending_cross_parent_root.is_some(),
    };
    if let Err(error) = validate_pending_write(&write).await {
        return DownloadedFinalization::Deferred {
            write,
            error: Box::new(error),
        };
    }
    match db
        .mark_verified_download(
            library,
            &task.asset_id,
            task.version_size.as_str(),
            &task.download_path,
            &write.local_checksum,
            write.download_checksum.as_deref(),
            mark_capture_repair,
        )
        .await
    {
        Ok(()) => {
            update_metadata_marker(
                db,
                library,
                &task.asset_id,
                task.version_size.as_str(),
                exif_ok,
            )
            .await;
            #[cfg(all(test, target_os = "linux"))]
            crate::test_helpers::process_death_point("state-persisted");
            DownloadedFinalization::Persisted
        }
        Err(error) => {
            #[cfg(test)]
            finalization_probe::observe(&task.download_path).await;
            DownloadedFinalization::Deferred {
                write,
                error: Box::new(error),
            }
        }
    }
}

/// Persist failure state for a task that could not be downloaded.
pub(super) async fn finalize_failed<D>(
    db: &D,
    library: &Arc<str>,
    task: &DownloadTask,
    error: &str,
) -> Result<(), crate::state::error::StateError>
where
    D: DownloadStateStore + ?Sized,
{
    let durable_error = match task.publication() {
        super::file::FinalPublication::NoReplace => error,
        super::file::FinalPublication::ReplaceTruncated(_) => {
            crate::commands::reconcile::FILE_TRUNCATED_REASON
        }
    };
    db.mark_failed(
        library,
        &task.asset_id,
        task.version_size.as_str(),
        durable_error,
    )
    .await
}

/// Record a metadata-rewrite marker when the EXIF/XMP writer failed.
///
/// A successful write does not retire an existing marker. This writes the file
/// from the snapshot the task was planned with, while the marker describes the
/// row, and a concurrent pass may have moved the row on since. Retiring it here
/// would leave the row and the file on different snapshots with nothing left to
/// repair them. The rewrite drain owns that decision, because it writes the
/// file from the same row it then clears.
async fn update_metadata_marker<D>(
    db: &D,
    library: &str,
    asset_id: &str,
    version_size: &str,
    exif_ok: bool,
) where
    D: MetadataRewriteStore + ?Sized,
{
    if exif_ok {
        return;
    }
    if let Err(e) = db
        .record_metadata_write_failure(library, asset_id, version_size)
        .await
    {
        tracing::warn!(
            asset_id,
            error = %e,
            "Could not set metadata-write-failed marker"
        );
    }
}

async fn validate_pending_write(
    write: &PendingStateWrite,
) -> Result<(), crate::state::error::StateError> {
    if write.requires_retained && write.retained.is_none() {
        return Err(crate::state::error::StateError::Invariant {
            operation: "reserved pending finalization",
            detail: "Missing original publication proof".into(),
        });
    }
    if let Some(proof) = &write.retained {
        let result = async {
            proof.validate().await?;
            anyhow::ensure!(
                data_encoding::HEXLOWER.encode(&proof.fingerprint.sha256) == write.local_checksum,
                "Retained publication hash does not match the receipt"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        result.map_err(|error| crate::state::error::StateError::Invariant {
            operation: "reserved pending finalization",
            detail: error.to_string(),
        })?;
    }
    Ok(())
}

async fn retry_pending_state_write<D>(
    db: &D,
    write: &PendingStateWrite,
    pending_count: usize,
) -> bool
where
    D: DownloadStateStore + ?Sized,
{
    use rand::RngExt;

    for attempt in 1..=STATE_WRITE_MAX_RETRIES {
        if let Err(error) = validate_pending_write(write).await {
            tracing::warn!(asset_id = %write.asset_id, %error,
                "Reserved pending publication proof changed; retaining media and pending debt");
            return false;
        }
        match db
            .mark_verified_download(
                &write.library,
                &write.asset_id,
                write.version_size.as_str(),
                &write.download_path,
                &write.local_checksum,
                write.download_checksum.as_deref(),
                write.mark_capture_repair,
            )
            .await
        {
            Ok(()) => {
                if attempt > 1 {
                    tracing::info!(
                        asset_id = %write.asset_id,
                        pending_count,
                        attempt,
                        "Recovered deferred state write"
                    );
                }
                return true;
            }
            Err(e) => {
                if attempt < STATE_WRITE_MAX_RETRIES {
                    tracing::info!(
                        asset_id = %write.asset_id,
                        pending_count,
                        attempt,
                        error = %e,
                        "State write retry failed, will retry"
                    );
                    let base_ms = 200 * u64::from(1u32 << (attempt - 1));
                    let jitter_ms = rand::rng().random_range(0..base_ms.max(1) / 4);
                    tokio::time::sleep(Duration::from_millis(base_ms + jitter_ms)).await;
                } else {
                    tracing::error!(
                        asset_id = %write.asset_id,
                        path = %write.download_path.display(),
                        error = %e,
                        "State write failed after {STATE_WRITE_MAX_RETRIES} attempts - \
                         file on disk but untracked; next sync will detect it via \
                         filesystem check and skip re-download"
                    );
                }
            }
        }
    }
    false
}

pub(super) async fn flush_pending_state_writes_retaining_failures<D>(
    db: &D,
    pending: &mut Vec<PendingStateWrite>,
) -> StateWriteFlush
where
    D: DownloadStateStore + ?Sized,
{
    if pending.is_empty() {
        return StateWriteFlush::default();
    }
    let pending_count = pending.len();
    tracing::info!(pending_count, "Retrying deferred state writes");

    let mut failed = Vec::new();
    for write in pending.drain(..) {
        if !retry_pending_state_write(db, &write, pending_count).await {
            failed.push(write);
        }
    }

    let flush = StateWriteFlush {
        attempted: pending_count,
        failures: failed.len(),
    };
    *pending = failed;

    if flush.failures > 0 {
        tracing::warn!(
            failures = flush.failures,
            total = flush.attempted,
            "Some state writes could not be saved"
        );
    } else {
        tracing::debug!(
            count = flush.attempted,
            "All deferred state writes recovered"
        );
    }
    flush
}

/// Retry all pending state writes that failed during a download pass.
///
/// Returns the number of writes that still failed after all retries.
pub(super) async fn flush_pending_state_writes<D>(db: &D, pending: &[PendingStateWrite]) -> usize
where
    D: DownloadStateStore + ?Sized,
{
    if pending.is_empty() {
        return 0;
    }
    let pending_count = pending.len();
    tracing::info!(pending_count, "Retrying deferred state writes");

    let mut failures = 0;
    for write in pending {
        if !retry_pending_state_write(db, write, pending_count).await {
            failures += 1;
        }
    }

    if failures > 0 {
        tracing::warn!(
            failures,
            total = pending.len(),
            "Some state writes could not be saved"
        );
    } else {
        tracing::debug!(count = pending.len(), "All deferred state writes recovered");
    }
    failures
}

pub(super) fn state_write_circuit_breaker_tripped(flush: &StateWriteFlush) -> bool {
    flush.attempted >= STATE_DB_UNWRITABLE_THRESHOLD && flush.failures == flush.attempted
}

pub(super) fn state_db_unwritable_error(pending_total: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "The state database appears unwritable: all {pending_total} deferred state writes failed after {STATE_WRITE_MAX_RETRIES} retries each. Check disk space and permissions on the state database file. Stopping sync to avoid downloading files that kei cannot track."
    )
}

pub(super) async fn check_state_write_circuit_breaker<D>(
    db: &D,
    pending: &mut Vec<PendingStateWrite>,
) -> Option<anyhow::Error>
where
    D: DownloadStateStore + ?Sized,
{
    if pending.len() < STATE_DB_UNWRITABLE_THRESHOLD {
        return None;
    }

    let flush = flush_pending_state_writes_retaining_failures(db, pending).await;
    if state_write_circuit_breaker_tripped(&flush) {
        return Some(state_db_unwritable_error(flush.attempted));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use chrono::Local;
    use tempfile::TempDir;

    use crate::state::{MediaType, SqliteStateDb, VersionSizeKey};
    use crate::test_helpers::TestAssetRecord;

    use super::super::filter::{DownloadTask, MetadataPayload};
    use super::*;

    const LIBRARY: &str = "PrimarySync";

    fn task(asset_id: &'static str, path: PathBuf) -> DownloadTask {
        DownloadTask {
            url: "https://example.test/photo.jpg".into(),
            download_path: path,
            replacement_fingerprint: None,
            pending_cross_parent_root: None,
            checksum: "remote_checksum".into(),
            asset_id: Arc::from(asset_id),
            asset_record_name: Arc::from(asset_id),
            library: Arc::from(LIBRARY),
            metadata: Arc::new(MetadataPayload::default()),
            size: 12,
            created_local: Local::now().fixed_offset(),
            version_size: VersionSizeKey::Original,
            media_type: MediaType::Photo,
        }
    }

    async fn seed_pending(db: &SqliteStateDb, asset_id: &str, filename: &str) {
        let record = TestAssetRecord::new(asset_id)
            .library(LIBRARY)
            .filename(filename)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    async fn write_file(path: &Path) {
        tokio::fs::write(path, b"finalized").await.unwrap();
    }

    async fn reserved_deferred_boundary(mutation: &str) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("old-album/pending.jpg");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_file(&path).await;
        let checksum = super::super::file::compute_sha256(&path).await.unwrap();
        let proof = super::super::file::retain_pending_file(dir.path(), &path)
            .await
            .unwrap()
            .unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let mut pending_task = task("PENDING", path.clone());
        pending_task.pending_cross_parent_root = Some(Arc::new(dir.path().to_path_buf()));
        let result = finalize_downloaded_with_proof(
            &db,
            &Arc::from(LIBRARY),
            &pending_task,
            checksum.clone(),
            Some(checksum),
            true,
            false,
            Some(proof),
        )
        .await;
        let DownloadedFinalization::Deferred { write, .. } = result else {
            panic!("missing row must defer the write");
        };
        seed_pending(&db, "PENDING", "pending.jpg").await;
        match mutation {
            "unchanged" => {}
            "bytes" => std::fs::write(&path, b"different").unwrap(),
            "inode" => {
                std::fs::rename(&path, path.with_extension("preserved")).unwrap();
                write_file(&path).await;
            }
            "ancestor" => {
                #[cfg(windows)]
                assert!(
                    std::fs::rename(path.parent().unwrap(), dir.path().join("preserved-album"))
                        .is_err(),
                    "live Windows directory capabilities must deny ancestor replacement"
                );
                #[cfg(not(windows))]
                {
                    std::fs::rename(path.parent().unwrap(), dir.path().join("preserved-album"))
                        .unwrap();
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    write_file(&path).await;
                }
            }
            #[cfg(unix)]
            "leaf-link" => {
                std::fs::rename(&path, path.with_extension("preserved")).unwrap();
                std::os::unix::fs::symlink(path.with_extension("preserved"), &path).unwrap();
            }
            #[cfg(unix)]
            "ancestor-link" => {
                std::fs::rename(path.parent().unwrap(), dir.path().join("preserved-album"))
                    .unwrap();
                std::os::unix::fs::symlink(
                    dir.path().join("preserved-album"),
                    path.parent().unwrap(),
                )
                .unwrap();
            }
            _ => panic!("unknown mutation"),
        }
        let mut pending = vec![write];
        let flush = flush_pending_state_writes_retaining_failures(&db, &mut pending).await;
        #[cfg(windows)]
        let unsafe_change = mutation != "unchanged" && mutation != "ancestor";
        #[cfg(not(windows))]
        let unsafe_change = mutation != "unchanged";
        assert_eq!(flush.failures, usize::from(unsafe_change), "{mutation}");
        assert_eq!(
            pending.len(),
            usize::from(unsafe_change),
            "must retain refused write"
        );
        assert_eq!(
            db.get_downloaded_page(0, 10).await.unwrap().len(),
            usize::from(!unsafe_change)
        );
        if unsafe_change {
            assert_eq!(db.get_pending().await.unwrap().len(), 1);
            let again = flush_pending_state_writes_retaining_failures(&db, &mut pending).await;
            assert_eq!(
                again.failures, 1,
                "proof cannot be minted during another attempt"
            );
        }
    }

    #[tokio::test]
    async fn issue_770_recovery_deferred_publication_rechecks_bytes_identity_and_namespace() {
        for mutation in ["unchanged", "bytes", "inode", "ancestor"] {
            reserved_deferred_boundary(mutation).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn issue_770_recovery_deferred_publication_refuses_leaf_and_ancestor_links() {
        for mutation in ["leaf-link", "ancestor-link"] {
            reserved_deferred_boundary(mutation).await;
        }
    }

    #[tokio::test]
    async fn issue_770_recovery_immediate_finalization_requires_original_unchanged_proof() {
        for missing in [false, true] {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("pending.jpg");
            write_file(&path).await;
            let checksum = super::super::file::compute_sha256(&path).await.unwrap();
            let proof = super::super::file::retain_pending_file(dir.path(), &path)
                .await
                .unwrap()
                .unwrap();
            std::fs::write(&path, b"different").unwrap();
            let db = SqliteStateDb::open_in_memory().unwrap();
            seed_pending(&db, "PENDING", "pending.jpg").await;
            let mut pending_task = task("PENDING", path);
            pending_task.pending_cross_parent_root = Some(Arc::new(dir.path().to_path_buf()));
            let result = finalize_downloaded_with_proof(
                &db,
                &Arc::from(LIBRARY),
                &pending_task,
                checksum.clone(),
                Some(checksum),
                true,
                false,
                if missing { None } else { Some(proof) },
            )
            .await;
            let DownloadedFinalization::Deferred { write, .. } = result else {
                panic!("missing or changed publication proof must refuse immediate write");
            };
            let mut pending = vec![write];
            assert_eq!(
                flush_pending_state_writes_retaining_failures(&db, &mut pending)
                    .await
                    .failures,
                1
            );
            assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
            assert_eq!(db.get_pending().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn finalize_downloaded_marks_persisted_and_keeps_metadata_marker() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("persisted.jpg");
        write_file(&path).await;
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_pending(&db, "FINAL_OK", "persisted.jpg").await;
        db.record_metadata_write_failure(LIBRARY, "FINAL_OK", "original")
            .await
            .unwrap();

        let result = finalize_downloaded(
            &db,
            &Arc::from(LIBRARY),
            &task("FINAL_OK", path.clone()),
            "local_checksum".to_string(),
            Some("download_checksum".to_string()),
            true,
            false,
        )
        .await;

        assert!(matches!(result, DownloadedFinalization::Persisted));
        assert!(
            !db.should_download(LIBRARY, "FINAL_OK", "original", "checksum123", &path)
                .await
                .unwrap(),
            "downloaded row with existing file should not be queued again"
        );
        assert_eq!(
            db.get_pending_metadata_rewrites(32).await.unwrap().len(),
            1,
            "the marker describes the row, so only the drain may retire it"
        );
    }

    #[tokio::test]
    async fn finalize_downloaded_requeues_capture_repair_for_replacement() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("capture-replacement.jpg");
        write_file(&path).await;
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_pending(&db, "FINAL_CAPTURE", "capture-replacement.jpg").await;

        let result = finalize_downloaded(
            &db,
            &Arc::from(LIBRARY),
            &task("FINAL_CAPTURE", path),
            "replacement_checksum".to_string(),
            None,
            true,
            true,
        )
        .await;

        assert!(matches!(result, DownloadedFinalization::Persisted));
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                crate::state::db::MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].asset.id.as_ref(), "FINAL_CAPTURE");
        assert_eq!(
            pending[0].asset.local_checksum.as_deref(),
            Some("replacement_checksum")
        );
    }

    #[tokio::test]
    async fn deferred_verified_download_preserves_source_checksum_after_reopen() {
        use crate::state::db::MetadataRewriteQueue;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("source.jpg");
        let db_path = dir.path().join("state.db");
        write_file(&path).await;
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let result = finalize_downloaded(
            &db,
            &Arc::from(LIBRARY),
            &task("SOURCE_DEFER", path.clone()),
            checksum.clone(),
            Some(checksum.clone()),
            true,
            false,
        )
        .await;
        let DownloadedFinalization::Deferred { write, .. } = result else {
            panic!("missing catalogue row must defer source provenance too");
        };
        seed_pending(&db, "SOURCE_DEFER", "source.jpg").await;
        assert_eq!(flush_pending_state_writes(&db, &[write]).await, 0);
        db.record_metadata_write_failure(LIBRARY, "SOURCE_DEFER", "original")
            .await
            .unwrap();
        drop(db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].asset.local_path.as_deref(), Some(path.as_path()));
        assert_eq!(
            pending[0].source_checksum.as_deref(),
            Some(checksum.as_str())
        );
        assert_eq!(
            pending[0].asset.local_checksum.as_deref(),
            Some(checksum.as_str())
        );
    }

    #[tokio::test]
    async fn finalize_downloaded_failure_defers_write() {
        let path = TempDir::new().unwrap().path().join("missing-row.jpg");
        let db = SqliteStateDb::open_in_memory().unwrap();

        let result = finalize_downloaded(
            &db,
            &Arc::from(LIBRARY),
            &task("FINAL_DEFER", path.clone()),
            "local_checksum".to_string(),
            None,
            true,
            false,
        )
        .await;

        let DownloadedFinalization::Deferred { write, error: _ } = result else {
            panic!("missing state row should defer the state write");
        };
        assert_eq!(write.library.as_ref(), LIBRARY);
        assert_eq!(write.asset_id.as_ref(), "FINAL_DEFER");
        assert_eq!(write.version_size, VersionSizeKey::Original);
        assert_eq!(write.download_path, path);
        assert_eq!(write.local_checksum, "local_checksum");
        assert_eq!(write.download_checksum, None);
        assert!(!write.mark_capture_repair);
    }

    #[tokio::test]
    async fn finalize_downloaded_metadata_failure_sets_retry_marker() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rewrite-needed.jpg");
        write_file(&path).await;
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_pending(&db, "FINAL_REWRITE", "rewrite-needed.jpg").await;

        let result = finalize_downloaded(
            &db,
            &Arc::from(LIBRARY),
            &task("FINAL_REWRITE", path),
            "local_checksum".to_string(),
            None,
            false,
            false,
        )
        .await;

        assert!(matches!(result, DownloadedFinalization::Persisted));
        let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id.as_ref(), "FINAL_REWRITE");
    }

    #[tokio::test]
    async fn finalize_failed_records_failure_status() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_pending(&db, "FINAL_FAILED", "failed.jpg").await;
        let task = task("FINAL_FAILED", PathBuf::from("failed.jpg"));

        finalize_failed(&db, &Arc::from(LIBRARY), &task, "cdn expired")
            .await
            .unwrap();

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id.as_ref(), "FINAL_FAILED");
        assert_eq!(failed[0].last_error.as_deref(), Some("cdn expired"));
    }

    #[tokio::test]
    async fn failed_truncated_repair_preserves_durable_authorization() {
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_pending(&db, "FINAL_REPAIR", "repair.jpg").await;
        let mut task = task("FINAL_REPAIR", PathBuf::from("repair.jpg"));
        task.replacement_fingerprint =
            Some(Arc::new(crate::download::file::ExistingFileFingerprint {
                size: 3,
                sha256: [7; 32],
            }));

        finalize_failed(&db, &Arc::from(LIBRARY), &task, "network failed")
            .await
            .unwrap();

        let failed = db.get_failed().await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(
            failed[0].last_error.as_deref(),
            Some(crate::commands::reconcile::FILE_TRUNCATED_REASON)
        );
    }
}
