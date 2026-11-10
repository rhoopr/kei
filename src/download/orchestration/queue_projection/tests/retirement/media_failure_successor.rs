//! State-owner mechanism tests using the real two-rendition admission fixture.

use std::sync::Arc;

use rusqlite::params;

use super::{admission, owner, spec};
use crate::state::SqliteStateDb;
use crate::state::db::provider_generations::{
    CapturedSelectionRetry, DeferredMediaSuccessor, MAX_GENERATION_BYTES,
};

struct Fixture {
    dir: tempfile::TempDir,
    db: Arc<SqliteStateDb>,
    root: String,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap(),
        );
        let root = db
            .begin_selection_generation(owner(), spec(), MAX_GENERATION_BYTES)
            .await
            .unwrap();
        let (manifest, tasks, _) =
            admission(&db, &root.id, &media, 0, true, MAX_GENERATION_BYTES).await;
        assert_eq!(tasks.len(), 2, "real selected Live Photo renditions");
        assert_eq!(manifest.decision.destinations.len(), 2);
        Self {
            dir,
            db,
            root: root.id,
        }
    }

    async fn captures(&self) -> Vec<CapturedSelectionRetry> {
        let identity = self
            .db
            .selection_retry_identity(
                owner(),
                self.root.clone(),
                "pass".into(),
                "asset-compact-0".into(),
            )
            .await
            .unwrap()
            .unwrap();
        identity
            .destinations
            .iter()
            .map(|destination| CapturedSelectionRetry {
                generation: identity.generation.clone(),
                specification_hash: identity.specification_hash.clone(),
                pass_key: identity.pass_key.clone(),
                child: identity.child.clone(),
                state_id: identity.state_id.clone(),
                manifest_hash: identity.manifest_hash.clone(),
                admission_hash: identity.admission_hash.clone(),
                destination: destination.clone(),
            })
            .collect()
    }

    fn decision(&self) -> (String, i64, i64, String) {
        self.db.acquire_lock("media successor decision").unwrap().query_row(
            "SELECT reason,attempts,next_retry_at,admission_hash FROM provider_active_decisions WHERE generation=?1 AND pass_key='pass' AND child='asset-compact-0'",
            [&self.root], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).unwrap()
    }
}

fn assert_successor(
    successor: &DeferredMediaSuccessor,
    input: &CapturedSelectionRetry,
    state: &(String, i64, i64, String),
) {
    assert_eq!(successor.captured.generation, input.generation);
    assert_eq!(
        successor.captured.specification_hash,
        input.specification_hash
    );
    assert_eq!(successor.captured.pass_key, input.pass_key);
    assert_eq!(successor.captured.child, input.child);
    assert_eq!(successor.captured.state_id, input.state_id);
    assert_eq!(successor.captured.manifest_hash, input.manifest_hash);
    assert_eq!(successor.captured.admission_hash, input.admission_hash);
    assert_eq!(successor.captured.destination, input.destination);
    assert_eq!(successor.admission, "admitted");
    assert_eq!(successor.reason, state.0);
    assert_eq!(successor.attempts, state.1);
    assert_eq!(successor.next_retry_at, state.2);
    assert_eq!(successor.committed_admission_hash, state.3);
}

#[tokio::test]
async fn grouped_successors_one_write_for_two_admitted_renditions() {
    let f = Fixture::new().await;
    let captures = f.captures().await;
    assert_eq!(captures.len(), 2);
    assert_eq!(captures[0].admission_hash, captures[1].admission_hash);
    assert_ne!(captures[0].destination, captures[1].destination);
    let before = f.decision();
    let successors =
        f.db.defer_media_failure_successors(owner(), captures.clone())
            .await
            .unwrap()
            .unwrap();
    let after = f.decision();
    assert_eq!(after.0, "download_failed");
    assert_eq!(
        after.1,
        before.1 + 1,
        "one decision write, not one per rendition"
    );
    assert_eq!(successors.len(), 2);
    for (successor, captured) in successors.iter().zip(&captures) {
        assert_successor(successor, captured, &after);
    }
    assert!(
        f.db.defer_media_failure_successors(owner(), captures)
            .await
            .unwrap()
            .is_none(),
        "stale prehash must refuse"
    );
    assert_eq!(f.decision(), after);
}

#[tokio::test]
async fn grouped_successors_never_grant_an_unsupplied_sibling() {
    let f = Fixture::new().await;
    let captures = f.captures().await;
    let result =
        f.db.defer_media_failure_successors(owner(), vec![captures[0].clone()])
            .await
            .unwrap()
            .unwrap();
    assert_eq!(result.len(), 1);
    assert_successor(&result[0], &captures[0], &f.decision());
    assert_ne!(result[0].captured.destination, captures[1].destination);
    let rows: i64 = f.db.acquire_lock("unverified sibling").unwrap().query_row(
        "SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND pass_key='pass' AND child='asset-compact-0' AND admitted=1 AND verified_media=0", [&f.root], |r| r.get(0),
    ).unwrap();
    assert_eq!(rows, 2);
}

#[tokio::test]
async fn grouped_successors_refuse_duplicates_drift_and_foreign_owner() {
    let f = Fixture::new().await;
    let captures = f.captures().await;
    let before = f.decision();
    let mut bad_metadata = captures[1].clone();
    bad_metadata.destination.metadata_hash.push('x');
    let mut bad_checksum = captures[1].clone();
    bad_checksum.destination.checksum.push('x');
    let mut wrong_root = captures[1].clone();
    wrong_root.generation.push('x');
    let mut stale = captures[1].clone();
    stale.admission_hash.push('x');
    for input in [
        vec![],
        vec![captures[0].clone(), captures[0].clone()],
        vec![captures[0].clone(), bad_metadata],
        vec![captures[0].clone(), bad_checksum],
        vec![captures[0].clone(), wrong_root],
        vec![captures[0].clone(), stale],
    ] {
        assert!(
            f.db.defer_media_failure_successors(owner(), input)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(f.decision(), before);
    }
    let unpinned =
        crate::state::db::account::AccountOwner::configured("foreign@example.invalid", "com");
    assert!(
        f.db.defer_media_failure_successors(unpinned, captures.clone())
            .await
            .is_err()
    );
    let foreign = crate::state::db::account::AccountOwner::authenticated(
        "queue-stage@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"foreign-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(
        f.db.defer_media_failure_successors(foreign, captures)
            .await
            .is_err()
    );
    assert_eq!(f.decision(), before);
}

#[tokio::test]
async fn grouped_successors_refuse_prepared_and_frozen_destination_faults() {
    for fault in ["prepared", "frozen"] {
        let f = Fixture::new().await;
        let captures = f.captures().await;
        let before = f.decision();
        let path = captures[1].destination.path.key().unwrap();
        {
            let conn =
                f.db.acquire_lock("inject unfinished publication fault")
                    .unwrap();
            if fault == "prepared" {
                conn.execute("UPDATE provider_active_destinations SET prepared_checksum=?1,prepared_size=1,prepared_hash=?1 WHERE generation=?2 AND pass_key='pass' AND child='asset-compact-0' AND version_size=?3 AND path=?4", params!["a".repeat(64), f.root, captures[1].destination.version_size, path]).unwrap();
            } else {
                conn.execute("UPDATE provider_active_destinations SET frozen_metadata=X'01',frozen_metadata_hash=X'02' WHERE generation=?1 AND pass_key='pass' AND child='asset-compact-0' AND version_size=?2 AND path=?3", params![f.root, captures[1].destination.version_size, path]).unwrap();
            }
        }
        assert!(
            f.db.defer_media_failure_successors(owner(), captures)
                .await
                .unwrap()
                .is_none(),
            "{fault}"
        );
        assert_eq!(f.decision(), before, "{fault} must not write backoff");
    }
}

#[tokio::test]
async fn grouped_successors_write_fault_rolls_back_and_reopens() {
    let f = Fixture::new().await;
    let captures = f.captures().await;
    let before = f.decision();
    f.db.acquire_lock("inject failed backoff write").unwrap().execute_batch(
        "CREATE TRIGGER fail_media_backoff BEFORE UPDATE OF admission_hash ON provider_active_decisions WHEN OLD.attempts=1 AND OLD.reason='download_failed' BEGIN SELECT RAISE(ABORT,'injected post-update hash write failure'); END;",
    ).unwrap();
    assert!(
        f.db.defer_media_failure_successors(owner(), captures)
            .await
            .is_err()
    );
    assert_eq!(f.decision(), before);
    let Fixture { dir, db, root } = f;
    assert_eq!(Arc::strong_count(&db), 1);
    drop(db);
    let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    let after: (String, i64, i64, String) = reopened.acquire_lock("reopened failed backoff").unwrap().query_row(
        "SELECT reason,attempts,next_retry_at,admission_hash FROM provider_active_decisions WHERE generation=?1 AND pass_key='pass' AND child='asset-compact-0'", [root], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).unwrap();
    assert_eq!(after, before);
}

#[tokio::test]
async fn grouped_successors_postwrite_destination_drift_rolls_back() {
    let f = Fixture::new().await;
    let captures = f.captures().await;
    let before = f.decision();
    f.db.acquire_lock("inject postwrite destination drift").unwrap().execute_batch(
        "CREATE TRIGGER drift_media_after_backoff AFTER UPDATE OF attempts ON provider_active_decisions BEGIN UPDATE provider_active_destinations SET frozen_metadata=X'01' WHERE generation=NEW.generation AND pass_key=NEW.pass_key AND child=NEW.child; END;",
    ).unwrap();
    assert!(
        f.db.defer_media_failure_successors(owner(), captures)
            .await
            .is_err()
    );
    assert_eq!(f.decision(), before);
    let frozen: i64 = f.db.acquire_lock("verify destination drift rollback").unwrap().query_row(
        "SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND frozen_metadata IS NOT NULL", [&f.root], |row| row.get(0),
    ).unwrap();
    assert_eq!(frozen, 0);
}

#[tokio::test]
async fn grouped_successors_accept_saturated_same_second_equal_successor() {
    let f = Fixture::new().await;
    for expected in 1..=32 {
        let captures = f.captures().await;
        let result =
            f.db.defer_media_failure_successors(owner(), captures)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].attempts, expected);
    }
    let mut equal = false;
    for _ in 0..20 {
        let before = f.decision();
        let captures = f.captures().await;
        let result =
            f.db.defer_media_failure_successors(owner(), captures.clone())
                .await
                .unwrap()
                .unwrap();
        let after = f.decision();
        assert_eq!(after.1, 32);
        for (successor, input) in result.iter().zip(&captures) {
            assert_successor(successor, input, &after);
        }
        if before == after {
            equal = true;
            break;
        }
    }
    assert!(
        equal,
        "two real same-second saturated updates must admit equal successors"
    );
}

#[tokio::test]
async fn legacy_boolean_keeps_original_destination_match_and_stale_cas() {
    let f = Fixture::new().await;
    let mut captured = f.captures().await.remove(0);
    captured.destination.metadata_hash.push('x');
    assert!(
        f.db.defer_media_failure_successors(owner(), vec![captured.clone()])
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.db.defer_selection_media_failure(owner(), captured.clone())
            .await
            .unwrap(),
        "existing boolean caller compares the original four destination fields"
    );
    assert!(
        !f.db
            .defer_selection_media_failure(owner(), captured)
            .await
            .unwrap()
    );
    assert_eq!(f.decision().1, 1);
}
