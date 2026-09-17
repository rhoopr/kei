//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::fs;

use chrono::{TimeZone, Utc};

use crate::state::db::SqliteStateDb;
use crate::state::db::test_support::test_dir;
use crate::state::error::StateError;
use crate::state::types::{AssetStatus, SyncRunStats};
use crate::test_helpers::TestAssetRecord;

#[tokio::test]
async fn get_failed_orders_by_last_seen_desc() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    for id in &["OLDEST", "MIDDLE", "NEWEST"] {
        let record = TestAssetRecord::new(id)
            .checksum(&format!("ck_{id}"))
            .filename(&format!("{}.jpg", id.to_lowercase()))
            .size(100)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", id, "original", "boom")
            .await
            .unwrap();
    }

    // Force a deterministic order by backdating.
    db.backdate_last_seen("OLDEST", 1_000);
    db.backdate_last_seen("MIDDLE", 2_000);
    db.backdate_last_seen("NEWEST", 3_000);

    let failed = db.get_failed().await.unwrap();
    let ids: Vec<&str> = failed.iter().map(|r| &*r.id).collect();
    assert_eq!(
        ids,
        vec!["NEWEST", "MIDDLE", "OLDEST"],
        "get_failed must sort last_seen_at DESC"
    );
}

#[tokio::test]
async fn get_failed_sample_respects_limit_and_returns_total() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    for i in 0..5 {
        let id = format!("FAIL_{i}");
        let record = TestAssetRecord::new(&id)
            .checksum(&format!("ck_{i}"))
            .filename(&format!("{i}.jpg"))
            .size(100)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", &id, "original", "boom")
            .await
            .unwrap();
    }
    // Newest first: FAIL_4 > FAIL_3 > FAIL_2 ...
    for i in 0..5 {
        db.backdate_last_seen(&format!("FAIL_{i}"), 1_000 + i as i64);
    }

    let (sample, total) = db.get_failed_sample(2).await.unwrap();
    assert_eq!(total, 5, "total should reflect full failed count");
    assert_eq!(sample.len(), 2, "limit should cap returned rows");
    assert_eq!(&*sample[0].id, "FAIL_4");
    assert_eq!(&*sample[1].id, "FAIL_3");

    // limit > total returns all and the correct total
    let (sample, total) = db.get_failed_sample(100).await.unwrap();
    assert_eq!(total, 5);
    assert_eq!(sample.len(), 5);
}

#[tokio::test]
async fn get_failed_sample_empty_returns_zero_total() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let (sample, total) = db.get_failed_sample(10).await.unwrap();
    assert!(sample.is_empty());
    assert_eq!(total, 0);
}

#[tokio::test]
async fn test_get_summary() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // Add some assets in different states
    for i in 0..3 {
        let record = TestAssetRecord::new(&format!("PENDING_{}", i))
            .checksum(&format!("checksum_{}", i))
            .filename(&format!("photo_{}.jpg", i))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    let dir = test_dir();
    for i in 0..2 {
        let record = TestAssetRecord::new(&format!("DOWNLOADED_{}", i))
            .checksum(&format!("dl_checksum_{}", i))
            .filename(&format!("dl_photo_{}.jpg", i))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        let path = dir.path().join(format!("dl_photo_{}.jpg", i));
        fs::write(&path, b"content").unwrap();
        db.mark_downloaded(
            "PrimarySync",
            &format!("DOWNLOADED_{}", i),
            "original",
            &path,
            "hash",
            None,
        )
        .await
        .unwrap();
    }

    let record = TestAssetRecord::new("FAILED_1")
        .checksum("fail_checksum")
        .filename("fail_photo.jpg")
        .size(1000)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_failed("PrimarySync", "FAILED_1", "original", "Error")
        .await
        .unwrap();

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 6);
    assert_eq!(summary.pending, 3);
    assert_eq!(summary.downloaded, 2);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.policy_excluded, 0);
}

#[tokio::test]
async fn test_sync_run_lifecycle() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let run_id = db.start_sync_run().await.unwrap();
    assert!(run_id > 0);

    let stats = SyncRunStats {
        assets_seen: 100,
        assets_downloaded: 95,
        assets_failed: 5,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };

    db.complete_sync_run(run_id, &stats).await.unwrap();

    let summary = db.get_summary().await.unwrap();
    assert!(summary.last_sync_started.is_some());
    assert!(summary.last_sync_completed.is_some());
}

#[tokio::test]
async fn summary_tracks_running_sync_even_when_latest_row_completed() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    let active_start = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
    let active_run_id = db.start_sync_run_at(active_start).await.unwrap();
    let completed_run_id = db
        .start_sync_run_at(Utc.timestamp_opt(1_700_000_030, 0).unwrap())
        .await
        .unwrap();
    assert!(
        completed_run_id > active_run_id,
        "completed row must be newest by id for this regression"
    );

    let stats = SyncRunStats {
        assets_seen: 1,
        assets_downloaded: 1,
        assets_failed: 0,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };
    db.complete_sync_run(completed_run_id, &stats)
        .await
        .unwrap();

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.active_sync_started, Some(active_start));
    assert!(
        summary.last_sync_completed.is_some(),
        "latest completed row should still be available for non-active status"
    );
}

#[tokio::test]
async fn summary_lists_full_enumeration_progress_markers() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    db.begin_enum_progress("SharedSync-Z").await.unwrap();
    db.begin_enum_progress("PrimarySync").await.unwrap();

    let summary = db.get_summary().await.unwrap();
    assert_eq!(
        summary.active_enumeration_zones,
        vec!["PrimarySync", "SharedSync-Z"]
    );
}

// ── sync_runs status lifecycle ─────────────────────────────────────────

fn status_of(db: &SqliteStateDb, run_id: i64) -> String {
    db.sync_run_snapshot_for_test(run_id).unwrap().0
}

#[tokio::test]
async fn sync_run_status_is_running_after_start() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();
    assert_eq!(status_of(&db, run_id), "running");
}

#[tokio::test]
async fn sync_run_status_is_complete_after_clean_complete() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();
    let stats = SyncRunStats {
        assets_seen: 1,
        assets_downloaded: 1,
        assets_failed: 0,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };
    db.complete_sync_run(run_id, &stats).await.unwrap();
    assert_eq!(status_of(&db, run_id), "complete");
}

/// `enumeration_errors` must round-trip from `SyncRunStats` into the
/// on-disk `sync_runs.enumeration_errors` column.
#[tokio::test]
async fn complete_sync_run_persists_enumeration_errors_column() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();
    let stats = SyncRunStats {
        assets_seen: 0,
        assets_downloaded: 0,
        assets_failed: 0,
        enumeration_errors: 17,
        interrupted: false,
        ..Default::default()
    };
    db.complete_sync_run(run_id, &stats).await.unwrap();

    let conn = db.acquire_lock("test_enum_errors_column").unwrap();
    let stored: i64 = conn
        .query_row(
            "SELECT enumeration_errors FROM sync_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored, 17,
        "enumeration_errors must round-trip from SyncRunStats to sync_runs row"
    );
}

#[tokio::test]
async fn complete_sync_run_persists_inventory_columns() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();
    let stats = SyncRunStats {
        api_total_at_start: Some(95),
        api_total_at_start_partial: true,
        inventory_drop_warnings: 1,
        inventory_drop_previous_total: Some(100),
        inventory_drop_current_total: Some(95),
        inventory_drop_library: Some("PrimarySync".to_string()),
        ..Default::default()
    };
    db.complete_sync_run(run_id, &stats).await.unwrap();

    let conn = db.acquire_lock("test_inventory_columns").unwrap();
    let stored: (
        Option<i64>,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT api_total_at_start, api_total_at_start_partial, \
                        inventory_drop_detected, inventory_drop_previous_total, \
                        inventory_drop_current_total, inventory_drop_library \
                 FROM sync_runs WHERE id = ?1",
            [run_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        stored,
        (
            Some(95),
            1,
            1,
            Some(100),
            Some(95),
            Some("PrimarySync".to_string())
        )
    );
}

#[tokio::test]
async fn complete_sync_run_unknown_id_returns_error() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let stats = SyncRunStats {
        assets_seen: 3,
        assets_downloaded: 2,
        assets_failed: 1,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };

    let err = db
        .complete_sync_run(999_999, &stats)
        .await
        .expect_err("unknown sync_run id must fail loudly");
    match err {
        StateError::Invariant { operation, detail } => {
            assert_eq!(operation, "complete_sync_run");
            assert!(
                detail.contains("999999") || detail.contains("999_999"),
                "error detail should name the missing run id, got: {detail}"
            );
        }
        other => panic!("expected StateError::Invariant, got {other:?}"),
    }

    let conn = db.acquire_lock("unknown_sync_run").unwrap();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sync_runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0, "unknown completion must not create a row");
}

#[tokio::test]
async fn sync_run_status_is_interrupted_when_flagged() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();
    let stats = SyncRunStats {
        assets_seen: 1,
        assets_downloaded: 0,
        assets_failed: 0,
        enumeration_errors: 0,
        interrupted: true,
        ..Default::default()
    };
    db.complete_sync_run(run_id, &stats).await.unwrap();
    assert_eq!(status_of(&db, run_id), "interrupted");
}

#[tokio::test]
async fn promote_orphaned_sync_runs_flips_running_rows() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    // Simulate two crashed runs plus one clean one
    let a = db.start_sync_run().await.unwrap();
    let b = db.start_sync_run().await.unwrap();
    let c = db.start_sync_run().await.unwrap();
    let clean = SyncRunStats {
        assets_seen: 0,
        assets_downloaded: 0,
        assets_failed: 0,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };
    db.complete_sync_run(c, &clean).await.unwrap();

    let promoted = db.promote_orphaned_sync_runs().await.unwrap();
    assert_eq!(promoted, 2);
    assert_eq!(status_of(&db, a), "interrupted");
    assert_eq!(status_of(&db, b), "interrupted");
    // The cleanly completed row must be untouched
    assert_eq!(status_of(&db, c), "complete");
}

#[tokio::test]
async fn promote_orphaned_sync_runs_noop_when_none_pending() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();
    let stats = SyncRunStats {
        assets_seen: 0,
        assets_downloaded: 0,
        assets_failed: 0,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };
    db.complete_sync_run(run_id, &stats).await.unwrap();

    let promoted = db.promote_orphaned_sync_runs().await.unwrap();
    assert_eq!(promoted, 0);
}

/// Promotion is idempotent — once an orphan has been flipped to
/// `interrupted`, a second invocation must return 0 rows promoted and
/// must not re-touch the row. The current implementation guards via
/// `WHERE status = 'running'`, but no test pinned the second-call
/// behavior. A future refactor that broadened the WHERE clause (e.g.
/// `status != 'completed'`) would silently double-promote rows the
/// next time init runs.
#[tokio::test]
async fn promote_orphaned_sync_runs_idempotent_second_call_promotes_zero() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // Three running rows; flush them once.
    let a = db.start_sync_run().await.unwrap();
    let b = db.start_sync_run().await.unwrap();
    let c = db.start_sync_run().await.unwrap();
    let first = db.promote_orphaned_sync_runs().await.unwrap();
    assert_eq!(first, 3, "first call should flip all 3 running rows");

    // Capture the post-promote interrupted timestamps so we can verify
    // the second call doesn't re-touch them. Scope the lock guard so
    // it never sits across an .await on the next line.
    let (snap_a, snap_b, snap_c) = {
        let conn = db.acquire_lock("snapshot_after_first_promote").unwrap();
        let read_run = |id: i64| {
            let (status, interrupted): (String, i32) = conn
                .query_row(
                    "SELECT status, interrupted FROM sync_runs WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            (status, interrupted)
        };
        (read_run(a), read_run(b), read_run(c))
    };
    assert_eq!(snap_a, ("interrupted".to_string(), 1));
    assert_eq!(snap_b, ("interrupted".to_string(), 1));
    assert_eq!(snap_c, ("interrupted".to_string(), 1));

    // Second call must be a no-op.
    let second = db.promote_orphaned_sync_runs().await.unwrap();
    assert_eq!(
        second, 0,
        "second call must not re-promote already-interrupted rows"
    );

    // And no row's state changed.
    let after_a: (String, i32) = {
        let conn = db.acquire_lock("snapshot_after_second_promote").unwrap();
        conn.query_row(
            "SELECT status, interrupted FROM sync_runs WHERE id = ?1",
            [a],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(after_a, ("interrupted".to_string(), 1));
}

/// Corollary to the idempotency test: a freshly-completed `sync_runs` row in between
/// invocations must not be promoted. Pins the "WHERE status = 'running'"
/// invariant against churn — a misclassified completed run would
/// silently corrupt operator dashboards.
#[tokio::test]
async fn promote_orphaned_sync_runs_does_not_touch_completed_rows() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    // First batch: one row, complete it cleanly.
    let r1 = db.start_sync_run().await.unwrap();
    let stats = SyncRunStats {
        assets_seen: 1,
        assets_downloaded: 1,
        assets_failed: 0,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };
    db.complete_sync_run(r1, &stats).await.unwrap();

    // Second batch: another row, leave running.
    let r2 = db.start_sync_run().await.unwrap();
    let promoted = db.promote_orphaned_sync_runs().await.unwrap();
    assert_eq!(promoted, 1, "only the running row should be promoted");

    // r1 must still be complete.
    let conn = db.acquire_lock("verify").unwrap();
    let s1: String = conn
        .query_row("SELECT status FROM sync_runs WHERE id = ?1", [r1], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(s1, "complete");
    let s2: String = conn
        .query_row("SELECT status FROM sync_runs WHERE id = ?1", [r2], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(s2, "interrupted");
}

#[tokio::test]
async fn promote_orphaned_sync_runs_sets_interrupted_flag_too() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();
    let _ = db.promote_orphaned_sync_runs().await.unwrap();

    let conn = db.acquire_lock("verify_interrupted").unwrap();
    let interrupted: i32 = conn
        .query_row(
            "SELECT interrupted FROM sync_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(interrupted, 1);
}

#[tokio::test]
async fn test_get_downloaded_page() {
    let dir = test_dir();
    let db = SqliteStateDb::open_in_memory().unwrap();

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

    // Fetch all in one page
    let page = db.get_downloaded_page(0, 100).await.unwrap();
    assert_eq!(page.len(), 3);

    // Paginate: page of 2, then remainder
    let first = db.get_downloaded_page(0, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    let second = db.get_downloaded_page(2, 2).await.unwrap();
    assert_eq!(second.len(), 1);
    let third = db.get_downloaded_page(4, 2).await.unwrap();
    assert!(third.is_empty());
}

#[tokio::test]
async fn test_get_failed_page() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    for i in 0..3 {
        let id = format!("FAIL_{i}");
        let record = TestAssetRecord::new(&id)
            .checksum(&format!("checksum_{i}"))
            .filename(&format!("photo_{i}.jpg"))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_failed("PrimarySync", &id, "original", "boom")
            .await
            .unwrap();
    }
    // Newest-first ordering: FAIL_2 > FAIL_1 > FAIL_0
    for i in 0..3i64 {
        db.backdate_last_seen(&format!("FAIL_{i}"), 1_000 + i);
    }

    // Fetch all in one page
    let page = db.get_failed_page(0, 100).await.unwrap();
    assert_eq!(page.len(), 3);
    assert_eq!(&*page[0].id, "FAIL_2");
    assert_eq!(&*page[2].id, "FAIL_0");

    // Paginate: page of 2, then remainder
    let first = db.get_failed_page(0, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(&*first[0].id, "FAIL_2");
    assert_eq!(&*first[1].id, "FAIL_1");
    let second = db.get_failed_page(2, 2).await.unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(&*second[0].id, "FAIL_0");
    let third = db.get_failed_page(4, 2).await.unwrap();
    assert!(third.is_empty());
}

#[tokio::test]
async fn test_get_pending_page() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    for i in 0..3 {
        let id = format!("PEND_{i}");
        let record = TestAssetRecord::new(&id)
            .checksum(&format!("checksum_{i}"))
            .filename(&format!("photo_{i}.jpg"))
            .size(1000)
            .build();
        db.upsert_seen(&record).await.unwrap();
    }
    // Newest-first ordering: PEND_2 > PEND_1 > PEND_0
    for i in 0..3i64 {
        db.backdate_last_seen(&format!("PEND_{i}"), 1_000 + i);
    }

    // Fetch all in one page
    let page = db.get_pending_page(0, 100).await.unwrap();
    assert_eq!(page.len(), 3);
    assert_eq!(&*page[0].id, "PEND_2");
    assert_eq!(&*page[2].id, "PEND_0");

    // Paginate: page of 2, then remainder
    let first = db.get_pending_page(0, 2).await.unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(&*first[0].id, "PEND_2");
    assert_eq!(&*first[1].id, "PEND_1");
    let second = db.get_pending_page(2, 2).await.unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(&*second[0].id, "PEND_0");
    let third = db.get_pending_page(4, 2).await.unwrap();
    assert!(third.is_empty());
}

#[tokio::test]
async fn get_failed_pending_page_scales_to_large_count() {
    // Mirror of `test_get_downloaded_page_scales_to_large_count` for the
    // failed and pending lists. Bulk-insert 10k rows, paginate through,
    // assert we never need to materialize more than `page_size` at once
    // and the total count matches.
    let db = SqliteStateDb::open_in_memory().unwrap();
    let count: usize = 10_000;
    {
        let conn = db.conn.lock().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let mut stmt = conn
            .prepare(
                "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at, download_attempts, last_error)
                     VALUES ('PrimarySync', ?1, 'original', ?2, ?3, ?4, ?5, 'photo', ?6, ?4, 1, ?7)",
            )
            .unwrap();
        let now = Utc::now().timestamp();
        for i in 0..count {
            let id = format!("ASSET_{i:05}");
            let checksum = format!("cksum_{i:05}");
            let filename = format!("IMG_{i:05}.jpg");
            let status = if i % 2 == 0 { "failed" } else { "pending" };
            let err = if status == "failed" {
                Some("boom")
            } else {
                None
            };
            stmt.execute(rusqlite::params![
                id,
                checksum,
                filename,
                now + i as i64,
                4096,
                status,
                err
            ])
            .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
    }

    let page_size: u32 = 1000;

    // Failed: 5000 rows
    let mut total = 0usize;
    let mut offset = 0u64;
    loop {
        let page = db.get_failed_page(offset, page_size).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert!(
            page.len() <= page_size as usize,
            "get_failed_page must respect the requested limit"
        );
        assert!(page.iter().all(|r| r.status == AssetStatus::Failed));
        total += page.len();
        offset += page.len() as u64;
    }
    assert_eq!(total, count / 2);

    // Pending: 5000 rows
    let mut total = 0usize;
    let mut offset = 0u64;
    loop {
        let page = db.get_pending_page(offset, page_size).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert!(
            page.len() <= page_size as usize,
            "get_pending_page must respect the requested limit"
        );
        assert!(page.iter().all(|r| r.status == AssetStatus::Pending));
        total += page.len();
        offset += page.len() as u64;
    }
    assert_eq!(total, count / 2);
}

#[tokio::test]
async fn test_get_downloaded_page_scales_to_large_count() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    let count: usize = 10_000;

    // Bulk-insert records directly for speed
    {
        let conn = db.conn.lock().unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let mut stmt = conn
            .prepare(
                "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, downloaded_at, local_path, local_checksum, last_seen_at)
                     VALUES ('PrimarySync', ?1, 'original', ?2, ?3, ?4, ?5, 'photo', 'downloaded', ?4, ?6, ?2, ?4)",
            )
            .unwrap();
        let now = Utc::now().timestamp();
        for i in 0..count {
            let id = format!("ASSET_{i:05}");
            let checksum = format!("cksum_{i:05}");
            let filename = format!("IMG_{i:05}.jpg");
            let path = format!("/photos/2026/01/01/{filename}");
            stmt.execute(rusqlite::params![id, checksum, filename, now, 4096, path])
                .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
    }

    // Paginate through all records
    let page_size: u32 = 1000;
    let mut total = 0usize;
    let mut offset = 0u64;
    let mut first_id = String::new();
    let mut last_id = String::new();
    loop {
        let page = db.get_downloaded_page(offset, page_size).await.unwrap();
        if page.is_empty() {
            break;
        }
        if total == 0 {
            first_id = page[0].id.to_string();
        }
        last_id = page.last().unwrap().id.to_string();
        assert!(page.iter().all(|r| r.status == AssetStatus::Downloaded));
        total += page.len();
        offset += page.len() as u64;
    }

    assert_eq!(total, count);
    assert_eq!(first_id, "ASSET_00000");
    assert_eq!(last_id, format!("ASSET_{:05}", count - 1));
}

#[tokio::test]
async fn upsert_seen_then_summary_counts_accurate_across_transitions() {
    // Arrange: create assets and move them through pending -> downloaded -> failed transitions
    let db = SqliteStateDb::open_in_memory().unwrap();
    let dir = test_dir();

    let now = Utc::now();
    let ids = ["AEt9xLq2V0", "AEt9xLq2V1", "AEt9xLq2V2", "AEt9xLq2V3"];
    for (i, id) in ids.iter().enumerate() {
        let record = TestAssetRecord::new(id)
            .checksum(&format!(
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8{:02x}",
                i
            ))
            .filename(&format!("IMG_{}.JPG", 1000 + i))
            .created_at(now)
            .added_at(now - chrono::Duration::days(1))
            .size(u64::try_from(4_194_304 + i * 1024).unwrap_or(0))
            .build();
        db.upsert_seen(&record).await.unwrap();
    }

    // All 4 start as pending
    let s1 = db.get_summary().await.unwrap();
    assert_eq!(s1.total_assets, 4);
    assert_eq!(s1.pending, 4);
    assert_eq!(s1.downloaded, 0);
    assert_eq!(s1.failed, 0);

    // Act: download two, fail one, leave one pending
    let path0 = dir.path().join("IMG_1000.JPG");
    fs::write(&path0, b"JPEG data").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        ids[0],
        "original",
        &path0,
        "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592",
        None,
    )
    .await
    .unwrap();

    let path1 = dir.path().join("IMG_1001.JPG");
    fs::write(&path1, b"JPEG data 2").unwrap();
    db.mark_downloaded(
        "PrimarySync",
        ids[1],
        "original",
        &path1,
        "ef2d127de37b942baad06145e54b0c619a1f22327b2ebbcfbec78f5564afe39d",
        None,
    )
    .await
    .unwrap();

    db.mark_failed(
        "PrimarySync",
        ids[2],
        "original",
        "HTTP 503 Service Unavailable",
    )
    .await
    .unwrap();

    // Assert: counts reflect exact transitions
    let s2 = db.get_summary().await.unwrap();
    assert_eq!(s2.total_assets, 4);
    assert_eq!(s2.downloaded, 2);
    assert_eq!(s2.failed, 1);
    assert_eq!(s2.pending, 1);

    // Act: reset failed back to pending
    let reset_count = db.reset_failed().await.unwrap();
    assert_eq!(reset_count, 1);

    // Assert: failed count goes to 0, pending increases
    let s3 = db.get_summary().await.unwrap();
    assert_eq!(s3.total_assets, 4);
    assert_eq!(s3.downloaded, 2);
    assert_eq!(s3.failed, 0);
    assert_eq!(s3.pending, 2);
}

#[tokio::test]
async fn sync_run_zero_value_stats() {
    // Arrange
    let db = SqliteStateDb::open_in_memory().unwrap();
    let run_id = db.start_sync_run().await.unwrap();

    // Act: complete the sync run with all-zero stats
    let stats = SyncRunStats {
        assets_seen: 0,
        assets_downloaded: 0,
        assets_failed: 0,
        enumeration_errors: 0,
        interrupted: false,
        ..Default::default()
    };
    db.complete_sync_run(run_id, &stats).await.unwrap();

    // Assert: summary reflects the completed run with timestamps populated
    let summary = db.get_summary().await.unwrap();
    assert!(summary.last_sync_started.is_some());
    assert!(summary.last_sync_completed.is_some());

    // Verify the raw sync_runs row has zero values
    let (seen, downloaded, failed, interrupted): (i64, i64, i64, i64) = {
        let conn = db.conn.lock().unwrap();
        conn.query_row(
            "SELECT assets_seen, assets_downloaded, assets_failed, interrupted FROM sync_runs WHERE id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap()
    };
    assert_eq!(seen, 0);
    assert_eq!(downloaded, 0);
    assert_eq!(failed, 0);
    assert_eq!(interrupted, 0);
}

#[tokio::test]
async fn sync_run_killed_mid_pass_preserves_downloaded_rows_on_next_open() {
    let dir = test_dir();
    let db_path = dir.path().join("crash_test.db");

    let file_path_1 = dir.path().join("photo1.jpg");
    let file_path_2 = dir.path().join("photo2.jpg");
    std::fs::write(&file_path_1, b"photo 1 content").unwrap();
    std::fs::write(&file_path_2, b"photo 2 content").unwrap();

    {
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let _run_id = db.start_sync_run().await.unwrap();

        for (id, path) in [
            ("A1", &file_path_1),
            ("A2", &file_path_2),
            ("A3", &file_path_1),
        ] {
            let record = TestAssetRecord::new(id).build();
            db.upsert_seen(&record).await.unwrap();
            if id != "A3" {
                db.mark_downloaded("PrimarySync", id, "original", path, "hash", None)
                    .await
                    .unwrap();
            }
        }
        // Drop without calling complete_sync_run — simulates kill -9
    }

    let db2 = SqliteStateDb::open(&db_path).await.unwrap();
    let promoted = db2.promote_orphaned_sync_runs().await.unwrap();
    assert_eq!(
        promoted, 1,
        "the orphaned running sync_run must be promoted"
    );

    let summary = db2.get_summary().await.unwrap();
    assert_eq!(
        summary.downloaded, 2,
        "the two downloaded assets must survive the crash"
    );
    assert_eq!(
        summary.pending, 1,
        "the one pending asset must still be pending"
    );
}
