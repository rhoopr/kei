use crate::state;
use crate::sync_loop::reconcile::run_bounded_local_drift_probe;
use crate::sync_loop::should_reconcile_this_cycle;

// Periodic reconciliation cadence. The watch loop calls
// `should_reconcile_this_cycle` once per cycle to decide whether to walk
// the state DB and warn on missing local files. Tests pin the cadence
// so a future refactor can't silently disable the schedule.

/// When `every_n` is `None`, the predicate must NEVER fire — this
/// is the default-disabled behaviour for daemons that don't opt into
/// periodic reconciliation.
#[test]
fn periodic_reconcile_disabled_when_every_n_is_none() {
    for cycle in [1u64, 2, 24, 1_000, u64::MAX] {
        assert!(
            !should_reconcile_this_cycle(cycle, None),
            "cycle {cycle} with every_n=None must NOT trigger reconciliation"
        );
    }
}

/// `Some(0)` is treated identically to `None` — the config
/// resolver also filters this case, but the predicate is the load-bearing
/// gate so we pin both spellings here.
#[test]
fn periodic_reconcile_disabled_when_every_n_is_zero() {
    for cycle in [1u64, 2, 24, 1_000] {
        assert!(
            !should_reconcile_this_cycle(cycle, Some(0)),
            "cycle {cycle} with every_n=Some(0) must NOT trigger"
        );
    }
}

/// The first firing must be at cycle == every_n, NOT cycle 0 or
/// cycle 1. A freshly-started daemon must run at least one full sync
/// before burning startup time on a state-DB walk.
#[test]
fn periodic_reconcile_first_fires_at_cycle_n_not_at_cycle_zero() {
    // every_n = 24: cycles 1..23 must NOT fire; cycle 24 fires.
    for cycle in 1u64..24 {
        assert!(
            !should_reconcile_this_cycle(cycle, Some(24)),
            "cycle {cycle} must NOT trigger when every_n=24"
        );
    }
    assert!(
        should_reconcile_this_cycle(24, Some(24)),
        "cycle 24 with every_n=24 must trigger"
    );
    // Cycle 0 is the pre-loop sentinel and must never fire even when
    // 0 is divisible by N.
    assert!(
        !should_reconcile_this_cycle(0, Some(24)),
        "cycle 0 (pre-loop sentinel) must NEVER trigger"
    );
}

/// Subsequent firings must repeat at every multiple of `every_n`.
/// Pinning a few cycles past the first firing guards against an
/// off-by-one that lets the cadence drift over a long run.
#[test]
fn periodic_reconcile_fires_on_every_multiple_of_n() {
    let n = 24;
    for &cycle in &[24u64, 48, 72, 240, 24_000] {
        assert!(
            should_reconcile_this_cycle(cycle, Some(n)),
            "cycle {cycle} (multiple of {n}) must trigger"
        );
    }
    for &cycle in &[25u64, 47, 49, 71, 73, 239, 241] {
        assert!(
            !should_reconcile_this_cycle(cycle, Some(n)),
            "cycle {cycle} (NOT a multiple of {n}) must NOT trigger"
        );
    }
}

/// `every_n=1` makes every cycle trigger reconciliation. Allowed
/// (chatty but not a bug) and pinned because users debugging a drift
/// suspicion are likely to set it to 1 temporarily.
#[test]
fn periodic_reconcile_every_one_fires_every_cycle() {
    for cycle in 1u64..=10 {
        assert!(
            should_reconcile_this_cycle(cycle, Some(1)),
            "cycle {cycle} with every_n=1 must trigger"
        );
    }
    // Sentinel still excluded.
    assert!(!should_reconcile_this_cycle(0, Some(1)));
}

async fn seed_downloaded_for_local_drift_probe(
    db: &state::SqliteStateDb,
    id: &str,
    path: &std::path::Path,
    size: u64,
) {
    let record = crate::test_helpers::TestAssetRecord::new(id)
        .checksum(&format!("ck_{id}"))
        .filename(&format!("{id}.jpg"))
        .size(size)
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

#[tokio::test]
async fn bounded_local_drift_probe_marks_missing_file_failed() {
    let dir = tempfile::tempdir().unwrap();
    let db = state::SqliteStateDb::open_in_memory().unwrap();
    let missing_path = dir.path().join("missing.jpg");
    seed_downloaded_for_local_drift_probe(&db, "MISSING_PROBE", &missing_path, 100).await;

    let outcome = run_bounded_local_drift_probe(&db, 1).await;

    assert_eq!(outcome.scanned, 1);
    assert_eq!(outcome.drifted, 1);
    assert_eq!(outcome.marked_failed, 1);
    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(&*failed[0].id, "MISSING_PROBE");
    assert_eq!(
        failed[0].last_error.as_deref(),
        Some(crate::commands::reconcile::FILE_MISSING_REASON)
    );
}

#[tokio::test]
async fn bounded_local_drift_probe_marks_truncated_file_failed() {
    let dir = tempfile::tempdir().unwrap();
    let db = state::SqliteStateDb::open_in_memory().unwrap();
    let path = dir.path().join("truncated.jpg");
    std::fs::write(&path, b"short").unwrap();
    seed_downloaded_for_local_drift_probe(&db, "TRUNCATED_PROBE", &path, 100).await;

    let outcome = run_bounded_local_drift_probe(&db, 1).await;

    assert_eq!(outcome.scanned, 1);
    assert_eq!(outcome.drifted, 1);
    assert_eq!(outcome.marked_failed, 1);
    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(&*failed[0].id, "TRUNCATED_PROBE");
    assert_eq!(
        failed[0].last_error.as_deref(),
        Some(crate::commands::reconcile::FILE_TRUNCATED_REASON)
    );
}
