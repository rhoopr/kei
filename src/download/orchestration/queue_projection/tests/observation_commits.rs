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
async fn prior_inventory_proof_requires_matching_owner_plans_policy_and_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let mut selected = spec();
    selected.profile["current_download_metadata"] = serde_json::json!(1);
    let root = db
        .begin_selection_generation(owner(), selected.clone(), BUDGET)
        .await
        .unwrap();
    admission(&db, &root.id, &dir.path().join("media"), 0, false, BUDGET).await;
    let receipt = db
        .seal_observed_selection(owner(), root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let proof = ObservationProof::inventory(vec![receipt.clone()]).unwrap();
    assert!(
        db.inventory_proof_matches(owner(), proof.clone(), vec![selected.clone()])
            .await
            .unwrap()
    );
    let mut changed_config = selected.clone();
    changed_config.config_hash = "different-config".into();
    assert!(
        !db.inventory_proof_matches(owner(), proof.clone(), vec![changed_config])
            .await
            .unwrap()
    );
    let mut changed_profile = selected.clone();
    changed_profile.profile["recent"] = serde_json::json!(5);
    assert!(
        !db.inventory_proof_matches(owner(), proof.clone(), vec![changed_profile])
            .await
            .unwrap()
    );
    db.set_metadata("config_hash", "changed-policy")
        .await
        .unwrap();
    assert!(
        !db.inventory_proof_matches(owner(), proof.clone(), vec![selected.clone()])
            .await
            .unwrap()
    );
    let other = crate::state::db::account::AccountOwner::authenticated(
        "other@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"other"}})).unwrap(),
    )
    .unwrap();
    assert!(
        !db.inventory_proof_matches(other, proof.clone(), vec![selected])
            .await
            .unwrap()
    );
    let delta = ObservationProof::delivery(receipt)
        .delta("before", "after")
        .unwrap();
    assert!(proof.clone().bridge(delta.clone(), "before").is_some());
    assert!(proof.bridge(delta, "other-boundary").is_none());
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

#[tokio::test]
async fn staged_baseline_revalidation_survives_delta_and_reopen() {
    use sha2::{Digest, Sha256};
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
    let boundary = format!("{:x}", Sha256::digest(b"before"));
    {
        let conn = db.acquire_lock("synthetic baseline round").unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds VALUES('round','PrimarySync',?1,'next-enum',?2,'','','observing')", rusqlite::params![selected.scope, boundary]).unwrap();
    }
    let root = db
        .begin_selection_generation(owner(), selected.clone(), BUDGET)
        .await
        .unwrap();
    admission(&db, &root.id, &dir.path().join("media"), 0, false, BUDGET).await;
    let receipt = db
        .seal_observed_selection(owner(), root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let baseline = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt.clone())
                .delta("before", "after")
                .unwrap(),
            "before",
        )
        .unwrap();
    let staged = "pending_sync_token:v27:next-enum:PrimarySync";
    let mut save = transition(&db, "after").await;
    save.metadata_updates[0].0 = staged.into();
    save.expected_metadata.push((staged.into(), None));
    db.commit_observation_transition(save, Some(baseline), false)
        .await
        .unwrap();
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let delta = ObservationProof::delivery(receipt)
        .delta("after", "quiet")
        .unwrap();
    let mut advance = transition(&db, "quiet").await;
    advance.metadata_updates[0].0 = staged.into();
    advance
        .expected_metadata
        .push((staged.into(), Some("after".into())));
    db.commit_observation_transition(advance, Some(delta), false)
        .await
        .unwrap();
    let mut promote = transition(&db, "quiet").await;
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
        .push((staged.into(), Some("quiet".into())));
    db.commit_observation_transition(promote, None, true)
        .await
        .unwrap();
    drop(db);
    let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    {
        let conn = reopened
            .acquire_lock("verify promoted source round")
            .unwrap();
        let phase: String = conn
            .query_row(
                "SELECT phase FROM source_revalidation_rounds WHERE id='round'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "active");
    }

    // Seed more than two real due pages. Unknown results must yield to later
    // candidates, and an authenticated live result settles only its own debt.
    for index in 0..130 {
        let id = format!("candidate-{index:03}");
        let media = dir.path().join(&id);
        std::fs::write(&media, b"preserved media").unwrap();
        let record = crate::test_helpers::TestAssetRecord::new(&id)
            .library("PrimarySync")
            .build();
        reopened.upsert_seen(&record).await.unwrap();
        reopened
            .mark_downloaded("PrimarySync", &id, "original", &media, "synthetic", None)
            .await
            .unwrap();
        if index == 128 {
            reopened
                .upsert_asset_master_mapping("PrimarySync", &id, "candidate-master")
                .await
                .unwrap();
        }
    }
    {
        let conn = reopened.acquire_lock("seed multiple source pages").unwrap();
        for index in 0..130 {
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES('round',?1)",
                [format!("candidate-{index:03}")],
            )
            .unwrap();
        }
    }
    // Status keeps existing per-rendition verification separate from source
    // obligations, and distinguishes retained stale scopes and policies.
    let verification = crate::test_helpers::TestAssetRecord::new("existing-verification")
        .library("PrimarySync")
        .build();
    reopened.upsert_seen(&verification).await.unwrap();
    reopened
        .set_asset_verification(
            "PrimarySync",
            "existing-verification",
            "original",
            crate::state::AssetVerificationState::Unknown,
            "synthetic inconclusive lookup",
        )
        .await
        .unwrap();
    {
        let conn = reopened.acquire_lock("seed status scope drift").unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds VALUES('stale-scope','PrimarySync','other-scope','next-enum','stale-boundary','','','active')", []).unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds VALUES('stale-policy','PrimarySync',?1,'old-enum','stale-boundary','','','active')", [&selected.scope]).unwrap();
        conn.execute("INSERT INTO source_revalidation_debt(round,id) VALUES('stale-scope','same-asset'),('stale-policy','same-asset')", []).unwrap();
        conn.execute("UPDATE source_revalidation_debt SET next_retry_at=?1 WHERE round='round' AND id='candidate-129'", [chrono::Utc::now().timestamp()+3600]).unwrap();
    }
    let summary = reopened.get_summary().await.unwrap();
    assert_eq!(summary.awaiting_provider_verification, 1);
    assert_eq!(summary.source_revalidation_eligible, 130);
    assert_eq!(summary.source_revalidation_retained, 2);
    {
        let conn = reopened
            .acquire_lock("check pending policy and deferred reporting")
            .unwrap();
        conn.execute_batch("SAVEPOINT synthetic_policy_drift; INSERT INTO metadata(key,value) VALUES('pending_enum_config_hash','different-policy');").unwrap();
        assert_eq!(
            crate::state::db::source_revalidation::summary_counts(&conn).unwrap(),
            (0, 132)
        );
        assert!(!crate::state::db::source_revalidation::has_due(&conn, "PrimarySync").unwrap());
        conn.execute_batch("ROLLBACK TO synthetic_policy_drift; RELEASE synthetic_policy_drift;")
            .unwrap();
        conn.execute("UPDATE source_revalidation_debt SET next_retry_at=0 WHERE round='round' AND id='candidate-129'", []).unwrap();
    }
    assert!(
        reopened
            .due_source_revalidation(owner(), "PrimarySync".into(), "different-scope".into())
            .await
            .unwrap()
            .is_empty()
    );
    for page in 0..3 {
        let due = reopened
            .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
            .await
            .unwrap();
        assert_eq!(due.len(), if page < 2 { 64 } else { 2 });
        for candidate in due {
            let is_live = candidate.id == "candidate-128";
            assert!(
                reopened
                    .settle_source_revalidation(owner(), candidate, None, is_live)
                    .await
                    .unwrap()
            );
        }
    }
    assert!(
        reopened
            .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        std::fs::read(dir.path().join("candidate-128")).unwrap(),
        b"preserved media"
    );
    // A legacy owner's mapping may move while another sibling remains under
    // the old master. That sibling must never settle the owner's debt.
    let legacy_path = dir.path().join("legacy-owner");
    std::fs::write(&legacy_path, b"legacy bytes").unwrap();
    let legacy = crate::test_helpers::TestAssetRecord::new("legacy-master")
        .library("PrimarySync")
        .build();
    reopened.upsert_seen(&legacy).await.unwrap();
    reopened
        .mark_downloaded(
            "PrimarySync",
            "legacy-master",
            "original",
            &legacy_path,
            "synthetic",
            None,
        )
        .await
        .unwrap();
    reopened
        .upsert_asset_master_mapping("PrimarySync", "owner-child", "legacy-master")
        .await
        .unwrap();
    assert!(
        reopened
            .claim_legacy_master_state_owner("PrimarySync", "legacy-master", "owner-child")
            .await
            .unwrap()
    );
    reopened
        .upsert_asset_master_mapping("PrimarySync", "owner-child", "new-master")
        .await
        .unwrap();
    reopened
        .upsert_asset_master_mapping("PrimarySync", "other-child", "legacy-master")
        .await
        .unwrap();
    {
        let conn = reopened.acquire_lock("seed conflicted owner debt").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('round','legacy-master')",
            [],
        )
        .unwrap();
    }
    let mut due = reopened
        .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    let conflicted = due.pop().unwrap();
    assert_eq!(conflicted.id, "legacy-master");
    assert!(conflicted.child.is_none());
    assert!(!conflicted.owned_child);
    reopened
        .settle_source_revalidation(owner(), conflicted, None, true)
        .await
        .unwrap();
    {
        let conn = reopened
            .acquire_lock("verify conflicted owner is pending")
            .unwrap();
        let pending:bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM source_revalidation_debt WHERE round='round' AND id='legacy-master')", [], |r| r.get(0)).unwrap();
        assert!(pending);
        conn.execute(
            "UPDATE source_revalidation_debt SET next_retry_at=0 WHERE id='legacy-master'",
            [],
        )
        .unwrap();
    }
    reopened
        .upsert_asset_master_mapping("PrimarySync", "owner-child", "legacy-master")
        .await
        .unwrap();
    let mut due = reopened
        .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    let owned = due.pop().unwrap();
    assert_eq!(owned.child.as_deref(), Some("owner-child"));
    assert!(owned.owned_child);
    reopened
        .settle_source_revalidation(owner(), owned, None, true)
        .await
        .unwrap();
    assert_eq!(std::fs::read(legacy_path).unwrap(), b"legacy bytes");

    // A unique mapped sibling is still insufficient to identify an unowned
    // master-keyed row. Its live result stays pending until owner evidence exists.
    let unowned_path = dir.path().join("unowned-master");
    std::fs::write(&unowned_path, b"unowned bytes").unwrap();
    let unowned = crate::test_helpers::TestAssetRecord::new("unowned-master")
        .library("PrimarySync")
        .build();
    reopened.upsert_seen(&unowned).await.unwrap();
    reopened
        .mark_downloaded(
            "PrimarySync",
            "unowned-master",
            "original",
            &unowned_path,
            "synthetic",
            None,
        )
        .await
        .unwrap();
    reopened
        .upsert_asset_master_mapping("PrimarySync", "unowned-sibling", "unowned-master")
        .await
        .unwrap();
    {
        let conn = reopened.acquire_lock("seed unowned source debt").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('round','unowned-master')",
            [],
        )
        .unwrap();
    }
    let mut due = reopened
        .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    let unowned_candidate = due.pop().unwrap();
    assert!(unowned_candidate.child.is_none());
    assert!(!unowned_candidate.owned_child);
    reopened
        .settle_source_revalidation(owner(), unowned_candidate, Some((false, None)), false)
        .await
        .unwrap();
    {
        let conn = reopened.acquire_lock("retry unowned source debt").unwrap();
        let deleted: i64 = conn
            .query_row(
                "SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='unowned-master'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(deleted, 0);
        conn.execute(
            "UPDATE source_revalidation_debt SET next_retry_at=0 WHERE id='unowned-master'",
            [],
        )
        .unwrap();
    }
    let mut due = reopened
        .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    reopened
        .settle_source_revalidation(owner(), due.pop().unwrap(), None, true)
        .await
        .unwrap();
    assert_eq!(std::fs::read(unowned_path).unwrap(), b"unowned bytes");
    {
        let conn = reopened.acquire_lock("verify fair source pages").unwrap();
        let (count,status,deleted):(i64,String,i64) = conn.query_row("SELECT (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='round'), status,is_deleted FROM assets WHERE library='PrimarySync' AND id='candidate-128'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!((count, status.as_str(), deleted), (130, "downloaded", 0));
        conn.execute(
            "UPDATE source_revalidation_debt SET next_retry_at=0 WHERE id='candidate-129'",
            [],
        )
        .unwrap();
    }
    let mut selected_before_drift = reopened
        .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
        .await
        .unwrap();
    assert_eq!(selected_before_drift.len(), 1);
    let candidate_before_drift = selected_before_drift.pop().unwrap();
    reopened
        .set_metadata("enum_config_hash", "changed-policy")
        .await
        .unwrap();
    assert!(
        !reopened
            .settle_source_revalidation(owner(), candidate_before_drift, Some((true, None)), false)
            .await
            .unwrap()
    );
    assert!(
        reopened
            .due_source_revalidation(owner(), "PrimarySync".into(), selected.scope.clone())
            .await
            .unwrap()
            .is_empty()
    );
    {
        let conn = reopened
            .acquire_lock("verify scope drift keeps debt")
            .unwrap();
        let (count,deleted):(i64,i64) = conn.query_row("SELECT (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='round'), is_deleted FROM assets WHERE library='PrimarySync' AND id='candidate-129'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((count, deleted), (130, 0));
    }
    let summary = reopened.get_summary().await.unwrap();
    assert_eq!(summary.awaiting_provider_verification, 1);
    assert_eq!(summary.source_revalidation_eligible, 0);
    assert_eq!(summary.source_revalidation_retained, 132);
}

#[tokio::test]
async fn source_revalidation_interrupted_marking_cannot_advance_without_baseline_proof() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    db.set_metadata("enum_config_hash", "policy").await.unwrap();
    let baseline_key = "baseline_cursor:v27:synthetic:policy:";
    db.set_metadata(baseline_key, "before").await.unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("historical")
        .library("PrimarySync")
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "historical",
        "original",
        &dir.path().join("historical"),
        "synthetic",
        None,
    )
    .await
    .unwrap();
    let boundary = format!("{:x}", Sha256::digest(b"before"));
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    db.seed_source_revalidation(
        owner(),
        "PrimarySync".into(),
        "scope".into(),
        "policy".into(),
        boundary.clone(),
        cancelled,
    )
    .await
    .unwrap();
    let mut attempt = transition(&db, "after").await;
    attempt
        .expected_metadata
        .push((baseline_key.into(), Some("before".into())));
    assert!(
        db.commit_observation_transition(attempt.clone(), None, false)
            .await
            .is_err()
    );
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    db.seed_source_revalidation(
        owner(),
        "PrimarySync".into(),
        "scope".into(),
        "policy".into(),
        boundary,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        db.commit_observation_transition(attempt, None, false)
            .await
            .is_err()
    );
    {
        let conn = db.acquire_lock("verify resumed candidate marking").unwrap();
        let (rounds, phase, debt): (i64,String,i64) = conn.query_row("SELECT COUNT(*), MIN(phase), (SELECT COUNT(*) FROM source_revalidation_debt) FROM source_revalidation_rounds", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!((rounds, phase.as_str(), debt), (1, "observing", 1));
    }
    assert!(db.get_metadata(TOKEN).await.unwrap().is_none());
}
