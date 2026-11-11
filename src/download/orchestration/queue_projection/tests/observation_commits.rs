//! Observation certificates share the checkpoint transaction and policy fences.
use super::owner;
use super::retirement::{BUDGET, admission, spec};
use crate::commands::{AlbumPass, PassKind};
use crate::download::{DownloadConfig, DownloadControls, catalog_plan_context_for_source};
use crate::icloud::photos::PhotoLibrary;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::state::db::provider_generations::GenerationSpec;
use crate::state::db::provider_generations::observation::{
    AdmissionReceipt, ObservationProof, catalog_plan_digest_from_observation,
};
use crate::state::db::source_revalidation::{
    CatalogBaselineClaim, CatalogBootstrapEligibility, CatalogClaimOutcome, CatalogOrigin,
    CatalogPairEvidence, CatalogResumeRelation, CatalogSettlementAuthority,
    FreshCatalogOriginRequest, FreshCatalogPass, FreshCatalogPlan,
    prepare_existing_owned_database_for_test, quarantine_all_rounds,
};
use crate::state::{CheckpointTransition, SqliteStateDb};
use crate::test_helpers::MockPhotosSession;
use serde_json::Value;
use std::collections::HashMap;
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

// Owner-API mechanism fixture. The provider and media are synthetic; no P0 or
// full inventory claim is made by these tests.
struct CatalogOwnerFixture {
    db: Arc<SqliteStateDb>,
    library: PhotoLibrary,
    plan: FreshCatalogPlan,
    receipt: AdmissionReceipt,
    baseline_key: String,
    epoch: String,
    attempt: String,
    policy: String,
}

impl CatalogOwnerFixture {
    async fn new(dir: &std::path::Path, policy: &str, pending: bool) -> Self {
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.join("state.db"), &owner())
                .await
                .unwrap(),
        );
        db.set_metadata("enum_config_hash", policy).await.unwrap();
        if pending {
            db.set_metadata("pending_enum_config_hash", policy)
                .await
                .unwrap();
            db.set_metadata("pending_download_config_hash", "next-path")
                .await
                .unwrap();
        }
        let zone = serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let mut mock = MockPhotosSession::new().ok(serde_json::json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}));
        for _ in 0..8 {
            mock = mock.ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"before"}]}));
        }
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::new(HashMap::new()),
            Box::new(mock),
            Arc::new(zone),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(ShadowCapture::new(db.clone(), owner(), "com"));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        };
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir);
        config.state_db = Some(db.clone());
        config.library = Arc::from("PrimarySync");
        let context = catalog_plan_context_for_source(
            &[pass],
            &config,
            DownloadControls::download_hidden(),
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let plan = FreshCatalogPlan {
            account: context.account.clone(),
            provider: context.provider.clone(),
            database: context.database.clone(),
            zone: context.zone.clone(),
            scope: context.scope.clone(),
            config_hash: context.config_hash.clone(),
            metadata_enabled: context.metadata_enabled,
            metadata_flags: context.metadata_flags,
            passes: context
                .passes
                .iter()
                .map(|pass| FreshCatalogPass {
                    key: pass.key.clone(),
                    kind: pass.kind.clone(),
                    scope: pass.scope.clone(),
                    excludes: pass.excludes.clone(),
                })
                .collect(),
            profile: context.profile.clone(),
        };
        let scope = library.baseline_scope();
        db.enroll_sync_epoch(vec![("PrimarySync".into(), scope.clone())])
            .await
            .unwrap();
        assert_eq!(
            db.sync_epoch_gate("PrimarySync".into(), scope.clone(), None)
                .await
                .unwrap(),
            crate::state::SyncEpochGate::Open
        );
        let selected = GenerationSpec {
            format: 2,
            scope: context.scope,
            zone: context.zone,
            config_hash: context.config_hash.clone(),
            basis: context.config_hash,
            profile: context.profile,
            metadata_enabled: context.metadata_enabled,
            metadata_flags: context.metadata_flags,
        };
        let root = db
            .begin_selection_generation(owner(), selected, BUDGET)
            .await
            .unwrap();
        let receipt = db
            .seal_observed_selection(owner(), root.id, true, None)
            .await
            .unwrap()
            .unwrap();
        let proof = ObservationProof::inventory(vec![receipt.clone()])
            .unwrap()
            .bridge(
                ObservationProof::delivery(receipt.clone())
                    .delta("initial", "after")
                    .unwrap(),
                "initial",
            )
            .unwrap();
        db.commit_observation_transition(transition(&db, "after").await, Some(proof), false)
            .await
            .unwrap();
        let (epoch, attempt) = {
            let conn = db.acquire_lock("read owner fixture epoch").unwrap();
            let epoch = crate::state::db::sync_epoch::validate(&conn).unwrap();
            let record = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
                .unwrap()
                .unwrap();
            assert_eq!(record.state, "complete");
            (epoch.epoch, record.attempt)
        };
        let baseline_key = format!("baseline_cursor:v27:{epoch}:{scope}:{policy}:");
        Self {
            db,
            library,
            plan,
            receipt,
            baseline_key,
            epoch,
            attempt,
            policy: policy.into(),
        }
    }

    async fn install(&self) -> CatalogOrigin {
        let mut expected_metadata = Vec::new();
        for key in [
            crate::state::db::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
            crate::state::db::sync_epoch::library_key("PrimarySync"),
            "enum_config_hash".into(),
            "pending_enum_config_hash".into(),
            "config_hash".into(),
            "pending_download_config_hash".into(),
            TOKEN.into(),
            PROOF.into(),
            "download_ledger_scope:PrimarySync".into(),
        ] {
            expected_metadata.push((key.clone(), self.db.get_metadata(&key).await.unwrap()));
        }
        let expected_baseline = self.db.get_metadata(&self.baseline_key).await.unwrap();
        let expected_catalog_snapshot = self
            .db
            .fresh_catalog_snapshot(owner(), "PrimarySync".into())
            .await
            .unwrap();
        let fresh_cursor = self.library.fresh_baseline_cursor().await.unwrap();
        self.db
            .install_fresh_catalog_origin(
                owner(),
                FreshCatalogOriginRequest {
                    library: "PrimarySync".into(),
                    plan: self.plan.clone(),
                    baseline_key: self.baseline_key.clone(),
                    preservation_config_hash: String::new(),
                    expected_baseline,
                    fresh_cursor,
                    enum_hash: self.policy.clone(),
                    expected_epoch: self.epoch.clone(),
                    expected_attempt: self.attempt.clone(),
                    expected_metadata,
                    expected_catalog_snapshot,
                },
            )
            .await
            .unwrap()
    }

    async fn hint_request(&self, token: &str) -> FreshCatalogOriginRequest {
        let mut expected_metadata = Vec::new();
        for key in [
            crate::state::db::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
            crate::state::db::sync_epoch::library_key("PrimarySync"),
            "enum_config_hash".into(),
            "pending_enum_config_hash".into(),
            "config_hash".into(),
            "pending_download_config_hash".into(),
            TOKEN.into(),
            PROOF.into(),
            "download_ledger_scope:PrimarySync".into(),
        ] {
            expected_metadata.push((key.clone(), self.db.get_metadata(&key).await.unwrap()));
        }
        FreshCatalogOriginRequest {
            library: "PrimarySync".into(),
            plan: self.plan.clone(),
            baseline_key: self.baseline_key.clone(),
            preservation_config_hash: String::new(),
            expected_baseline: self.db.get_metadata(&self.baseline_key).await.unwrap(),
            fresh_cursor: token.into(),
            enum_hash: self.policy.clone(),
            expected_epoch: self.epoch.clone(),
            expected_attempt: self.attempt.clone(),
            expected_metadata,
            expected_catalog_snapshot: self
                .db
                .fresh_catalog_snapshot(owner(), "PrimarySync".into())
                .await
                .unwrap(),
        }
    }

    async fn stage(&self, origin: &CatalogOrigin) {
        self.db
            .seed_source_revalidation(
                owner(),
                "PrimarySync".into(),
                origin.scope(),
                self.policy.clone(),
                CatalogOrigin::boundary("before"),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        let proof = ObservationProof::inventory(vec![self.receipt.clone()])
            .unwrap()
            .bridge(
                ObservationProof::delivery(self.receipt.clone())
                    .delta("before", "staged")
                    .unwrap(),
                "before",
            )
            .unwrap()
            .with_catalog_origin(origin.clone());
        let staged = format!("pending_sync_token:v27:{}:PrimarySync", self.policy);
        let mut save = transition(&self.db, "staged").await;
        save.metadata_updates[0].0 = staged.clone();
        save.expected_metadata.push((staged, None));
        save.expected_metadata
            .push((self.baseline_key.clone(), Some("before".into())));
        self.db
            .commit_observation_transition(save, Some(proof), false)
            .await
            .unwrap();
    }

    async fn promote(&self, token: &str) {
        let staged = format!("pending_sync_token:v27:{}:PrimarySync", self.policy);
        let mut promote = transition(&self.db, token).await;
        promote
            .metadata_updates
            .push(("enum_config_hash".into(), self.policy.clone()));
        promote
            .expected_metadata
            .push((staged.clone(), Some(token.into())));
        promote.metadata_deletes.push(staged);
        self.db
            .commit_observation_transition(promote, None, true)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn owner_created_no_link_active_empty_census_allows_health_prework() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let raw = fixture.db.get_metadata(PROOF).await.unwrap().unwrap();
    let descriptor: Value = serde_json::from_str(&raw).unwrap();
    assert!(descriptor.get("revalidation_round").is_none());
    let count: i64 = fixture
        .db
        .acquire_lock("inspect empty census")
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library='PrimarySync'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    let plan = &fixture.plan;
    let request = crate::state::db::provider_generations::operational_hold::SourceRequest {
        owner: owner(),
        specification: GenerationSpec {
            format: 2,
            scope: plan.scope.clone(),
            zone: plan.zone.clone(),
            config_hash: plan.config_hash.clone(),
            basis: plan.config_hash.clone(),
            profile: plan.profile.clone(),
            metadata_enabled: plan.metadata_enabled,
            metadata_flags: plan.metadata_flags,
        },
        baseline_scope: fixture.library.baseline_scope(),
        epoch: fixture.epoch.clone(),
        attempt: fixture.attempt.clone(),
        directory: dir.path().to_path_buf(),
        catalog_plan: Some(crate::state::db::source_revalidation::fresh_catalog_plan_digest(plan)),
        enum_policy: fixture.policy.clone(),
        active_token_key: Some(TOKEN.to_owned()),
        preservation_policy: None,
    };
    assert!(
        fixture
            .db
            .operational_prework(request)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn catalog_required_origin_rolls_back_checkpoint_on_mismatch_and_sql_fault() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let origin = fixture.install().await;
    fixture
        .db
        .seed_source_revalidation(
            owner(),
            "PrimarySync".into(),
            origin.scope(),
            fixture.policy.clone(),
            CatalogOrigin::boundary("before"),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    {
        let conn = fixture
            .db
            .acquire_lock("add retained catalog debt")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'retained',42)",
            [&origin.nonce],
        )
        .unwrap();
    }
    let old_proof = fixture.db.get_metadata(PROOF).await.unwrap();
    let proof = ObservationProof::inventory(vec![fixture.receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(fixture.receipt.clone())
                .delta("before", "new-token")
                .unwrap(),
            "before",
        )
        .unwrap();
    let mut save = transition(&fixture.db, "new-token").await;
    save.expected_metadata
        .push((fixture.baseline_key.clone(), Some("before".into())));

    let mut wrong_plan = origin.clone();
    wrong_plan.plan_digest = "a".repeat(64);
    let mut wrong_epoch = origin.clone();
    wrong_epoch.epoch = uuid::Uuid::new_v4().to_string();
    let mut wrong_attempt = origin.clone();
    wrong_attempt.attempt = uuid::Uuid::new_v4().to_string();
    for bad in [wrong_plan, wrong_epoch, wrong_attempt] {
        assert!(
            fixture
                .db
                .commit_observation_transition(
                    save.clone(),
                    Some(proof.clone().with_catalog_origin(bad)),
                    false
                )
                .await
                .is_err()
        );
        assert_eq!(
            fixture.db.get_metadata(TOKEN).await.unwrap().as_deref(),
            Some("after")
        );
        assert_eq!(fixture.db.get_metadata(PROOF).await.unwrap(), old_proof);
    }

    for trigger in [
        "CREATE TRIGGER required_origin_interleave AFTER UPDATE ON metadata WHEN NEW.key='sync_token:v27:PrimarySync' BEGIN UPDATE source_revalidation_rounds SET boundary='changed' WHERE mode='catalog_v2'; END",
        "CREATE TRIGGER required_origin_interleave BEFORE INSERT ON metadata WHEN NEW.key='observed_selection:v27:PrimarySync' BEGIN SELECT RAISE(ABORT,'injected descriptor fault'); END",
        "CREATE TRIGGER required_origin_interleave BEFORE UPDATE OF phase ON source_revalidation_rounds WHEN NEW.phase='active' BEGIN SELECT RAISE(IGNORE); END",
    ] {
        {
            let conn = fixture
                .db
                .acquire_lock("inject required origin failure")
                .unwrap();
            conn.execute_batch(trigger).unwrap();
        }
        assert!(
            fixture
                .db
                .commit_observation_transition(
                    save.clone(),
                    Some(proof.clone().with_catalog_origin(origin.clone())),
                    false
                )
                .await
                .is_err()
        );
        {
            let conn = fixture
                .db
                .acquire_lock("remove required origin failure")
                .unwrap();
            conn.execute_batch("DROP TRIGGER required_origin_interleave")
                .unwrap();
        }
        let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap();
        assert_eq!(
            reopened.get_metadata(TOKEN).await.unwrap().as_deref(),
            Some("after")
        );
        assert_eq!(reopened.get_metadata(PROOF).await.unwrap(), old_proof);
        let conn = reopened
            .acquire_lock("check rolled back origin and debt")
            .unwrap();
        let row: (String, String, i64) = conn.query_row(
            "SELECT r.phase,r.boundary,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 AND d.id='retained'",
            [&origin.nonce], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
        ).unwrap();
        assert_eq!(
            row,
            ("observing".into(), CatalogOrigin::boundary("before"), 42)
        );
    }
    fixture
        .db
        .commit_observation_transition(save, Some(proof.with_catalog_origin(origin.clone())), false)
        .await
        .unwrap();
    let active: Value =
        serde_json::from_str(&fixture.db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
    assert_eq!(active["revalidation_round"], origin.nonce);
    assert_eq!(
        fixture.db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("new-token")
    );
}

#[tokio::test]
async fn catalog_optional_stale_carried_link_does_not_block_delta() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    {
        let conn = fixture
            .db
            .acquire_lock("stale carried catalog link")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'retained',42)",
            [&origin.nonce],
        )
        .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
    }
    let delta = ObservationProof::delivery(fixture.receipt.clone())
        .delta("staged", "later")
        .unwrap();
    fixture
        .db
        .commit_observation_transition(transition(&fixture.db, "later").await, Some(delta), false)
        .await
        .unwrap();
    assert_eq!(
        fixture.db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("later")
    );
    let descriptor: Value =
        serde_json::from_str(&fixture.db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
    assert!(
        descriptor
            .get("revalidation_round")
            .is_none_or(Value::is_null)
    );
    let conn = fixture
        .db
        .acquire_lock("verify stale carried debt")
        .unwrap();
    let row: (String, i64) = conn.query_row("SELECT r.phase,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 AND d.id='retained'", [&origin.nonce], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(row, ("abandoned".into(), 42));
}

#[tokio::test]
async fn catalog_stale_abandoned_link_allows_observed_delta_bootstrap() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let old = fixture.install().await;
    fixture.stage(&old).await;
    fixture.promote("staged").await;
    {
        let conn = fixture
            .db
            .acquire_lock("retain stale catalog debt")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'retained',31)",
            [&old.nonce],
        )
        .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
            [&old.nonce],
        )
        .unwrap();
    }
    let mut fences = Vec::new();
    for key in [
        crate::state::db::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
        crate::state::db::sync_epoch::library_key("PrimarySync"),
        "enum_config_hash".into(),
        "pending_enum_config_hash".into(),
        "config_hash".into(),
        "pending_download_config_hash".into(),
        TOKEN.into(),
        PROOF.into(),
        "download_ledger_scope:PrimarySync".into(),
    ] {
        fences.push((key.clone(), fixture.db.get_metadata(&key).await.unwrap()));
    }
    let request = FreshCatalogOriginRequest {
        library: "PrimarySync".into(),
        plan: fixture.plan.clone(),
        baseline_key: fixture.baseline_key.clone(),
        preservation_config_hash: String::new(),
        expected_baseline: fixture
            .db
            .get_metadata(&fixture.baseline_key)
            .await
            .unwrap(),
        fresh_cursor: "later".into(),
        enum_hash: fixture.policy.clone(),
        expected_epoch: fixture.epoch.clone(),
        expected_attempt: fixture.attempt.clone(),
        expected_metadata: fences,
        expected_catalog_snapshot: fixture
            .db
            .fresh_catalog_snapshot(owner(), "PrimarySync".into())
            .await
            .unwrap(),
    };
    let proof = ObservationProof::delivery(fixture.receipt.clone())
        .delta("staged", "later")
        .unwrap()
        .with_epoch_attempt(Some(fixture.attempt.clone()));
    let checkpoint = transition(&fixture.db, "later").await;
    let origin = fixture
        .db
        .install_catalog_origin_after_delta(owner(), request, checkpoint, proof)
        .await
        .unwrap();
    assert_eq!(
        fixture.db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("later")
    );
    assert_eq!(
        fixture
            .db
            .get_metadata(&fixture.baseline_key)
            .await
            .unwrap()
            .as_deref(),
        Some("later")
    );
    let descriptor: Value =
        serde_json::from_str(&fixture.db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
    assert!(
        descriptor
            .get("revalidation_round")
            .is_none_or(Value::is_null)
    );
    let conn = fixture
        .db
        .acquire_lock("verify stale history retained")
        .unwrap();
    let (old_phase, old_deadline, new_phase):(String,i64,String) = conn.query_row("SELECT r.phase,d.next_retry_at,(SELECT phase FROM source_revalidation_rounds WHERE id=?2) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 AND d.id='retained'", rusqlite::params![old.nonce,origin.nonce], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        (old_phase.as_str(), old_deadline, new_phase.as_str()),
        ("abandoned", 31, "marking")
    );
}

#[tokio::test]
async fn catalog_observed_delta_installs_origin_atomically_including_quiet_eof() {
    for next in ["after", "delta-eof"] {
        let dir = tempfile::tempdir().unwrap();
        let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
        let db = &fixture.db;
        let media = dir.path().join("retained.jpg");
        std::fs::write(&media, b"retained media").unwrap();
        let mtime = std::fs::metadata(&media).unwrap().modified().unwrap();
        let asset = crate::test_helpers::TestAssetRecord::new("retained-asset")
            .library("PrimarySync")
            .build();
        db.upsert_seen(&asset).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "retained-asset",
            "original",
            &media,
            "synthetic",
            None,
        )
        .await
        .unwrap();
        let old_origins: Vec<_> = (0..2)
            .map(|_| {
                CatalogOrigin::new(
                    crate::state::db::source_revalidation::fresh_catalog_plan_digest(&fixture.plan),
                    fixture.epoch.clone(),
                    fixture.attempt.clone(),
                    uuid::Uuid::new_v4().to_string(),
                )
                .unwrap()
            })
            .collect();
        {
            let conn = db
                .acquire_lock("seed unrelated retained source debt")
                .unwrap();
            conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES('unknown-round','PrimarySync','unknown','policy','old','','','observing','future_mode')", []).unwrap();
            conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES('unknown-round','retained',37)", []).unwrap();
            for (old, deadline) in old_origins.iter().zip([19, 7]) {
                conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy','old','retained-asset','','observing','catalog_v2')", rusqlite::params![old.nonce,old.scope()]).unwrap();
                conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'retained-asset',?2)", rusqlite::params![old.nonce,deadline]).unwrap();
            }
        }
        let old_descriptor = db.get_metadata(PROOF).await.unwrap();
        let mut fences = Vec::new();
        for key in [
            crate::state::db::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
            crate::state::db::sync_epoch::library_key("PrimarySync"),
            "enum_config_hash".into(),
            "pending_enum_config_hash".into(),
            "config_hash".into(),
            "pending_download_config_hash".into(),
            TOKEN.into(),
            PROOF.into(),
            "download_ledger_scope:PrimarySync".into(),
        ] {
            fences.push((key.clone(), db.get_metadata(&key).await.unwrap()));
        }
        let request = FreshCatalogOriginRequest {
            library: "PrimarySync".into(),
            plan: fixture.plan.clone(),
            baseline_key: fixture.baseline_key.clone(),
            preservation_config_hash: String::new(),
            expected_baseline: None,
            fresh_cursor: next.into(),
            enum_hash: fixture.policy.clone(),
            expected_epoch: fixture.epoch.clone(),
            expected_attempt: fixture.attempt.clone(),
            expected_metadata: fences,
            expected_catalog_snapshot: db
                .fresh_catalog_snapshot(owner(), "PrimarySync".into())
                .await
                .unwrap(),
        };
        let mut pre_stream = request.clone();
        pre_stream.fresh_cursor = "after".into();
        assert_eq!(
            db.catalog_bootstrap_eligibility(owner(), pre_stream)
                .await
                .unwrap(),
            CatalogBootstrapEligibility::AttemptBootstrap
        );
        let checkpoint = transition(db, next).await;
        let source = &fixture.plan.passes[0].scope;
        let page = serde_json::json!({"zones":[{"zoneID":fixture.plan.zone,"records":[],"syncToken":next,"moreComing":false}]});
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from(
                    "https://synthetic.example/database/1/photos/production/private",
                ),
                name: Arc::from(source["name"].as_str().unwrap()),
                list_type: Arc::from(source["list_type"].as_str().unwrap()),
                obj_type: Arc::from(source["obj_type"].as_str().unwrap()),
                query_filter: None,
                page_size: 200,
                zone_id: Arc::new(fixture.plan.zone.clone()),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: vec![],
            },
            Box::new(MockPhotosSession::new().ok(page)),
        );
        album.set_shadow_capture(
            ShadowCapture::new(db.clone(), owner(), "com"),
            Arc::from("private"),
        );
        let passes = vec![AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        }];
        let controls = DownloadControls::download_hidden();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.library = Arc::from("PrimarySync");
        config.sync_mode = crate::download::SyncMode::Incremental {
            zone_sync_token: "after".into(),
        };
        let context =
            crate::download::orchestration::generation::context(&passes, &config, controls)
                .await
                .unwrap()
                .unwrap();
        config.selection_context = Some(Arc::new(context));
        let mut result = crate::download::orchestration::incremental::download_photos_incremental(
            &reqwest::Client::new(),
            &passes,
            &Arc::new(config),
            "after",
            controls,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.sync_token.as_deref(), Some(next));
        let proof = result
            .checkpoint
            .observation
            .take()
            .expect("completed empty delta must produce an observation")
            .with_epoch_attempt(Some(fixture.attempt.clone()));
        let wrong = ObservationProof::delivery(fixture.receipt.clone())
            .delta("wrong-prior", next)
            .unwrap()
            .with_epoch_attempt(Some(fixture.attempt.clone()));
        assert!(
            db.install_catalog_origin_after_delta(
                owner(),
                request.clone(),
                checkpoint.clone(),
                wrong
            )
            .await
            .is_err()
        );
        for proof_attempt in [None, Some(uuid::Uuid::new_v4().to_string())] {
            assert!(
                db.install_catalog_origin_after_delta(
                    owner(),
                    request.clone(),
                    checkpoint.clone(),
                    proof.clone().with_epoch_attempt(proof_attempt)
                )
                .await
                .is_err()
            );
        }
        for bad_request in [
            {
                let mut r = request.clone();
                r.plan.database = "shared".into();
                r
            },
            {
                let mut r = request.clone();
                r.plan.zone["ownerRecordName"] = serde_json::json!("other-owner");
                r
            },
            {
                let mut r = request.clone();
                r.plan.provider = "wrong-pin".into();
                r
            },
            {
                let mut r = request.clone();
                r.plan.config_hash = "a".repeat(64);
                r
            },
            {
                let mut r = request.clone();
                r.expected_attempt = uuid::Uuid::new_v4().to_string();
                r
            },
            {
                let mut r = request.clone();
                r.expected_epoch = uuid::Uuid::new_v4().to_string();
                r
            },
            {
                let mut r = request.clone();
                r.expected_catalog_snapshot = "stale".into();
                r
            },
        ] {
            assert!(
                db.install_catalog_origin_after_delta(
                    owner(),
                    bad_request,
                    checkpoint.clone(),
                    proof.clone()
                )
                .await
                .is_err()
            );
        }
        let mut missing_policy_fence = checkpoint.clone();
        missing_policy_fence
            .expected_metadata
            .retain(|(key, _)| key != "config_hash");
        assert!(
            db.install_catalog_origin_after_delta(
                owner(),
                request.clone(),
                missing_policy_fence,
                proof.clone()
            )
            .await
            .is_err()
        );
        for trigger in [
            "CREATE TRIGGER delta_bootstrap_fault BEFORE INSERT ON metadata WHEN NEW.key='observed_selection:v27:PrimarySync' BEGIN SELECT RAISE(IGNORE); END",
            "CREATE TRIGGER delta_bootstrap_fault BEFORE INSERT ON source_revalidation_rounds WHEN NEW.mode='catalog_v2' BEGIN SELECT RAISE(ABORT,'origin fault'); END",
            "CREATE TRIGGER delta_bootstrap_fault AFTER INSERT ON metadata WHEN NEW.key LIKE 'baseline_cursor:v27:%' BEGIN UPDATE metadata SET value='tampered' WHERE key='sync_token:v27:PrimarySync'; END",
        ] {
            {
                let conn = db.acquire_lock("inject delta bootstrap fault").unwrap();
                conn.execute_batch(trigger).unwrap();
            }
            assert!(
                db.install_catalog_origin_after_delta(
                    owner(),
                    request.clone(),
                    checkpoint.clone(),
                    proof.clone()
                )
                .await
                .is_err()
            );
            {
                let conn = db.acquire_lock("clear delta bootstrap fault").unwrap();
                conn.execute_batch("DROP TRIGGER delta_bootstrap_fault")
                    .unwrap();
            }
            let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap();
            assert_eq!(
                reopened.get_metadata(TOKEN).await.unwrap().as_deref(),
                Some("after")
            );
            assert_eq!(reopened.get_metadata(PROOF).await.unwrap(), old_descriptor);
            assert_eq!(
                reopened.get_metadata(&fixture.baseline_key).await.unwrap(),
                None
            );
            let conn = reopened.acquire_lock("check delta rollback debt").unwrap();
            let debt:i64 = conn.query_row("SELECT next_retry_at FROM source_revalidation_debt WHERE round='unknown-round' AND id='retained'", [], |r| r.get(0)).unwrap();
            assert_eq!(debt, 37);
            let unchanged:i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_rounds WHERE id IN (?1,?2) AND phase='observing'", rusqlite::params![old_origins[0].nonce,old_origins[1].nonce], |r| r.get(0)).unwrap();
            assert_eq!(unchanged, 2);
            assert_eq!(std::fs::read(&media).unwrap(), b"retained media");
            assert_eq!(
                std::fs::metadata(&media).unwrap().modified().unwrap(),
                mtime
            );
        }
        let origin = db
            .install_catalog_origin_after_delta(owner(), request, checkpoint, proof)
            .await
            .unwrap();
        assert_eq!(db.get_metadata(TOKEN).await.unwrap().as_deref(), Some(next));
        assert_eq!(
            db.get_metadata(&fixture.baseline_key)
                .await
                .unwrap()
                .as_deref(),
            Some(next)
        );
        let descriptor: Value =
            serde_json::from_str(&db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
        assert_ne!(db.get_metadata(PROOF).await.unwrap(), old_descriptor);
        assert!(
            descriptor
                .get("revalidation_round")
                .is_none_or(Value::is_null)
        );
        let resume = db
            .catalog_resume(owner(), fixture.hint_request(next).await)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resume.origin, origin);
        assert_eq!(resume.boundary, next);
        assert_eq!(resume.relation, CatalogResumeRelation::CurrentToken);
        let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap();
        let resumed = reopened
            .catalog_resume(owner(), fixture.hint_request(next).await)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(resumed, resume);
        {
            let conn = db.acquire_lock("verify delta bootstrap origin").unwrap();
            let copied:i64 = conn.query_row("SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='retained-asset'", [&origin.nonce], |r| r.get(0)).unwrap();
            assert_eq!(copied, 7);
            let previous:i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_rounds WHERE id IN (?1,?2) AND phase='abandoned'", rusqlite::params![old_origins[0].nonce,old_origins[1].nonce], |r| r.get(0)).unwrap();
            assert_eq!(previous, 2);
            let (phase,boundary,debt,high):(String,String,i64,String) = conn.query_row("SELECT phase,boundary,(SELECT next_retry_at FROM source_revalidation_debt WHERE round='unknown-round' AND id='retained'),high_id FROM source_revalidation_rounds WHERE id=?1", [&origin.nonce], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
            assert_eq!(
                (phase.as_str(), boundary, debt, high.as_str()),
                (
                    "marking",
                    CatalogOrigin::boundary(next),
                    37,
                    "retained-asset"
                )
            );
        }
        assert!(
            db.due_catalog_candidates(owner(), "PrimarySync".into(), origin, "policy".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(std::fs::read(&media).unwrap(), b"retained media");
        assert_eq!(
            std::fs::metadata(&media).unwrap().modified().unwrap(),
            mtime
        );
    }
}

#[tokio::test]
async fn catalog_pending_shared_full_zone_install_and_required_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    prepare_existing_owned_database_for_test(&path, &owner()).unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    db.set_metadata("enum_config_hash", "policy").await.unwrap();
    let name = "SharedSync-example";
    let zone = serde_json::json!({"zoneName":name,"ownerRecordName":"shared-owner", "zoneType":"extra-field"});
    let mock = MockPhotosSession::new()
        .ok(serde_json::json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}))
        .ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"shared-boundary"}]}));
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/shared".into(),
        Arc::new(HashMap::new()),
        Box::new(mock),
        Arc::new(zone.clone()),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(db.clone(), owner(), "com"));
    let hidden = library.albums().await.unwrap().remove("Hidden").unwrap();
    let passes = vec![
        AlbumPass {
            kind: PassKind::SmartFolder,
            album: hidden,
            exclude_ids: Arc::default(),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        },
    ];
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.library = Arc::from(name);
    let context = catalog_plan_context_for_source(
        &passes,
        &config,
        DownloadControls::download_hidden(),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(context.passes.len(), 2);
    let plan = FreshCatalogPlan {
        account: context.account.clone(),
        provider: context.provider.clone(),
        database: context.database.clone(),
        zone: context.zone.clone(),
        scope: context.scope.clone(),
        config_hash: context.config_hash.clone(),
        metadata_enabled: context.metadata_enabled,
        metadata_flags: context.metadata_flags,
        profile: context.profile.clone(),
        passes: context
            .passes
            .iter()
            .map(|p| FreshCatalogPass {
                key: p.key.clone(),
                kind: p.kind.clone(),
                scope: p.scope.clone(),
                excludes: p.excludes.clone(),
            })
            .collect(),
    };
    let scope = library.baseline_scope();
    db.enroll_sync_epoch(vec![(name.into(), scope.clone())])
        .await
        .unwrap();
    let attempt = match db
        .sync_epoch_gate(name.into(), scope.clone(), None)
        .await
        .unwrap()
    {
        crate::state::SyncEpochGate::Baseline { attempt } => attempt,
        other => panic!("expected initial pending attempt, got {other:?}"),
    };
    let epoch = {
        let conn = db.acquire_lock("read initial shared epoch").unwrap();
        crate::state::db::sync_epoch::validate(&conn).unwrap().epoch
    };
    let baseline_key = format!("baseline_cursor:v27:{epoch}:{scope}:policy:");
    let old = CatalogOrigin::new(
        crate::state::db::source_revalidation::fresh_catalog_plan_digest(&plan),
        epoch.clone(),
        attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    for id in [
        "case-db",
        "case-owner",
        "case-empty",
        "case-zone",
        "case-extra",
        "case-missing",
        "case-stale",
        "case-token",
        "census-a",
        "census-z",
    ] {
        let asset = crate::test_helpers::TestAssetRecord::new(id)
            .library(name)
            .build();
        let media = dir.path().join(format!("{id}.jpg"));
        std::fs::write(&media, b"retained media").unwrap();
        db.upsert_seen(&asset).await.unwrap();
        db.mark_downloaded(name, id, "original", &media, "synthetic", None)
            .await
            .unwrap();
    }
    let medium = dir.path().join("census-a-medium.jpg");
    std::fs::write(&medium, b"retained medium").unwrap();
    let asset = crate::test_helpers::TestAssetRecord::new("census-a")
        .library(name)
        .version_size(crate::state::VersionSizeKey::Medium)
        .build();
    db.upsert_seen(&asset).await.unwrap();
    db.mark_downloaded(name, "census-a", "medium", &medium, "synthetic", None)
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("seed retained shared debt").unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,?2,?3,'policy','old-boundary','high','after','observing','catalog_v2')", rusqlite::params![old.nonce,name,old.scope()]).unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'retained',42)",
            [&old.nonce],
        )
        .unwrap();
    }
    let token_key = format!("sync_token:v27:{name}");
    let proof_key = format!("observed_selection:v27:{name}");
    let mut expected_metadata = Vec::new();
    for key in [
        crate::state::db::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
        crate::state::db::sync_epoch::library_key(name),
        "enum_config_hash".into(),
        "pending_enum_config_hash".into(),
        "config_hash".into(),
        "pending_download_config_hash".into(),
        token_key.clone(),
        proof_key.clone(),
        format!("download_ledger_scope:{name}"),
    ] {
        expected_metadata.push((key.clone(), db.get_metadata(&key).await.unwrap()));
    }
    let snapshot = db
        .fresh_catalog_snapshot(owner(), name.into())
        .await
        .unwrap();
    let cursor = library.fresh_baseline_cursor().await.unwrap();
    let request = FreshCatalogOriginRequest {
        library: name.into(),
        plan: plan.clone(),
        baseline_key: baseline_key.clone(),
        preservation_config_hash: String::new(),
        expected_baseline: None,
        fresh_cursor: cursor.clone(),
        enum_hash: "policy".into(),
        expected_epoch: epoch.clone(),
        expected_attempt: attempt.clone(),
        expected_metadata,
        expected_catalog_snapshot: snapshot,
    };
    for changed in [
        {
            let mut r = request.clone();
            r.plan.database = "private".into();
            r
        },
        {
            let mut r = request.clone();
            r.plan.zone["ownerRecordName"] = serde_json::json!("wrong-owner");
            r
        },
        {
            let mut r = request.clone();
            r.plan.account = "wrong-pin".into();
            r
        },
        {
            let mut r = request.clone();
            r.expected_attempt = uuid::Uuid::new_v4().to_string();
            r
        },
        {
            let mut r = request.clone();
            r.expected_catalog_snapshot = "stale".into();
            r
        },
    ] {
        assert!(
            db.install_fresh_catalog_origin(owner(), changed)
                .await
                .is_err()
        );
    }
    {
        let conn = db
            .acquire_lock("inject shared baseline write fault")
            .unwrap();
        conn.execute_batch("CREATE TRIGGER fail_shared_baseline BEFORE INSERT ON metadata WHEN NEW.key LIKE 'baseline_cursor:v27:%' BEGIN SELECT RAISE(ABORT,'baseline fault'); END").unwrap();
    }
    assert!(
        db.install_fresh_catalog_origin(owner(), request.clone())
            .await
            .is_err()
    );
    {
        let conn = db.acquire_lock("clear shared fault").unwrap();
        conn.execute_batch("DROP TRIGGER fail_shared_baseline")
            .unwrap();
    }
    let reopened = SqliteStateDb::open_owned(&path, &owner()).await.unwrap();
    assert_eq!(reopened.get_metadata(&baseline_key).await.unwrap(), None);
    {
        let conn = reopened
            .acquire_lock("verify rolled back shared origin")
            .unwrap();
        let row:(i64,String,i64) = conn.query_row("SELECT (SELECT COUNT(*) FROM source_revalidation_rounds),r.phase,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1", [&old.nonce], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(row, (1, "observing".into(), 42));
    }
    let origin = db
        .install_fresh_catalog_origin(owner(), request.clone())
        .await
        .unwrap();
    let mut pending_resume = request.clone();
    pending_resume.fresh_cursor.clear();
    pending_resume.expected_baseline = Some(cursor.clone());
    pending_resume.expected_catalog_snapshot = db
        .fresh_catalog_snapshot(owner(), name.into())
        .await
        .unwrap();
    let resumed = db
        .catalog_resume(owner(), pending_resume)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.origin, origin);
    assert_eq!(resumed.boundary, cursor);
    assert_eq!(resumed.relation, CatalogResumeRelation::PendingAttempt);
    assert_eq!(db.get_metadata(&token_key).await.unwrap(), None);
    assert_eq!(db.get_metadata(&proof_key).await.unwrap(), None);
    {
        let conn = db.acquire_lock("verify shared pending transfer").unwrap();
        let row:(String,i64,String,String,String) = conn.query_row("SELECT r.phase,d.next_retry_at,(SELECT phase FROM source_revalidation_rounds WHERE id=?2),r.high_id,r.after_id FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1", rusqlite::params![origin.nonce,old.nonce], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(
            row,
            (
                "marking".into(),
                42,
                "abandoned".into(),
                "census-z".into(),
                String::new()
            )
        );
    }
    db.seed_source_revalidation(
        owner(),
        name.into(),
        origin.scope(),
        "policy".into(),
        CatalogOrigin::boundary(&cursor),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    {
        let conn = db.acquire_lock("verify complete shared census").unwrap();
        let (phase, count, retained): (String, i64, i64) = conn.query_row("SELECT r.phase,(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=r.id),(SELECT next_retry_at FROM source_revalidation_debt WHERE round=r.id AND id='retained') FROM source_revalidation_rounds r WHERE r.id=?1", [&origin.nonce], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!((phase.as_str(), count, retained), ("observing", 11, 42));
    }
    // A resource selected by Primary is not authority to attach the Shared
    // origin to Primary's checkpoint, even when the source is authenticated.
    let mut primary_spec = spec();
    primary_spec.profile["current_download_metadata"] = serde_json::json!(1);
    let primary_root = db
        .begin_selection_generation(owner(), primary_spec, BUDGET)
        .await
        .unwrap();
    let primary_receipt = db
        .seal_observed_selection(owner(), primary_root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let primary_proof = ObservationProof::inventory(vec![primary_receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(primary_receipt)
                .delta("primary-start", "primary-active")
                .unwrap(),
            "primary-start",
        )
        .unwrap();
    db.commit_observation_transition(
        transition(&db, "primary-active").await,
        Some(primary_proof),
        false,
    )
    .await
    .unwrap();
    let prior_primary = db.get_metadata(PROOF).await.unwrap();
    let primary_zone =
        serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://synthetic.example/private"),
            name: Arc::from("selected"),
            list_type: Arc::from("CPLContainerRelationNotDeletedByAssetDate"),
            obj_type: Arc::from("CPLContainerRelationNotDeletedByAssetDate"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(primary_zone),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: Some(Arc::from("selected-album")),
            cross_zone_sources: vec![],
        },
        Box::new(MockPhotosSession::new()),
    );
    album.set_shadow_capture(
        ShadowCapture::new(db.clone(), owner(), "com"),
        Arc::from("private"),
    );
    let cross_pass = AlbumPass {
        kind: PassKind::Album,
        album: album.with_cross_zone_sources(vec![library.all()]),
        exclude_ids: Arc::default(),
    };
    let mut primary_config = config.clone();
    primary_config.library = Arc::from("PrimarySync");
    let cross = catalog_plan_context_for_source(
        &[cross_pass],
        &primary_config,
        DownloadControls::download_hidden(),
        Some(name),
    )
    .await
    .unwrap()
    .unwrap();
    let cross_spec = GenerationSpec {
        format: 2,
        scope: cross.scope,
        zone: cross.zone,
        config_hash: cross.config_hash.clone(),
        basis: cross.config_hash,
        profile: cross.profile,
        metadata_enabled: cross.metadata_enabled,
        metadata_flags: cross.metadata_flags,
    };
    let cross_root = db
        .begin_selection_generation(owner(), cross_spec, BUDGET)
        .await
        .unwrap();
    let cross_receipt = db
        .seal_observed_selection(owner(), cross_root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let cross_proof = ObservationProof::inventory(vec![cross_receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(cross_receipt)
                .delta(&cursor, "wrong-owner-token")
                .unwrap(),
            &cursor,
        )
        .unwrap()
        .with_catalog_origin(origin.clone());
    let mut cross_transition = transition(&db, "wrong-owner-token").await;
    cross_transition
        .expected_metadata
        .push((baseline_key.clone(), Some(cursor.clone())));
    assert!(
        db.commit_observation_transition(cross_transition, Some(cross_proof), false)
            .await
            .is_err()
    );
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("primary-active")
    );
    assert_eq!(db.get_metadata(PROOF).await.unwrap(), prior_primary);
    {
        let conn = db.acquire_lock("verify cross-owner refusal").unwrap();
        let (phase, count): (String, i64) = conn.query_row("SELECT r.phase,(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=r.id) FROM source_revalidation_rounds r WHERE r.id=?1", [&origin.nonce], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((phase.as_str(), count), ("observing", 11));
    }
    let spec = GenerationSpec {
        format: 2,
        scope: context.scope,
        zone: context.zone,
        config_hash: context.config_hash.clone(),
        basis: context.config_hash,
        profile: context.profile,
        metadata_enabled: context.metadata_enabled,
        metadata_flags: context.metadata_flags,
    };
    let root = db
        .begin_selection_generation(owner(), spec, BUDGET)
        .await
        .unwrap();
    let receipt = db
        .seal_observed_selection(owner(), root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta(&cursor, "shared-eof")
                .unwrap(),
            &cursor,
        )
        .unwrap()
        .with_epoch_attempt(Some(attempt.clone()))
        .with_catalog_origin(origin.clone());
    let staged_key = format!("pending_sync_token:v27:policy:{name}");
    let mut expected = Vec::new();
    for key in [
        "enum_config_hash",
        "pending_enum_config_hash",
        "config_hash",
        "pending_download_config_hash",
        token_key.as_str(),
        staged_key.as_str(),
        baseline_key.as_str(),
    ] {
        expected.push((key.into(), db.get_metadata(key).await.unwrap()));
    }
    let transition = CheckpointTransition {
        expected_metadata: expected,
        legacy_preservation_proofs: vec![],
        legacy_config_hash: None,
        sparse_identity_proofs: vec![],
        metadata_updates: vec![(staged_key.clone(), "shared-eof".into())],
        metadata_deletes: vec![],
    };
    db.commit_observation_transition(transition, Some(proof), false)
        .await
        .unwrap();
    assert_eq!(
        db.sync_epoch_gate(name.into(), scope, None).await.unwrap(),
        crate::state::SyncEpochGate::Promote { attempt }
    );
    let mut ready_request = request;
    ready_request.expected_baseline = Some(cursor.clone());
    ready_request.expected_catalog_snapshot = db
        .fresh_catalog_snapshot(owner(), name.into())
        .await
        .unwrap();
    for (key, value) in &mut ready_request.expected_metadata {
        *value = db.get_metadata(key).await.unwrap();
    }
    assert!(
        db.install_fresh_catalog_origin(owner(), ready_request.clone())
            .await
            .is_err()
    );
    assert_eq!(
        db.get_metadata(&baseline_key).await.unwrap().as_deref(),
        Some(cursor.as_str())
    );
    let mut expected = Vec::new();
    for key in [
        "enum_config_hash",
        "pending_enum_config_hash",
        "config_hash",
        "pending_download_config_hash",
        token_key.as_str(),
        staged_key.as_str(),
        baseline_key.as_str(),
    ] {
        expected.push((key.to_owned(), db.get_metadata(key).await.unwrap()));
    }
    let promote = CheckpointTransition {
        expected_metadata: expected,
        legacy_preservation_proofs: vec![],
        legacy_config_hash: None,
        sparse_identity_proofs: vec![],
        metadata_updates: vec![
            (token_key.clone(), "shared-eof".into()),
            ("enum_config_hash".into(), "policy".into()),
        ],
        metadata_deletes: vec![staged_key],
    };
    db.commit_observation_transition(promote, None, true)
        .await
        .unwrap();
    let descriptor: Value =
        serde_json::from_str(&db.get_metadata(&proof_key).await.unwrap().unwrap()).unwrap();
    assert_eq!(descriptor["revalidation_round"], origin.nonce);
    assert_eq!(
        db.get_metadata(&token_key).await.unwrap().as_deref(),
        Some("shared-eof")
    );
    ready_request.fresh_cursor = "shared-eof".into();
    ready_request.expected_baseline = db.get_metadata(&baseline_key).await.unwrap();
    ready_request.expected_catalog_snapshot = db
        .fresh_catalog_snapshot(owner(), name.into())
        .await
        .unwrap();
    for (key, value) in &mut ready_request.expected_metadata {
        *value = db.get_metadata(key).await.unwrap();
    }
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), ready_request.clone())
            .await
            .unwrap(),
        CatalogBootstrapEligibility::CurrentLink
    );
    ready_request.plan.zone["ownerRecordName"] = serde_json::json!("other-owner");
    assert!(
        db.catalog_bootstrap_eligibility(owner(), ready_request)
            .await
            .is_err()
    );
    let ids = [
        "case-db",
        "case-owner",
        "case-empty",
        "case-zone",
        "case-extra",
        "case-missing",
        "case-stale",
        "case-token",
        "census-a",
    ];
    for id in ids {
        db.upsert_asset_master_mapping(name, id, &format!("master-{id}"))
            .await
            .unwrap();
    }
    let media: Vec<_> = ids
        .iter()
        .map(|id| {
            let path = dir.path().join(format!("{id}.jpg"));
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            (path, mtime)
        })
        .collect();
    let medium_mtime = std::fs::metadata(&medium).unwrap().modified().unwrap();
    let due = db
        .due_catalog_candidates(owner(), name.into(), origin.clone(), "policy".into())
        .await
        .unwrap();
    let candidate = |id: &str| {
        due.iter()
            .find(|candidate| candidate.id == id)
            .unwrap()
            .clone()
    };
    let pair = |id: &str| CatalogPairEvidence {
        database: "shared".into(),
        zone: zone.clone(),
        master: format!("master-{id}"),
        child: id.into(),
        master_type: "CPLMaster".into(),
        child_type: "CPLAsset".into(),
        reference_exact: true,
        explicit_deleted: true,
    };
    for id in [
        "case-db",
        "case-owner",
        "case-empty",
        "case-zone",
        "case-extra",
        "case-missing",
    ] {
        let mut evidence = pair(id);
        match id {
            "case-db" => evidence.database = "private".into(),
            "case-owner" => evidence.zone["ownerRecordName"] = serde_json::json!("other-owner"),
            "case-empty" => evidence.zone["ownerRecordName"] = serde_json::json!(""),
            "case-zone" => evidence.zone["zoneName"] = serde_json::json!("SharedSync-other"),
            "case-extra" => evidence.zone["zoneType"] = serde_json::json!("other-extra"),
            "case-missing" => {
                evidence.zone.as_object_mut().unwrap().remove("zoneType");
            }
            _ => panic!("unexpected source control {id}"),
        }
        assert!(
            !db.settle_catalog_candidate(
                owner(),
                name.into(),
                origin.clone(),
                "policy".into(),
                candidate(id),
                CatalogSettlementAuthority::Deleted {
                    deleted_at: None,
                    evidence
                }
            )
            .await
            .unwrap(),
            "wrong source evidence must retain {id}"
        );
    }
    let stale = CatalogOrigin::new(
        origin.plan_digest.clone(),
        origin.epoch.clone(),
        uuid::Uuid::new_v4().to_string(),
        origin.nonce.clone(),
    )
    .unwrap();
    assert!(
        !db.settle_catalog_candidate(
            owner(),
            name.into(),
            stale,
            "policy".into(),
            candidate("case-stale"),
            CatalogSettlementAuthority::Deleted {
                deleted_at: None,
                evidence: pair("case-stale")
            }
        )
        .await
        .unwrap()
    );
    assert!(
        db.settle_catalog_candidate(
            owner(),
            name.into(),
            origin.clone(),
            "policy".into(),
            candidate("census-a"),
            CatalogSettlementAuthority::Deleted {
                deleted_at: None,
                evidence: pair("census-a")
            }
        )
        .await
        .unwrap(),
        "exact shared full-zone evidence must settle the owner-produced candidate"
    );
    db.set_metadata(&token_key, "changed-current-token")
        .await
        .unwrap();
    assert!(
        !db.settle_catalog_candidate(
            owner(),
            name.into(),
            origin.clone(),
            "policy".into(),
            candidate("case-token"),
            CatalogSettlementAuthority::Deleted {
                deleted_at: None,
                evidence: pair("case-token")
            }
        )
        .await
        .unwrap()
    );
    {
        let conn = db
            .acquire_lock("verify shared settlement source guards")
            .unwrap();
        for id in [
            "case-db",
            "case-owner",
            "case-empty",
            "case-zone",
            "case-extra",
            "case-missing",
            "case-stale",
            "case-token",
        ] {
            let (deleted, debt):(i64,i64) = conn.query_row("SELECT (SELECT is_deleted FROM assets WHERE library=?1 AND id=?2 AND version_size='original'), (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?3 AND id=?2)", rusqlite::params![name,id,origin.nonce], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!((deleted, debt), (0, 1), "wrong source must retain {id}");
        }
        for id in [
            "case-db",
            "case-owner",
            "case-empty",
            "case-zone",
            "case-extra",
            "case-missing",
        ] {
            let deadline: i64 = conn
                .query_row(
                    "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                    rusqlite::params![origin.nonce, id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                deadline > chrono::Utc::now().timestamp(),
                "source refusal must defer {id}"
            );
        }
        let (versions, deleted, debt):(i64,i64,i64) = conn.query_row("SELECT COUNT(*),SUM(is_deleted),(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?2 AND id='census-a') FROM assets WHERE library=?1 AND id='census-a'", rusqlite::params![name,origin.nonce], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!((versions, deleted, debt), (2, 2, 0));
    }
    for (path, mtime) in media {
        assert_eq!(std::fs::read(&path).unwrap(), b"retained media");
        assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
    }
    assert_eq!(std::fs::read(&medium).unwrap(), b"retained medium");
    assert_eq!(
        std::fs::metadata(&medium).unwrap().modified().unwrap(),
        medium_mtime
    );
}

#[tokio::test]
async fn catalog_hint_current_legacy_link_preserves_ordinary_delta_and_debt() {
    for with_debt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
        let db = &fixture.db;
        let media = dir.path().join("legacy.jpg");
        if with_debt {
            std::fs::write(&media, b"legacy media").unwrap();
            let asset = crate::test_helpers::TestAssetRecord::new("legacy-child")
                .library("PrimarySync")
                .build();
            db.upsert_seen(&asset).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "legacy-child",
                "original",
                &media,
                "synthetic",
                None,
            )
            .await
            .unwrap();
        }
        let boundary = fixture.library.fresh_baseline_cursor().await.unwrap();
        db.set_metadata(&fixture.baseline_key, &boundary)
            .await
            .unwrap();
        db.seed_source_revalidation(
            owner(),
            "PrimarySync".into(),
            fixture.plan.scope.clone(),
            "policy".into(),
            CatalogOrigin::boundary(&boundary),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        let proof = ObservationProof::inventory(vec![fixture.receipt.clone()])
            .unwrap()
            .bridge(
                ObservationProof::delivery(fixture.receipt.clone())
                    .delta(&boundary, "legacy-active")
                    .unwrap(),
                &boundary,
            )
            .unwrap();
        let mut save = transition(db, "legacy-active").await;
        save.expected_metadata
            .push((fixture.baseline_key.clone(), Some(boundary)));
        db.commit_observation_transition(save, Some(proof), false)
            .await
            .unwrap();
        let descriptor: Value =
            serde_json::from_str(&db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
        let round = descriptor["revalidation_round"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("legacy-active").await)
                .await
                .unwrap(),
            CatalogBootstrapEligibility::CurrentLegacyLink
        );
        for (column, changed, original) in [
            (
                "mode",
                "future_mode".to_owned(),
                "legacy_unfiled_v1".to_owned(),
            ),
            (
                "mode",
                "catalog_v2".to_owned(),
                "legacy_unfiled_v1".to_owned(),
            ),
            ("phase", "observing".to_owned(), "active".to_owned()),
            (
                "scope",
                "foreign-scope".to_owned(),
                fixture.plan.scope.clone(),
            ),
            ("enum_hash", "wrong-policy".to_owned(), "policy".to_owned()),
        ] {
            {
                let conn = db.acquire_lock("invalidate current legacy link").unwrap();
                conn.execute(
                    &format!("UPDATE source_revalidation_rounds SET {column}=?2 WHERE id=?1"),
                    rusqlite::params![round, changed],
                )
                .unwrap();
            }
            assert!(
                db.catalog_bootstrap_eligibility(
                    owner(),
                    fixture.hint_request("legacy-active").await
                )
                .await
                .is_err()
            );
            {
                let conn = db.acquire_lock("restore current legacy link").unwrap();
                conn.execute(
                    &format!("UPDATE source_revalidation_rounds SET {column}=?2 WHERE id=?1"),
                    rusqlite::params![round, original],
                )
                .unwrap();
            }
        }
        let delta = ObservationProof::delivery(fixture.receipt.clone())
            .delta("legacy-active", "legacy-next")
            .unwrap();
        for trigger in [
            "CREATE TRIGGER legacy_delta_fault BEFORE INSERT ON metadata WHEN NEW.key='observed_selection:v27:PrimarySync' BEGIN SELECT RAISE(ABORT,'legacy checkpoint fault'); END",
            "CREATE TRIGGER legacy_delta_fault AFTER UPDATE ON metadata WHEN NEW.key='sync_token:v27:PrimarySync' BEGIN UPDATE source_revalidation_rounds SET phase='abandoned' WHERE mode='legacy_unfiled_v1'; END",
        ] {
            {
                let conn = db.acquire_lock("inject legacy delta failure").unwrap();
                conn.execute_batch(trigger).unwrap();
            }
            assert!(
                db.commit_observation_transition(
                    transition(db, "legacy-next").await,
                    Some(delta.clone()),
                    false
                )
                .await
                .is_err()
            );
            {
                let conn = db.acquire_lock("clear legacy delta failure").unwrap();
                conn.execute_batch("DROP TRIGGER legacy_delta_fault")
                    .unwrap();
            }
            let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap();
            assert_eq!(
                reopened.get_metadata(TOKEN).await.unwrap().as_deref(),
                Some("legacy-active")
            );
            assert_eq!(
                reopened
                    .catalog_bootstrap_eligibility(
                        owner(),
                        fixture.hint_request("legacy-active").await
                    )
                    .await
                    .unwrap(),
                CatalogBootstrapEligibility::CurrentLegacyLink
            );
        }
        db.commit_observation_transition(transition(db, "legacy-next").await, Some(delta), false)
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata(TOKEN).await.unwrap().as_deref(),
            Some("legacy-next")
        );
        {
            let conn = db
                .acquire_lock("verify legacy round after ordinary delta")
                .unwrap();
            let (mode,phase,debt):(String,String,i64) = conn.query_row("SELECT r.mode,r.phase,COUNT(d.id) FROM source_revalidation_rounds r LEFT JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 GROUP BY r.id", [&round], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(
                (mode.as_str(), phase.as_str(), debt),
                ("legacy_unfiled_v1", "active", i64::from(with_debt))
            );
        }
        let due = db
            .due_source_revalidation(owner(), "PrimarySync".into(), fixture.plan.scope.clone())
            .await
            .unwrap();
        assert_eq!(due.len(), usize::from(with_debt));
        assert_eq!(
            db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("legacy-next").await)
                .await
                .unwrap(),
            CatalogBootstrapEligibility::CurrentLegacyLink
        );
        let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .catalog_bootstrap_eligibility(owner(), fixture.hint_request("legacy-next").await)
                .await
                .unwrap(),
            CatalogBootstrapEligibility::CurrentLegacyLink
        );
        if with_debt {
            assert_eq!(std::fs::read(&media).unwrap(), b"legacy media");
        }
    }
}

#[derive(Clone)]
struct QuietLegacySession(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for QuietLegacySession {
    async fn post(
        &self,
        _url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                serde_json::json!({"records":[{"recordName":"index","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        anyhow::bail!("quiet legacy inspection must not contact the provider")
    }
    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LegacyCensusCase {
    Legacy,
    UnknownMode,
    MalformedScope,
}

async fn quiet_current_legacy_census(case: LegacyCensusCase) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let media = dir.path().join("legacy.jpg");
    std::fs::write(&media, b"retained legacy bytes").unwrap();
    let asset = crate::test_helpers::TestAssetRecord::new("legacy-child")
        .library("PrimarySync")
        .build();
    fixture.db.upsert_seen(&asset).await.unwrap();
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "legacy-child",
            "original",
            &media,
            "synthetic",
            None,
        )
        .await
        .unwrap();
    let boundary = fixture.library.fresh_baseline_cursor().await.unwrap();
    fixture
        .db
        .set_metadata(&fixture.baseline_key, &boundary)
        .await
        .unwrap();
    fixture
        .db
        .seed_source_revalidation(
            owner(),
            "PrimarySync".into(),
            fixture.plan.scope.clone(),
            "policy".into(),
            CatalogOrigin::boundary(&boundary),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let proof = ObservationProof::inventory(vec![fixture.receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(fixture.receipt.clone())
                .delta(&boundary, "legacy-active")
                .unwrap(),
            &boundary,
        )
        .unwrap();
    let mut save = transition(&fixture.db, "legacy-active").await;
    save.expected_metadata
        .push((fixture.baseline_key.clone(), Some(boundary)));
    fixture
        .db
        .commit_observation_transition(save, Some(proof), false)
        .await
        .unwrap();
    let candidates = fixture
        .db
        .due_source_revalidation(owner(), "PrimarySync".into(), fixture.plan.scope.clone())
        .await
        .unwrap();
    assert_eq!(
        candidates.len(),
        1,
        "actual owner-created legacy debt must be due"
    );
    fixture
        .db
        .settle_source_revalidation(owner(), candidates.into_iter().next().unwrap(), None, false)
        .await
        .unwrap();
    let snapshot = |db: &SqliteStateDb| -> (String, String, i64) {
        db.acquire_lock("legacy quiet debt snapshot").unwrap().query_row(
            "SELECT r.id,r.mode,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library='PrimarySync' AND r.phase='active' AND d.id='legacy-child'",
            [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap()
    };
    let before = snapshot(&fixture.db);
    assert!(
        before.2 > chrono::Utc::now().timestamp(),
        "real refusal must set a future deadline"
    );
    drop(fixture);
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    assert_eq!(snapshot(&db), before, "legacy owner survives reopen");
    let active_key = "observed_selection:v27:PrimarySync";
    let active = db.get_metadata(active_key).await.unwrap().unwrap();
    let descriptor: Value = serde_json::from_str(&active).unwrap();
    assert_eq!(descriptor["revalidation_round"], before.0);
    let token = db.get_metadata(TOKEN).await.unwrap().unwrap();
    let original_scope: String = db
        .acquire_lock("read owner-created legacy scope")
        .unwrap()
        .query_row(
            "SELECT scope FROM source_revalidation_rounds WHERE id=?1",
            [&before.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        PhotoLibrary::provider_scope_projection(&original_scope)
            .unwrap()
            .0,
        "PrimarySync"
    );
    if case == LegacyCensusCase::UnknownMode {
        db.acquire_lock("simulate future mode on the actual selected round")
            .unwrap()
            .execute(
                "UPDATE source_revalidation_rounds SET mode='future_mode' WHERE id=?1",
                [&before.0],
            )
            .unwrap();
    } else if case == LegacyCensusCase::MalformedScope {
        assert_eq!(db.acquire_lock("simulate malformed scope on the actual selected round").unwrap().execute(
            "UPDATE source_revalidation_rounds SET scope='future-malformed-scope' WHERE id=?1 AND mode='legacy_unfiled_v1' AND enum_hash='policy'",
            [&before.0],
        ).unwrap(), 1);
    }
    let before = snapshot(&db);
    let rows = |db: &SqliteStateDb| {
        let conn = db
            .acquire_lock("snapshot selected legacy round and debt")
            .unwrap();
        let mut stmt = conn.prepare("SELECT r.*,d.* FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 ORDER BY d.id").unwrap();
        stmt.query_map([&before.0], |row| {
            (0..row.as_ref().column_count())
                .map(|index| row.get::<_, rusqlite::types::Value>(index))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
    };
    let exact_rows = rows(&db);
    assert_eq!(
        db.get_metadata(active_key).await.unwrap().as_deref(),
        Some(active.as_str())
    );
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some(token.as_str())
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(HashMap::new()),
        Box::new(QuietLegacySession(calls.clone())),
        Arc::new(serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(db.clone(), owner(), "com"));
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::default(),
    };
    let state = crate::sync_cycle::LibraryState {
        library,
        cross_zone_libraries: Vec::new(),
        pass_scope: crate::commands::PassScope {
            include_albums: false,
            include_smart_folders: false,
            include_unfiled: true,
        },
        zone_name: "PrimarySync".into(),
        sync_token_key: TOKEN.into(),
        plan: crate::commands::AlbumPlan { passes: vec![pass] },
        plan_is_stale: false,
        plan_needs_refresh: false,
    };
    let mut template = DownloadConfig::test_default();
    template.directory = Arc::from(dir.path());
    template.state_db = Some(db.clone());
    template.library = Arc::from("PrimarySync");
    let builder = |_, _, _, _| Arc::new(template.clone());
    let before_calls = calls.load(Ordering::SeqCst);
    let quiet = crate::sync_loop::quiet_catalog_cycle_for_test(
        &[&state],
        &builder,
        DownloadControls::download_hidden(),
        "policy",
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("selected legacy or unknown-mode debt must not become quiet success");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before_calls,
        "future work must not trigger provider lookup"
    );
    assert_eq!(snapshot(&db), before);
    assert_eq!(
        rows(&db),
        exact_rows,
        "the selected round and debt must remain unchanged"
    );
    assert_eq!(
        db.get_metadata(active_key).await.unwrap().as_deref(),
        Some(active.as_str())
    );
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some(token.as_str())
    );
    assert_eq!(std::fs::read(&media).unwrap(), b"retained legacy bytes");
    assert_eq!(quiet.failed_count, 0);
    let notifier = crate::notifications::Notifier::new(None);
    let report = dir.path().join("quiet-legacy.json");
    let globals = crate::config::GlobalArgs {
        username: Some("synthetic@example.invalid".into()),
        domain: None,
        data_dir: None,
    };
    let sync = crate::cli::SyncArgs {
        config_overrides: crate::config::SyncConfigOverrides {
            download_dir: Some(dir.path().display().to_string()),
            ..Default::default()
        },
        ..Default::default()
    };
    let config =
        crate::config::Config::build(&globals, &crate::cli::PasswordArgs::default(), sync, None)
            .unwrap();
    let reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: true,
            report_path: Some(&report),
            run_options: crate::report::RunOptions::from_config(&config),
            health_dir: dir.path(),
            personality_mode: crate::personality::Mode::Off,
            state_db: Some(db.as_ref()),
            metrics_handle: None,
            notifier: &notifier,
        });
    let mut health = crate::health::HealthStatus::new();
    reporter
        .report_completed_cycle(
            &mut health,
            crate::cycle_reporter::CycleFacts::from_cycle_result(&quiet, std::time::Duration::ZERO),
        )
        .await;
    let output: Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(output["status"], "partial_failure");
    assert_eq!(output["stats"]["failed"], 0);
    assert!(health.last_success_at.is_none());
    if case == LegacyCensusCase::MalformedScope {
        assert_eq!(rows(&db), exact_rows);
        assert_eq!(calls.load(Ordering::SeqCst), before_calls);
        return;
    }
    db.acquire_lock("classify unrelated historical debt")
        .unwrap()
        .execute(
            "UPDATE source_revalidation_rounds SET enum_hash='historical-policy' WHERE id=?1",
            [&before.0],
        )
        .unwrap();
    let historical = crate::sync_loop::quiet_catalog_cycle_for_test(
        &[&state],
        &builder,
        DownloadControls::download_hidden(),
        "policy",
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        historical.is_none(),
        "unrelated historical policy must not veto current health"
    );
    assert_eq!(calls.load(Ordering::SeqCst), before_calls);
    if case == LegacyCensusCase::Legacy {
        let mut foreign: Value = serde_json::from_str(&original_scope).unwrap();
        foreign["zone"]["zoneName"] = "PrimarySync-foreign".into();
        let foreign = foreign.to_string();
        assert_eq!(
            PhotoLibrary::provider_scope_projection(&foreign).unwrap().0,
            "PrimarySync-foreign"
        );
        assert_eq!(
            db.acquire_lock("classify a known foreign legacy scope")
                .unwrap()
                .execute(
                    "UPDATE source_revalidation_rounds SET enum_hash='policy',scope=?2 WHERE id=?1",
                    rusqlite::params![before.0, foreign],
                )
                .unwrap(),
            1
        );
        let foreign_result = crate::sync_loop::quiet_catalog_cycle_for_test(
            &[&state],
            &builder,
            DownloadControls::download_hidden(),
            "policy",
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            foreign_result.is_none(),
            "a known valid foreign scope must not veto current health"
        );
        assert_eq!(calls.load(Ordering::SeqCst), before_calls);
    }
}

#[tokio::test]
async fn quiet_current_legacy_future_debt_is_visible() {
    quiet_current_legacy_census(LegacyCensusCase::Legacy).await;
}

#[tokio::test]
async fn quiet_current_unknown_mode_is_visible() {
    quiet_current_legacy_census(LegacyCensusCase::UnknownMode).await;
}

#[tokio::test]
async fn quiet_current_linked_legacy_malformed_scope_debt_is_visible() {
    quiet_current_legacy_census(LegacyCensusCase::MalformedScope).await;
}

#[tokio::test]
async fn catalog_resume_returns_actual_saved_expiry_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    assert!(
        fixture
            .db
            .catalog_resume(owner(), fixture.hint_request("after").await)
            .await
            .unwrap()
            .is_none()
    );
    let origin = fixture.install().await;
    let resume = fixture
        .db
        .catalog_resume(owner(), fixture.hint_request("after").await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resume.origin, origin);
    assert_eq!(resume.boundary, "before");
    assert_eq!(resume.relation, CatalogResumeRelation::SavedBoundary);
    let request = fixture.hint_request("after").await;
    for invalid in [
        {
            let mut r = request.clone();
            r.fresh_cursor = "wrong-token".into();
            r
        },
        {
            let mut r = request.clone();
            r.plan.database = "shared".into();
            r
        },
        {
            let mut r = request.clone();
            r.plan.zone["zoneType"] = serde_json::json!("wrong-full-zone");
            r
        },
        {
            let mut r = request.clone();
            r.plan.provider = "wrong-pin".into();
            r
        },
        {
            let mut r = request.clone();
            r.plan.config_hash = "a".repeat(64);
            r
        },
        {
            let mut r = request.clone();
            r.expected_attempt = uuid::Uuid::new_v4().to_string();
            r
        },
        {
            let mut r = request.clone();
            r.expected_epoch = uuid::Uuid::new_v4().to_string();
            r
        },
        {
            let mut r = request.clone();
            r.expected_catalog_snapshot = "stale".into();
            r
        },
        {
            let mut r = request.clone();
            r.baseline_key.push_str("alias");
            r
        },
    ] {
        assert!(fixture.db.catalog_resume(owner(), invalid).await.is_err());
    }
    let wrong_owner = crate::state::db::account::AccountOwner::authenticated(
        "other@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"other-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(
        fixture
            .db
            .catalog_resume(wrong_owner, fixture.hint_request("after").await)
            .await
            .is_err()
    );
    for invalid_scope in [
        "catalog:v2:malformed".to_owned(),
        CatalogOrigin::new(
            origin.plan_digest.clone(),
            uuid::Uuid::new_v4().to_string(),
            origin.attempt.clone(),
            origin.nonce.clone(),
        )
        .unwrap()
        .scope(),
        CatalogOrigin::new(
            origin.plan_digest.clone(),
            origin.epoch.clone(),
            origin.attempt.clone(),
            uuid::Uuid::new_v4().to_string(),
        )
        .unwrap()
        .scope(),
    ] {
        {
            let conn = fixture
                .db
                .acquire_lock("invalidate catalog resume identity")
                .unwrap();
            conn.execute(
                "UPDATE source_revalidation_rounds SET scope=?2 WHERE id=?1",
                rusqlite::params![origin.nonce, invalid_scope],
            )
            .unwrap();
        }
        assert!(
            fixture
                .db
                .catalog_resume(owner(), fixture.hint_request("after").await)
                .await
                .is_err()
        );
        {
            let conn = fixture
                .db
                .acquire_lock("restore catalog resume identity")
                .unwrap();
            conn.execute(
                "UPDATE source_revalidation_rounds SET scope=?2 WHERE id=?1",
                rusqlite::params![origin.nonce, origin.scope()],
            )
            .unwrap();
        }
    }
    let duplicate = CatalogOrigin::new(
        origin.plan_digest.clone(),
        origin.epoch.clone(),
        origin.attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    {
        let conn = fixture
            .db
            .acquire_lock("insert competing catalog resume")
            .unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'','','marking','catalog_v2')", rusqlite::params![duplicate.nonce,duplicate.scope(),CatalogOrigin::boundary("before")]).unwrap();
    }
    assert!(
        fixture
            .db
            .catalog_resume(owner(), fixture.hint_request("after").await)
            .await
            .is_err()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("make competing catalog round active")
            .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='active' WHERE id=?1",
            [&duplicate.nonce],
        )
        .unwrap();
    }
    assert!(
        fixture
            .db
            .catalog_resume(owner(), fixture.hint_request("after").await)
            .await
            .is_err()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("retain competing catalog history")
            .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
            [&duplicate.nonce],
        )
        .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET mode='future_mode' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
    }
    assert!(
        fixture
            .db
            .catalog_resume(owner(), fixture.hint_request("after").await)
            .await
            .is_err()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("restore catalog mode and inject read fault")
            .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET mode='catalog_v2' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
        conn.execute_batch(
            "ALTER TABLE source_revalidation_rounds RENAME TO unavailable_resume_rounds",
        )
        .unwrap();
    }
    assert!(
        fixture
            .db
            .catalog_resume(owner(), request.clone())
            .await
            .is_err()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("restore catalog resume read")
            .unwrap();
        conn.execute_batch(
            "ALTER TABLE unavailable_resume_rounds RENAME TO source_revalidation_rounds",
        )
        .unwrap();
    }
    let before_seed = fixture.hint_request("after").await;
    fixture
        .db
        .seed_source_revalidation(
            owner(),
            "PrimarySync".into(),
            origin.scope(),
            "policy".into(),
            CatalogOrigin::boundary(&resume.boundary),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        fixture
            .db
            .catalog_resume(owner(), before_seed)
            .await
            .is_err()
    );
    let reopened = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    let resume = reopened
        .catalog_resume(owner(), fixture.hint_request("after").await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resume.origin, origin);
    assert_eq!(resume.boundary, "before");
    assert_eq!(resume.relation, CatalogResumeRelation::SavedBoundary);
}

#[tokio::test]
async fn catalog_resume_equal_fresh_boundary_is_not_eof_authority() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let zone = fixture.plan.zone.clone();
    let mock = MockPhotosSession::new()
        .ok(serde_json::json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}))
        .ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"after"}]}));
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(HashMap::new()),
        Box::new(mock),
        Arc::new(zone),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(fixture.db.clone(), owner(), "com"));
    let mut request = fixture.hint_request("after").await;
    let actual_fresh_boundary = library.fresh_baseline_cursor().await.unwrap();
    assert_eq!(actual_fresh_boundary, "after");
    request.fresh_cursor = actual_fresh_boundary;
    let origin = fixture
        .db
        .install_fresh_catalog_origin(owner(), request)
        .await
        .unwrap();
    let resume = fixture
        .db
        .catalog_resume(owner(), fixture.hint_request("after").await)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resume.origin, origin);
    assert_eq!(resume.boundary, "after");
    assert_eq!(resume.relation, CatalogResumeRelation::CurrentToken);
    assert!(
        fixture
            .db
            .due_catalog_candidates(owner(), "PrimarySync".into(), origin, "policy".into())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn catalog_bootstrap_hint_preserves_linked_progress_and_rejects_bad_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let db = &fixture.db;
    let request = fixture.hint_request("after").await;
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), request.clone())
            .await
            .unwrap(),
        CatalogBootstrapEligibility::AttemptBootstrap
    );
    let before = db.get_metadata(PROOF).await.unwrap();
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("after")
    );
    assert_eq!(db.get_metadata(&fixture.baseline_key).await.unwrap(), None);
    for bad in [
        {
            let mut r = request.clone();
            r.fresh_cursor = "wrong-token".into();
            r
        },
        {
            let mut r = request.clone();
            r.plan.database = "shared".into();
            r
        },
        {
            let mut r = request.clone();
            r.plan.zone["ownerRecordName"] = serde_json::json!("wrong-owner");
            r
        },
        {
            let mut r = request.clone();
            r.plan.provider = "wrong-pin".into();
            r
        },
        {
            let mut r = request.clone();
            r.plan.config_hash = "a".repeat(64);
            r
        },
        {
            let mut r = request.clone();
            r.expected_epoch = uuid::Uuid::new_v4().to_string();
            r
        },
        {
            let mut r = request.clone();
            r.expected_attempt = uuid::Uuid::new_v4().to_string();
            r
        },
        {
            let mut r = request.clone();
            r.expected_catalog_snapshot = "stale".into();
            r
        },
        {
            let mut r = request.clone();
            r.baseline_key.push_str("alias");
            r
        },
    ] {
        assert!(
            db.catalog_bootstrap_eligibility(owner(), bad)
                .await
                .is_err()
        );
    }
    assert_eq!(db.get_metadata(PROOF).await.unwrap(), before);
    {
        let conn = db.acquire_lock("inject catalog read failure").unwrap();
        conn.execute_batch("ALTER TABLE source_revalidation_rounds RENAME TO catalog_rounds_temporarily_unavailable").unwrap();
    }
    assert!(
        db.catalog_bootstrap_eligibility(owner(), request)
            .await
            .is_err()
    );
    {
        let conn = db.acquire_lock("restore catalog read").unwrap();
        conn.execute_batch("ALTER TABLE catalog_rounds_temporarily_unavailable RENAME TO source_revalidation_rounds").unwrap();
    }
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("after").await)
            .await
            .unwrap(),
        CatalogBootstrapEligibility::AttemptBootstrap
    );
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    assert!(
        db.catalog_pending_signals(owner(), "PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("staged").await)
            .await
            .unwrap(),
        CatalogBootstrapEligibility::CurrentLink
    );
    let delta = ObservationProof::delivery(fixture.receipt.clone())
        .delta("staged", "next")
        .unwrap();
    db.commit_observation_transition(transition(db, "next").await, Some(delta), false)
        .await
        .unwrap();
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("next").await)
            .await
            .unwrap(),
        CatalogBootstrapEligibility::CurrentLink
    );
    let descriptor: Value =
        serde_json::from_str(&db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
    assert_eq!(descriptor["revalidation_round"], origin.nonce);
    let wrong_owner = crate::state::db::account::AccountOwner::authenticated(
        "other@example.invalid",
        "com",
        &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"other-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(
        db.catalog_bootstrap_eligibility(wrong_owner, fixture.hint_request("next").await)
            .await
            .is_err()
    );
    for (column, changed, original) in [
        ("mode", "future_mode".to_owned(), "catalog_v2".to_owned()),
        ("scope", "catalog:v2:malformed".to_owned(), origin.scope()),
        (
            "scope",
            CatalogOrigin::new(
                origin.plan_digest.clone(),
                uuid::Uuid::new_v4().to_string(),
                origin.attempt.clone(),
                origin.nonce.clone(),
            )
            .unwrap()
            .scope(),
            origin.scope(),
        ),
        (
            "scope",
            CatalogOrigin::new(
                origin.plan_digest.clone(),
                origin.epoch.clone(),
                uuid::Uuid::new_v4().to_string(),
                origin.nonce.clone(),
            )
            .unwrap()
            .scope(),
            origin.scope(),
        ),
        ("phase", "observing".to_owned(), "active".to_owned()),
        ("enum_hash", "other-policy".to_owned(), "policy".to_owned()),
    ] {
        {
            let conn = db.acquire_lock("change catalog hint evidence").unwrap();
            conn.execute(
                &format!("UPDATE source_revalidation_rounds SET {column}=?2 WHERE id=?1"),
                rusqlite::params![origin.nonce, changed],
            )
            .unwrap();
        }
        assert!(
            db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("next").await)
                .await
                .is_err()
        );
        let conn = db.acquire_lock("restore catalog hint evidence").unwrap();
        conn.execute(
            &format!("UPDATE source_revalidation_rounds SET {column}=?2 WHERE id=?1"),
            rusqlite::params![origin.nonce, original],
        )
        .unwrap();
    }
    let moved_nonce = uuid::Uuid::new_v4().to_string();
    {
        let conn = db.acquire_lock("remove linked round identity").unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET id=?2 WHERE id=?1",
            rusqlite::params![origin.nonce, moved_nonce],
        )
        .unwrap();
    }
    assert!(
        db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("next").await)
            .await
            .is_err()
    );
    {
        let conn = db.acquire_lock("restore linked round identity").unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET id=?1 WHERE id=?2",
            rusqlite::params![origin.nonce, moved_nonce],
        )
        .unwrap();
    }
    let raw = db.get_metadata(PROOF).await.unwrap().unwrap();
    {
        let conn = db
            .acquire_lock("remove descriptor for hint control")
            .unwrap();
        conn.execute("DELETE FROM metadata WHERE key=?1", [PROOF])
            .unwrap();
    }
    assert!(
        db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("next").await)
            .await
            .is_err()
    );
    db.set_metadata(PROOF, "malformed descriptor")
        .await
        .unwrap();
    assert!(
        db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("next").await)
            .await
            .is_err()
    );
    db.set_metadata(PROOF, &raw).await.unwrap();
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), fixture.hint_request("next").await)
            .await
            .unwrap(),
        CatalogBootstrapEligibility::CurrentLink
    );
}

#[tokio::test]
async fn catalog_bootstrap_hint_abandoned_link_and_stale_hint_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let db = &fixture.db;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    {
        let conn = db.acquire_lock("prepare abandoned hint").unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'retained',31)",
            [&origin.nonce],
        )
        .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
    }
    let hint = fixture.hint_request("staged").await;
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), hint.clone())
            .await
            .unwrap(),
        CatalogBootstrapEligibility::AttemptBootstrap
    );
    let proof = ObservationProof::delivery(fixture.receipt.clone())
        .delta("staged", "staged")
        .unwrap()
        .with_epoch_attempt(Some(fixture.attempt.clone()));
    let checkpoint = transition(db, "staged").await;
    let mut stale = hint.clone();
    stale.fresh_cursor = "staged".into();
    {
        let conn = db
            .acquire_lock("invalidate pre-stream catalog hint")
            .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET high_id='changed' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
    }
    assert!(
        db.install_catalog_origin_after_delta(owner(), stale, checkpoint.clone(), proof.clone())
            .await
            .is_err()
    );
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("staged")
    );
    let request = fixture.hint_request("staged").await;
    assert_eq!(
        db.catalog_bootstrap_eligibility(owner(), request.clone())
            .await
            .unwrap(),
        CatalogBootstrapEligibility::AttemptBootstrap
    );
    let new_origin = db
        .install_catalog_origin_after_delta(owner(), request, checkpoint, proof)
        .await
        .unwrap();
    assert_ne!(new_origin.nonce, origin.nonce);
    let conn = db
        .acquire_lock("verify abandoned hint debt retained")
        .unwrap();
    let debt: i64 = conn
        .query_row(
            "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='retained'",
            [&origin.nonce],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(debt, 31);
}

async fn strict_deleted_pair(db: Arc<SqliteStateDb>) -> CatalogSettlementAuthority {
    use crate::icloud::photos::{
        PhotoAlbum, PhotoAlbumConfig, ProviderRecordId, StrictCatalogPairResult,
    };
    let zone = serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    let mut records = super::records("master", super::CURRENT, "strict pair");
    for record in &mut records {
        record["zoneID"] = zone.clone();
        record["fields"]["isDeleted"] = serde_json::json!({"value":1,"type":"INT64"});
        record["fields"]["isExpunged"] = serde_json::json!({"value":1,"type":"INT64"});
        record["fields"]["dateExpunged"] =
            serde_json::json!({"value":1792541032000_i64,"type":"TIMESTAMP"});
    }
    records[1]["fields"]["masterRef"]["value"]["zoneID"] = zone.clone();
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from(
                "https://synthetic.example/database/1/photos/production/private",
            ),
            name: Arc::from("strict"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(zone),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(MockPhotosSession::new().ok(serde_json::json!({"records":records}))),
    );
    album.set_shadow_capture(ShadowCapture::new(db, owner(), "com"), Arc::from("private"));
    let StrictCatalogPairResult::Deleted {
        deleted_at,
        evidence,
    } = album
        .lookup_catalog_pair_strict(
            &ProviderRecordId::new("master"),
            &ProviderRecordId::new("asset-master"),
        )
        .await
        .result
    else {
        panic!("strict deleted pair must qualify")
    };
    CatalogSettlementAuthority::Deleted {
        deleted_at,
        evidence: CatalogPairEvidence {
            database: evidence.database.to_string(),
            zone: evidence.zone,
            master: evidence.master_record_name.as_str().to_owned(),
            child: evidence.child_record_name.as_str().to_owned(),
            master_type: evidence.master_type.to_string(),
            child_type: evidence.child_type.to_string(),
            reference_exact: evidence.reference_exact,
            explicit_deleted: evidence.explicit_deleted,
        },
    }
}

#[tokio::test]
async fn catalog_strict_deleted_pair_does_not_delete_historical_sibling() {
    use crate::state::VersionSizeKey;
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    for (id, version) in [
        ("asset-master", VersionSizeKey::Original),
        ("asset-master", VersionSizeKey::Medium),
        ("sibling", VersionSizeKey::Original),
    ] {
        let path = dir.path().join(format!("{id}-{}.jpg", version.as_str()));
        std::fs::write(&path, b"media preserved").unwrap();
        let record = crate::test_helpers::TestAssetRecord::new(id)
            .library("PrimarySync")
            .version_size(version)
            .build();
        fixture.db.upsert_seen(&record).await.unwrap();
        fixture
            .db
            .mark_downloaded(
                "PrimarySync",
                id,
                version.as_str(),
                &path,
                "synthetic",
                None,
            )
            .await
            .unwrap();
    }
    for id in ["asset-master", "sibling"] {
        fixture
            .db
            .upsert_asset_master_mapping("PrimarySync", id, "master")
            .await
            .unwrap();
    }
    let sibling_path = dir.path().join("sibling-original.jpg");
    let sibling_mtime = std::fs::metadata(&sibling_path)
        .unwrap()
        .modified()
        .unwrap();
    let before_mtime = std::fs::metadata(dir.path().join("asset-master-medium.jpg"))
        .unwrap()
        .modified()
        .unwrap();
    {
        let conn = fixture.db.acquire_lock("prepare sibling census").unwrap();
        conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'asset-master',0),(?1,'sibling',0)", [&origin.nonce]).unwrap();
        conn.execute("INSERT INTO asset_verifications VALUES('PrimarySync','asset-master','medium','unknown','protected rendition',1)", []).unwrap();
    }
    let authority = strict_deleted_pair(fixture.db.clone()).await;
    let due = fixture
        .db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    let candidate = due
        .iter()
        .find(|candidate| candidate.id == "asset-master")
        .unwrap()
        .clone();
    assert!(
        !fixture
            .db
            .settle_catalog_candidate(
                owner(),
                "PrimarySync".into(),
                origin.clone(),
                "policy".into(),
                candidate,
                authority.clone()
            )
            .await
            .unwrap()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("verify protected rendition retained")
            .unwrap();
        let (deleted,debt):(i64,i64) = conn.query_row("SELECT (SELECT SUM(is_deleted) FROM assets WHERE library='PrimarySync' AND id='asset-master'),(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='asset-master')", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!((deleted, debt), (0, 1));
        conn.execute("DELETE FROM asset_verifications WHERE library='PrimarySync' AND id='asset-master' AND version_size='medium'", []).unwrap();
        conn.execute("UPDATE source_revalidation_debt SET next_retry_at=0 WHERE round=?1 AND id='asset-master'", [&origin.nonce]).unwrap();
    }
    let due = fixture
        .db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    let candidate = due
        .iter()
        .find(|candidate| candidate.id == "asset-master")
        .unwrap()
        .clone();
    assert!(
        fixture
            .db
            .settle_catalog_candidate(
                owner(),
                "PrimarySync".into(),
                origin.clone(),
                "policy".into(),
                candidate,
                authority
            )
            .await
            .unwrap()
    );
    let conn = fixture
        .db
        .acquire_lock("verify exact pair deletion")
        .unwrap();
    let (a_deleted,b_deleted,b_debt):(i64,i64,i64) = conn.query_row("SELECT (SELECT SUM(is_deleted) FROM assets WHERE library='PrimarySync' AND id='asset-master'),(SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='sibling'),(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='sibling')", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!((a_deleted, b_deleted, b_debt), (2, 0, 1));
    assert_eq!(
        std::fs::metadata(&sibling_path)
            .unwrap()
            .modified()
            .unwrap(),
        sibling_mtime
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("asset-master-medium.jpg"))
            .unwrap()
            .modified()
            .unwrap(),
        before_mtime
    );
    assert_eq!(std::fs::read(sibling_path).unwrap(), b"media preserved");
}

#[tokio::test]
async fn catalog_and_unknown_candidates_cannot_use_legacy_settler() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    let path = dir.path().join("asset.jpg");
    std::fs::write(&path, b"media retained").unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("asset-master")
        .library("PrimarySync")
        .build();
    fixture.db.upsert_seen(&record).await.unwrap();
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "asset-master",
            "original",
            &path,
            "synthetic",
            None,
        )
        .await
        .unwrap();
    fixture
        .db
        .upsert_asset_master_mapping("PrimarySync", "asset-master", "master")
        .await
        .unwrap();
    {
        let conn = fixture
            .db
            .acquire_lock("prepare legacy boundary control")
            .unwrap();
        conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'asset-master',7)", [&origin.nonce]).unwrap();
    }
    let candidate = fixture
        .db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        !fixture
            .db
            .settle_source_revalidation(owner(), candidate.clone(), Some((true, None)), false)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .db
            .settle_source_revalidation(owner(), candidate.clone(), None, true)
            .await
            .unwrap()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("make unknown mode control")
            .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET mode='future_mode' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
    }
    assert!(
        !fixture
            .db
            .settle_source_revalidation(owner(), candidate.clone(), Some((true, None)), false)
            .await
            .unwrap()
    );
    assert!(
        !fixture
            .db
            .settle_source_revalidation(owner(), candidate, None, true)
            .await
            .unwrap()
    );
    let conn = fixture
        .db
        .acquire_lock("verify legacy boundary preserves debt")
        .unwrap();
    let (deleted,deadline):(i64,i64) = conn.query_row("SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='asset-master'),(SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='asset-master')", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!((deleted, deadline), (0, 7));
    assert_eq!(std::fs::read(path).unwrap(), b"media retained");
}

#[tokio::test]
async fn catalog_legacy_master_requires_exclusive_child_owner_for_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    let path = dir.path().join("legacy-master.jpg");
    std::fs::write(&path, b"legacy retained").unwrap();
    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("master")
        .library("PrimarySync")
        .build();
    fixture.db.upsert_seen(&record).await.unwrap();
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "master",
            "original",
            &path,
            "synthetic",
            None,
        )
        .await
        .unwrap();
    fixture
        .db
        .upsert_asset_master_mapping("PrimarySync", "asset-master", "master")
        .await
        .unwrap();
    {
        let conn = fixture
            .db
            .acquire_lock("prepare unowned legacy debt")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'master',0)",
            [&origin.nonce],
        )
        .unwrap();
    }
    let authority = strict_deleted_pair(fixture.db.clone()).await;
    let due = fixture
        .db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert!(!due[0].owned_child);
    assert!(
        !fixture
            .db
            .settle_catalog_candidate(
                owner(),
                "PrimarySync".into(),
                origin.clone(),
                "policy".into(),
                due[0].clone(),
                authority.clone()
            )
            .await
            .unwrap()
    );
    {
        let conn = fixture
            .db
            .acquire_lock("verify unowned legacy retained")
            .unwrap();
        let (deleted,debt):(i64,i64) = conn.query_row("SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='master'),(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='master')", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!((deleted, debt), (0, 1));
        conn.execute(
            "UPDATE source_revalidation_debt SET next_retry_at=0 WHERE round=?1 AND id='master'",
            [&origin.nonce],
        )
        .unwrap();
    }
    assert!(
        fixture
            .db
            .claim_legacy_master_state_owner("PrimarySync", "master", "asset-master")
            .await
            .unwrap()
    );
    let due = fixture
        .db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    assert!(due[0].owned_child);
    assert!(
        fixture
            .db
            .settle_catalog_candidate(
                owner(),
                "PrimarySync".into(),
                origin.clone(),
                "policy".into(),
                due[0].clone(),
                authority
            )
            .await
            .unwrap()
    );
    let conn = fixture
        .db
        .acquire_lock("verify exclusive legacy owner settlement")
        .unwrap();
    let (deleted,debt):(i64,i64) = conn.query_row("SELECT (SELECT is_deleted FROM assets WHERE library='PrimarySync' AND id='master'),(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='master')", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!((deleted, debt), (1, 0));
    assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), mtime);
    assert_eq!(std::fs::read(path).unwrap(), b"legacy retained");
}

#[tokio::test]
async fn catalog_observing_seed_reopens_idempotently_and_bridges_to_drain() {
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let media = dir.path().join("historical.jpg");
    std::fs::write(&media, b"retained media").unwrap();
    let asset = crate::test_helpers::TestAssetRecord::new("child")
        .library("PrimarySync")
        .build();
    fixture.db.upsert_seen(&asset).await.unwrap();
    fixture
        .db
        .mark_downloaded(
            "PrimarySync",
            "child",
            "original",
            &media,
            "synthetic",
            None,
        )
        .await
        .unwrap();
    fixture
        .db
        .upsert_asset_master_mapping("PrimarySync", "child", "master")
        .await
        .unwrap();
    let origin = fixture.install().await;
    let seed = |db: Arc<SqliteStateDb>, scope: String, policy: String, boundary: String| async move {
        db.seed_source_revalidation(
            owner(),
            "PrimarySync".into(),
            scope,
            policy,
            boundary,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    };
    let boundary = CatalogOrigin::boundary("before");
    seed(
        fixture.db.clone(),
        origin.scope(),
        "policy".into(),
        boundary.clone(),
    )
    .await
    .unwrap();
    {
        let conn = fixture
            .db
            .acquire_lock("set saved census deadline")
            .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='child'",
                [&origin.nonce],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        conn.execute(
            "UPDATE source_revalidation_debt SET next_retry_at=71 WHERE round=?1 AND id='child'",
            [&origin.nonce],
        )
        .unwrap();
    }
    seed(
        fixture.db.clone(),
        origin.scope(),
        "policy".into(),
        boundary.clone(),
    )
    .await
    .unwrap();
    let receipt = fixture.receipt.clone();
    let baseline_key = fixture.baseline_key.clone();
    drop(fixture);
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    seed(
        db.clone(),
        origin.scope(),
        "policy".into(),
        boundary.clone(),
    )
    .await
    .unwrap();
    for (scope, policy, saved_boundary) in [
        (origin.scope(), "other".to_owned(), boundary.clone()),
        (
            origin.scope(),
            "policy".to_owned(),
            CatalogOrigin::boundary("wrong"),
        ),
        (
            "catalog:v2:malformed".to_owned(),
            "policy".to_owned(),
            boundary.clone(),
        ),
        (
            "catalog:v3:unknown".to_owned(),
            "policy".to_owned(),
            boundary.clone(),
        ),
        (
            CatalogOrigin::new(
                origin.plan_digest.clone(),
                origin.epoch.clone(),
                origin.attempt.clone(),
                uuid::Uuid::new_v4().to_string(),
            )
            .unwrap()
            .scope(),
            "policy".to_owned(),
            boundary.clone(),
        ),
    ] {
        assert!(
            seed(db.clone(), scope, policy, saved_boundary)
                .await
                .is_err()
        );
    }
    for stale in [
        CatalogOrigin::new(
            origin.plan_digest.clone(),
            uuid::Uuid::new_v4().to_string(),
            origin.attempt.clone(),
            origin.nonce.clone(),
        )
        .unwrap(),
        CatalogOrigin::new(
            origin.plan_digest.clone(),
            origin.epoch.clone(),
            uuid::Uuid::new_v4().to_string(),
            origin.nonce.clone(),
        )
        .unwrap(),
        CatalogOrigin::new(
            format!("{:x}", Sha256::digest(b"different resolved plan")),
            origin.epoch.clone(),
            origin.attempt.clone(),
            origin.nonce.clone(),
        )
        .unwrap(),
    ] {
        assert!(
            seed(db.clone(), stale.scope(), "policy".into(), boundary.clone())
                .await
                .is_err()
        );
    }
    for (column, invalid, restored) in [
        ("mode", "legacy_unfiled_v1", "catalog_v2"),
        ("phase", "abandoned", "observing"),
    ] {
        {
            let conn = db.acquire_lock("invalidate catalog seed control").unwrap();
            conn.execute(
                &format!("UPDATE source_revalidation_rounds SET {column}=?2 WHERE id=?1"),
                rusqlite::params![origin.nonce, invalid],
            )
            .unwrap();
        }
        assert!(
            seed(
                db.clone(),
                origin.scope(),
                "policy".into(),
                boundary.clone()
            )
            .await
            .is_err()
        );
        let conn = db.acquire_lock("restore catalog seed control").unwrap();
        conn.execute(
            &format!("UPDATE source_revalidation_rounds SET {column}=?2 WHERE id=?1"),
            rusqlite::params![origin.nonce, restored],
        )
        .unwrap();
    }
    {
        let conn = db
            .acquire_lock("check observing origin after resume")
            .unwrap();
        let (rounds, phase, mode, deadline): (i64, String, String, i64) = conn.query_row(
            "SELECT (SELECT COUNT(*) FROM source_revalidation_rounds),r.phase,r.mode,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 AND d.id='child'",
            [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).unwrap();
        assert_eq!(
            (rounds, phase.as_str(), mode.as_str(), deadline),
            (1, "observing", "catalog_v2", 71)
        );
    }
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta("before", "staged")
                .unwrap(),
            "before",
        )
        .unwrap()
        .with_catalog_origin(origin.clone());
    let staged = "pending_sync_token:v27:policy:PrimarySync";
    let mut save = transition(&db, "staged").await;
    save.metadata_updates[0].0 = staged.into();
    save.expected_metadata.push((staged.into(), None));
    save.expected_metadata
        .push((baseline_key, Some("before".into())));
    db.commit_observation_transition(save, Some(proof), false)
        .await
        .unwrap();
    let mut promote = transition(&db, "staged").await;
    promote
        .metadata_updates
        .push(("enum_config_hash".into(), "policy".into()));
    promote
        .expected_metadata
        .push((staged.into(), Some("staged".into())));
    promote.metadata_deletes.push(staged.into());
    db.commit_observation_transition(promote, None, true)
        .await
        .unwrap();
    let active: Value =
        serde_json::from_str(&db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
    assert_eq!(active["revalidation_round"], origin.nonce);
    let due = db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            origin.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    assert_eq!(due.len(), 1);
    let evidence = CatalogPairEvidence {
        database: "private".into(),
        zone: serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
        master: "master".into(),
        child: "child".into(),
        master_type: "CPLMaster".into(),
        child_type: "CPLAsset".into(),
        reference_exact: true,
        explicit_deleted: false,
    };
    assert!(
        db.settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            origin,
            "policy".into(),
            due[0].clone(),
            CatalogSettlementAuthority::Live { evidence }
        )
        .await
        .unwrap()
    );
    assert!(
        db.catalog_pending_signals(owner(), "PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(std::fs::read(media).unwrap(), b"retained media");
}

async fn pending_claim_fixture(
    dir: &std::path::Path,
) -> (
    Arc<SqliteStateDb>,
    CatalogBaselineClaim,
    CatalogOrigin,
    GenerationSpec,
) {
    let path = dir.join("pending.db");
    prepare_existing_owned_database_for_test(&path, &owner()).unwrap();
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    db.set_metadata("enum_config_hash", "policy").await.unwrap();
    let zone = serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    let mock = MockPhotosSession::new()
        .ok(serde_json::json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}))
        .ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"shared-boundary"}]}));
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(HashMap::new()),
        Box::new(mock),
        Arc::new(zone),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(db.clone(), owner(), "com"));
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::default(),
    };
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir);
    config.state_db = Some(db.clone());
    config.library = Arc::from("PrimarySync");
    let context = catalog_plan_context_for_source(
        &[pass],
        &config,
        DownloadControls::download_hidden(),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let digest = crate::download::catalog_plan_digest_from_source_context(&context);
    let selected = GenerationSpec {
        format: 2,
        scope: context.scope.clone(),
        zone: context.zone.clone(),
        config_hash: context.config_hash.clone(),
        basis: context.config_hash.clone(),
        profile: context.profile.clone(),
        metadata_enabled: context.metadata_enabled,
        metadata_flags: context.metadata_flags,
    };
    let scope = library.baseline_scope();
    db.enroll_sync_epoch(vec![("PrimarySync".into(), scope.clone())])
        .await
        .unwrap();
    let attempt = match db
        .sync_epoch_gate("PrimarySync".into(), scope.clone(), None)
        .await
        .unwrap()
    {
        crate::state::SyncEpochGate::Baseline { attempt } => attempt,
        other => panic!("expected genuine pending baseline, got {other:?}"),
    };
    let cursor = library.fresh_baseline_cursor().await.unwrap();
    let (epoch, record) = {
        let conn = db.acquire_lock("read pending catalog fixture").unwrap();
        (
            crate::state::db::sync_epoch::validate(&conn).unwrap(),
            crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
                .unwrap()
                .unwrap(),
        )
    };
    assert_eq!(record.state, "pending");
    assert_eq!(record.attempt, attempt);
    let baseline_key = format!("baseline_cursor:v27:{}:{scope}:policy:", epoch.epoch);
    db.set_metadata(&baseline_key, &cursor).await.unwrap();
    let origin = CatalogOrigin::new(
        digest.clone(),
        epoch.epoch.clone(),
        attempt,
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    {
        let conn = db.acquire_lock("prepare retained pending census").unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'high','after','marking','catalog_v2')", rusqlite::params![origin.nonce, origin.scope(), CatalogOrigin::boundary(&cursor)]).unwrap();
        conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'asset1',10),(?1,'asset2',20)", [&origin.nonce]).unwrap();
    }
    let claim = CatalogBaselineClaim {
        library: "PrimarySync".into(),
        baseline_key,
        expected_baseline: Some(cursor),
        plan_digest: digest,
        expected_enum_hash: "policy".into(),
        expected_epoch: epoch.epoch.clone(),
        expected_epoch_json: serde_json::to_string(&epoch).unwrap(),
        expected_library_json: serde_json::to_string(&record).unwrap(),
        expected_metadata: vec![],
        now: record.next_attempt_at,
        watch_interval: Some(3600),
    };
    (db, claim, origin, selected)
}

#[tokio::test]
async fn reset_catalog_census_copy_is_atomic_and_retains_foreign_work() {
    let dir = tempfile::tempdir().unwrap();
    let (db, claim, origin, _) = pending_claim_fixture(dir.path()).await;
    let foreign = CatalogOrigin::new(
        origin.plan_digest.clone(),
        uuid::Uuid::new_v4().to_string(),
        origin.attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    {
        let conn = db.acquire_lock("prepare reset census control").unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'foreign-high','foreign-after','marking','catalog_v2')", rusqlite::params![foreign.nonce,foreign.scope(),CatalogOrigin::boundary(claim.expected_baseline.as_deref().unwrap())]).unwrap();
        conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'foreign-asset',42)", [&foreign.nonce]).unwrap();
        conn.execute_batch(&format!("CREATE TRIGGER fail_reset_census_copy BEFORE INSERT ON source_revalidation_debt WHEN NEW.round<>'{}' BEGIN SELECT RAISE(ABORT,'reset census copy fault'); END;", origin.nonce)).unwrap();
    }
    let error = db.reset_sync_tokens_atomic().await.unwrap_err();
    assert!(error.to_string().contains("reset census copy fault"));
    {
        let conn = db.acquire_lock("verify reset census rollback").unwrap();
        let record = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            claim.expected_library_json
        );
        let (phase, count): (String,i64) = conn.query_row("SELECT r.phase,COUNT(d.id) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 GROUP BY r.id", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!((phase.as_str(), count), ("marking", 2));
        conn.execute_batch("DROP TRIGGER fail_reset_census_copy")
            .unwrap();
    }
    db.reset_sync_tokens_atomic().await.unwrap();
    assert_eq!(
        db.get_metadata(&claim.baseline_key).await.unwrap(),
        claim.expected_baseline
    );
    drop(db);
    let reopened = SqliteStateDb::open_owned(&dir.path().join("pending.db"), &owner())
        .await
        .unwrap();
    let conn = reopened
        .acquire_lock("verify reset census after reopen")
        .unwrap();
    let record = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
        .unwrap()
        .unwrap();
    assert_ne!(record.attempt, origin.attempt);
    let (new_id, new_scope, phase): (String,String,String) = conn.query_row("SELECT id,scope,phase FROM source_revalidation_rounds WHERE library='PrimarySync' AND mode='catalog_v2' AND phase='marking' AND scope<>?1", [&foreign.scope()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    let transferred = CatalogOrigin::parse(&new_scope).unwrap();
    assert_eq!(transferred.nonce, new_id);
    assert_eq!(transferred.epoch, origin.epoch);
    assert_eq!(transferred.attempt, record.attempt);
    assert_eq!(transferred.plan_digest, origin.plan_digest);
    assert_eq!(phase, "marking");
    let debt: Vec<(String, i64)> = conn
        .prepare("SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id")
        .unwrap()
        .query_map([&new_id], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(debt, vec![("asset1".into(), 10), ("asset2".into(), 20)]);
    let (old_phase, old_debt, foreign_phase, foreign_due): (String,i64,String,i64) = conn.query_row("SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),(SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1),(SELECT phase FROM source_revalidation_rounds WHERE id=?2),(SELECT next_retry_at FROM source_revalidation_debt WHERE round=?2 AND id='foreign-asset')", rusqlite::params![origin.nonce,foreign.nonce], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    assert_eq!(
        (
            old_phase.as_str(),
            old_debt,
            foreign_phase.as_str(),
            foreign_due
        ),
        ("abandoned", 2, "marking", 42)
    );
}

#[tokio::test]
async fn catalog_claim_transfers_only_qualified_owner_and_retains_foreign_rows() {
    let dir = tempfile::tempdir().unwrap();
    let (db, claim, origin, _) = pending_claim_fixture(dir.path()).await;
    let foreign_epoch = CatalogOrigin::new(
        origin.plan_digest.clone(),
        uuid::Uuid::new_v4().to_string(),
        origin.attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    let wrong_nonce = CatalogOrigin::new(
        origin.plan_digest.clone(),
        origin.epoch.clone(),
        origin.attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    let mismatch_id = uuid::Uuid::new_v4().to_string();
    let boundary = CatalogOrigin::boundary(claim.expected_baseline.as_deref().unwrap());
    let foreign_ids = [
        foreign_epoch.nonce.clone(),
        mismatch_id.clone(),
        "malformed-catalog".to_owned(),
    ];
    let before = {
        let conn = db.acquire_lock("prepare foreign claim controls").unwrap();
        for (id, scope) in [
            (&foreign_epoch.nonce, foreign_epoch.scope()),
            (&mismatch_id, wrong_nonce.scope()),
            (&foreign_ids[2], "catalog:v2:malformed".into()),
        ] {
            conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'foreign-high','foreign-after','marking','catalog_v2')", rusqlite::params![id,scope,boundary]).unwrap();
            conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'foreign-debt',37)", [id]).unwrap();
        }
        conn.query_row("SELECT json_group_array(json_array(r.id,r.library,r.scope,r.enum_hash,r.boundary,r.high_id,r.after_id,r.phase,r.mode,d.id,d.next_retry_at)) FROM (SELECT * FROM source_revalidation_rounds WHERE id IN (?1,?2,?3) ORDER BY id) r JOIN source_revalidation_debt d ON d.round=r.id", rusqlite::params![foreign_ids[0],foreign_ids[1],foreign_ids[2]], |row| row.get::<_,String>(0)).unwrap()
    };
    let outcome = db
        .claim_catalog_baseline(owner(), claim.clone())
        .await
        .unwrap();
    let CatalogClaimOutcome::Transferred {
        old_round,
        new_round,
        attempt,
        copied_debt,
    } = outcome
    else {
        panic!("qualified owner must transfer")
    };
    assert_eq!(old_round, origin.nonce);
    assert_ne!(attempt, origin.attempt);
    assert_eq!(copied_debt, 2);
    {
        let conn = db.acquire_lock("verify exact claim transfer").unwrap();
        let after: String = conn.query_row("SELECT json_group_array(json_array(r.id,r.library,r.scope,r.enum_hash,r.boundary,r.high_id,r.after_id,r.phase,r.mode,d.id,d.next_retry_at)) FROM (SELECT * FROM source_revalidation_rounds WHERE id IN (?1,?2,?3) ORDER BY id) r JOIN source_revalidation_debt d ON d.round=r.id", rusqlite::params![foreign_ids[0],foreign_ids[1],foreign_ids[2]], |row| row.get(0)).unwrap();
        assert_eq!(before, after);
        let debt: Vec<(String, i64)> = conn
            .prepare(
                "SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id",
            )
            .unwrap()
            .query_map([&new_round], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            debt,
            vec![("asset1".to_owned(), 10), ("asset2".to_owned(), 20)]
        );
        let old: (String,i64) = conn.query_row("SELECT r.phase,COUNT(d.id) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 GROUP BY r.id", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!((old.0.as_str(), old.1), ("abandoned", 2));
    }
    let transferred =
        CatalogOrigin::new(claim.plan_digest, claim.expected_epoch, attempt, new_round).unwrap();
    db.seed_source_revalidation(
        owner(),
        "PrimarySync".into(),
        transferred.scope(),
        "policy".into(),
        boundary,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    db.seed_source_revalidation(
        owner(),
        "PrimarySync".into(),
        transferred.scope(),
        "policy".into(),
        CatalogOrigin::boundary(claim.expected_baseline.as_deref().unwrap()),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let conn = db
        .acquire_lock("verify pending transferred census resumed")
        .unwrap();
    let (phase,deadline): (String,i64) = conn.query_row("SELECT r.phase,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 AND d.id='asset1'", [&transferred.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!((phase.as_str(), deadline), ("observing", 10));
}

#[tokio::test]
async fn catalog_claim_refuses_multiple_qualified_owners_without_rotating_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let (db, claim, origin, _) = pending_claim_fixture(dir.path()).await;
    let second = CatalogOrigin::new(
        origin.plan_digest.clone(),
        origin.epoch.clone(),
        origin.attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    {
        let conn = db
            .acquire_lock("prepare competing qualified origin")
            .unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'high','after','observing','catalog_v2')", rusqlite::params![second.nonce,second.scope(),CatalogOrigin::boundary(claim.expected_baseline.as_deref().unwrap())]).unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'second',45)",
            [&second.nonce],
        )
        .unwrap();
    }
    assert!(db.claim_catalog_baseline(owner(), claim).await.is_err());
    let conn = db.acquire_lock("verify competing claim preserved").unwrap();
    let record = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
        .unwrap()
        .unwrap();
    assert_eq!(record.attempt, origin.attempt);
    let rows: (i64,i64) = conn.query_row("SELECT (SELECT COUNT(*) FROM source_revalidation_rounds WHERE id IN (?1,?2) AND phase IN ('marking','observing')), (SELECT COUNT(*) FROM source_revalidation_debt WHERE round IN (?1,?2))", rusqlite::params![origin.nonce,second.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(rows, (2, 3));
}

#[tokio::test]
async fn catalog_claim_without_qualified_owner_keeps_foreign_debt() {
    let dir = tempfile::tempdir().unwrap();
    let (db, claim, origin, _) = pending_claim_fixture(dir.path()).await;
    let foreign = CatalogOrigin::new(
        origin.plan_digest.clone(),
        uuid::Uuid::new_v4().to_string(),
        origin.attempt.clone(),
        origin.nonce.clone(),
    )
    .unwrap();
    let before = {
        let conn = db.acquire_lock("prepare foreign-only claim").unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET scope=?2 WHERE id=?1",
            rusqlite::params![origin.nonce, foreign.scope()],
        )
        .unwrap();
        conn.query_row("SELECT json_array(r.scope,r.boundary,r.high_id,r.after_id,r.phase,(SELECT json_group_array(json_array(id,next_retry_at)) FROM (SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=r.id ORDER BY id))) FROM source_revalidation_rounds r WHERE r.id=?1", [&origin.nonce], |row| row.get::<_,String>(0)).unwrap()
    };
    let outcome = db.claim_catalog_baseline(owner(), claim).await.unwrap();
    let CatalogClaimOutcome::Baseline { attempt } = outcome else {
        panic!("unqualified old origin requires a fresh baseline")
    };
    assert_ne!(attempt, origin.attempt);
    let conn = db
        .acquire_lock("verify foreign-only claim retained")
        .unwrap();
    let after: String = conn.query_row("SELECT json_array(r.scope,r.boundary,r.high_id,r.after_id,r.phase,(SELECT json_group_array(json_array(id,next_retry_at)) FROM (SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=r.id ORDER BY id))) FROM source_revalidation_rounds r WHERE r.id=?1", [&origin.nonce], |row| row.get(0)).unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn catalog_claim_stale_ready_transfers_owned_census_and_valid_ready_promotes() {
    let dir = tempfile::tempdir().unwrap();
    let (db, mut claim, origin, selected) = pending_claim_fixture(dir.path()).await;
    let boundary = claim.expected_baseline.clone().unwrap();
    db.seed_source_revalidation(
        owner(),
        "PrimarySync".into(),
        origin.scope(),
        "policy".into(),
        CatalogOrigin::boundary(&boundary),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let root = db
        .begin_selection_generation(owner(), selected, BUDGET)
        .await
        .unwrap();
    let receipt = db
        .seal_observed_selection(owner(), root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta(&boundary, "ready-token")
                .unwrap(),
            &boundary,
        )
        .unwrap()
        .with_epoch_attempt(Some(origin.attempt.clone()))
        .with_catalog_origin(origin.clone());
    let staged = "pending_sync_token:v27:policy:PrimarySync";
    let staged_descriptor = "observed_selection_staged:v27:policy:PrimarySync";
    let mut save = transition(&db, "ready-token").await;
    save.metadata_updates[0].0 = staged.into();
    save.expected_metadata.push((staged.into(), None));
    save.expected_metadata
        .push((claim.baseline_key.clone(), Some(boundary.clone())));
    db.commit_observation_transition(save, Some(proof), false)
        .await
        .unwrap();
    let read_record = || {
        let conn = db.acquire_lock("read ready catalog attempt").unwrap();
        crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap()
    };
    let ready = read_record();
    assert_eq!(ready.state, "ready");
    assert_eq!(ready.attempt, origin.attempt);
    assert!(ready.binding.is_some());
    let descriptor: Value =
        serde_json::from_str(&db.get_metadata(staged_descriptor).await.unwrap().unwrap()).unwrap();
    assert_eq!(descriptor["revalidation_round"], origin.nonce);
    let competing = CatalogOrigin::new(
        origin.plan_digest.clone(),
        origin.epoch.clone(),
        origin.attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    {
        let conn = db
            .acquire_lock("prepare competing ready catalog control")
            .unwrap();
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy',?3,'high','after','observing','catalog_v2')", rusqlite::params![competing.nonce,competing.scope(),CatalogOrigin::boundary(&boundary)]).unwrap();
    }
    claim.expected_library_json = serde_json::to_string(&ready).unwrap();
    assert_eq!(
        db.claim_catalog_baseline(owner(), claim.clone())
            .await
            .unwrap(),
        CatalogClaimOutcome::Promote {
            attempt: origin.attempt.clone()
        }
    );
    assert_eq!(read_record().attempt, origin.attempt);
    {
        let conn = db
            .acquire_lock("retire competing ready test control")
            .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='abandoned' WHERE id=?1",
            [&competing.nonce],
        )
        .unwrap();
    }
    // Missing-boundary rotation must roll back the original READY binding,
    // descriptor, staged token and debt together if the owner copy fails.
    let staged_bytes = db.get_metadata(staged).await.unwrap().unwrap();
    let descriptor_bytes = db.get_metadata(staged_descriptor).await.unwrap().unwrap();
    {
        let conn = db
            .acquire_lock("inject missing-boundary copy fault")
            .unwrap();
        conn.execute("DELETE FROM metadata WHERE key=?1", [&claim.baseline_key])
            .unwrap();
        conn.execute_batch(&format!("CREATE TRIGGER fail_missing_baseline_copy BEFORE INSERT ON source_revalidation_debt WHEN NEW.round<>'{}' BEGIN SELECT RAISE(ABORT,'missing-boundary copy fault'); END;", origin.nonce)).unwrap();
    }
    claim.expected_baseline = None;
    let error = db
        .claim_catalog_baseline(owner(), claim.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("missing-boundary copy fault"));
    assert_eq!(
        serde_json::to_string(&read_record()).unwrap(),
        claim.expected_library_json
    );
    assert_eq!(
        db.get_metadata(staged).await.unwrap().as_deref(),
        Some(staged_bytes.as_str())
    );
    assert_eq!(
        db.get_metadata(staged_descriptor).await.unwrap().as_deref(),
        Some(descriptor_bytes.as_str())
    );
    {
        let conn = db.acquire_lock("verify missing-boundary rollback").unwrap();
        let (phase, debt): (String, i64) = conn.query_row("SELECT r.phase,COUNT(d.id) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 GROUP BY r.id", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!((phase.as_str(), debt), ("observing", 2));
        conn.execute_batch("DROP TRIGGER fail_missing_baseline_copy")
            .unwrap();
        conn.execute(
            "INSERT INTO metadata(key,value) VALUES(?1,?2)",
            rusqlite::params![claim.baseline_key, boundary],
        )
        .unwrap();
    }
    claim.expected_baseline = Some(boundary.clone());
    db.set_metadata(staged, "ready-token").await.unwrap();
    assert!(db.get_metadata(staged_descriptor).await.unwrap().is_none());
    let stale = read_record();
    assert_eq!(stale.state, "ready");
    claim.expected_library_json = serde_json::to_string(&stale).unwrap();
    {
        let conn = db
            .acquire_lock("prepare nonresumable ready control")
            .unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='active' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
    }
    assert!(
        db.claim_catalog_baseline(owner(), claim.clone())
            .await
            .is_err()
    );
    assert_eq!(read_record().attempt, origin.attempt);
    {
        let conn = db.acquire_lock("restore resumable ready control").unwrap();
        conn.execute(
            "UPDATE source_revalidation_rounds SET phase='observing' WHERE id=?1",
            [&origin.nonce],
        )
        .unwrap();
    }
    let alias = format!("{}different-path", claim.baseline_key);
    db.set_metadata(&alias, &boundary).await.unwrap();
    let mut wrong_binding = claim.clone();
    wrong_binding.baseline_key = alias;
    assert!(
        db.claim_catalog_baseline(owner(), wrong_binding)
            .await
            .is_err()
    );
    let preserved = read_record();
    assert_eq!(
        (preserved.state.as_str(), preserved.attempt.as_str()),
        ("ready", origin.attempt.as_str())
    );
    {
        let conn = db.acquire_lock("inject ready transfer copy fault").unwrap();
        conn.execute_batch(&format!("CREATE TRIGGER fail_ready_copy BEFORE INSERT ON source_revalidation_debt WHEN NEW.round<>'{}' BEGIN SELECT RAISE(ABORT,'synthetic ready copy'); END;", origin.nonce)).unwrap();
    }
    let error = db
        .claim_catalog_baseline(owner(), claim.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("synthetic ready copy"));
    assert_eq!(
        (read_record().state.as_str(), read_record().attempt.as_str()),
        ("ready", origin.attempt.as_str())
    );
    {
        let conn = db.acquire_lock("remove ready transfer copy fault").unwrap();
        let (phase,debt):(String,i64) = conn.query_row("SELECT r.phase,COUNT(d.id) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 GROUP BY r.id", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert_eq!((phase.as_str(), debt), ("observing", 2));
        conn.execute_batch("DROP TRIGGER fail_ready_copy;").unwrap();
    }
    let outcome = db.claim_catalog_baseline(owner(), claim).await.unwrap();
    let CatalogClaimOutcome::Transferred {
        old_round,
        new_round,
        attempt,
        copied_debt,
    } = outcome
    else {
        panic!("stale READY must transfer the existing census")
    };
    assert_eq!(old_round, origin.nonce);
    assert_ne!(attempt, origin.attempt);
    assert_eq!(copied_debt, 2);
    let conn = db.acquire_lock("verify stale READY transfer").unwrap();
    let (phase, high, after, boundary_hash): (String, String, String, String) = conn
        .query_row(
            "SELECT phase,high_id,after_id,boundary FROM source_revalidation_rounds WHERE id=?1",
            [&new_round],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (
            phase.as_str(),
            high.as_str(),
            after.as_str(),
            boundary_hash.as_str()
        ),
        (
            "observing",
            "high",
            "after",
            CatalogOrigin::boundary(&boundary).as_str()
        )
    );
    let debts: Vec<(String, i64)> = conn
        .prepare("SELECT id,next_retry_at FROM source_revalidation_debt WHERE round=?1 ORDER BY id")
        .unwrap()
        .query_map([&new_round], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(debts, vec![("asset1".into(), 10), ("asset2".into(), 20)]);
    let (old_phase,old_debts):(String,i64) = conn.query_row("SELECT r.phase,COUNT(d.id) FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 GROUP BY r.id", [&origin.nonce], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!((old_phase.as_str(), old_debts), ("abandoned", 2));
    let record = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
        .unwrap()
        .unwrap();
    assert_eq!(
        (record.state.as_str(), record.attempt.as_str()),
        ("pending", attempt.as_str())
    );
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
    let delta = ObservationProof::delivery(receipt.clone())
        .delta("before", "after")
        .unwrap();
    assert!(proof.clone().bridge(delta.clone(), "before").is_some());
    assert!(
        proof
            .clone()
            .bridge(delta.clone(), "other-boundary")
            .is_none()
    );
    let first = proof.clone().bridge(delta, "before").unwrap();
    let second = proof
        .clone()
        .bridge(
            ObservationProof::delivery(receipt.clone())
                .delta("after", "final")
                .unwrap(),
            "after",
        )
        .unwrap();
    let joined = first.clone().continue_baseline(second).unwrap();
    assert!(joined.is_completed_baseline_for_boundary("PrimarySync", "final", "before"));
    assert!(!joined.is_completed_baseline_for_boundary("PrimarySync", "final", "after"));
    let wrong = proof
        .bridge(
            ObservationProof::delivery(receipt)
                .delta("other", "final")
                .unwrap(),
            "other",
        )
        .unwrap();
    assert!(first.continue_baseline(wrong).is_none());
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
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let fixture = CatalogOwnerFixture::new(dir.path(), "next-enum", true).await;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    let receipt_for_delta = fixture.receipt.clone();
    let db = fixture.db.clone();
    let old_active = db.get_metadata(PROOF).await.unwrap();
    {
        let conn = db
            .acquire_lock("seed linked staged catalog origin")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'asset')",
            [&origin.nonce],
        )
        .unwrap();
    }
    let staged = "pending_sync_token:v27:next-enum:PrimarySync";
    let staged_proof = "observed_selection_staged:v27:next-enum:PrimarySync";
    drop(fixture);
    drop(db);

    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let staged_json: Value =
        serde_json::from_str(&db.get_metadata(staged_proof).await.unwrap().unwrap()).unwrap();
    assert_eq!(staged_json["revalidation_round"], origin.nonce);

    let delta = ObservationProof::delivery(receipt_for_delta)
        .delta("staged", "quiet")
        .unwrap();
    let mut advance = transition(&db, "quiet").await;
    advance.metadata_updates[0].0 = staged.into();
    advance
        .expected_metadata
        .push((staged.into(), Some("staged".into())));
    db.commit_observation_transition(advance, Some(delta), false)
        .await
        .unwrap();
    let staged_json: Value =
        serde_json::from_str(&db.get_metadata(staged_proof).await.unwrap().unwrap()).unwrap();
    assert_eq!(staged_json["revalidation_round"], origin.nonce);

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
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1)",
                [&origin.nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), debt), ("observing", 1));
        let active: String = conn
            .query_row("SELECT value FROM metadata WHERE key=?1", [PROOF], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(Some(active), old_active);
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
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1)",
                [&origin.nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let active: Value = serde_json::from_str(&active_raw).unwrap();
        assert_eq!((phase.as_str(), debt), ("active", 1));
        assert_eq!(active["revalidation_round"], origin.nonce);
        let quarantined = quarantine_all_rounds(&conn).unwrap();
        assert_eq!(quarantined.0, 1);
    }
    {
        let conn = db.acquire_lock("verify quarantine retained debt").unwrap();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1)",
                [&origin.nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((phase.as_str(), debt), ("abandoned", 1));
    }
}

#[tokio::test]
async fn catalog_v2_linked_active_delta_keeps_debt_and_ordinary_producer_cannot_retire() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.db");
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    let receipt = fixture.receipt.clone();
    let db = fixture.db.clone();
    {
        let conn = db
            .acquire_lock("seed linked active catalog origin")
            .unwrap();
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id) VALUES(?1,'asset')",
            [&origin.nonce],
        )
        .unwrap();
    }
    drop(fixture);
    drop(db);
    let db = Arc::new(SqliteStateDb::open_owned(&path, &owner()).await.unwrap());
    let active: Value = serde_json::from_str(
        &db.get_metadata("observed_selection:v27:PrimarySync")
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(active["revalidation_round"], origin.nonce);
    let delta = ObservationProof::delivery(receipt)
        .delta("staged", "quiet")
        .unwrap();
    let advance = transition(&db, "quiet").await;
    db.commit_observation_transition(advance.clone(), Some(delta), false)
        .await
        .unwrap();
    {
        let conn = db.acquire_lock("verify linked active delta").unwrap();
        let (phase, debt): (String, i64) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1)",
                [&origin.nonce],
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
    assert_eq!(active["revalidation_round"], origin.nonce);
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
                "SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1",
                [&origin.nonce],
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
    let dir = tempfile::tempdir().unwrap();
    let fixture = CatalogOwnerFixture::new(dir.path(), "policy", false).await;
    let origin = fixture.install().await;
    fixture.stage(&origin).await;
    fixture.promote("staged").await;
    let db = fixture.db.clone();
    let nonce = origin.nonce.clone();
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
    {
        let conn = db.acquire_lock("seed active catalog settlement").unwrap();
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
        database: "private".into(),
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
    let rollback = db
        .settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            origin,
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

#[tokio::test]
async fn fresh_catalog_origin_install_uses_real_context_and_atomic_fences() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    db.set_metadata("enum_config_hash", "policy").await.unwrap();
    let zone = serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    let mut library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::new(HashMap::new()),
        Box::new(MockPhotosSession::new().ok(serde_json::json!({
            "records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]
        }))
        .ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"fresh-cursor"}]}))
        .ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"fresh-cursor"}]}))
        .ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"new-cursor"}]}))
        .ok(serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"failed-cursor"}]}))),
        Arc::new(zone.clone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    library.shadow_capture = Some(ShadowCapture::new(db.clone(), owner(), "com"));
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: library.all(),
        exclude_ids: Arc::default(),
    };
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.library = Arc::from("PrimarySync");
    let context = catalog_plan_context_for_source(
        &[pass],
        &config,
        DownloadControls::download_hidden(),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let plan = FreshCatalogPlan {
        account: context.account.clone(),
        provider: context.provider.clone(),
        database: context.database.clone(),
        zone: context.zone.clone(),
        scope: context.scope.clone(),
        config_hash: context.config_hash.clone(),
        metadata_enabled: context.metadata_enabled,
        metadata_flags: context.metadata_flags,
        passes: context
            .passes
            .iter()
            .map(|pass| FreshCatalogPass {
                key: pass.key.clone(),
                kind: pass.kind.clone(),
                scope: pass.scope.clone(),
                excludes: pass.excludes.clone(),
            })
            .collect(),
        profile: context.profile.clone(),
    };
    let baseline_scope = library.baseline_scope();
    db.enroll_sync_epoch(vec![("PrimarySync".into(), baseline_scope.clone())])
        .await
        .unwrap();
    match db
        .sync_epoch_gate("PrimarySync".into(), baseline_scope.clone(), None)
        .await
        .unwrap()
    {
        crate::state::SyncEpochGate::Open => {}
        other => panic!("expected fresh-install open gate, got {other:?}"),
    }
    let selected = GenerationSpec {
        format: 2,
        scope: context.scope.clone(),
        zone: context.zone.clone(),
        config_hash: context.config_hash.clone(),
        basis: context.config_hash.clone(),
        profile: context.profile.clone(),
        metadata_enabled: context.metadata_enabled,
        metadata_flags: context.metadata_flags,
    };
    let root = db
        .begin_selection_generation(owner(), selected, BUDGET)
        .await
        .unwrap();
    let receipt = db
        .seal_observed_selection(owner(), root.id, true, None)
        .await
        .unwrap()
        .unwrap();
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt.clone())
                .delta("before", "after")
                .unwrap(),
            "before",
        )
        .unwrap();
    let save = transition(&db, "after").await;
    db.commit_observation_transition(save, Some(proof), false)
        .await
        .unwrap();
    let descriptor = db.get_metadata(PROOF).await.unwrap().unwrap();
    assert_eq!(
        catalog_plan_digest_from_observation(&descriptor, "PrimarySync"),
        Some(crate::download::catalog_plan_digest_from_source_context(
            &context
        ))
    );
    let (epoch_raw, library_raw, epoch_id, attempt) = {
        let conn = db.acquire_lock("read genuine epoch fences").unwrap();
        let epoch = crate::state::db::sync_epoch::validate(&conn).unwrap();
        let library = crate::state::db::sync_epoch::read_library(&conn, "PrimarySync")
            .unwrap()
            .unwrap();
        (
            serde_json::to_string(&epoch).unwrap(),
            serde_json::to_string(&library).unwrap(),
            epoch.epoch,
            library.attempt,
        )
    };
    let baseline_key = format!("baseline_cursor:v27:{epoch_id}:{baseline_scope}:policy:");
    let mut expected_metadata = vec![
        (
            crate::state::db::sync_epoch::SYNC_EPOCH_KEY.to_owned(),
            Some(epoch_raw),
        ),
        (
            crate::state::db::sync_epoch::library_key("PrimarySync"),
            Some(library_raw),
        ),
    ];
    for key in [
        "enum_config_hash",
        "pending_enum_config_hash",
        "config_hash",
        "pending_download_config_hash",
        TOKEN,
        PROOF,
        "download_ledger_scope:PrimarySync",
    ] {
        expected_metadata.push((key.into(), db.get_metadata(key).await.unwrap()));
    }
    let request = |fresh_cursor: &str, expected_baseline: Option<&str>, snapshot: &str| {
        FreshCatalogOriginRequest {
            library: "PrimarySync".into(),
            plan: plan.clone(),
            baseline_key: baseline_key.clone(),
            preservation_config_hash: String::new(),
            expected_baseline: expected_baseline.map(str::to_owned),
            fresh_cursor: fresh_cursor.into(),
            enum_hash: "policy".into(),
            expected_epoch: epoch_id.clone(),
            expected_attempt: attempt.clone(),
            expected_metadata: expected_metadata.clone(),
            expected_catalog_snapshot: snapshot.to_owned(),
        }
    };
    let snapshot = db
        .fresh_catalog_snapshot(owner(), "PrimarySync".into())
        .await
        .unwrap();
    let boundary = library.fresh_baseline_cursor().await.unwrap();
    let origin = db
        .install_fresh_catalog_origin(owner(), request(&boundary, None, &snapshot))
        .await
        .unwrap();
    let mut foreign_plan = plan.clone();
    foreign_plan.profile["foreign-test-selection"] = serde_json::json!(true);
    let foreign_origin = CatalogOrigin::new(
        crate::state::db::source_revalidation::fresh_catalog_plan_digest(&foreign_plan),
        epoch_id.clone(),
        attempt.clone(),
        uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    {
        let conn = db.acquire_lock("verify fresh catalog origin").unwrap();
        let (mode, phase, rounds): (String, String, i64) = conn
            .query_row(
                "SELECT (SELECT mode FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT COUNT(*) FROM source_revalidation_rounds WHERE mode='catalog_v2')",
                [origin.nonce.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (mode.as_str(), phase.as_str(), rounds),
            ("catalog_v2", "marking", 1)
        );
        let key: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&baseline_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(key, "fresh-cursor");
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'old-debt',17)",
            [&origin.nonce],
        )
        .unwrap();
        for (id, mode, scope) in [
            ("foreign-legacy", "legacy_unfiled_v1", "legacy-scope"),
            ("foreign-unknown", "future_mode", "unknown-scope"),
            ("foreign-malformed", "catalog_v2", "catalog:v2:malformed"),
        ] {
            conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy','foreign','high','after','active',?3)", rusqlite::params![id, scope, mode]).unwrap();
        }
        for id in ["foreign-legacy", "foreign-unknown"] {
            conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'foreign-debt',42)", [id]).unwrap();
        }
        conn.execute("INSERT INTO source_revalidation_rounds(id,library,scope,enum_hash,boundary,high_id,after_id,phase,mode) VALUES(?1,'PrimarySync',?2,'policy','foreign','high','after','active','catalog_v2')", rusqlite::params![foreign_origin.nonce, foreign_origin.scope()]).unwrap();
        conn.execute("INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'foreign-debt',42)", [&foreign_origin.nonce]).unwrap();
    }
    let stale = request("fresh-cursor", Some("fresh-cursor"), &snapshot);
    assert!(
        db.install_fresh_catalog_origin(owner(), stale)
            .await
            .is_err()
    );
    let snapshot = db
        .fresh_catalog_snapshot(owner(), "PrimarySync".into())
        .await
        .unwrap();
    let boundary = library.fresh_baseline_cursor().await.unwrap();
    let resumed = db
        .install_fresh_catalog_origin(owner(), request(&boundary, Some("fresh-cursor"), &snapshot))
        .await
        .unwrap();
    assert_ne!(resumed, origin);
    {
        let conn = db.acquire_lock("verify same-boundary resume").unwrap();
        let rounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_rounds WHERE mode='catalog_v2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rounds, 4);
        let copied: i64 = conn.query_row("SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='old-debt'", [&resumed.nonce], |row| row.get(0)).unwrap();
        assert_eq!(copied, 17);
        conn.execute(
            "UPDATE source_revalidation_debt SET next_retry_at=7 WHERE round=?1 AND id='old-debt'",
            [&resumed.nonce],
        )
        .unwrap();
    }
    db.seed_source_revalidation(
        owner(),
        "PrimarySync".into(),
        resumed.scope(),
        "policy".into(),
        CatalogOrigin::boundary("fresh-cursor"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let prior_proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt.clone())
                .delta("fresh-cursor", "old-active-token")
                .unwrap(),
            "fresh-cursor",
        )
        .unwrap()
        .with_catalog_origin(resumed.clone());
    let prior_staged = "pending_sync_token:v27:policy:PrimarySync";
    let mut save = transition(&db, "old-active-token").await;
    save.metadata_updates[0].0 = prior_staged.into();
    save.expected_metadata.push((prior_staged.into(), None));
    save.expected_metadata
        .push((baseline_key.clone(), Some("fresh-cursor".into())));
    db.commit_observation_transition(save, Some(prior_proof), false)
        .await
        .unwrap();
    let mut promote = transition(&db, "old-active-token").await;
    promote
        .metadata_updates
        .push(("enum_config_hash".into(), "policy".into()));
    promote
        .expected_metadata
        .push((prior_staged.into(), Some("old-active-token".into())));
    promote.metadata_deletes.push(prior_staged.into());
    db.commit_observation_transition(promote, None, true)
        .await
        .unwrap();
    let mut current_metadata = expected_metadata.clone();
    for (key, value) in &mut current_metadata {
        *value = db.get_metadata(key).await.unwrap();
    }
    let request = |fresh_cursor: &str, expected_baseline: Option<&str>, snapshot: &str| {
        FreshCatalogOriginRequest {
            library: "PrimarySync".into(),
            plan: plan.clone(),
            baseline_key: baseline_key.clone(),
            preservation_config_hash: String::new(),
            expected_baseline: expected_baseline.map(str::to_owned),
            fresh_cursor: fresh_cursor.into(),
            enum_hash: "policy".into(),
            expected_epoch: epoch_id.clone(),
            expected_attempt: attempt.clone(),
            expected_metadata: current_metadata.clone(),
            expected_catalog_snapshot: snapshot.into(),
        }
    };
    let snapshot = db
        .fresh_catalog_snapshot(owner(), "PrimarySync".into())
        .await
        .unwrap();
    let boundary = library.fresh_baseline_cursor().await.unwrap();
    let fresh = db
        .install_fresh_catalog_origin(owner(), request(&boundary, Some("fresh-cursor"), &snapshot))
        .await
        .unwrap();
    assert_ne!(fresh, origin);
    let signals = db
        .catalog_pending_signals(owner(), "PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].round, fresh.nonce);
    let outcome = db
        .catalog_source_outcome(owner(), "PrimarySync".into())
        .await
        .unwrap();
    assert!(outcome.pending);
    assert_eq!(outcome.retained, 2);
    {
        let conn = db.acquire_lock("verify old active superseded").unwrap();
        let (old_phase, old_debt, key): (String, i64, String) = conn
            .query_row(
                "SELECT (SELECT phase FROM source_revalidation_rounds WHERE id=?1),
                        (SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='old-debt'),
                        (SELECT value FROM metadata WHERE key=?2)",
                [&resumed.nonce, &baseline_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (old_phase.as_str(), old_debt, key.as_str()),
            ("abandoned", 7, "new-cursor")
        );
        let marking: (String, String) = conn
            .query_row(
                "SELECT phase,after_id FROM source_revalidation_rounds WHERE id=?1",
                [&origin.nonce],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((marking.0.as_str(), marking.1.as_str()), ("abandoned", ""));
        let original_deadline: i64 = conn.query_row("SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='old-debt'", [&origin.nonce], |row| row.get(0)).unwrap();
        assert_eq!(original_deadline, 17);
        let foreign: i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_rounds r WHERE r.id IN ('foreign-legacy','foreign-unknown','foreign-malformed') AND r.phase='active' AND r.boundary='foreign' AND r.high_id='high' AND r.after_id='after'", [], |row| row.get(0)).unwrap();
        let foreign_debt: i64 = conn.query_row("SELECT COUNT(*) FROM source_revalidation_debt WHERE round IN ('foreign-legacy','foreign-unknown') AND id='foreign-debt' AND next_retry_at=42", [], |row| row.get(0)).unwrap();
        assert_eq!((foreign, foreign_debt), (3, 2));
        let foreign_catalog: (String, i64) = conn.query_row("SELECT r.phase,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 AND d.id='foreign-debt'", [&foreign_origin.nonce], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(
            (foreign_catalog.0.as_str(), foreign_catalog.1),
            ("active", 42)
        );
        let copied: i64 = conn.query_row("SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='old-debt'", [&fresh.nonce], |row| row.get(0)).unwrap();
        assert_eq!(copied, 7);
    }
    {
        let conn = db.acquire_lock("install baseline write failure").unwrap();
        conn.execute_batch(&format!(
            "CREATE TRIGGER fail_fresh_baseline BEFORE INSERT ON metadata WHEN NEW.key='{baseline_key}' BEGIN SELECT RAISE(ABORT,'synthetic fresh baseline'); END;"
        ))
        .unwrap();
    }
    let snapshot = db
        .fresh_catalog_snapshot(owner(), "PrimarySync".into())
        .await
        .unwrap();
    let boundary = library.fresh_baseline_cursor().await.unwrap();
    let failed_request = request(&boundary, Some("new-cursor"), &snapshot);
    let error = db
        .install_fresh_catalog_origin(owner(), failed_request)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("synthetic fresh baseline"));
    {
        let conn = db.acquire_lock("verify failed install preserved").unwrap();
        let failed_key: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [&baseline_key],
                |row| row.get(0),
            )
            .unwrap();
        let rounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_rounds WHERE mode='catalog_v2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((failed_key.as_str(), rounds), ("new-cursor", 5));
        let (phase, deadline): (String, i64) = conn.query_row("SELECT r.phase,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.id=?1 AND d.id='old-debt'", [&fresh.nonce], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!((phase.as_str(), deadline), ("marking", 7));
        conn.execute_batch("DROP TRIGGER fail_fresh_baseline;")
            .unwrap();
    }
    drop(config);
    drop(library);
    drop(db);
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap(),
    );
    assert_eq!(
        db.get_metadata(&baseline_key).await.unwrap().as_deref(),
        Some("new-cursor")
    );
    {
        let conn = db
            .acquire_lock("verify fresh origin rollback after reopen")
            .unwrap();
        let rounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_rounds WHERE mode='catalog_v2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let deadline: i64 = conn.query_row("SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='old-debt'", [&resumed.nonce], |row| row.get(0)).unwrap();
        assert_eq!((rounds, deadline), (5, 7));
    }
    let mut wrong_attempt = request("attempt-cursor", Some("new-cursor"), &snapshot);
    wrong_attempt.expected_attempt = "00000000-0000-0000-0000-000000000000".into();
    assert!(
        db.install_fresh_catalog_origin(owner(), wrong_attempt)
            .await
            .is_err()
    );
    let mut wrong_plan = request("plan-cursor", Some("new-cursor"), &snapshot);
    wrong_plan.plan.config_hash = "f".repeat(64);
    assert!(
        db.install_fresh_catalog_origin(owner(), wrong_plan)
            .await
            .is_err()
    );
    let mut wrong_pin = request("pin-cursor", Some("new-cursor"), &snapshot);
    wrong_pin.plan.account = "other".into();
    assert!(
        db.install_fresh_catalog_origin(owner(), wrong_pin)
            .await
            .is_err()
    );
    let mut wrong_epoch = request("epoch-cursor", Some("new-cursor"), &snapshot);
    wrong_epoch.expected_epoch = uuid::Uuid::new_v4().to_string();
    assert!(
        db.install_fresh_catalog_origin(owner(), wrong_epoch)
            .await
            .is_err()
    );
    let wrong_baseline = request("baseline-cursor", Some("stale"), &snapshot);
    assert!(
        db.install_fresh_catalog_origin(owner(), wrong_baseline)
            .await
            .is_err()
    );
    let mut wrong_descriptor = request("descriptor-cursor", Some("new-cursor"), &snapshot);
    wrong_descriptor
        .expected_metadata
        .iter_mut()
        .find(|(key, _)| key == PROOF)
        .unwrap()
        .1 = None;
    assert!(
        db.install_fresh_catalog_origin(owner(), wrong_descriptor)
            .await
            .is_err()
    );

    db.seed_source_revalidation(
        owner(),
        "PrimarySync".into(),
        fresh.scope(),
        "policy".into(),
        CatalogOrigin::boundary("new-cursor"),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    let proof = ObservationProof::inventory(vec![receipt.clone()])
        .unwrap()
        .bridge(
            ObservationProof::delivery(receipt)
                .delta("new-cursor", "staged-next")
                .unwrap(),
            "new-cursor",
        )
        .unwrap()
        .with_catalog_origin(fresh.clone());
    let staged = "pending_sync_token:v27:policy:PrimarySync";
    let staged_descriptor = "observed_selection_staged:v27:policy:PrimarySync";
    let mut save = transition(&db, "staged-next").await;
    save.metadata_updates[0].0 = staged.into();
    save.expected_metadata.push((staged.into(), None));
    save.expected_metadata
        .push((baseline_key.clone(), Some("new-cursor".into())));
    db.commit_observation_transition(save, Some(proof), false)
        .await
        .unwrap();
    let descriptor: Value =
        serde_json::from_str(&db.get_metadata(staged_descriptor).await.unwrap().unwrap()).unwrap();
    assert_eq!(descriptor["revalidation_round"], fresh.nonce);
    let staged_snapshot = db
        .fresh_catalog_snapshot(owner(), "PrimarySync".into())
        .await
        .unwrap();
    assert!(
        db.install_fresh_catalog_origin(
            owner(),
            request("refused", Some("new-cursor"), &staged_snapshot)
        )
        .await
        .is_err()
    );
    let mut promote = transition(&db, "staged-next").await;
    promote
        .metadata_updates
        .push(("enum_config_hash".into(), "policy".into()));
    promote
        .expected_metadata
        .push((staged.into(), Some("staged-next".into())));
    promote.metadata_deletes.push(staged.into());
    {
        let conn = db
            .acquire_lock("inject genuine catalog promotion failure")
            .unwrap();
        conn.execute_batch("CREATE TRIGGER fail_genuine_catalog_activation BEFORE UPDATE ON source_revalidation_rounds WHEN NEW.phase='active' BEGIN SELECT RAISE(ABORT,'synthetic genuine activation'); END;").unwrap();
    }
    let error = db
        .commit_observation_transition(promote.clone(), None, true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("synthetic genuine activation"));
    assert_eq!(
        db.get_metadata(TOKEN).await.unwrap().as_deref(),
        Some("old-active-token")
    );
    assert!(db.get_metadata(staged_descriptor).await.unwrap().is_some());
    {
        let conn = db
            .acquire_lock("remove genuine catalog promotion failure")
            .unwrap();
        conn.execute_batch("DROP TRIGGER fail_genuine_catalog_activation;")
            .unwrap();
    }
    db.commit_observation_transition(promote, None, true)
        .await
        .unwrap();
    let descriptor: Value =
        serde_json::from_str(&db.get_metadata(PROOF).await.unwrap().unwrap()).unwrap();
    assert_eq!(descriptor["revalidation_round"], fresh.nonce);
    {
        let conn = db.acquire_lock("verify actual catalog promotion").unwrap();
        let phase: String = conn
            .query_row(
                "SELECT phase FROM source_revalidation_rounds WHERE id=?1",
                [&fresh.nonce],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(phase, "active");
        conn.execute(
            "INSERT INTO source_revalidation_debt(round,id,next_retry_at) VALUES(?1,'new-debt',0)",
            [&fresh.nonce],
        )
        .unwrap();
    }
    assert!(
        db.due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            resumed.clone(),
            "policy".into()
        )
        .await
        .unwrap()
        .is_empty()
    );
    let candidates = db
        .due_catalog_candidates(
            owner(),
            "PrimarySync".into(),
            fresh.clone(),
            "policy".into(),
        )
        .await
        .unwrap();
    assert_eq!(candidates.len(), 2);
    let new_candidate = candidates
        .iter()
        .find(|candidate| candidate.id == "new-debt")
        .unwrap()
        .clone();
    assert!(
        !db.settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            resumed.clone(),
            "policy".into(),
            new_candidate.clone(),
            CatalogSettlementAuthority::Refused
        )
        .await
        .unwrap()
    );
    let wrong_source = CatalogPairEvidence {
        database: "shared".into(),
        zone: serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
        master: "new-debt".into(),
        child: "new-debt".into(),
        master_type: "CPLMaster".into(),
        child_type: "CPLAsset".into(),
        reference_exact: true,
        explicit_deleted: false,
    };
    assert!(
        !db.settle_catalog_candidate(
            owner(),
            "PrimarySync".into(),
            fresh.clone(),
            "policy".into(),
            new_candidate,
            CatalogSettlementAuthority::Live {
                evidence: wrong_source
            }
        )
        .await
        .unwrap()
    );
    let conn = db.acquire_lock("verify old catalog debt retained").unwrap();
    let deadline: i64 = conn
        .query_row(
            "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='old-debt'",
            [&resumed.nonce],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(deadline, 7);
    let copied_deadline: i64 = conn
        .query_row(
            "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id='old-debt'",
            [&fresh.nonce],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(copied_deadline, 7);
    let retained: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id='new-debt'",
            [&fresh.nonce],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, 1);
}
