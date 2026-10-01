use crate::download;
use crate::sync_cycle::CycleResult;
use crate::sync_loop::reporting::{merge_refresh_tail_outcome, sync_run_stats_from_cycle};

#[test]
fn refresh_tail_outcome_feeds_ledger_and_cycle_reporting() {
    let mut cycle_result = CycleResult {
        failed_count: 1,
        session_expired: false,
        stats: download::SyncStats {
            failed: 1,
            ..download::SyncStats::default()
        },
        db_sync_token_advance_safe: true,
    };

    merge_refresh_tail_outcome(&mut cycle_result, 2, false);

    assert_eq!(cycle_result.failed_count, 3);
    assert_eq!(cycle_result.stats.failed, 3);
    let ledger = sync_run_stats_from_cycle(&cycle_result);
    assert_eq!(ledger.assets_failed, 3);
    let facts = crate::cycle_reporter::CycleFacts::new(
        &cycle_result.stats,
        cycle_result.failed_count,
        cycle_result.session_expired,
        std::time::Duration::ZERO,
    );
    assert_eq!(facts.status, crate::cycle_reporter::CycleStatus::Failed);
}

#[tokio::test]
async fn cycle_ledger_preserves_interrupted_facts_across_reopen_and_clean_cycle() {
    const FIRST_RUN_ID: i64 = 1;
    let dir = tempfile::tempdir().expect("state directory");
    let path = dir.path().join("state.db");
    let db = crate::state::SqliteStateDb::open(&path)
        .await
        .expect("open state");
    let interrupted = CycleResult {
        failed_count: 2,
        session_expired: false,
        stats: download::SyncStats {
            assets_seen: 7,
            failed: 2,
            enumeration_errors: 1,
            interrupted: true,
            ..download::SyncStats::default()
        },
        db_sync_token_advance_safe: false,
    };
    super::record_cycle_run(Some(&db), &interrupted, chrono::Utc::now()).await;
    drop(db);

    let db = crate::state::SqliteStateDb::open(&path)
        .await
        .expect("reopen state");
    let expected = ("interrupted".to_string(), 7, 2, 1, 1);
    assert_eq!(
        db.sync_run_snapshot_for_test(FIRST_RUN_ID).unwrap(),
        expected
    );
    let clean = CycleResult {
        failed_count: 0,
        session_expired: false,
        stats: download::SyncStats::default(),
        db_sync_token_advance_safe: true,
    };
    super::record_cycle_run(Some(&db), &clean, chrono::Utc::now()).await;
    assert_eq!(
        db.sync_run_snapshot_for_test(FIRST_RUN_ID).unwrap(),
        expected
    );
    assert_eq!(
        db.sync_run_snapshot_for_test(FIRST_RUN_ID + 1).unwrap(),
        ("complete".to_string(), 0, 0, 0, 0)
    );
    assert_eq!(db.promote_orphaned_sync_runs().await.unwrap(), 0);
}
