//! Observation certificates share the checkpoint transaction and policy fences.
use super::owner;
use super::retirement::{BUDGET, admission, spec};
use crate::state::db::provider_generations::observation::{
    ObservationProof, attach_revalidation_round_for_test, catalog_plan_digest_from_observation,
};
use crate::state::db::source_revalidation::{
    CatalogOrigin, CatalogPairEvidence, CatalogSettlementAuthority, quarantine_all_rounds,
};
use crate::state::{CheckpointTransition, SqliteStateDb};
use serde_json::Value;
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
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('round','PrimarySync',?1,'next-enum',?2,'','','observing')", rusqlite::params![selected.scope, boundary]).unwrap();
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
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('stale-scope','PrimarySync','other-scope','next-enum','stale-boundary','','','active')", []).unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES('stale-policy','PrimarySync',?1,'old-enum','stale-boundary','','','active')", [&selected.scope]).unwrap();
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

/// Baseline-reader evidence for the proposed catalog scope codec.
///
/// The pre-change producer and drain paths pass the ordinary selected plan
/// scope (`root.spec.scope` / the Unfiled download scope). A catalog round
/// tagged with its own versioned scope can therefore never be retired by an
/// ordinary confirmation or drained by the old Unfiled path, even when the
/// asset ID matches. This is conservative retention, not deletion authority.
/// The test uses the baseline methods themselves rather than assuming the
/// property from the tag spelling.
#[tokio::test]
async fn catalog_tag_scope_survives_baseline_unfiled_reader_paths() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    let catalog = "catalog:v1:0123456789abcdef";
    {
        let conn = db.acquire_lock("seed catalog-tag reader evidence").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('catalog-round','PrimarySync',?1,'policy','boundary','','','observing','catalog_v2')",
            [catalog],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('catalog-round','asset')",
            [],
        )
        .unwrap();
    }
    // The ordinary producer confirmation passes the current selected-plan
    // scope, not the catalog tag.
    db.source_revalidation_seen(
        owner(),
        "PrimarySync".into(),
        "ordinary-selected-scope".into(),
        "asset".into(),
    )
    .await
    .unwrap();
    {
        let conn = db
            .acquire_lock("catalog debt survives ordinary confirm")
            .unwrap();
        let debt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt WHERE round='catalog-round'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            debt, 1,
            "ordinary selected-plan confirmation cannot retire catalog-tag debt"
        );
    }
    // The baseline Unfiled drain is scoped to the ordinary plan scope too.
    assert!(
        db.due_source_revalidation(
            owner(),
            "PrimarySync".into(),
            "ordinary-selected-scope".into()
        )
        .await
        .unwrap()
        .is_empty(),
        "baseline Unfiled drain cannot take catalog-tag work"
    );
    // The catalog debt also remains after a second ordinary selected-plan
    // confirmation. Product isolation must additionally reject the tag itself
    // in `source_revalidation_seen`; this baseline evidence already shows the
    // ordinary producer cannot reach it.
    db.source_revalidation_seen(
        owner(),
        "PrimarySync".into(),
        "ordinary-selected-scope".into(),
        "asset".into(),
    )
    .await
    .unwrap();
    let conn = db
        .acquire_lock("catalog debt remains after repeat confirm")
        .unwrap();
    let debt: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM source_revalidation_debt WHERE round='catalog-round'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(debt, 1);
}

/// Baseline-key ABA on the actual old writer primitives.
///
/// `sync_cycle.rs:1859-1861` writes the baseline key; `sync_cycle.rs:2139`
/// deletes it before a rejected-boundary retry; the next attempt writes the
/// same key and cursor again. The old writer knows nothing about catalog
/// rounds, so an observing catalog origin with the same library/boundary
/// survives untouched. This test executes that exact durable write/delete/
/// rewrite sequence with the actual state-owner methods and shows the new
/// reader cannot distinguish same-origin resume from the recreated key.
#[tokio::test]
async fn old_writer_baseline_key_aba_leaves_observing_catalog_origin_indistinguishable() {
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let cursor = "same-provider-boundary";
    let boundary = format!("{:x}", Sha256::digest(cursor.as_bytes()));
    let baseline_key = "baseline_cursor:v27:synthetic:policy:";
    let epoch = "11111111-1111-1111-1111-111111111111";
    let attempt = "22222222-2222-2222-2222-222222222222";
    let nonce = "33333333-3333-3333-3333-333333333333";
    let scope = format!("catalog:v2:{}:{epoch}:{attempt}:{nonce}", "a".repeat(64));
    let epoch_key = crate::state::db::sync_epoch::library_key("PrimarySync");
    let descriptor_key = "observed_selection:v27:PrimarySync";
    db.set_metadata(&epoch_key, "epoch-snapshot").await.unwrap();
    db.set_metadata(descriptor_key, "descriptor-snapshot")
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("seed surviving catalog origin").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase) VALUES(?1,'PrimarySync',?2,'policy',?3,'','','observing')",
            rusqlite::params![nonce, scope, boundary],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'asset')",
            [nonce],
        )
        .unwrap();
    }
    let durable = |db: &SqliteStateDb| {
        let conn = db.acquire_lock("snapshot ABA state").unwrap();
        let key: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [baseline_key],
                |row| row.get(0),
            )
            .ok();
        let epoch: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&epoch_key],
                |row| row.get(0),
            )
            .ok();
        let descriptor: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [descriptor_key],
                |row| row.get(0),
            )
            .ok();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1)",
                [nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        (key, phase, debt, epoch, descriptor)
    };

    // Old writer first attempt: write the key.
    db.set_metadata(baseline_key, cursor).await.unwrap();
    let before = durable(db.as_ref());
    assert_eq!(before.0.as_deref(), Some(cursor));
    // Old writer rejected-boundary path deletes the key.
    assert_eq!(db.delete_metadata_by_prefix(baseline_key).await.unwrap(), 1);
    assert_eq!(durable(db.as_ref()).0, None);
    // Next old writer baseline recreates the exact same key and cursor.
    db.set_metadata(baseline_key, cursor).await.unwrap();
    drop(db);

    let reopened = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
    let after = durable(&reopened);
    assert_eq!(after.0.as_deref(), Some(cursor));
    assert_eq!(after.1, "observing");
    assert_eq!(after.2, 1);
    assert_eq!(after.3, before.3);
    assert_eq!(after.4, before.4);
    // The proposed same-origin predicate sees the recreated key plus exactly
    // one compatible live origin with no marker showing which write owns it.
    let compatible = {
        let conn = reopened
            .acquire_lock("check same-origin predicate")
            .unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM source_revalidation_rounds r
             WHERE r.library='PrimarySync' AND r.boundary=?1
               AND r.phase='observing' AND r.scope LIKE 'catalog:v2:%'
               AND EXISTS(SELECT 1 FROM source_revalidation_debt d WHERE d.round=r.id)",
            [boundary],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    eprintln!(
        "B-ABA key_present={} round_phase={} debt={} compatible_live_origins={compatible}",
        after.0.is_some(),
        after.1,
        after.2
    );
    assert_eq!(compatible, 1);
}

#[tokio::test]
async fn catalog_v2_linked_staged_promotion_reopen_rollback_and_quarantine() {
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    db.set_metadata("pending_enum_config_hash", "next-enum")
        .await
        .unwrap();
    db.set_metadata("pending_download_config_hash", "next-path")
        .await
        .unwrap();
    let mut selected = spec();
    selected.profile["current_download_metadata"] = serde_json::json!(1);
    let boundary = format!("{:x}", Sha256::digest(b"before"));
    let catalog = format!(
        "catalog:v2:{}:11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222:33333333-3333-3333-3333-333333333333",
        "a".repeat(64)
    );
    {
        let conn = db
            .acquire_lock("seed linked staged catalog origin")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('catalog','PrimarySync',?1,'next-enum',?2,'','','observing','catalog_v2')",
            rusqlite::params![catalog, boundary],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('catalog','asset')",
            [],
        )
        .unwrap();
    }
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
    let receipt_for_delta = receipt.clone();
    let baseline = ObservationProof::inventory(vec![receipt.clone()])
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
    db.commit_observation_transition(save, Some(baseline), false)
        .await
        .unwrap();
    let staged_proof = "observed_selection_staged:v27:next-enum:PrimarySync";
    {
        let conn = db
            .acquire_lock("attach catalog link to staged proof")
            .unwrap();
        assert!(
            attach_revalidation_round_for_test(&conn, staged_proof, "after", "catalog").unwrap()
        );
    }
    drop(db);

    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let staged_json: Value =
        serde_json::from_str(&db.get_metadata(staged_proof).await.unwrap().unwrap()).unwrap();
    assert_eq!(staged_json["revalidation_round"], "catalog");

    let delta = ObservationProof::delivery(receipt_for_delta)
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
    let staged_json: Value =
        serde_json::from_str(&db.get_metadata(staged_proof).await.unwrap().unwrap()).unwrap();
    assert_eq!(staged_json["revalidation_round"], "catalog");

    // A SQL error during activation must roll back the whole promotion.
    {
        let conn = db.acquire_lock("install activation failure").unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_catalog_activation BEFORE UPDATE ON source_revalidation_rounds \
             WHEN NEW.phase='active' BEGIN SELECT RAISE(ABORT,'synthetic catalog activation'); END;",
        )
        .unwrap();
    }
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
    assert!(
        db.commit_observation_transition(promote.clone(), None, true)
            .await
            .is_err(),
        "catalog activation SQL failure must surface"
    );
    {
        let conn = db
            .acquire_lock("verify rollback after activation failure")
            .unwrap();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id='catalog'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='catalog')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), debt), ("observing", 1));
        assert!(
            conn.query_row(
                "SELECT COUNT(*) FROM metadata WHERE key='observed_selection:v27:PrimarySync'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
                == 0
        );
        conn.execute_batch("DROP TRIGGER fail_catalog_activation;")
            .unwrap();
    }

    // Reopen and promote through the ordinary staged -> ACTIVE path.
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
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
    let active_raw = db
        .get_metadata("observed_selection:v27:PrimarySync")
        .await
        .unwrap()
        .unwrap();
    {
        let conn = db.acquire_lock("verify linked staged promotion").unwrap();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id='catalog'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='catalog')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let active: Value = serde_json::from_str(&active_raw).unwrap();
        assert_eq!((phase.as_str(), debt), ("active", 1));
        assert_eq!(active["revalidation_round"], "catalog");
        let quarantined = quarantine_all_rounds(&conn).unwrap();
        assert_eq!(quarantined.0, 1);
    }
    {
        let conn = db.acquire_lock("verify quarantine retained debt").unwrap();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id='catalog'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='catalog')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), debt), ("abandoned", 1));
    }
}

#[tokio::test]
async fn catalog_v2_linked_active_delta_keeps_debt_and_ordinary_producer_cannot_retire() {
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let (db, baseline) = fixture(dir.path()).await;
    let save = transition(&db, "after").await;
    db.commit_observation_transition(save.clone(), Some(baseline), false)
        .await
        .unwrap();
    let boundary = format!("{:x}", Sha256::digest(b"before"));
    let catalog = format!(
        "catalog:v2:{}:11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222:44444444-4444-4444-4444-444444444444",
        "b".repeat(64)
    );
    {
        let conn = db
            .acquire_lock("seed linked active catalog origin")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('active-catalog','PrimarySync',?1,'policy',?2,'','','active','catalog_v2')",
            rusqlite::params![catalog, boundary],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('active-catalog','asset')",
            [],
        )
        .unwrap();
        assert!(
            attach_revalidation_round_for_test(
                &conn,
                "observed_selection:v27:PrimarySync",
                "after",
                "active-catalog"
            )
            .unwrap()
        );
    }
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let active: Value = serde_json::from_str(
        &db.get_metadata("observed_selection:v27:PrimarySync")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(active["revalidation_round"], "active-catalog");

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
    let delta = ObservationProof::delivery(receipt)
        .delta("after", "quiet")
        .unwrap();
    let advance = transition(&db, "quiet").await;
    db.commit_observation_transition(advance.clone(), Some(delta), false)
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("verify linked active delta").unwrap();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id='active-catalog'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='active-catalog')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), debt), ("active", 1));
    }
    let active: Value = serde_json::from_str(
        &db.get_metadata("observed_selection:v27:PrimarySync")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    // A current catalog link is carried through ordinary ACTIVE deltas.
    // Debt survives, and no deletion authority is inferred from the tag.
    eprintln!(
        "B-LINKED-ACTIVE-DELTA link={:?}",
        active.get("revalidation_round")
    );
    assert_eq!(active["revalidation_round"], "active-catalog");
    db.source_revalidation_seen(
        owner(),
        "PrimarySync".into(),
        "ordinary-selected-scope".into(),
        "asset".into(),
    )
    .await
    .unwrap();
    assert!(
        db.due_source_revalidation(
            owner(),
            "PrimarySync".into(),
            "ordinary-selected-scope".into()
        )
        .await
        .unwrap()
        .is_empty()
    );
    {
        let conn = db.acquire_lock("linked active debt retained").unwrap();
        let debt_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt WHERE round='active-catalog'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(debt_after, 1);
    }
}

#[tokio::test]
async fn catalog_v2_missing_link_does_not_veto_checkpoint_and_preserves_debt() {
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    db.set_metadata("enum_config_hash", "policy").await.unwrap();
    let baseline_key = "baseline_cursor:v27:synthetic:policy:";
    db.set_metadata(baseline_key, "before").await.unwrap();
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
    let baseline = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta("before", "after")
                .unwrap(),
            "before",
        )
        .unwrap();
    let boundary = format!("{:x}", Sha256::digest(b"before"));
    let catalog = format!(
        "catalog:v2:{}:11111111-1111-1111-1111-111111111111:22222222-2222-2222-2222-222222222222:55555555-5555-5555-5555-555555555555",
        "c".repeat(64)
    );
    {
        let conn = db.acquire_lock("seed missing-link catalog origin").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('missing-link','PrimarySync',?1,'policy',?2,'','','observing','catalog_v2')",
            rusqlite::params![catalog, boundary],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES('missing-link','asset')",
            [],
        )
        .unwrap();
    }
    let mut attempt = transition(&db, "after").await;
    attempt
        .expected_metadata
        .push((baseline_key.into(), Some("before".into())));
    db.commit_observation_transition(attempt, Some(baseline), false)
        .await
        .expect("known missing catalog link must not veto a legitimate checkpoint");
    {
        let conn = db
            .acquire_lock("verify conservative missing-link block")
            .unwrap();
        let (phase, debt, key): (String, i64, String) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id='missing-link'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round='missing-link'),
                        (SELECT value FROM metadata WHERE key=?1)",
                [baseline_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (phase.as_str(), debt, key.as_str()),
            ("observing", 1, "before")
        );
    }
}

#[tokio::test]
async fn catalog_due_and_strict_deleted_live_settlement_owner_transaction() {
    use sha2::{Digest, Sha256};

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    db.set_metadata("enum_config_hash", "policy").await.unwrap();
    db.enroll_sync_epoch(vec![("PrimarySync".into(), "scope".into())])
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("mark epoch complete").unwrap();
        conn.execute(
            "UPDATE metadata SET value=json_set(value,'$.state','complete','$.completed_at',1) WHERE key=?1",
            [crate::state::db::sync_epoch::library_key("PrimarySync")],
        )
        .unwrap();
    }
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
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta("before", "after")
                .unwrap(),
            "before",
        )
        .unwrap();
    let save = transition(&db, "after").await;
    db.commit_observation_transition(save, Some(proof), false)
        .await
        .unwrap();
    let active_raw = db.get_metadata(PROOF).await.unwrap().unwrap();
    let digest = catalog_plan_digest_from_observation(&active_raw, "PrimarySync").unwrap();
    let record = {
        let conn = db.acquire_lock("read complete epoch").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap()
    };
    let nonce = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string();
    let origin = CatalogOrigin::new(
        digest.clone(),
        record.epoch.clone(),
        record.attempt.clone(),
        nonce.clone(),
    )
    .unwrap();
    let master_path = dir.path().join("master.jpg");
    std::fs::write(&master_path, b"catalog master").unwrap();
    let master = crate::test_helpers::TestAssetRecord::new("master")
        .library("PrimarySync")
        .build();
    db.upsert_seen(&master).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "master",
        "original",
        &master_path,
        "synthetic",
        None,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "child", "master")
        .await
        .unwrap();
    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "master", "child")
            .await
            .unwrap()
    );
    for (id, revision) in [
        ("missing-capture", None),
        ("stale-capture", Some(0_i64)),
        ("retry-capture", Some(1_i64)),
    ] {
        let path = dir.path().join(format!("{id}.jpg"));
        std::fs::write(&path, b"catalog control").unwrap();
        let record = crate::test_helpers::TestAssetRecord::new(id)
            .library("PrimarySync")
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("PrimarySync", id, "original", &path, "synthetic", None)
            .await
            .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", &format!("child-{id}"), id)
            .await
            .unwrap();
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", id, &format!("child-{id}"))
                .await
                .unwrap()
        );
        {
            let conn = db.acquire_lock("seed capture revision control").unwrap();
            match revision {
                None => {
                    conn.execute(
                        "DELETE FROM asset_metadata_capture_revisions WHERE library='PrimarySync' AND asset_id=?1",
                        [id],
                    )
                    .unwrap();
                }
                Some(revision) => {
                    conn.execute(
                        "INSERT INTO asset_metadata_capture_revisions(library,asset_id,revision,updated_at) VALUES('PrimarySync',?1,?2,1) ON CONFLICT(library,asset_id) DO UPDATE SET revision=excluded.revision,updated_at=excluded.updated_at",
                        rusqlite::params![id, revision],
                    )
                    .unwrap();
                }
            }
        }
    }
    {
        let conn = db.acquire_lock("seed retry capture control").unwrap();
        conn.execute(
            "INSERT INTO metadata_capture_retries(library,asset_id,target_revision,evidence,generation,attempts,last_attempt_at,next_retry_at) VALUES('PrimarySync','retry-capture',1,'[]',1,1,1,2)",
            [],
        )
        .unwrap();
    }
    let boundary = format!("{:x}", Sha256::digest(b"catalog-boundary"));
    {
        let conn = db.acquire_lock("seed active catalog settlement").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'','','active','catalog_v2')",
            rusqlite::params![nonce, origin.scope(), boundary],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'master')",
            [&nonce],
        )
        .unwrap();
        for id in ["missing-capture", "stale-capture", "retry-capture"] {
            conn.execute(
                "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,?2)",
                rusqlite::params![nonce, id],
            )
            .unwrap();
        }
    }
    for id in ["pending-hash-capture", "prepared-capture", "path-capture"] {
        let path = dir.path().join(format!("{id}.jpg"));
        std::fs::write(&path, b"catalog owner control").unwrap();
        let record = crate::test_helpers::TestAssetRecord::new(id)
            .library("PrimarySync")
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("PrimarySync", id, "original", &path, "synthetic", None)
            .await
            .unwrap();
        db.upsert_asset_master_mapping("PrimarySync", &format!("child-{id}"), id)
            .await
            .unwrap();
        assert!(
            db.claim_legacy_master_state_owner("PrimarySync", id, &format!("child-{id}"))
                .await
                .unwrap()
        );
        let conn = db.acquire_lock("seed catalog owner controls").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,?2)",
            rusqlite::params![nonce, id],
        )
        .unwrap();
        match id {
            "pending-hash-capture" => {
                conn.execute(
                    "UPDATE assets SET capture_repair_metadata_hash='pending' WHERE library='PrimarySync' AND id=?1",
                    [id],
                )
                .unwrap();
            }
            "prepared-capture" => {
                conn.execute(
                    "UPDATE assets SET capture_repair_metadata_hash='prepared',capture_repair_output_checksum=?2,capture_repair_output_size=1 WHERE library='PrimarySync' AND id=?1",
                    rusqlite::params![id, "c".repeat(64)],
                )
                .unwrap();
            }
            "path-capture" => {
                conn.execute(
                    "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,capture_repair_metadata_hash) VALUES('PrimarySync',?1,'original',?2,'provider','local','path-pending') ON CONFLICT(library,id,version_size,local_path) DO UPDATE SET capture_repair_metadata_hash='path-pending'",
                    rusqlite::params![id, path.to_str().unwrap()],
                )
                .unwrap();
            }
            _ => {}
        }
    }
    let evidence = |deleted: bool| CatalogPairEvidence {
        database: "shared".into(),
        zone: serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
        master: "master".into(),
        child: "child".into(),
        master_type: "CPLMaster".into(),
        child_type: "CPLAsset".into(),
        reference_exact: true,
        explicit_deleted: deleted,
    };
    let due = db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    assert_eq!(due.len(), 7);
    let master_due = due
        .iter()
        .find(|candidate| candidate.id == "master")
        .cloned()
        .unwrap();
    eprintln!(
        "B-CATALOG-DUE candidate round={} id={} master={} child={:?} owned={}",
        master_due.round,
        master_due.id,
        master_due.master,
        master_due.child,
        master_due.owned_child
    );
    let deleted_result = db
        .settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
            master_due.clone(),
            CatalogSettlementAuthority::Deleted {
                deleted_at: None,
                evidence: evidence(true),
            },
        )
        .await
        .unwrap();
    eprintln!("B-CATALOG-SETTLE-RESULT deleted={deleted_result}");
    assert!(deleted_result);
    for (control, expect_settled) in [
        ("missing-capture", true),
        ("stale-capture", true),
        ("retry-capture", false),
        ("pending-hash-capture", false),
        ("prepared-capture", false),
        ("path-capture", false),
    ] {
        let candidate = due
            .iter()
            .find(|candidate| candidate.id == control)
            .cloned()
            .unwrap();
        let settled = db
            .settle_catalog_candidate(
                owner(),
                "PrimarySync".into(),
                origin.clone(),
                "policy".into(),
                candidate,
                CatalogSettlementAuthority::Deleted {
                    deleted_at: None,
                    evidence: CatalogPairEvidence {
                        master: control.into(),
                        child: format!("child-{control}"),
                        ..evidence(true)
                    },
                },
            )
            .await
            .unwrap();
        assert_eq!(settled, expect_settled, "{control}");
        let conn = db.acquire_lock("verify capture control").unwrap();
        let (deleted, debt): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?2 AND id=?1)",
                rusqlite::params![control, nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (deleted, debt),
            (i64::from(expect_settled), i64::from(!expect_settled)),
            "{control}"
        );
    }
    {
        let conn = db.acquire_lock("verify revision status unchanged").unwrap();
        let (missing, stale): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM asset_metadata_capture_revisions WHERE asset_id='missing-capture'),
                        (SELECT revision FROM asset_metadata_capture_revisions WHERE asset_id='stale-capture')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((missing, stale), (0, 0));
        let (pending_hash, prepared_hash, path_hash): (String, String, String) = conn
            .query_row(
                "SELECT (SELECT capture_repair_metadata_hash FROM assets WHERE id='pending-hash-capture'),
                        (SELECT capture_repair_metadata_hash FROM assets WHERE id='prepared-capture'),
                        (SELECT capture_repair_metadata_hash FROM asset_metadata_paths WHERE id='path-capture')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (
                pending_hash.as_str(),
                prepared_hash.as_str(),
                path_hash.as_str()
            ),
            ("pending", "prepared", "path-pending")
        );
    }
    {
        let conn = db
            .acquire_lock("verify completed capture settlement")
            .unwrap();
        let (deleted, debt): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='master'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='master')",
                [&nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((deleted, debt), (1, 0));
        conn.execute(
            "UPDATE assets SET is_deleted=0 WHERE library='PrimarySync' AND id='master'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'master')",
            [&nonce],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset_verifications VALUES('PrimarySync','master','original','unknown','frozen metadata',1)",
            [],
        )
        .unwrap();
    }
    let due = db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    let master_due = due
        .iter()
        .find(|candidate| candidate.id == "master")
        .cloned()
        .unwrap();
    let live_result = db
        .settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
            master_due,
            CatalogSettlementAuthority::Live {
                evidence: evidence(false),
            },
        )
        .await
        .unwrap();
    eprintln!("B-CATALOG-SETTLE-RESULT live={live_result}");
    assert!(live_result);
    {
        let conn = db.acquire_lock("verify strict live retirement").unwrap();
        let (deleted, debt, verification): (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='master'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='master'),
                        (SELECT COUNT(*) FROM asset_verifications WHERE library='PrimarySync' AND id='master')",
                [&nonce],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((deleted, debt, verification), (0, 0, 1));
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'master')",
            [&nonce],
        )
        .unwrap();
    }
    let due = db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    let master_due = due
        .iter()
        .find(|candidate| candidate.id == "master")
        .cloned()
        .unwrap();
    let protected = db
        .settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
            master_due,
            CatalogSettlementAuthority::Deleted {
                deleted_at: None,
                evidence: evidence(true),
            },
        )
        .await
        .unwrap();
    assert!(!protected);
    {
        let conn = db.acquire_lock("verify protected catalog set").unwrap();
        let (deleted, debt): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='master'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='master')",
                [&nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((deleted, debt), (0, 1));
        conn.execute(
            "DELETE FROM asset_verifications WHERE library='PrimarySync' AND id='master'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE source_revalidation_debt SET next_retry_at=0 WHERE round=?1 AND id='master'",
            [&nonce],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_catalog_deletion BEFORE UPDATE ON assets              WHEN NEW.is_deleted=1 BEGIN SELECT RAISE(ABORT,'synthetic catalog deletion'); END;",
        )
        .unwrap();
    }
    let due = db
        .due_catalog_candidates(owner(), "PrimarySync".into(), origin, "policy".into())
        .await
        .unwrap();
    let master_due = due
        .iter()
        .find(|candidate| candidate.id == "master")
        .cloned()
        .unwrap();
    let rollback = db
        .settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            CatalogOrigin::new(
                digest,
                record.epoch.clone(),
                record.attempt.clone(),
                nonce.clone(),
            )
            .unwrap(),
            "policy".into(),
            master_due,
            CatalogSettlementAuthority::Deleted {
                deleted_at: None,
                evidence: evidence(true),
            },
        )
        .await;
    assert!(rollback.is_err());
    {
        let conn = db.acquire_lock("verify catalog deletion rollback").unwrap();
        let (deleted, debt): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='master'),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='master')",
                [&nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((deleted, debt), (0, 1));
        conn.execute_batch("DROP TRIGGER fail_catalog_deletion;")
            .unwrap();
    }
}
