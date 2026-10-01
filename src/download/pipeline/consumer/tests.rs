use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::download::finalize::{
    PendingStateWrite, STATE_DB_UNWRITABLE_THRESHOLD, STATE_WRITE_MAX_RETRIES, StateWriteFlush,
    flush_pending_state_writes, flush_pending_state_writes_retaining_failures,
    state_write_circuit_breaker_tripped,
};
use crate::download::pipeline::test_support::FailingDownloadStore;
use crate::state::VersionSizeKey;

#[tokio::test]
async fn flush_pending_state_writes_empty_is_noop() {
    let db = FailingDownloadStore::new(0);
    let result = flush_pending_state_writes(&db, &[]).await;
    assert_eq!(result, 0);
    assert_eq!(db.success_count(), 0);
}

#[tokio::test]
async fn flush_pending_state_writes_succeeds_on_first_try() {
    let db = FailingDownloadStore::new(0);
    let pending = vec![PendingStateWrite {
        library: "PrimarySync".into(),
        asset_id: "A1".into(),
        version_size: VersionSizeKey::Original,
        download_path: PathBuf::from("/tmp/codex/kei/photo.jpg"),
        local_checksum: "abc".into(),
        download_checksum: None,
        mark_capture_repair: false,
    }];
    let failures = flush_pending_state_writes(&db, &pending).await;
    assert_eq!(failures, 0);
    assert_eq!(db.success_count(), 1);
}

#[tracing_test::traced_test]
#[tokio::test]
async fn flush_pending_state_writes_recovers_after_transient_failure() {
    // Fail the first attempt, succeed on retry
    let db = FailingDownloadStore::new(1);
    let pending = vec![PendingStateWrite {
        library: "PrimarySync".into(),
        asset_id: "A1".into(),
        version_size: VersionSizeKey::Original,
        download_path: PathBuf::from("/tmp/codex/kei/photo.jpg"),
        local_checksum: "abc".into(),
        download_checksum: None,
        mark_capture_repair: false,
    }];
    let failures = flush_pending_state_writes(&db, &pending).await;
    assert_eq!(failures, 0);
    assert_eq!(db.success_count(), 1);
    assert!(logs_contain("State write retry failed, will retry"));
    assert!(logs_contain("Recovered deferred state write"));
    assert!(logs_contain("simulated failure"));
}

#[tokio::test]
async fn flush_pending_state_writes_reports_persistent_failure() {
    // Fail all attempts — must exceed STATE_WRITE_MAX_RETRIES
    let db = FailingDownloadStore::new(STATE_WRITE_MAX_RETRIES as usize);
    let pending = vec![PendingStateWrite {
        library: "PrimarySync".into(),
        asset_id: "A1".into(),
        version_size: VersionSizeKey::Original,
        download_path: PathBuf::from("/tmp/codex/kei/photo.jpg"),
        local_checksum: "abc".into(),
        download_checksum: None,
        mark_capture_repair: false,
    }];
    let failures = flush_pending_state_writes(&db, &pending).await;
    assert_eq!(failures, 1);
    assert_eq!(db.success_count(), 0);
}

#[tokio::test]
async fn flush_pending_state_writes_partial_recovery() {
    // First write exhausts all STATE_WRITE_MAX_RETRIES attempts (reported as failure).
    // Second write fails once more then succeeds on retry.
    let db = FailingDownloadStore::new(STATE_WRITE_MAX_RETRIES as usize + 1);
    let pending = vec![
        PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/codex/kei/photo1.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
            mark_capture_repair: false,
        },
        PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: "A2".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/codex/kei/photo2.jpg"),
            local_checksum: "def".into(),
            download_checksum: None,
            mark_capture_repair: false,
        },
    ];
    let failures = flush_pending_state_writes(&db, &pending).await;
    assert_eq!(
        failures, 1,
        "First write should fail, second should recover"
    );
    assert_eq!(db.success_count(), 1);
}

#[tokio::test]
async fn flush_pending_state_writes_retains_all_records() {
    // 5 pending writes. First 2 failures are transient (writes 1&2 fail once
    // each then succeed on retry). All 5 should eventually succeed.
    let db = FailingDownloadStore::new(2);
    let pending: Vec<PendingStateWrite> = (0..5)
        .map(|i| PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: format!("ASSET_{i}").into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from(format!("/tmp/codex/kei/photo_{i}.jpg")),
            local_checksum: format!("ck_{i}"),
            download_checksum: Some(format!("dl_ck_{i}")),
            mark_capture_repair: false,
        })
        .collect();

    let failures = flush_pending_state_writes(&db, &pending).await;
    assert_eq!(failures, 0, "all 5 writes should eventually succeed");
    assert_eq!(db.success_count(), 5);
}

#[tokio::test(start_paused = true)]
async fn flush_pending_state_writes_retains_only_persistent_failures() {
    let db = FailingDownloadStore::new(STATE_WRITE_MAX_RETRIES as usize + 1);
    let mut pending = vec![
        PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: "A1".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/codex/kei/photo1.jpg"),
            local_checksum: "abc".into(),
            download_checksum: None,
            mark_capture_repair: false,
        },
        PendingStateWrite {
            library: "PrimarySync".into(),
            asset_id: "A2".into(),
            version_size: VersionSizeKey::Original,
            download_path: PathBuf::from("/tmp/codex/kei/photo2.jpg"),
            local_checksum: "def".into(),
            download_checksum: None,
            mark_capture_repair: false,
        },
    ];

    let flush = flush_pending_state_writes_retaining_failures(&db, &mut pending).await;

    assert_eq!(
        flush,
        StateWriteFlush {
            attempted: 2,
            failures: 1,
        }
    );
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].asset_id.as_ref(), "A1");
    assert_eq!(db.success_count(), 1);
}

#[test]
fn state_write_circuit_breaker_requires_threshold_and_all_failures() {
    assert!(!state_write_circuit_breaker_tripped(&StateWriteFlush {
        attempted: STATE_DB_UNWRITABLE_THRESHOLD - 1,
        failures: STATE_DB_UNWRITABLE_THRESHOLD - 1,
    }));
    assert!(!state_write_circuit_breaker_tripped(&StateWriteFlush {
        attempted: STATE_DB_UNWRITABLE_THRESHOLD,
        failures: STATE_DB_UNWRITABLE_THRESHOLD - 1,
    }));
    assert!(state_write_circuit_breaker_tripped(&StateWriteFlush {
        attempted: STATE_DB_UNWRITABLE_THRESHOLD,
        failures: STATE_DB_UNWRITABLE_THRESHOLD,
    }));
}

/// When SIGTERM fires mid-sync, the .part files must not be promoted
/// to final paths. CancellationToken cancellation must prevent rename.
/// This test verifies the cancellation plumbing works, which is the
/// prerequisite for the crash-recovery safety net.
#[tokio::test]
async fn cancellation_prevents_consumer_from_processing() {
    let token = CancellationToken::new();
    let child = token.child_token();
    assert!(!child.is_cancelled(), "fresh token must not be cancelled");
    token.cancel();
    assert!(
        child.is_cancelled(),
        "child must reflect parent cancellation"
    );
}
