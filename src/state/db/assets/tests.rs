//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::collections::HashSet;
use std::fs;
use std::path::Path;

use chrono::{DateTime, TimeZone, Utc};

use crate::state::db::test_support::test_dir;
use crate::state::db::{
    AssetVerificationState, DownloadStateStore, MetadataRewriteCompletion, MetadataRewriteQueue,
    RetryErrorRetention, SqliteStateDb,
};
use crate::state::error::StateError;
use crate::state::types::{AssetMetadata, AssetStatus, MediaType, VersionSizeKey};
use crate::test_helpers::TestAssetRecord;

#[tokio::test]
async fn verified_source_checksum_round_trip_is_path_and_rendition_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    let first = dir.path().join("first.jpg");
    let second = dir.path().join("second.jpg");
    let mut record = TestAssetRecord::new("PROVENANCE")
        .checksum("provider-v1")
        .metadata(AssetMetadata {
            metadata_hash: Some("metadata".into()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_verified_download(
        "PrimarySync",
        "PROVENANCE",
        "original",
        &first,
        "local-first",
        Some("source-first"),
        false,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
        .await
        .unwrap();
    // Import/reconciliation-style registration must not treat recovered
    // download_checksum values as source provenance for a different path.
    db.mark_downloaded(
        "PrimarySync",
        "PROVENANCE",
        "original",
        &second,
        "local-second",
        Some("recovered-second"),
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
        .await
        .unwrap();
    drop(db);
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert_eq!(pending.len(), 2);
    for work in &pending {
        let expected = if work.asset.local_path.as_deref() == Some(first.as_path()) {
            Some("source-first")
        } else {
            None
        };
        assert_eq!(work.source_checksum.as_deref(), expected);
    }
    // Metadata finalization must preserve the additional path's source
    // evidence while updating the independent local/size checksum state.
    let extra = pending
        .iter()
        .find(|work| work.asset.local_path.as_deref() == Some(first.as_path()))
        .unwrap();
    db.finish_metadata_rewrite(
        extra,
        MetadataRewriteQueue::Ordinary,
        Some("local-rewritten"),
        Some("local-first"),
        MetadataRewriteCompletion::None,
    )
    .await
    .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    let extra = pending
        .iter()
        .find(|work| work.asset.local_path.as_deref() == Some(first.as_path()))
        .unwrap();
    assert_eq!(extra.source_checksum.as_deref(), Some("source-first"));
    assert_eq!(
        extra.asset.local_checksum.as_deref(),
        Some("local-rewritten")
    );

    db.mark_verified_download(
        "PrimarySync",
        "PROVENANCE",
        "original",
        &second,
        "local-second",
        Some("source-second"),
        false,
    )
    .await
    .unwrap();
    record.checksum = "provider-v2".into();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "PROVENANCE",
        "original",
        &second,
        "local-v2",
        Some("recovered-v2"),
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
        .await
        .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert_eq!(
        pending.len(),
        1,
        "old provider paths cannot supply current metadata evidence"
    );
    assert_eq!(
        pending[0].asset.local_path.as_deref(),
        Some(second.as_path())
    );
    assert!(pending[0].source_checksum.is_none());
}

#[tokio::test]
async fn verified_source_checksum_failure_rolls_back_download_finalization() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    let media = dir.path().join("source.jpg");
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    let record = TestAssetRecord::new("SOURCE_COMMIT").build();
    db.upsert_seen(&record).await.unwrap();
    {
        let conn = db.acquire_lock("test_source_checksum_failure").unwrap();
        conn.execute_batch(
            "CREATE TEMP TRIGGER fail_source_checksum BEFORE UPDATE OF source_checksum \
                 ON asset_metadata_paths BEGIN SELECT RAISE(ABORT, 'source checksum failure'); END;",
        )
        .unwrap();
    }
    assert!(
        db.mark_verified_download(
            "PrimarySync",
            "SOURCE_COMMIT",
            "original",
            &media,
            "local",
            Some("source"),
            false
        )
        .await
        .is_err()
    );
    assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
    {
        let conn = db.acquire_lock("test_source_checksum_rollback").unwrap();
        let paths: i64 = conn
            .query_row("SELECT COUNT(*) FROM asset_metadata_paths", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(paths, 0);
    }
    drop(db);
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    db.mark_verified_download(
        "PrimarySync",
        "SOURCE_COMMIT",
        "original",
        &media,
        "local",
        Some("source"),
        false,
    )
    .await
    .unwrap();
    db.record_metadata_write_failure("PrimarySync", "SOURCE_COMMIT", "original")
        .await
        .unwrap();
    let pending = db
        .get_pending_metadata_rewrites_page_for_queue(MetadataRewriteQueue::Ordinary, None, 0, 10)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].source_checksum.as_deref(), Some("source"));
}

#[tokio::test]
async fn master_family_soft_delete_marks_sibling_asset_state_rows() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let master = TestAssetRecord::new("master-a").build();
    let sibling = TestAssetRecord::new("asset-b").build();
    let unrelated = TestAssetRecord::new("asset-c").build();

    db.upsert_seen(&master).await.unwrap();
    db.upsert_seen(&sibling).await.unwrap();
    db.upsert_seen(&unrelated).await.unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-b", "master-a")
        .await
        .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-c", "master-other")
        .await
        .unwrap();

    let updated = db
        .mark_master_family_soft_deleted("PrimarySync", "master-a", None)
        .await
        .unwrap();

    assert_eq!(updated, 2);
    assert!(read_asset_writer_contract_row(&db, "master-a").is_deleted);
    assert!(read_asset_writer_contract_row(&db, "asset-b").is_deleted);
    assert!(!read_asset_writer_contract_row(&db, "asset-c").is_deleted);
}

#[derive(Debug)]
struct AssetWriterContractRow {
    status: String,
    downloaded_at: Option<i64>,
    local_path: Option<String>,
    local_checksum: Option<String>,
    download_checksum: Option<String>,
    last_error: Option<String>,
    is_deleted: bool,
    deleted_at: Option<i64>,
}

fn read_asset_writer_contract_row(db: &SqliteStateDb, asset_id: &str) -> AssetWriterContractRow {
    let conn = db.acquire_lock("read_asset_writer_contract_row").unwrap();
    conn.query_row(
        "SELECT status, downloaded_at, local_path, local_checksum, download_checksum, \
             last_error, is_deleted, deleted_at FROM assets \
             WHERE library = 'PrimarySync' AND id = ?1 AND version_size = 'original'",
        [asset_id],
        |row| {
            let is_deleted: i64 = row.get(6)?;
            Ok(AssetWriterContractRow {
                status: row.get(0)?,
                downloaded_at: row.get(1)?,
                local_path: row.get(2)?,
                local_checksum: row.get(3)?,
                download_checksum: row.get(4)?,
                last_error: row.get(5)?,
                is_deleted: is_deleted != 0,
                deleted_at: row.get(7)?,
            })
        },
    )
    .unwrap()
}

fn assert_downloaded_tombstone_row(
    db: &SqliteStateDb,
    asset_id: &str,
    path: &Path,
    deleted_at: DateTime<Utc>,
) {
    let row = read_asset_writer_contract_row(db, asset_id);
    assert_eq!(row.status, "downloaded");
    assert!(
        row.downloaded_at.is_some(),
        "download writer state must preserve downloaded_at"
    );
    assert_eq!(row.local_path, Some(path.to_string_lossy().into_owned()));
    assert_eq!(row.local_checksum.as_deref(), Some("local_hash"));
    assert_eq!(row.download_checksum.as_deref(), Some("download_hash"));
    assert_eq!(row.last_error, None);
    assert!(row.is_deleted);
    assert_eq!(row.deleted_at, Some(deleted_at.timestamp()));
}

/// The same (id, version_size) under two different libraries must
/// coexist as distinct rows; without per-zone PK scope, the second
/// `upsert_seen` would UPDATE the first row in place and silently
/// drop the other zone's separate-asset state.
#[tokio::test]
async fn upsert_seen_keeps_distinct_rows_per_library() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let primary = TestAssetRecord::new("ASSET_X")
        .library("PrimarySync")
        .checksum("ck_primary")
        .build();
    let shared = TestAssetRecord::new("ASSET_X")
        .library("SharedSync-A1B2C3D4")
        .checksum("ck_shared")
        .build();
    db.upsert_seen(&primary).await.unwrap();
    db.upsert_seen(&shared).await.unwrap();

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 2);

    // mark_downloaded for the primary row must not flip the shared row's status.
    let dir = test_dir();
    let path = dir.path().join("photo.jpg");
    std::fs::write(&path, b"x").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ASSET_X",
        "original",
        &path,
        "lck_primary",
        None,
    )
    .await
    .unwrap();
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 1);

    // get_downloaded_ids returns the (library, id, version) triple
    // so consumers can route per-library skip decisions correctly.
    let ids = db.get_downloaded_ids().await.unwrap();
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&(
        "PrimarySync".to_string(),
        "ASSET_X".to_string(),
        "original".to_string(),
    )));
}

#[tokio::test]
async fn concurrent_asset_writers_preserve_library_id_version_pk() {
    let dir = test_dir();
    let db_path = dir.path().join("concurrent-writers.db");
    let db = std::sync::Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let media_dir = dir.path().join("photos");
    std::fs::create_dir_all(&media_dir).unwrap();

    let cases = [
        (
            "PrimarySync",
            "ASSET_PK",
            VersionSizeKey::Original,
            "ck_orig",
        ),
        ("PrimarySync", "ASSET_PK", VersionSizeKey::Medium, "ck_med"),
        (
            "SharedSync-A1B2C3D4",
            "ASSET_PK",
            VersionSizeKey::Original,
            "ck_shared",
        ),
    ];
    let mut handles = Vec::new();
    for (library, id, version_size, checksum) in cases {
        let db = std::sync::Arc::clone(&db);
        let path = media_dir.join(format!("{}_{}_{}.jpg", library, id, version_size.as_str()));
        handles.push(tokio::spawn(async move {
            std::fs::write(&path, b"image-bytes").unwrap();
            let record = TestAssetRecord::new(id)
                .library(library)
                .version_size(version_size)
                .checksum(checksum)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                library,
                id,
                version_size.as_str(),
                &path,
                checksum,
                Some(checksum),
            )
            .await
            .unwrap();
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let conn = db.acquire_lock("verify concurrent writer rows").unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT library, id, version_size, status FROM assets \
                 WHERE id = 'ASSET_PK' ORDER BY library, version_size",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(
        rows,
        vec![
            (
                "PrimarySync".to_string(),
                "ASSET_PK".to_string(),
                "medium".to_string(),
                "downloaded".to_string()
            ),
            (
                "PrimarySync".to_string(),
                "ASSET_PK".to_string(),
                "original".to_string(),
                "downloaded".to_string()
            ),
            (
                "SharedSync-A1B2C3D4".to_string(),
                "ASSET_PK".to_string(),
                "original".to_string(),
                "downloaded".to_string()
            ),
        ],
        "concurrent writers must preserve every library/id/version row"
    );
}

/// `mark_failed` must scope to one zone; the other zone's row for
/// the same (id, version_size) keeps its prior status.
#[tokio::test]
async fn mark_failed_is_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for lib in ["PrimarySync", "SharedSync-AAAA"] {
        let r = TestAssetRecord::new("DUP")
            .library(lib)
            .checksum(&format!("ck_{lib}"))
            .build();
        db.upsert_seen(&r).await.unwrap();
    }
    db.mark_failed("PrimarySync", "DUP", "original", "boom")
        .await
        .unwrap();
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.failed, 1, "only PrimarySync row was marked failed");
    assert_eq!(summary.pending, 1, "SharedSync row stays pending");
}

#[tokio::test]
async fn test_should_download_not_in_db() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let result = db
        .should_download(
            "PrimarySync",
            "ABC123",
            "original",
            "checksum",
            Path::new("/tmp/file.jpg"),
        )
        .await
        .unwrap();
    assert!(result);
}

#[tokio::test]
async fn test_upsert_and_should_download_pending() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ABC123").build();

    db.upsert_seen(&record).await.unwrap();

    // Pending assets should be downloaded
    let result = db
        .should_download(
            "PrimarySync",
            "ABC123",
            "original",
            "checksum123",
            Path::new("/tmp/file.jpg"),
        )
        .await
        .unwrap();
    assert!(result);
}

#[tokio::test]
async fn test_mark_downloaded_then_should_not_download() {
    let dir = test_dir();
    let file_path = dir.path().join("photo.jpg");
    fs::write(&file_path, b"test content").unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ABC123").build();

    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ABC123",
        "original",
        &file_path,
        "abc123hash",
        None,
    )
    .await
    .unwrap();

    // Downloaded asset with existing file should not be downloaded
    let result = db
        .should_download(
            "PrimarySync",
            "ABC123",
            "original",
            "checksum123",
            &file_path,
        )
        .await
        .unwrap();
    assert!(!result);
}

#[tokio::test]
async fn test_should_download_file_missing() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ABC123").build();

    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ABC123",
        "original",
        Path::new("/nonexistent/file.jpg"),
        "abc123hash",
        None,
    )
    .await
    .unwrap();

    // Downloaded asset with missing file should be re-downloaded
    let result = db
        .should_download(
            "PrimarySync",
            "ABC123",
            "original",
            "checksum123",
            Path::new("/nonexistent/file.jpg"),
        )
        .await
        .unwrap();
    assert!(result);
}

#[tokio::test]
async fn test_should_download_checksum_changed() {
    let dir = test_dir();
    let file_path = dir.path().join("photo.jpg");
    fs::write(&file_path, b"test content").unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ABC123")
        .checksum("old_checksum")
        .build();

    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ABC123",
        "original",
        &file_path,
        "oldhash",
        None,
    )
    .await
    .unwrap();

    // Different checksum should trigger re-download
    let result = db
        .should_download(
            "PrimarySync",
            "ABC123",
            "original",
            "new_checksum",
            &file_path,
        )
        .await
        .unwrap();
    assert!(result);
}

#[tokio::test]
async fn should_download_empty_remote_checksum_does_not_skip_existing_file() {
    let dir = test_dir();
    let file_path = dir.path().join("photo.jpg");
    fs::write(&file_path, b"test content").unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = TestAssetRecord::new("ABC123").checksum("").build();

    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ABC123",
        "original",
        &file_path,
        "oldhash",
        None,
    )
    .await
    .unwrap();

    let result = db
        .should_download("PrimarySync", "ABC123", "original", "", &file_path)
        .await
        .unwrap();
    assert!(
        result,
        "empty remote checksum must not hard-skip a downloaded row"
    );
}

#[tokio::test]
async fn test_mark_failed_and_get_failed() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ABC123").build();

    db.upsert_seen(&record).await.unwrap();
    db.mark_failed("PrimarySync", "ABC123", "original", "Connection timeout")
        .await
        .unwrap();

    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(&*failed[0].id, "ABC123");
    assert_eq!(failed[0].last_error.as_deref(), Some("Connection timeout"));
    assert_eq!(failed[0].download_attempts, 1);
}

#[tokio::test]
async fn get_pending_orders_by_last_seen_desc() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    for id in &["OLD", "MID", "NEW"] {
        let record = TestAssetRecord::new(id)
            .checksum(&format!("ck_{id}"))
            .filename(&format!("{}.jpg", id.to_lowercase()))
            .size(100)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    db.backdate_last_seen("OLD", 1_000);
    db.backdate_last_seen("MID", 2_000);
    db.backdate_last_seen("NEW", 3_000);

    let pending = db.get_pending().await.unwrap();
    let ids: Vec<&str> = pending.iter().map(|r| &*r.id).collect();
    assert_eq!(
        ids,
        vec!["NEW", "MID", "OLD"],
        "get_pending must sort last_seen_at DESC"
    );
}

#[tokio::test]
async fn test_reset_failed() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ABC123").build();

    db.upsert_seen(&record).await.unwrap();
    db.mark_failed("PrimarySync", "ABC123", "original", "Error")
        .await
        .unwrap();

    let count = db.reset_failed().await.unwrap();
    assert_eq!(count, 1);

    let failed = db.get_failed().await.unwrap();
    assert!(failed.is_empty());
}

#[tokio::test]
async fn policy_excluded_asset_is_non_actionable_until_redispatched() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = TestAssetRecord::new("POLICY_EXCLUDED")
        .checksum("excluded-checksum")
        .filename("old.mov")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.set_asset_verification(
        "PrimarySync",
        "POLICY_EXCLUDED",
        "original",
        AssetVerificationState::Unknown,
        "before policy decision",
    )
    .await
    .unwrap();

    assert!(
        db.mark_policy_excluded("PrimarySync", "POLICY_EXCLUDED", "original")
            .await
            .unwrap()
    );
    assert!(
        !db.mark_policy_excluded("PrimarySync", "POLICY_EXCLUDED", "original")
            .await
            .unwrap()
    );
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.policy_excluded, 1);
    assert_eq!(summary.awaiting_provider_verification, 0);
    assert!(db.get_pending().await.unwrap().is_empty());

    db.upsert_seen(&record).await.unwrap();
    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].status, AssetStatus::Pending);
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.policy_excluded, 0);
}

#[tokio::test]
async fn policy_excluded_revalidation_reader_is_live_and_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (id, library) in [
        ("LIVE_PRIMARY", "PrimarySync"),
        ("DELETED_PRIMARY", "PrimarySync"),
        ("LIVE_SHARED", "SharedSync"),
    ] {
        let record = TestAssetRecord::new(id).library(library).build();
        db.upsert_seen(&record).await.unwrap();
        assert!(
            db.mark_policy_excluded(library, id, "original")
                .await
                .unwrap()
        );
    }
    db.resolve_source_deleted("PrimarySync", "DELETED_PRIMARY", None)
        .await
        .unwrap();

    let ids = db
        .get_policy_excluded_ids_for_revalidation("PrimarySync")
        .await
        .unwrap();

    assert_eq!(ids, vec!["LIVE_PRIMARY"]);
}

#[tokio::test]
async fn source_deletion_supersedes_policy_exclusion() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = TestAssetRecord::new("EXCLUDED_DELETED")
        .checksum("excluded-deleted-checksum")
        .filename("deleted.mov")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    assert!(
        db.mark_policy_excluded("PrimarySync", "EXCLUDED_DELETED", "original")
            .await
            .unwrap()
    );

    assert_eq!(
        db.resolve_source_deleted("PrimarySync", "EXCLUDED_DELETED", None)
            .await
            .unwrap(),
        1
    );
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.policy_excluded, 0);
    assert_eq!(summary.source_deleted, 1);
}

#[tokio::test]
async fn test_upsert_preserves_status() {
    let dir = test_dir();
    let file_path = dir.path().join("photo.jpg");
    fs::write(&file_path, b"test content").unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ABC123").build();

    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ABC123",
        "original",
        &file_path,
        "abc123hash",
        None,
    )
    .await
    .unwrap();

    // Upsert again - should preserve downloaded status
    db.upsert_seen(&record).await.unwrap();

    // Should still be downloaded (file exists)
    let result = db
        .should_download(
            "PrimarySync",
            "ABC123",
            "original",
            "checksum123",
            &file_path,
        )
        .await
        .unwrap();
    assert!(!result);
}

// ── Batch operation tests ──

#[tokio::test]
async fn test_get_downloaded_ids() {
    let dir = test_dir();
    let db = SqliteStateDb::open_in_memory().unwrap();

    // Create some assets with different statuses
    for i in 0..3 {
        let record = TestAssetRecord::new(&format!("DL_{}", i))
            .checksum(&format!("checksum_{}", i))
            .filename(&format!("photo_{}.jpg", i))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join(format!("photo_{}.jpg", i));
        fs::write(&path, b"content").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &format!("DL_{}", i),
            "original",
            &path,
            "hash",
            None,
        )
        .await
        .unwrap();
    }

    // Add a pending asset (should not be in downloaded IDs)
    let pending = TestAssetRecord::new("PENDING_1")
        .checksum("pending_ck")
        .filename("pending.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&pending).await.unwrap();

    let ids = db.get_downloaded_ids().await.unwrap();
    assert_eq!(ids.len(), 3);
    assert!(ids.contains(&(
        "PrimarySync".to_string(),
        "DL_0".to_string(),
        "original".to_string()
    )));
    assert!(ids.contains(&(
        "PrimarySync".to_string(),
        "DL_1".to_string(),
        "original".to_string()
    )));
    assert!(ids.contains(&(
        "PrimarySync".to_string(),
        "DL_2".to_string(),
        "original".to_string()
    )));
    assert!(!ids.contains(&(
        "PrimarySync".to_string(),
        "PENDING_1".to_string(),
        "original".to_string()
    )));
}

#[tokio::test]
async fn downloaded_file_records_return_compact_skip_evidence() {
    let dir = test_dir();
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = TestAssetRecord::new("DOWNLOADED")
        .checksum("provider-checksum")
        .filename("downloaded.jpg")
        .build();
    db.upsert_seen(&record).await.unwrap();
    let path = dir.path().join("downloaded.jpg");
    db.mark_downloaded(
        "PrimarySync",
        "DOWNLOADED",
        "original",
        &path,
        "local-checksum",
        Some("download-checksum"),
    )
    .await
    .unwrap();
    db.upsert_seen(&TestAssetRecord::new("PENDING").build())
        .await
        .unwrap();

    let records = db.get_downloaded_file_records().await.unwrap();

    assert_eq!(records.len(), 1);
    let downloaded = &records[0];
    assert_eq!(downloaded.library, "PrimarySync");
    assert_eq!(downloaded.id, "DOWNLOADED");
    assert_eq!(downloaded.version_size, VersionSizeKey::Original);
    assert_eq!(downloaded.checksum, "provider-checksum");
    assert_eq!(downloaded.local_path.as_deref(), Some(path.as_path()));
    assert_eq!(downloaded.local_checksum.as_deref(), Some("local-checksum"));
    assert_eq!(
        downloaded.download_checksum.as_deref(),
        Some("download-checksum")
    );
}

#[tokio::test]
async fn test_get_downloaded_checksums() {
    let dir = test_dir();
    let db = SqliteStateDb::open_in_memory().unwrap();

    for i in 0..2 {
        let record = TestAssetRecord::new(&format!("DL_{}", i))
            .checksum(&format!("checksum_{}", i))
            .filename(&format!("photo_{}.jpg", i))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join(format!("photo_{}.jpg", i));
        fs::write(&path, b"content").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &format!("DL_{}", i),
            "original",
            &path,
            "hash",
            None,
        )
        .await
        .unwrap();
    }

    let checksums = db.get_downloaded_checksums().await.unwrap();
    assert_eq!(checksums.len(), 2);
    assert_eq!(
        checksums.get(&(
            "PrimarySync".to_string(),
            "DL_0".to_string(),
            "original".to_string()
        )),
        Some(&"checksum_0".to_string())
    );
    assert_eq!(
        checksums.get(&(
            "PrimarySync".to_string(),
            "DL_1".to_string(),
            "original".to_string()
        )),
        Some(&"checksum_1".to_string())
    );
}

#[tokio::test]
async fn test_get_all_known_ids() {
    let dir = test_dir();
    let db = SqliteStateDb::open_in_memory().unwrap();

    // Create downloaded assets
    for i in 0..2 {
        let record = TestAssetRecord::new(&format!("DL_{}", i))
            .checksum(&format!("checksum_{}", i))
            .filename(&format!("photo_{}.jpg", i))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join(format!("photo_{}.jpg", i));
        fs::write(&path, b"content").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &format!("DL_{}", i),
            "original",
            &path,
            "hash",
            None,
        )
        .await
        .unwrap();
    }

    // Create a pending asset
    let pending = TestAssetRecord::new("PENDING_1")
        .checksum("pending_ck")
        .filename("pending.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&pending).await.unwrap();

    // Create a failed asset
    let failed = TestAssetRecord::new("FAILED_1")
        .checksum("failed_ck")
        .filename("failed.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&failed).await.unwrap();
    db.mark_failed("PrimarySync", "FAILED_1", "original", "test error")
        .await
        .unwrap();
    let shared_same_id = TestAssetRecord::new("FAILED_1")
        .library("SharedSync-AAAA")
        .checksum("shared_failed_ck")
        .filename("shared_failed.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&shared_same_id).await.unwrap();

    let known_ids = db.get_all_known_ids().await.unwrap();
    // Should include all assets regardless of status, scoped by library.
    assert_eq!(known_ids.len(), 5);
    assert!(known_ids.contains(&("PrimarySync".to_string(), "DL_0".to_string())));
    assert!(known_ids.contains(&("PrimarySync".to_string(), "DL_1".to_string())));
    assert!(known_ids.contains(&("PrimarySync".to_string(), "PENDING_1".to_string())));
    assert!(known_ids.contains(&("PrimarySync".to_string(), "FAILED_1".to_string())));
    assert!(known_ids.contains(&("SharedSync-AAAA".to_string(), "FAILED_1".to_string())));

    // get_downloaded_ids should only return 2
    let downloaded_ids = db.get_downloaded_ids().await.unwrap();
    assert_eq!(downloaded_ids.len(), 2);
}

#[tokio::test]
async fn test_retry_failed_returns_zero_when_no_failures() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // With no assets at all, reset_failed returns 0
    let count = db.reset_failed().await.unwrap();
    assert_eq!(count, 0);

    // Add a downloaded asset — still no failures
    let record = TestAssetRecord::new("DL_1")
        .checksum("ck")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    let dir = test_dir();
    let path = dir.path().join("photo.jpg");
    fs::write(&path, b"content").unwrap();
    db.mark_downloaded("PrimarySync", "DL_1", "original", &path, "hash", None)
        .await
        .unwrap();

    let count = db.reset_failed().await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_retry_failed_resets_only_failed() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();

    // Add a downloaded asset
    let dl = TestAssetRecord::new("DL_1")
        .checksum("ck1")
        .filename("photo1.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&dl).await.unwrap();
    let path = dir.path().join("photo1.jpg");
    fs::write(&path, b"content").unwrap();
    db.mark_downloaded("PrimarySync", "DL_1", "original", &path, "hash", None)
        .await
        .unwrap();

    // Add a failed asset
    let failed = TestAssetRecord::new("FAIL_1")
        .checksum("ck2")
        .filename("photo2.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&failed).await.unwrap();
    db.mark_failed("PrimarySync", "FAIL_1", "original", "download error")
        .await
        .unwrap();

    // reset_failed should reset exactly 1
    let count = db.reset_failed().await.unwrap();
    assert_eq!(count, 1);

    // After reset, the failed asset should be in known_ids but not downloaded_ids
    let known = db.get_all_known_ids().await.unwrap();
    assert_eq!(known.len(), 2);
    assert!(known.contains(&("PrimarySync".to_string(), "DL_1".to_string())));
    assert!(known.contains(&("PrimarySync".to_string(), "FAIL_1".to_string())));

    let downloaded = db.get_downloaded_ids().await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert!(downloaded.contains(&(
        "PrimarySync".to_string(),
        "DL_1".to_string(),
        "original".to_string()
    )));
}

#[tokio::test]
async fn test_touch_last_seen() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("TOUCH_1")
        .checksum("ck")
        .created_at(Utc::now() - chrono::Duration::hours(1))
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();

    // Backdate last_seen_at so that touch_last_seen produces a strictly greater timestamp
    {
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "UPDATE assets SET last_seen_at = last_seen_at - 5 WHERE id = 'TOUCH_1'",
            [],
        )
        .unwrap();
    }

    let original_ts: i64 = {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT last_seen_at FROM assets WHERE id = 'TOUCH_1'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };

    // Touch last_seen_at — should set it to now(), which is > backdated value
    db.touch_last_seen_many("PrimarySync", &["TOUCH_1"])
        .await
        .unwrap();

    let updated_ts: i64 = {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT last_seen_at FROM assets WHERE id = 'TOUCH_1'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert!(
        updated_ts > original_ts,
        "last_seen_at should be updated: {updated_ts} > {original_ts}"
    );
}

// touch_last_seen_many must bump every id in one transaction and
// be a no-op for an empty slice (the producer feeds an empty set
// on libraries that don't skip anything).
#[tokio::test]
async fn touch_last_seen_many_bumps_every_id_in_one_batch() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for i in 0..5 {
        let id = format!("BATCH_{i}");
        let ck = format!("ck{i}");
        let fname = format!("f{i}.jpg");
        let rec = TestAssetRecord::new(&id)
            .checksum(&ck)
            .filename(&fname)
            .size(10)
            .build();
        db.upsert_seen(&rec).await.unwrap();
        db.backdate_last_seen(&id, 100);
    }

    let ids: Vec<&str> = (0..5).map(|_| "").collect();
    // Build the slice after constructing owned strings to keep them alive.
    let id_strings: Vec<String> = (0..5).map(|i| format!("BATCH_{i}")).collect();
    let id_refs: Vec<&str> = id_strings.iter().map(String::as_str).collect();
    // (ids above is just to document the slice shape.)
    let _ = ids;

    db.touch_last_seen_many("PrimarySync", &id_refs)
        .await
        .unwrap();

    let conn = db.conn.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT last_seen_at FROM assets WHERE id = ?1")
        .unwrap();
    for id in &id_refs {
        let ts: i64 = stmt.query_row([*id], |r| r.get(0)).unwrap();
        assert!(ts > 100, "row {id} must be bumped past the backdated 100");
    }
}

#[tokio::test]
async fn touch_last_seen_many_empty_slice_is_noop() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    // No rows; no assertion about state needed — just verify Ok(()).
    db.touch_last_seen_many("PrimarySync", &[]).await.unwrap();
}

// ── Gap tests: robustness and edge cases ──

#[tokio::test]
async fn should_download_unknown_version_size_treated_as_pending() {
    // Arrange: insert a row with a version_size string that doesn't map to any VersionSizeKey variant
    let db = SqliteStateDb::open_in_memory().unwrap();
    {
        let conn = db.conn.lock().unwrap();
        let now = Utc::now().timestamp();
        conn.execute(
            "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at)
                 VALUES ('PrimarySync', 'AQvz7R8kP4', 'superHD', 'a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6abcd', 'IMG_4231.HEIC', ?1, 8294400, 'photo', 'pending', ?1)",
            rusqlite::params![now],
        ).unwrap();
    }

    // Act: query should_download with the same unknown version_size
    let result = db
        .should_download(
            "PrimarySync",
            "AQvz7R8kP4",
            "superHD",
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6abcd",
            Path::new("/photos/2026/04/IMG_4231.HEIC"),
        )
        .await
        .unwrap();

    // Assert: pending asset should need download
    assert!(result);
}

/// T-3: Each download is reflected in the state DB immediately, not batched.
/// After marking each of 5 files as downloaded, the summary should reflect
/// the cumulative count at every step.
#[tokio::test]
async fn test_downloads_reflected_immediately_not_batched() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();

    for i in 0..5u32 {
        let id = format!("ASSET_{i}");
        let record = TestAssetRecord::new(&id)
            .checksum(&format!("checksum_{i}"))
            .filename(&format!("photo_{i}.jpg"))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();

        let path = dir.path().join(format!("photo_{i}.jpg"));
        fs::write(&path, b"jpeg data").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &id,
            "original",
            &path,
            &format!("local_ck_{i}"),
            None,
        )
        .await
        .unwrap();

        // Query immediately after each download
        let summary = db.get_summary().await.unwrap();
        assert_eq!(
            summary.downloaded,
            u64::from(i + 1),
            "after downloading asset {i}, DB should show {} downloaded",
            i + 1
        );
    }

    // Final check: all 5 are downloaded
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 5);
    assert_eq!(summary.downloaded, 5);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn reset_failed_precise_count_with_mixed_statuses() {
    // Arrange: create assets across all three statuses with multiple failed entries
    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();

    // 2 downloaded
    for i in 0..2 {
        let id = format!("ADl{}mNp3Q{}", i, i);
        let record = TestAssetRecord::new(&id)
            .checksum(&format!(
                "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48b{}",
                i
            ))
            .filename(&format!("IMG_{}.HEIC", 2000 + i))
            .size(5_242_880)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join(format!("IMG_{}.HEIC", 2000 + i));
        fs::write(&path, b"heic payload").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &id,
            "original",
            &path,
            &format!("localhash{i}"),
            None,
        )
        .await
        .unwrap();
    }

    // 3 pending (just upserted, never transitioned)
    for i in 0..3 {
        let record = TestAssetRecord::new(&format!("APn{}rWx5Z{}", i, i))
            .checksum(&format!(
                "3e23e8160039594a33894f6564e1b1348bbd7a0088d42c4acb73eeaed59c009{}",
                i
            ))
            .filename(&format!("IMG_{}.JPG", 3000 + i))
            .size(3_145_728)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    // 4 failed
    for i in 0..4 {
        let id = format!("AFl{}kRt7Y{}", i, i);
        let record = TestAssetRecord::new(&id)
            .checksum(&format!(
                "d4735e3a265e16eee03f59718b9b5d03019c07d8b6c51f90da3a666eec13ab3{}",
                i
            ))
            .filename(&format!("IMG_{}.MOV", 4000 + i))
            .size(10_485_760)
            .media_type(MediaType::Video)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed(
            "PrimarySync",
            &id,
            "original",
            &format!("HTTP 500 attempt {i}"),
        )
        .await
        .unwrap();
    }

    // Pre-check
    let before = db.get_summary().await.unwrap();
    assert_eq!(before.total_assets, 9);
    assert_eq!(before.downloaded, 2);
    assert_eq!(before.pending, 3);
    assert_eq!(before.failed, 4);

    // Act
    let reset_count = db.reset_failed().await.unwrap();

    // Assert: exactly 4 were reset
    assert_eq!(reset_count, 4);

    let after = db.get_summary().await.unwrap();
    assert_eq!(after.total_assets, 9);
    assert_eq!(after.downloaded, 2);
    assert_eq!(after.pending, 7); // 3 original pending + 4 reset from failed
    assert_eq!(after.failed, 0);

    // Verify the formerly-failed assets have cleared error and zero attempts
    let failed_after = db.get_failed().await.unwrap();
    assert!(failed_after.is_empty());
}

#[tokio::test]
async fn prepare_for_retry_resets_failed_and_stuck_pending() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();

    // 1 downloaded (should be untouched)
    let record = TestAssetRecord::new("ADwnloaded1")
        .checksum("aaaa")
        .filename("IMG_1000.HEIC")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    let path = dir.path().join("IMG_1000.HEIC");
    fs::write(&path, b"payload").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ADwnloaded1",
        "original",
        &path,
        "localhash1",
        None,
    )
    .await
    .unwrap();

    // 1 normal pending (attempts = 0, should be untouched)
    let record = TestAssetRecord::new("APending1")
        .checksum("bbbb")
        .filename("IMG_2000.JPG")
        .size(2000)
        .build();
    db.upsert_seen(&record).await.unwrap();

    // 1 stuck pending (attempts > 0, should get attempts cleared)
    let record = TestAssetRecord::new("AStuck1")
        .checksum("cccc")
        .filename("IMG_3000.JPG")
        .size(3000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    // Simulate accumulated attempts by marking failed then resetting status to pending
    // but keeping attempts high (as the old bug would produce)
    db.mark_failed("PrimarySync", "AStuck1", "original", "transient error")
        .await
        .unwrap();
    // Manually set back to pending with attempts preserved (simulating the old bug)
    db.conn
        .lock()
        .unwrap()
        .execute(
            "UPDATE assets SET status = 'pending' WHERE id = 'AStuck1'",
            [],
        )
        .unwrap();

    // 2 failed (should transition to pending)
    for i in 0..2 {
        let id = format!("AFailed{i}");
        let record = TestAssetRecord::new(&id)
            .checksum(&format!("dddd{i}"))
            .filename(&format!("IMG_400{i}.MOV"))
            .size(5000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", &id, "original", "HTTP 500")
            .await
            .unwrap();
    }

    let before = db.get_summary().await.unwrap();
    assert_eq!(before.downloaded, 1);
    assert_eq!(before.pending, 2); // normal + stuck
    assert_eq!(before.failed, 2);

    let (failed_reset, pending_reset, total_pending) = db
        .prepare_for_retry(None, RetryErrorRetention::Clear)
        .await
        .unwrap();

    assert_eq!(failed_reset, 2);
    assert_eq!(pending_reset, 1); // only the stuck one
    assert_eq!(total_pending, 4); // 2 original pending + 2 reset from failed

    let after = db.get_summary().await.unwrap();
    assert_eq!(after.downloaded, 1);
    assert_eq!(after.pending, 4);
    assert_eq!(after.failed, 0);

    // Verify attempt counts are all zero now
    let attempts = db.get_attempt_counts().await.unwrap();
    assert!(attempts.is_empty(), "all attempt counts should be zero");
}

#[tokio::test]
async fn prepare_for_retry_preserves_only_the_authorized_error() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (id, error) in [
        (
            "truncated",
            crate::commands::reconcile::FILE_TRUNCATED_REASON,
        ),
        ("network", "HTTP 500"),
    ] {
        let record = TestAssetRecord::new(id)
            .checksum(&format!("checksum-{id}"))
            .filename(&format!("{id}.jpg"))
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", id, "original", error)
            .await
            .unwrap();
    }

    db.prepare_for_retry(
        None,
        RetryErrorRetention::Preserve(crate::commands::reconcile::FILE_TRUNCATED_REASON),
    )
    .await
    .unwrap();

    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 2);
    let truncated = pending
        .iter()
        .find(|row| row.id.as_ref() == "truncated")
        .unwrap();
    let network = pending
        .iter()
        .find(|row| row.id.as_ref() == "network")
        .unwrap();
    assert_eq!(
        truncated.last_error.as_deref(),
        Some(crate::commands::reconcile::FILE_TRUNCATED_REASON)
    );
    assert!(network.last_error.is_none());
}

#[tokio::test]
async fn prepare_for_retry_can_scope_pending_work_by_library() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let shared = "SharedSync-ONE";

    for (library, suffix) in [("PrimarySync", "primary"), (shared, "shared")] {
        let pending_id = format!("APending-{suffix}");
        let pending = TestAssetRecord::new(&pending_id)
            .library(library)
            .checksum(&format!("pending-{suffix}"))
            .filename(&format!("pending-{suffix}.jpg"))
            .build();
        db.upsert_seen(&pending).await.unwrap();
        db.mark_failed(library, &pending_id, "original", "transient")
            .await
            .unwrap();
        db.conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE assets SET status = 'pending' WHERE library = ?1 AND id = ?2",
                rusqlite::params![library, pending_id],
            )
            .unwrap();

        let failed_id = format!("AFailed-{suffix}");
        let failed = TestAssetRecord::new(&failed_id)
            .library(library)
            .checksum(&format!("failed-{suffix}"))
            .filename(&format!("failed-{suffix}.jpg"))
            .build();
        db.upsert_seen(&failed).await.unwrap();
        db.mark_failed(library, &failed_id, "original", "HTTP 500")
            .await
            .unwrap();
    }

    let (failed_reset, pending_reset, total_pending) = db
        .prepare_for_retry(Some("PrimarySync"), RetryErrorRetention::Clear)
        .await
        .unwrap();

    assert_eq!(failed_reset, 1);
    assert_eq!(pending_reset, 1);
    assert_eq!(
        total_pending, 2,
        "only PrimarySync pending rows should drive the retry fallback gate"
    );

    let (shared_failed_reset, shared_pending_reset, shared_total_pending) = db
        .prepare_for_retry(Some(shared), RetryErrorRetention::Clear)
        .await
        .unwrap();
    assert_eq!(shared_failed_reset, 1);
    assert_eq!(shared_pending_reset, 1);
    assert_eq!(
        shared_total_pending, 2,
        "SharedSync retry work should remain untouched until its own library pass"
    );
}

#[tokio::test]
async fn promote_pending_to_failed_only_affects_pending() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();

    // 1 downloaded (should be untouched)
    let record = TestAssetRecord::new("ADownloaded")
        .checksum("aaaa")
        .filename("IMG_1000.HEIC")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    let path = dir.path().join("IMG_1000.HEIC");
    fs::write(&path, b"payload").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "ADownloaded",
        "original",
        &path,
        "localhash",
        None,
    )
    .await
    .unwrap();

    // 2 pending dispatched this sync (should be promoted to failed)
    for i in 0..2 {
        let id = format!("APending{i}");
        let record = TestAssetRecord::new(&id)
            .checksum(&format!("bbbb{i}"))
            .filename(&format!("IMG_200{i}.JPG"))
            .size(2000)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    // 1 already failed (should be untouched)
    let record = TestAssetRecord::new("AFailed")
        .checksum("cccc")
        .filename("IMG_3000.MOV")
        .size(3000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_failed("PrimarySync", "AFailed", "original", "HTTP 500")
        .await
        .unwrap();

    let before = db.get_summary().await.unwrap();
    assert_eq!(before.downloaded, 1);
    assert_eq!(before.pending, 2);
    assert_eq!(before.failed, 1);

    // Gate is `last_seen_at >= seen_since`. Use a timestamp in the past
    // so every pending asset seen in this test counts as "dispatched
    // this sync" and gets promoted.
    let past = chrono::Utc::now().timestamp() - 3600;
    let promoted = db.promote_pending_to_failed(past).await.unwrap();
    assert_eq!(promoted, 2);

    let after = db.get_summary().await.unwrap();
    assert_eq!(after.downloaded, 1);
    assert_eq!(after.pending, 0);
    assert_eq!(after.failed, 3);

    // Verify the promoted assets have the right error message
    let failed = db.get_failed().await.unwrap();
    let promoted_errors: Vec<_> = failed
        .iter()
        .filter(|a| a.id.starts_with("APending"))
        .map(|a| a.last_error.as_deref())
        .collect();
    assert_eq!(promoted_errors.len(), 2);
    for error in &promoted_errors {
        assert_eq!(*error, Some("Not resolved during sync"));
    }
}

#[tokio::test]
async fn provider_verification_marker_keeps_inconclusive_row_pending() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("VERIFY_UNKNOWN").build();
    db.upsert_seen(&record).await.unwrap();
    db.set_asset_verification(
        "PrimarySync",
        "VERIFY_UNKNOWN",
        "original",
        AssetVerificationState::Unknown,
        "lookup omitted record",
    )
    .await
    .unwrap();

    let promoted = db.promote_pending_to_failed(0).await.unwrap();
    assert_eq!(promoted, 0);
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.awaiting_provider_verification, 1);

    db.clear_asset_verification("PrimarySync", "VERIFY_UNKNOWN", "original")
        .await
        .unwrap();
    assert_eq!(db.promote_pending_to_failed(0).await.unwrap(), 1);
}

#[tokio::test]
async fn concurrent_mark_downloaded_all_succeed() {
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());

    // Insert 10 pending assets
    for i in 0..10 {
        let record = TestAssetRecord::new(&format!("CONCURRENT_{i}"))
            .checksum(&format!("ck_{i}"))
            .filename(&format!("photo_{i}.jpg"))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    // Spawn 10 tasks that each mark a different asset as downloaded
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let db = Arc::clone(&db);
            tokio::spawn(async move {
                db.mark_downloaded(
                    "PrimarySync",
                    &format!("CONCURRENT_{i}"),
                    "original",
                    Path::new(&format!("/tmp/photo_{i}.jpg")),
                    &format!("hash_{i}"),
                    None,
                )
                .await
            })
        })
        .collect();

    // All tasks should succeed without SQLite busy errors
    for handle in handles {
        handle.await.unwrap().unwrap();
    }

    // Verify all 10 assets are downloaded
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 10);
    assert_eq!(summary.pending, 0);
}

#[tokio::test]
async fn test_get_attempt_counts() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    for (library, id) in [
        ("PrimarySync", "A"),
        ("PrimarySync", "B"),
        ("SharedSync-AAAA", "A"),
    ] {
        let record = TestAssetRecord::new(id)
            .library(library)
            .checksum(&format!("ck_{id}"))
            .filename(&format!("{id}.jpg"))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    db.mark_failed("PrimarySync", "A", "original", "error 1")
        .await
        .unwrap();
    db.mark_failed("PrimarySync", "A", "original", "error 2")
        .await
        .unwrap();
    db.mark_failed("PrimarySync", "A", "original", "error 3")
        .await
        .unwrap();
    db.mark_failed("PrimarySync", "B", "original", "error 1")
        .await
        .unwrap();
    db.mark_failed("SharedSync-AAAA", "A", "original", "shared error 1")
        .await
        .unwrap();

    let counts = db.get_attempt_counts().await.unwrap();
    assert_eq!(
        counts.get(&("PrimarySync".to_string(), "A".to_string())),
        Some(&3)
    );
    assert_eq!(
        counts.get(&("PrimarySync".to_string(), "B".to_string())),
        Some(&1)
    );
    assert_eq!(
        counts.get(&("SharedSync-AAAA".to_string(), "A".to_string())),
        Some(&1)
    );
}

#[tokio::test]
async fn test_get_attempt_counts_empty() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let counts = db.get_attempt_counts().await.unwrap();
    assert!(counts.is_empty());
}

// ── Gap: mark_downloaded on non-existent record (no upsert_seen) ──

#[tokio::test]
async fn mark_downloaded_without_upsert_seen_returns_asset_row_missing() {
    // The UPDATE matches zero rows when the asset wasn't recorded
    // via upsert_seen. The caller must see this loudly so a missed
    // dispatch step doesn't silently drop a downloaded file.
    let db = SqliteStateDb::open_in_memory().unwrap();

    let err = db
        .mark_downloaded(
            "PrimarySync",
            "NEVER_SEEN",
            "original",
            Path::new("/tmp/never.jpg"),
            "abc123",
            None,
        )
        .await
        .expect_err("mark_downloaded on unknown asset must err");
    match err {
        StateError::AssetRowMissing {
            asset_id,
            version_size,
        } => {
            assert_eq!(asset_id, "NEVER_SEEN");
            assert_eq!(version_size, "original");
        }
        other => panic!("expected AssetRowMissing, got {other:?}"),
    }
}

/// A regression that increases the rate of zero-row `mark_downloaded`
/// calls (e.g. a producer-dispatch invariant quietly broken) needs to
/// be visible in /metrics, not only in logs / per-asset errors. Pin
/// the counter increment so the wiring can't be silently dropped on
/// a future refactor.
#[tokio::test]
async fn mark_downloaded_zero_rows_increments_metric_counter() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let before = crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.get();
    let _ = db
        .mark_downloaded(
            "PrimarySync",
            "NEVER_SEEN_FOR_METRIC",
            "original",
            Path::new("/tmp/never_metric.jpg"),
            "abc123",
            None,
        )
        .await;
    let after = crate::metrics::MARK_DOWNLOADED_ZERO_ROWS.get();

    assert!(
        after > before,
        "counter should advance by at least 1 (other parallel tests may also \
             increment); got before={before} after={after}"
    );
}

/// A `mark_failed` call without a prior `upsert_seen` is a
/// producer-dispatch invariant violation. Surface it as a typed
/// `StateError::Invariant` so callers can't silently treat the
/// failure as persisted, while still incrementing the metric for
/// observability.
#[tokio::test]
async fn mark_failed_zero_rows_returns_invariant_and_increments_metric() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let before = crate::metrics::MARK_FAILED_ZERO_ROWS.get();
    let err = db
        .mark_failed(
            "PrimarySync",
            "NEVER_SEEN_FOR_FAILED_METRIC",
            "original",
            "simulated transient error",
        )
        .await
        .expect_err("mark_failed on unknown row must surface as Invariant");
    match &err {
        StateError::Invariant { operation, detail } => {
            assert_eq!(*operation, "mark_failed");
            assert!(
                detail.contains("NEVER_SEEN_FOR_FAILED_METRIC"),
                "detail must include the asset id; got: {detail}"
            );
        }
        other => panic!("expected StateError::Invariant, got {other:?}"),
    }
    let after = crate::metrics::MARK_FAILED_ZERO_ROWS.get();

    assert!(
        after > before,
        "MARK_FAILED_ZERO_ROWS must advance by at least 1 (parallel tests \
             may also increment); got before={before} after={after}"
    );

    // The asset must NOT have been inserted as a side effect.
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.total_assets, 0);
}

// ── Gap: mark_failed increments download_attempts cumulatively ────

#[tokio::test]
async fn mark_failed_increments_attempts_cumulatively() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("RETRY_ME")
        .checksum("ck_retry")
        .filename("photo.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();

    // Fail three times
    for i in 1..=3 {
        db.mark_failed("PrimarySync", "RETRY_ME", "original", &format!("error {i}"))
            .await
            .unwrap();
    }

    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(
        failed[0].download_attempts, 3,
        "download_attempts should be 3 after three failures"
    );
    assert_eq!(
        failed[0].last_error.as_deref(),
        Some("error 3"),
        "last_error should be the most recent failure"
    );
}

// Promote: only the asset the producer touched this sync is a candidate.
// Anything with a stale last_seen_at is filtered / out of scope and must
// stay pending. See issue #211.

#[tokio::test]
async fn promote_pending_to_failed_skips_stale_last_seen() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // OLD_ASSET: upserted before this sync (last_seen_at 1 hour ago).
    // Stands in for filtered-out / out-of-scope / remotely deleted assets
    // whose last_seen_at didn't get refreshed this sync.
    let old_record = TestAssetRecord::new("OLD_ASSET")
        .checksum("ck_old")
        .filename("old.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&old_record).await.unwrap();
    db.backdate_last_seen("OLD_ASSET", chrono::Utc::now().timestamp() - 3600);

    // NEW_ASSET: producer called upsert_seen this sync, consumer never
    // finalized. This is the stuck-pipeline case the function exists
    // to catch.
    let new_record = TestAssetRecord::new("NEW_ASSET")
        .checksum("ck_new")
        .filename("new.jpg")
        .size(2000)
        .build();
    db.upsert_seen(&new_record).await.unwrap();

    // sync_started_at: 30 minutes ago. OLD_ASSET (1h ago) is before the
    // boundary and must be left alone. NEW_ASSET (now) is after and
    // must be promoted.
    let sync_started_at = chrono::Utc::now().timestamp() - 1800;
    let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();

    let summary = db.get_summary().await.unwrap();
    assert_eq!(
        promoted, 1,
        "only NEW_ASSET (dispatched this sync) should be promoted"
    );
    assert_eq!(summary.pending, 1, "OLD_ASSET should remain pending");
    assert_eq!(summary.failed, 1, "NEW_ASSET should be failed");

    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(&*failed[0].id, "NEW_ASSET");
}

#[tokio::test]
async fn prune_stale_pending_not_seen_since_deletes_only_old_pending_for_library() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let old_primary = TestAssetRecord::new("OLD_PRIMARY")
        .checksum("ck_old_primary")
        .filename("old-primary.jpg")
        .size(1000)
        .build();
    let fresh_primary = TestAssetRecord::new("FRESH_PRIMARY")
        .checksum("ck_fresh_primary")
        .filename("fresh-primary.jpg")
        .size(1000)
        .build();
    let old_shared = TestAssetRecord::new("OLD_SHARED")
        .library("SharedSync")
        .checksum("ck_old_shared")
        .filename("old-shared.jpg")
        .size(1000)
        .build();
    let downloaded = TestAssetRecord::new("DOWNLOADED_PRIMARY")
        .checksum("ck_downloaded_primary")
        .filename("downloaded-primary.jpg")
        .size(1000)
        .build();

    db.upsert_seen(&old_primary).await.unwrap();
    db.upsert_seen(&fresh_primary).await.unwrap();
    db.upsert_seen(&old_shared).await.unwrap();
    db.upsert_seen(&downloaded).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DOWNLOADED_PRIMARY",
        "original",
        Path::new("/tmp/downloaded-primary.jpg"),
        "local",
        Some("download"),
    )
    .await
    .unwrap();

    let sync_started_at = chrono::Utc::now().timestamp() - 1800;
    db.backdate_last_seen("OLD_PRIMARY", sync_started_at - 10);
    db.backdate_last_seen("OLD_SHARED", sync_started_at - 10);

    let pruned = db
        .prune_stale_pending_not_seen_since("PrimarySync", sync_started_at)
        .await
        .unwrap();

    assert_eq!(pruned, 1);
    let pending = db.get_pending().await.unwrap();
    let ids: HashSet<&str> = pending.iter().map(|row| row.id.as_ref()).collect();
    assert!(!ids.contains("OLD_PRIMARY"));
    assert!(ids.contains("FRESH_PRIMARY"));
    assert!(ids.contains("OLD_SHARED"));
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 1, "downloaded rows must not change");
}

// Regression test for #211: a pending asset the producer didn't enumerate
// this sync (because a filter excluded it, the album scope changed, or
// the upstream record was deleted) must not be promoted to failed.
// Previously, prepare_for_retry + unseen + promote would loop this asset
// between pending and failed on every sync.

#[tokio::test]
async fn promote_pending_to_failed_does_not_loop_filtered_asset() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // Sync 1: asset enumerated, upsert_seen, then never got finalized
    // and was subsequently "lost" from the enumeration scope (e.g. user
    // added --skip-videos). We simulate that by backdating last_seen_at.
    let record = TestAssetRecord::new("GHOST")
        .checksum("ck_ghost")
        .filename("ghost.mov")
        .size(4096)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.backdate_last_seen("GHOST", chrono::Utc::now().timestamp() - 86400);

    // Sync 2 begins now. The asset is filtered out - no upsert_seen, no
    // touch_last_seen. last_seen_at stays at one_day_ago.
    let sync_2_start = chrono::Utc::now().timestamp();
    let promoted = db.promote_pending_to_failed(sync_2_start).await.unwrap();
    assert_eq!(promoted, 0, "filtered asset must not be promoted");

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.failed, 0);

    // Sync 3, 4, 5: same filter still applied. Assert the state is
    // stable across repeated calls.
    for _ in 0..3 {
        let start = chrono::Utc::now().timestamp();
        let promoted = db.promote_pending_to_failed(start).await.unwrap();
        assert_eq!(promoted, 0, "stable: filtered asset stays pending");
    }
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.failed, 0);
}

// Canary for the touch_last_seen contract: if a caller bumps
// last_seen_at on a pending row, promote_pending_to_failed WILL promote
// it. The touch_last_seen trait docs warn against this. This test locks
// in that behavior so a silent regression (e.g. an unsafe touch added
// to a skip path) is caught.

#[tokio::test]
async fn touch_last_seen_on_pending_row_causes_promotion_at_sync_end() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // A pending row carried over from a prior sync (backdated).
    let record = TestAssetRecord::new("PENDING_CARRYOVER")
        .checksum("ck_p")
        .filename("pending.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.backdate_last_seen("PENDING_CARRYOVER", chrono::Utc::now().timestamp() - 86400);

    // Capture sync_started_at BEFORE touch_last_seen runs.
    let sync_started_at = chrono::Utc::now().timestamp();

    // Caller violates the contract: bumps last_seen_at on a pending row.
    db.touch_last_seen_many("PrimarySync", &["PENDING_CARRYOVER"])
        .await
        .unwrap();

    let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
    assert_eq!(
        promoted, 1,
        "touch_last_seen on a pending row must cause promotion at sync end"
    );

    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(&*failed[0].id, "PENDING_CARRYOVER");
}

// ── Gap: upsert_seen preserves downloaded status across updates ───

#[tokio::test]
async fn upsert_seen_preserves_downloaded_status_and_path() {
    let dir = test_dir();
    let file_path = dir.path().join("keep_me.jpg");
    fs::write(&file_path, b"content").unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();

    // Insert and mark downloaded
    let record = TestAssetRecord::new("PRESERVE")
        .checksum("ck_v1")
        .filename("keep_me.jpg")
        .size(7)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "PRESERVE",
        "original",
        &file_path,
        "hash_v1",
        None,
    )
    .await
    .unwrap();

    // Re-upsert with updated metadata for the same provider version.
    let updated = TestAssetRecord::new("PRESERVE")
        .checksum("ck_v1")
        .filename("keep_me.jpg")
        .size(7)
        .build();
    db.upsert_seen(&updated).await.unwrap();

    // Status should still be "downloaded", not reset to "pending"
    let summary = db.get_summary().await.unwrap();
    assert_eq!(
        summary.downloaded, 1,
        "upsert_seen should preserve downloaded status"
    );
    assert_eq!(
        summary.pending, 0,
        "upsert_seen should NOT reset to pending"
    );
}

// ── Gap: mark_downloaded with download_checksum ───────────────────

#[tokio::test]
async fn mark_downloaded_stores_download_checksum() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("DL_CK")
        .checksum("api_ck")
        .filename("photo.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DL_CK",
        "original",
        Path::new("/photos/photo.jpg"),
        "local_sha256",
        Some("pre_exif_sha256"),
    )
    .await
    .unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DL_CK",
        "original",
        Path::new("/photos/reconciled/photo.jpg"),
        "reconciled_local_sha256",
        None,
    )
    .await
    .unwrap();

    // Verify via get_downloaded_page that the asset is downloaded
    let page = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(&*page[0].id, "DL_CK");
    assert_eq!(
        page[0].local_checksum.as_deref(),
        Some("reconciled_local_sha256"),
        "the latest local checksum should be stored"
    );
    let conn = db.acquire_lock("verify download checksum").unwrap();
    let download_checksum: Option<String> = conn
        .query_row(
            "SELECT download_checksum FROM assets \
                 WHERE library = 'PrimarySync' AND id = 'DL_CK' AND version_size = 'original'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        download_checksum.as_deref(),
        Some("pre_exif_sha256"),
        "path reconciliation without downloaded-byte evidence must preserve the prior hash"
    );
}

// ── v5 metadata round-trip ──────────────────────────────────────────

#[tokio::test]
async fn upsert_seen_persists_and_roundtrips_metadata() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let mut meta = AssetMetadata {
        source: Some("icloud".into()),
        is_favorite: true,
        rating: Some(4),
        latitude: Some(37.7749),
        longitude: Some(-122.4194),
        altitude: Some(17.0),
        orientation: Some(6),
        duration_secs: Some(12.5),
        timezone_offset: Some(-28800),
        width: Some(4032),
        height: Some(3024),
        title: Some("A caption".into()),
        keywords: Some(r#"["vacation","beach"]"#.into()),
        description: Some("A longer description".into()),
        media_subtype: Some("portrait".into()),
        burst_id: Some("burst_abc".into()),
        is_hidden: false,
        is_archived: false,
        modified_at: Some(Utc.timestamp_opt(1_700_000_000, 0).unwrap()),
        is_deleted: false,
        deleted_at: None,
        provider_data: Some(r#"{"containerId":"x"}"#.into()),
        metadata_hash: None,
    };
    meta.refresh_hash();
    let hash = meta.metadata_hash.clone().unwrap();
    let record = TestAssetRecord::new("META_1")
        .checksum("ck1")
        .filename("photo.jpg")
        .metadata(meta)
        .build();
    db.upsert_seen(&record).await.unwrap();

    let page = db.get_downloaded_page(0, 10).await.unwrap();
    assert!(
        page.is_empty(),
        "pending rows should not be in downloaded page"
    );
    // pull via get_pending to verify round-trip
    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    let got = &pending[0];
    assert_eq!(got.metadata.source.as_deref(), Some("icloud"));
    assert!(got.metadata.is_favorite);
    assert_eq!(got.metadata.rating, Some(4));
    assert_eq!(got.metadata.latitude, Some(37.7749));
    assert_eq!(got.metadata.longitude, Some(-122.4194));
    assert_eq!(got.metadata.altitude, Some(17.0));
    assert_eq!(got.metadata.orientation, Some(6));
    assert_eq!(got.metadata.duration_secs, Some(12.5));
    assert_eq!(got.metadata.timezone_offset, Some(-28800));
    assert_eq!(got.metadata.width, Some(4032));
    assert_eq!(got.metadata.height, Some(3024));
    assert_eq!(got.metadata.title.as_deref(), Some("A caption"));
    assert_eq!(
        got.metadata.keywords.as_deref(),
        Some(r#"["vacation","beach"]"#)
    );
    assert_eq!(
        got.metadata.description.as_deref(),
        Some("A longer description")
    );
    assert_eq!(got.metadata.media_subtype.as_deref(), Some("portrait"));
    assert_eq!(got.metadata.burst_id.as_deref(), Some("burst_abc"));
    assert_eq!(got.metadata.metadata_hash.as_deref(), Some(hash.as_str()));
}

#[tokio::test]
async fn upsert_seen_computes_hash_when_caller_omits_it() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let meta = AssetMetadata {
        is_favorite: true,
        ..AssetMetadata::default()
    };
    let record = TestAssetRecord::new("META_2").metadata(meta).build();
    db.upsert_seen(&record).await.unwrap();
    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0].metadata.metadata_hash.is_some(),
        "upsert_seen must populate metadata_hash even when caller omits it"
    );
}

#[tokio::test]
async fn upsert_seen_updates_metadata_on_conflict() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let initial = TestAssetRecord::new("META_3")
        .metadata(AssetMetadata {
            is_favorite: false,
            title: Some("old".into()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&initial).await.unwrap();

    let updated = TestAssetRecord::new("META_3")
        .metadata(AssetMetadata {
            is_favorite: true,
            title: Some("new".into()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&updated).await.unwrap();

    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].metadata.is_favorite);
    assert_eq!(pending[0].metadata.title.as_deref(), Some("new"));
}

#[tokio::test]
async fn mark_soft_deleted_sets_flags_across_versions() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let orig = TestAssetRecord::new("DEL_1").checksum("c1").build();
    let med = TestAssetRecord::new("DEL_1")
        .checksum("c2")
        .version_size(VersionSizeKey::Medium)
        .build();
    db.upsert_seen(&orig).await.unwrap();
    db.upsert_seen(&med).await.unwrap();
    let when = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let updated = db
        .mark_soft_deleted("PrimarySync", "DEL_1", Some(when))
        .await
        .unwrap();

    assert_eq!(updated, 2);
    assert!(db.get_pending().await.unwrap().is_empty());
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 2);
    assert_eq!(summary.source_deleted, 2);
}

#[tokio::test]
async fn resolve_source_deleted_retains_history_and_preserves_downloaded() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let pending = TestAssetRecord::new("SRC_DEL")
        .version_size(VersionSizeKey::Original)
        .checksum("pending")
        .build();
    let failed = TestAssetRecord::new("SRC_DEL")
        .version_size(VersionSizeKey::Medium)
        .checksum("failed")
        .build();
    let downloaded = TestAssetRecord::new("SRC_DEL")
        .version_size(VersionSizeKey::Thumb)
        .checksum("downloaded")
        .build();
    db.upsert_seen(&pending).await.unwrap();
    db.upsert_seen(&failed).await.unwrap();
    db.upsert_seen(&downloaded).await.unwrap();
    db.mark_failed(
        "PrimarySync",
        "SRC_DEL",
        VersionSizeKey::Medium.as_str(),
        "prior failure",
    )
    .await
    .unwrap();
    let dir = test_dir();
    let path = dir.path().join("source-deleted-thumb.jpg");
    std::fs::write(&path, b"x").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "SRC_DEL",
        VersionSizeKey::Thumb.as_str(),
        &path,
        "local_hash",
        Some("download_hash"),
    )
    .await
    .unwrap();

    let deleted_at = Utc.timestamp_opt(1_700_000_003, 0).unwrap();
    let updated = db
        .resolve_source_deleted("PrimarySync", "SRC_DEL", Some(deleted_at))
        .await
        .unwrap();

    assert_eq!(updated, 3);
    assert!(db.get_pending().await.unwrap().is_empty());
    assert!(db.get_failed().await.unwrap().is_empty());
    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert_eq!(downloaded[0].version_size, VersionSizeKey::Thumb);
    assert!(downloaded[0].metadata.is_deleted);
    assert_eq!(downloaded[0].metadata.deleted_at, Some(deleted_at));
    assert_eq!(downloaded[0].local_path.as_deref(), Some(path.as_path()));
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 3);
    assert_eq!(summary.source_deleted, 3);
}

#[tokio::test]
async fn resolve_master_family_source_deleted_excludes_pending_siblings() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.upsert_seen(&TestAssetRecord::new("MASTER_FAMILY").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("asset-FAMILY-SIBLING").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("OTHER_MASTER").build())
        .await
        .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-FAMILY-SIBLING", "MASTER_FAMILY")
        .await
        .unwrap();

    let updated = db
        .resolve_master_family_source_deleted("PrimarySync", "MASTER_FAMILY", None)
        .await
        .unwrap();

    assert_eq!(updated, 2);
    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id.as_ref(), "OTHER_MASTER");
    assert_eq!(db.get_summary().await.unwrap().source_deleted, 2);
}

#[tokio::test]
async fn source_deleted_retries_are_retained_but_not_actionable() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.upsert_seen(&TestAssetRecord::new("PENDING_DELETED").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("FAILED_DELETED").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("DOWNLOADED_DELETED").build())
        .await
        .unwrap();
    db.upsert_seen(&TestAssetRecord::new("PENDING_LIVE").build())
        .await
        .unwrap();
    db.upsert_seen(
        &TestAssetRecord::new("SHARED_DELETED")
            .library("SharedSync-AAAA")
            .build(),
    )
    .await
    .unwrap();
    db.mark_failed(
        "PrimarySync",
        "FAILED_DELETED",
        VersionSizeKey::Original.as_str(),
        "prior failure",
    )
    .await
    .unwrap();
    let dir = test_dir();
    let path = dir.path().join("downloaded-deleted.jpg");
    std::fs::write(&path, b"x").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DOWNLOADED_DELETED",
        VersionSizeKey::Original.as_str(),
        &path,
        "local_hash",
        Some("download_hash"),
    )
    .await
    .unwrap();
    for asset_id in [
        "PENDING_DELETED",
        "FAILED_DELETED",
        "DOWNLOADED_DELETED",
        "SHARED_DELETED",
    ] {
        let library = if asset_id == "SHARED_DELETED" {
            "SharedSync-AAAA"
        } else {
            "PrimarySync"
        };
        db.mark_soft_deleted(library, asset_id, None).await.unwrap();
    }

    let pruned = db
        .prune_source_deleted_retries(Some("PrimarySync"))
        .await
        .unwrap();

    assert_eq!(pruned, 0);
    assert!(db.get_failed().await.unwrap().is_empty());
    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending.iter().any(|record| {
        record.library.as_ref() == "PrimarySync" && record.id.as_ref() == "PENDING_LIVE"
    }));
    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert_eq!(downloaded[0].id.as_ref(), "DOWNLOADED_DELETED");
    assert!(downloaded[0].metadata.is_deleted);
    assert_eq!(downloaded[0].local_path.as_deref(), Some(path.as_path()));
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 5);
    assert_eq!(summary.source_deleted, 4);
}

#[tokio::test]
async fn mark_soft_deleted_then_mark_downloaded_preserves_tombstone() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let rec = TestAssetRecord::new("DEL_DL_1")
        .checksum("remote_hash")
        .build();
    db.upsert_seen(&rec).await.unwrap();
    db.mark_failed("PrimarySync", "DEL_DL_1", "original", "prior failure")
        .await
        .unwrap();
    let deleted_at = Utc.timestamp_opt(1_700_000_001, 0).unwrap();
    db.mark_soft_deleted("PrimarySync", "DEL_DL_1", Some(deleted_at))
        .await
        .unwrap();

    let dir = test_dir();
    let path = dir.path().join("photo.jpg");
    std::fs::write(&path, b"x").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DEL_DL_1",
        "original",
        &path,
        "local_hash",
        Some("download_hash"),
    )
    .await
    .unwrap();

    assert_downloaded_tombstone_row(&db, "DEL_DL_1", &path, deleted_at);
}

#[tokio::test]
async fn mark_downloaded_then_mark_soft_deleted_preserves_download_state() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let rec = TestAssetRecord::new("DL_DEL_1")
        .checksum("remote_hash")
        .build();
    db.upsert_seen(&rec).await.unwrap();

    let dir = test_dir();
    let path = dir.path().join("photo.jpg");
    std::fs::write(&path, b"x").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DL_DEL_1",
        "original",
        &path,
        "local_hash",
        Some("download_hash"),
    )
    .await
    .unwrap();

    let deleted_at = Utc.timestamp_opt(1_700_000_002, 0).unwrap();
    db.mark_soft_deleted("PrimarySync", "DL_DEL_1", Some(deleted_at))
        .await
        .unwrap();

    assert_downloaded_tombstone_row(&db, "DL_DEL_1", &path, deleted_at);
}

#[tokio::test]
async fn mark_hidden_at_source_sets_flag() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let rec = TestAssetRecord::new("HID_1").build();
    db.upsert_seen(&rec).await.unwrap();
    let updated = db
        .mark_hidden_at_source("PrimarySync", "HID_1")
        .await
        .unwrap();
    assert_eq!(updated, 1);
    let pending = db.get_pending().await.unwrap();
    assert!(pending[0].metadata.is_hidden);
}

#[tokio::test]
async fn source_state_transitions_report_zero_rows() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let deleted = db
        .mark_soft_deleted("PrimarySync", "MISSING_DELETE", None)
        .await
        .unwrap();
    let hidden = db
        .mark_hidden_at_source("PrimarySync", "MISSING_HIDDEN")
        .await
        .unwrap();

    assert_eq!(deleted, 0);
    assert_eq!(hidden, 0);
}

// Full-sync conservatism invariant: an asset downloaded in a prior sync
// that is absent from every page of the current full enumeration must
// remain in the state DB as status='downloaded', untouched. Full sync
// does NOT infer "remotely deleted" from "not seen on any page" — that
// inference is reserved for incremental sync's explicit delete events.
//
// If a regression ever added a "sweep assets not seen this sync" pass
// to the full-sync path, users would silently lose local copies of
// assets that briefly dropped out of view (album scope change, filter
// tweak, pagination hiccup).
#[tokio::test]
async fn full_sync_absent_downloaded_asset_stays_downloaded() {
    let dir = test_dir();
    let file_path = dir.path().join("keeper.heic");
    fs::write(&file_path, b"image-bytes").unwrap();

    let db = SqliteStateDb::open_in_memory().unwrap();

    // Sync N (prior run): asset KEEPER_1 enumerated + downloaded. Then
    // we backdate last_seen_at so it looks like the upsert happened
    // well before the current sync's start boundary.
    let record = TestAssetRecord::new("KEEPER_1")
        .checksum("ck_keeper")
        .filename("keeper.heic")
        .size(11)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "KEEPER_1",
        "original",
        &file_path,
        "localhash",
        None,
    )
    .await
    .unwrap();
    let prior_sync_ts = chrono::Utc::now().timestamp() - 86_400;
    db.backdate_last_seen("KEEPER_1", prior_sync_ts);

    let summary_before = db.get_summary().await.unwrap();
    assert_eq!(summary_before.downloaded, 1);
    let ids_before = db.get_downloaded_ids().await.unwrap();
    assert!(ids_before.contains(&("PrimarySync".into(), "KEEPER_1".into(), "original".into())));

    // Sync N+1 begins now. Producer enumerates zero assets (absent from
    // every page). Nothing calls upsert_seen for KEEPER_1. At sync end,
    // promote_pending_to_failed runs with the new sync_started_at.
    let sync_started_at = chrono::Utc::now().timestamp();
    let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
    assert_eq!(
        promoted, 0,
        "full-sync with zero enumerated assets must not promote anything; \
             KEEPER_1's last_seen_at predates sync_started_at AND its status \
             is downloaded, so both filters protect it"
    );

    // Downloaded row is intact: same status, still in the downloaded set.
    let summary_after = db.get_summary().await.unwrap();
    assert_eq!(
        summary_after.downloaded, 1,
        "downloaded count must be unchanged after a zero-asset sync cycle"
    );
    assert_eq!(summary_after.failed, 0);
    assert_eq!(summary_after.pending, 0);

    let ids_after = db.get_downloaded_ids().await.unwrap();
    assert!(
        ids_after.contains(&("PrimarySync".into(), "KEEPER_1".into(), "original".into())),
        "KEEPER_1 must remain in the downloaded set after a full sync that \
             didn't re-enumerate it"
    );

    // last_seen_at was NOT refreshed (nothing touched it). A caller that
    // later wants to implement "assets not seen for N syncs" can use the
    // stale timestamp as a signal, but it must be an opt-in policy, not
    // a silent cleanup.
    let failed = db.get_failed().await.unwrap();
    assert!(
        failed.is_empty(),
        "the asset that wasn't enumerated this sync must not appear in the failed set"
    );
}

/// Read-side counterpart to `upsert_seen_keeps_distinct_rows_per_library`
/// and `mark_failed_is_library_scoped`: those tests already pin the write
/// side, this one pins that the bulk-loader queries used by the download
/// hot path surface per-zone rows without collapsing them on the shared
/// `(id, version_size)` pair the v8 PK split was created to disambiguate.
#[tokio::test]
async fn multi_library_read_queries_scope_per_zone() {
    let dir = test_dir();
    let db = SqliteStateDb::open_in_memory().unwrap();

    const ID: &str = "SHARED_ID";
    const PRIMARY: &str = "PrimarySync";
    const SHARED: &str = "SharedSync-A1B2C3D4";

    for (library, ck) in [(PRIMARY, "ck_primary"), (SHARED, "ck_shared")] {
        let record = TestAssetRecord::new(ID)
            .library(library)
            .checksum(ck)
            .filename("photo.jpg")
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    let primary_path = dir.path().join(PRIMARY).join("photo.jpg");
    let shared_path = dir.path().join(SHARED).join("photo.jpg");
    for path in [&primary_path, &shared_path] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }
    db.mark_downloaded(PRIMARY, ID, "original", &primary_path, "lh_primary", None)
        .await
        .unwrap();
    db.mark_downloaded(SHARED, ID, "original", &shared_path, "lh_shared", None)
        .await
        .unwrap();

    let checksums = db.get_downloaded_checksums().await.unwrap();
    let triple = |lib: &str| (lib.to_string(), ID.to_string(), "original".to_string());
    assert_eq!(
        checksums.get(&triple(PRIMARY)),
        Some(&"ck_primary".to_string())
    );
    assert_eq!(
        checksums.get(&triple(SHARED)),
        Some(&"ck_shared".to_string())
    );
}

#[tokio::test]
async fn mark_downloaded_fails_when_asset_row_missing() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let result = db
        .mark_downloaded(
            "PrimarySync",
            "NONEXISTENT_42",
            "original",
            Path::new("/tmp/codex/kei/photo.jpg"),
            "abc123hash",
            None,
        )
        .await;

    assert!(
        result.is_err(),
        "mark_downloaded on an absent row must fail"
    );
    let err = result.unwrap_err();
    assert!(
        matches!(
            err,
            StateError::AssetRowMissing {
                ref asset_id,
                ref version_size,
            } if asset_id == "NONEXISTENT_42" && version_size == "original"
        ),
        "expected AssetRowMissing with correct ids, got: {err:?}"
    );
}
