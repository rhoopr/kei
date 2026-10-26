//! Observation certificates share the checkpoint transaction and policy fences.
use super::owner;
use super::retirement::{BUDGET, admission, spec};
use crate::state::db::provider_generations::observation::ObservationProof;
use crate::state::{CheckpointTransition, SqliteStateDb};
use std::sync::Arc;

const TOKEN: &str = "sync_token:v27:PrimarySync";
const PROOF: &str = "observed_selection:v27:PrimarySync";

async fn transition(db: &SqliteStateDb, token: &str) -> CheckpointTransition {
    let mut expected_metadata = Vec::new();
    for key in [
        "enum_config_hash",
        "pending_enum_config_hash",
        "config_hash",
        "pending_download_config_hash",
        TOKEN,
    ] {
        expected_metadata.push((key.to_owned(), db.get_metadata(key).await.unwrap()));
    }
    CheckpointTransition {
        expected_metadata,
        legacy_preservation_proofs: vec![],
        legacy_config_hash: None,
        sparse_identity_proofs: vec![],
        metadata_updates: vec![(TOKEN.into(), token.into())],
        metadata_deletes: vec![],
    }
}

async fn fixture(dir: &std::path::Path) -> (Arc<SqliteStateDb>, ObservationProof) {
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut selected = spec();
    selected.profile["current_download_metadata"] = serde_json::json!(1);
    let root = db
        .begin_selection_generation(owner(), selected, BUDGET)
        .await
        .unwrap();
    admission(&db, &root.id, &dir.join("media"), 0, false, BUDGET).await;
    let receipt = db
        .seal_observed_selection(owner(), root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta("before", "after")
                .unwrap(),
            "before",
        )
        .unwrap();
    (db, proof)
}

#[tokio::test]
async fn observation_commit_pending_reopen_and_generic_token_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
        .await
        .unwrap();
    let saved = db.get_metadata(PROOF).await.unwrap().unwrap();
    assert!(!saved.contains("\"after\""));
    let status = db.observed_selection_status().await.unwrap();
    assert_eq!(status.len(), 1);
    assert!(status[0].pending_media);
    assert!(!status[0].settlement_unknown);
    drop(db);
    let db = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(PROOF).await.unwrap().as_deref(),
        Some(saved.as_str())
    );
    db.set_metadata(TOKEN, "after").await.unwrap();
    assert!(db.get_metadata(PROOF).await.unwrap().is_none());
    assert!(db.observed_selection_status().await.unwrap().is_empty());
}

#[tokio::test]
async fn observation_commit_descriptor_failure_rolls_back_token_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.set_metadata(TOKEN, "old").await.unwrap();
    {
        let conn = db.acquire_lock("inject observation write failure").unwrap();
        conn.execute_batch("CREATE TRIGGER refuse_observation BEFORE INSERT ON metadata WHEN NEW.key LIKE 'observed_selection:%' BEGIN SELECT RAISE(ABORT,'injected observation failure'); END;").unwrap();
    }
    assert!(
        db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
            .await
            .is_err()
    );
    drop(db);
    let db = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("old")
    );
    assert!(db.get_metadata(PROOF).await.unwrap().is_none());
}

#[tokio::test]
async fn observation_commit_old_policy_cannot_certify_changed_policy() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.set_metadata("config_hash", "new-policy").await.unwrap();
    db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("after")
    );
    assert!(db.get_metadata(PROOF).await.unwrap().is_none());
}

#[tokio::test]
async fn observation_commit_quiet_extension_requires_matching_predecessor() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
        .await
        .unwrap();
    let mut selected = spec();
    selected.profile["current_download_metadata"] = serde_json::json!(1);
    let receipt = db
        .empty_observation_admission(owner(), selected)
        .await
        .unwrap();
    assert!(ObservationProof::inventory(vec![receipt.clone()]).is_none());
    assert!(
        ObservationProof::delivery(receipt.clone())
            .restarted_inventory()
            .is_none()
    );
    let delta = ObservationProof::delivery(receipt.clone())
        .delta("after", "quiet")
        .unwrap();
    db.commit_observation_transition(transition(&db, "quiet").await, Some(delta), false)
        .await
        .unwrap();
    assert!(db.get_metadata(PROOF).await.unwrap().is_some());
    db.set_metadata(TOKEN, "quiet").await.unwrap();
    let delta = ObservationProof::delivery(receipt)
        .delta("quiet", "later")
        .unwrap();
    db.commit_observation_transition(transition(&db, "later").await, Some(delta), false)
        .await
        .unwrap();
    assert!(db.get_metadata(PROOF).await.unwrap().is_none());
}

#[tokio::test]
async fn observation_commit_staged_proof_activates_only_with_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    db.set_metadata("pending_enum_config_hash", "next-enum")
        .await
        .unwrap();
    db.set_metadata("pending_download_config_hash", "next-path")
        .await
        .unwrap();
    let mut selected = spec();
    selected.profile["current_download_metadata"] = serde_json::json!(1);
    let root = db
        .begin_selection_generation(owner(), selected, BUDGET)
        .await
        .unwrap();
    admission(&db, &root.id, &dir.path().join("media"), 0, false, BUDGET).await;
    let receipt = db
        .seal_observed_selection(owner(), root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta("before", "after")
                .unwrap(),
            "before",
        )
        .unwrap();
    let staged = "pending_sync_token:v27:next-enum:PrimarySync";
    let mut save = transition(&db, "after").await;
    save.metadata_updates[0].0 = staged.into();
    save.expected_metadata.push((staged.into(), None));
    db.commit_observation_transition(save, Some(proof), false)
        .await
        .unwrap();
    assert!(db.observed_selection_status().await.unwrap().is_empty());
    assert!(
        db.get_metadata("observed_selection_staged:v27:next-enum:PrimarySync")
            .await
            .unwrap()
            .is_some()
    );
    let mut expected_plan = spec();
    expected_plan.profile["current_download_metadata"] = serde_json::json!(1);
    assert!(
        db.current_observation_matches(owner(), staged.into(), vec![expected_plan.clone()])
            .await
            .unwrap()
    );
    let staged_proof_key = "observed_selection_staged:v27:next-enum:PrimarySync";
    let descriptor = db.get_metadata(staged_proof_key).await.unwrap().unwrap();
    db.set_metadata("pending_download_config_hash", "different-path")
        .await
        .unwrap();
    db.set_metadata(staged_proof_key, &descriptor)
        .await
        .unwrap();
    assert!(
        !db.current_observation_matches(owner(), staged.into(), vec![expected_plan.clone()])
            .await
            .unwrap()
    );
    db.set_metadata("pending_download_config_hash", "next-path")
        .await
        .unwrap();
    db.set_metadata(staged_proof_key, &descriptor)
        .await
        .unwrap();
    let mut promote = transition(&db, "after").await;
    promote.metadata_updates.extend([
        ("enum_config_hash".into(), "next-enum".into()),
        ("config_hash".into(), "next-path".into()),
    ]);
    promote.metadata_deletes = vec![
        "pending_enum_config_hash".into(),
        "pending_download_config_hash".into(),
        staged.into(),
    ];
    promote
        .expected_metadata
        .push((staged.into(), Some("after".into())));
    db.commit_observation_transition(promote, None, true)
        .await
        .unwrap();
    assert_eq!(db.observed_selection_status().await.unwrap().len(), 1);
    assert!(db.get_metadata(PROOF).await.unwrap().is_some());
    db.set_metadata("config_hash", "different-path")
        .await
        .unwrap();
    assert!(db.get_metadata(PROOF).await.unwrap().is_none());
}

#[tokio::test]
async fn observation_commit_token_deletion_and_wrong_predecessor_remain_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
        .await
        .unwrap();
    let mut selected = spec();
    selected.profile["current_download_metadata"] = serde_json::json!(1);
    let receipt = db
        .empty_observation_admission(owner(), selected)
        .await
        .unwrap();
    let delta = ObservationProof::delivery(receipt)
        .delta("wrong", "later")
        .unwrap();
    db.commit_observation_transition(transition(&db, "later").await, Some(delta), false)
        .await
        .unwrap();
    assert!(db.get_metadata(PROOF).await.unwrap().is_none());
    db.delete_metadata_by_prefix("sync_token:v27:")
        .await
        .unwrap();
    assert!(db.get_metadata(TOKEN).await.unwrap().is_none());
    assert!(db.observed_selection_status().await.unwrap().is_empty());
}

#[tokio::test]
async fn observation_commit_deleting_token_deletes_existing_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
        .await
        .unwrap();
    assert!(db.get_metadata(PROOF).await.unwrap().is_some());
    db.delete_metadata_by_prefix("sync_token:v27:")
        .await
        .unwrap();
    assert!(db.get_metadata(PROOF).await.unwrap().is_none());
    assert!(db.get_metadata(TOKEN).await.unwrap().is_none());
}

#[tokio::test]
async fn observation_commit_certificate_cannot_move_to_another_library() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
        .await
        .unwrap();
    let saved = db.get_metadata(PROOF).await.unwrap().unwrap();
    db.set_metadata("sync_token:v27:SharedSync-other", "after")
        .await
        .unwrap();
    db.set_metadata("observed_selection:v27:SharedSync-other", &saved)
        .await
        .unwrap();
    let observed = db.observed_selection_status().await.unwrap();
    assert_eq!(
        observed.len(),
        1,
        "copied proof must not certify another library"
    );
    assert_eq!(observed[0].selector_label, "Primary library");
}

#[tokio::test]
async fn observation_resume_requires_exact_plans_and_current_policy_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (db, proof) = fixture(dir.path()).await;
    db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
        .await
        .unwrap();
    drop(db);
    let db = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    let mut selected = spec();
    selected.profile["current_download_metadata"] = serde_json::json!(1);
    assert!(
        db.current_observation_matches(owner(), TOKEN.into(), vec![selected.clone()])
            .await
            .unwrap()
    );
    let mut changed = selected.clone();
    changed.profile["passes"] = serde_json::json!([]);
    assert!(
        !db.current_observation_matches(owner(), TOKEN.into(), vec![changed])
            .await
            .unwrap()
    );
    let mut source = selected.clone();
    source.scope.push_str("-different-source");
    assert!(
        !db.current_observation_matches(owner(), TOKEN.into(), vec![selected.clone(), source])
            .await
            .unwrap()
    );
    assert!(
        !db.current_observation_matches(
            owner(),
            TOKEN.into(),
            vec![selected.clone(), selected.clone()]
        )
        .await
        .unwrap()
    );
    assert!(
        !db.current_observation_matches(owner(), TOKEN.into(), vec![])
            .await
            .unwrap()
    );
    let descriptor = db.get_metadata(PROOF).await.unwrap().unwrap();
    db.set_metadata("pending_download_config_hash", "changed-policy")
        .await
        .unwrap();
    // A stale descriptor cannot authorize resume even if retained/corrupt old
    // state restores its bytes after policy invalidation.
    db.set_metadata(PROOF, &descriptor).await.unwrap();
    assert!(
        !db.current_observation_matches(owner(), TOKEN.into(), vec![selected])
            .await
            .unwrap()
    );
}
