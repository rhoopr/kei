use super::{SparseAttemptOutcome, SparseEvidence, SparseIdentityStore, SparseSourceId};
use crate::state::SqliteStateDb;
use crate::state::db::CheckpointTransition;
use crate::state::db::account::AccountOwner;
use chrono::Utc;

async fn owned(path: &std::path::Path) -> SqliteStateDb {
    let login =
        serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"origin-test"}})).unwrap();
    let owner = AccountOwner::authenticated("origin@example.invalid", "com", &login).unwrap();
    SqliteStateDb::open_owned(path, &owner).await.unwrap()
}

fn scope(owner: &str) -> String {
    serde_json::json!({"format":1,"realm":"com","container":"com.apple.photos.cloud","environment":"production","database":"private","zone":{"zoneName":"PrimarySync","ownerRecordName":owner}}).to_string()
}

fn checkpoint() -> CheckpointTransition {
    CheckpointTransition {
        expected_metadata: vec![],
        legacy_preservation_proofs: vec![],
        legacy_config_hash: None,
        sparse_identity_proofs: vec![],
        metadata_updates: vec![(
            crate::sync_cycle::sync_token_key("PrimarySync"),
            "next".into(),
        )],
        metadata_deletes: vec![],
    }
}

#[tokio::test]
async fn sparse_origin_same_evidence_promotes_and_omission_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = owned(&path).await;
    let source = SparseSourceId::new("source");
    let evidence = SparseEvidence::new("synthetic evidence".into());
    let old = db
        .observe_sparse_identity("PrimarySync", &source, &evidence, Utc::now())
        .await
        .unwrap();
    db.with_conn_mut("seed_history", |conn| {
        conn.execute(
            "UPDATE unresolved_sparse_identities SET observation_origin='historical-v1'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    db.commit_checkpoint_transition(checkpoint()).await.unwrap();
    let current = db
        .observe_current_sparse_identity(
            "PrimarySync",
            &source,
            &evidence,
            &scope("_defaultOwner"),
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(!current.is_historical());
    assert_ne!(current.generation, old.generation);
    assert_eq!(current.original, old.original);
    let mut stale = checkpoint();
    stale.sparse_identity_proofs.push(old.proof());
    assert!(db.commit_checkpoint_transition(stale).await.is_err());
    drop(db);
    let reopened = owned(&path).await;
    assert!(
        reopened
            .commit_checkpoint_transition(checkpoint())
            .await
            .is_err()
    );
    assert_eq!(
        reopened
            .sparse_identities("PrimarySync")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        reopened
            .observe_current_sparse_identity(
                "PrimarySync",
                &source,
                &evidence,
                &scope("another-owner"),
                Utc::now()
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn sparse_origin_unknown_marker_and_malformed_origin_cannot_save_progress() {
    let dir = tempfile::tempdir().unwrap();
    let db = owned(&dir.path().join("state.db")).await;
    db.set_metadata(&crate::state::unresolved_identity_key("PrimarySync"), "1")
        .await
        .unwrap();
    assert!(db.commit_checkpoint_transition(checkpoint()).await.is_err());
    db.observe_sparse_identity(
        "PrimarySync",
        &SparseSourceId::new("unknown"),
        &SparseEvidence::new("evidence".into()),
        Utc::now(),
    )
    .await
    .unwrap();
    assert!(db.commit_checkpoint_transition(checkpoint()).await.is_err());
    db.with_conn_mut("corrupt_origin", |conn| {
        conn.execute(
            "UPDATE unresolved_sparse_identities SET observation_origin='unsupported'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(db.sparse_identities("PrimarySync").await.is_err());
    assert!(db.commit_checkpoint_transition(checkpoint()).await.is_err());
}

#[tokio::test]
async fn sparse_origin_checkpoint_failure_restores_current_and_preserves_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = owned(&path).await;
    let evidence = SparseEvidence::new(r#"[1,"child","SharedSync-test","owner"]"#.into());
    db.observe_sparse_identity(
        "PrimarySync",
        &SparseSourceId::new("history"),
        &evidence,
        Utc::now(),
    )
    .await
    .unwrap();
    db.with_conn_mut("history", |conn| {
        conn.execute(
            "UPDATE unresolved_sparse_identities SET observation_origin='historical-v1'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let current = db
        .observe_current_sparse_identity(
            "PrimarySync",
            &SparseSourceId::new("current"),
            &evidence,
            &scope("_defaultOwner"),
            Utc::now(),
        )
        .await
        .unwrap();
    db.with_conn_mut("reject_token", |conn| {
        conn.execute_batch("CREATE TRIGGER reject_token BEFORE INSERT ON metadata WHEN NEW.key='sync_token:v27:PrimarySync' BEGIN SELECT RAISE(ABORT,'synthetic token failure'); END;")?;
        Ok(())
    }).await.unwrap();
    let mut transition = checkpoint();
    // Simulate the existing provider-proven deletion path; no target-file proof
    // is waived by production code. Generation must still match at retirement.
    transition.sparse_identity_proofs.push(current.proof());
    assert!(db.commit_checkpoint_transition(transition).await.is_err());
    drop(db);
    let db = owned(&path).await;
    assert_eq!(db.sparse_identities("PrimarySync").await.unwrap().len(), 2);
    db.with_conn_mut("allow_token", |conn| {
        conn.execute_batch("DROP TRIGGER reject_token")?;
        Ok(())
    })
    .await
    .unwrap();
    let mut transition = checkpoint();
    transition.sparse_identity_proofs.push(current.proof());
    db.commit_checkpoint_transition(transition).await.unwrap();
    let retained = db.sparse_identities("PrimarySync").await.unwrap();
    assert_eq!(retained.len(), 1);
    assert!(retained[0].is_historical());
    assert_eq!(
        db.get_metadata(&crate::state::unresolved_identity_key("PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        db.get_metadata(&crate::sync_cycle::sync_token_key("PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("next")
    );
}

#[tokio::test]
async fn sparse_origin_source_only_is_idempotent_and_failed_admission_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = owned(&path).await;
    let source = SparseSourceId::new("source-only");
    let key = crate::icloud::photos::asset::SparseShareEvidence::SourceOnly
        .durable_key()
        .unwrap();
    db.with_conn_mut("fail_admission", |conn| {
        conn.execute_batch("CREATE TRIGGER fail_sparse BEFORE INSERT ON unresolved_sparse_identities BEGIN SELECT RAISE(ABORT,'synthetic admission failure'); END;")?;
        Ok(())
    }).await.unwrap();
    assert!(
        db.observe_current_sparse_identity(
            "PrimarySync",
            &source,
            &key,
            &scope("_defaultOwner"),
            Utc::now()
        )
        .await
        .is_err()
    );
    assert!(
        db.sparse_identities("PrimarySync")
            .await
            .unwrap()
            .is_empty()
    );
    db.with_conn_mut("allow_admission", |conn| {
        conn.execute_batch("DROP TRIGGER fail_sparse")?;
        Ok(())
    })
    .await
    .unwrap();
    let first = db
        .observe_current_sparse_identity(
            "PrimarySync",
            &source,
            &key,
            &scope("_defaultOwner"),
            Utc::now(),
        )
        .await
        .unwrap();
    let deferred = db
        .record_sparse_attempt(
            &first,
            SparseAttemptOutcome::Unresolved(key.clone()),
            Utc::now(),
        )
        .await
        .unwrap();
    let again = db
        .observe_current_sparse_identity(
            "PrimarySync",
            &source,
            &key,
            &scope("_defaultOwner"),
            Utc::now(),
        )
        .await
        .unwrap();
    assert_eq!(again.generation, deferred.generation);
    assert_eq!(again.next_retry, deferred.next_retry);
    drop(db);
    let db = owned(&path).await;
    assert!(db.commit_checkpoint_transition(checkpoint()).await.is_err());
    assert_eq!(db.sparse_identities("PrimarySync").await.unwrap().len(), 1);
}

#[tokio::test]
async fn sparse_origin_blank_source_barrier_survives_history_and_later_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let db = owned(&path).await;
    let key = crate::icloud::photos::asset::SparseShareEvidence::SourceOnly
        .durable_key()
        .unwrap();
    db.observe_sparse_identity(
        "PrimarySync",
        &SparseSourceId::new("history"),
        &SparseEvidence::new(r#"[1,"child","SharedSync-test","owner"]"#.into()),
        Utc::now(),
    )
    .await
    .unwrap();
    db.with_conn_mut("history", |conn| {
        conn.execute(
            "UPDATE unresolved_sparse_identities SET observation_origin='historical-v1'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        db.observe_current_sparse_identity(
            "PrimarySync",
            &SparseSourceId::new(" "),
            &key,
            &scope("_defaultOwner"),
            Utc::now()
        )
        .await
        .is_err()
    );
    db.set_metadata(&crate::state::unresolved_identity_key("PrimarySync"), "1")
        .await
        .unwrap();
    db.observe_current_sparse_identity(
        "PrimarySync",
        &SparseSourceId::new("known"),
        &key,
        &scope("_defaultOwner"),
        Utc::now(),
    )
    .await
    .unwrap();
    drop(db);
    let db = owned(&path).await;
    assert_eq!(
        db.get_metadata(&crate::state::unresolved_identity_key("PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some(crate::state::UNACCOUNTED_CURRENT_IDENTITY)
    );
    assert!(db.commit_checkpoint_transition(checkpoint()).await.is_err());
}
