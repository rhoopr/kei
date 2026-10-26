//! Format 2 owns selected tasks without admitting historical rank/catalog proof.
use super::{CURRENT, Fixture, owner, records};
use crate::download::planner::{TaskPlanner, pending_record_for_task};
use crate::state::SqliteStateDb;
use crate::state::db::provider_generations::{
    ActiveDecision, GenerationSpec, MAX_GENERATION_BYTES,
};
use crate::state::db::provider_selection::{
    SelectionDecision, SelectionDestination, SelectionOutcome, SelectionPath,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn v027_format_two_admits_scoped_outcomes_without_shadow_catalog_or_rank() {
    let f = Fixture::new().await;
    let tables: Vec<String> = f.db.acquire_lock("find historical owners to forbid").unwrap()
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND (name LIKE 'provider_shadow_%' OR name LIKE 'provider_catalog_%' OR name LIKE 'provider_selection_rank_%' OR name='provider_selection_generations')")
        .unwrap().query_map([], |r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    for table in &tables {
        f.db.acquire_lock("reject new historical proof writes").unwrap().execute_batch(&format!(
            "CREATE TRIGGER reject_new_{table} BEFORE INSERT ON {table} BEGIN SELECT RAISE(ABORT,'format2 must not write historical proof'); END;"
        )).unwrap();
    }
    let mut expected = Vec::new();
    for (index, database, library, zone_owner) in [
        (0, "private", "PrimarySync", "_defaultOwner"),
        (
            1,
            "private",
            "SharedSync-format2-private",
            "other-zone-owner",
        ),
        (2, "shared", "SharedSync-format2-shared", "other-zone-owner"),
    ] {
        let zone = json!({"zoneName":library,"ownerRecordName":zone_owner});
        let scope = f.capture.scope(database, &zone).unwrap();
        let key = format!("selected-pass-{index}");
        let spec = GenerationSpec {
            format: 2,
            scope,
            zone: zone.clone(),
            config_hash: "a".repeat(64),
            // Format 2 must not compare this scheduling identity with catalog history.
            basis: "b".repeat(64),
            profile: json!({"coverage":"selected_downloads","passes":[{"key":key,"scope":{
                "database":database,"zone":zone,"kind":"unfiled"}}]}),
            metadata_enabled: false,
            metadata_flags: 0,
        };
        let root =
            f.db.begin_selection_generation(owner(), spec, MAX_GENERATION_BYTES)
                .await
                .unwrap();
        for (suffix, outcome) in [
            ("selected", SelectionOutcome::Selected),
            ("excluded", SelectionOutcome::Excluded),
            ("deferred", SelectionOutcome::Deferred),
        ] {
            let master = format!("format2-{index}-{suffix}");
            let child = format!("asset-{master}");
            let mut values = records(&master, CURRENT, "current facts");
            for record in &mut values {
                record["zoneID"] = zone.clone();
            }
            values[1]["fields"]["masterRef"]["value"]["zoneID"] = zone.clone();
            let confirmation = serde_json::to_vec(&json!({"records":values})).unwrap();
            let asset = crate::icloud::photos::current_asset(&confirmation, &child, &master, &zone)
                .unwrap();
            let mut config = f.config.clone();
            config.library = Arc::from(library);
            config.directory = f.config.directory.join(format!("{index}-{suffix}")).into();
            config.folder_structure = "none".into();
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            let selected = outcome == SelectionOutcome::Selected;
            let normalized = if selected {
                plan.tasks
                    .iter()
                    .map(|task| pending_record_for_task(&config, &asset, task))
                    .collect()
            } else {
                Vec::new()
            };
            let destinations = if selected {
                plan.tasks
                    .iter()
                    .map(|task| SelectionDestination {
                        version_size: task.version_size.as_str().into(),
                        path: SelectionPath::from_path(
                            &std::path::absolute(&task.download_path).unwrap(),
                        ),
                        checksum: task.checksum.to_string(),
                        size: task.size,
                        metadata_hash: crate::download::filter::metadata_for_selected_version(
                            &asset,
                            &config,
                            task.version_size,
                        )
                        .metadata_hash
                        .clone()
                        .unwrap(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let deferred = outcome == SelectionOutcome::Deferred;
            let decision = ActiveDecision {
                decision: SelectionDecision {
                    pass_key: key.clone(),
                    child: child.clone(),
                    master: Some(master),
                    confirmation: Some(confirmation),
                    outcome,
                    reason: if selected {
                        ""
                    } else if deferred {
                        "retained_current_observation"
                    } else {
                        "currently_filtered"
                    }
                    .into(),
                    destinations,
                },
                sources: Vec::new(),
                state_id: child.clone(),
            };
            if deferred {
                let mut incomplete = decision.clone();
                incomplete.decision.master = None;
                incomplete.decision.confirmation = None;
                assert!(
                    f.db.project_selection_decision(
                        owner(),
                        root.id.clone(),
                        incomplete,
                        Vec::new(),
                        MAX_GENERATION_BYTES,
                    )
                    .await
                    .is_err(),
                    "bare identities cannot replace lossless deferred evidence"
                );
            }
            let result =
                f.db.project_selection_decision(
                    owner(),
                    root.id.clone(),
                    decision.clone(),
                    normalized,
                    MAX_GENERATION_BYTES,
                )
                .await
                .unwrap();
            assert_eq!(result.admitted, selected);
            assert_eq!(
                f.db.selection_decision(owner(), root.id.clone(), key.clone(), child.clone())
                    .await
                    .unwrap(),
                Some(decision.clone())
            );
            expected.push((root.id.clone(), key.clone(), child, decision));
        }
    }
    for _ in 0..2 {
        let db = SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
            .await
            .unwrap();
        for (generation, key, child, decision) in &expected {
            let mut actual = db
                .selection_decision(owner(), generation.clone(), key.clone(), child.clone())
                .await
                .unwrap()
                .expect("scoped admitted outcome survives reopening");
            let mut expected_decision = decision.clone();
            if decision.decision.outcome == SelectionOutcome::Excluded {
                // A finished exclusion may lose its response body during
                // compaction. Unfinished selected/deferred evidence below must
                // still round-trip losslessly.
                assert!(
                    actual.decision.confirmation.is_none()
                        || actual.decision.confirmation == decision.decision.confirmation
                );
                actual.decision.confirmation = None;
                expected_decision.decision.confirmation = None;
            }
            assert_eq!(actual, expected_decision);
        }
        let pending = db.get_pending().await.unwrap();
        for library in [
            "PrimarySync",
            "SharedSync-format2-private",
            "SharedSync-format2-shared",
        ] {
            assert_eq!(
                pending
                    .iter()
                    .filter(|r| r.library.as_ref() == library)
                    .count(),
                1
            );
        }
        for table in &tables {
            assert_eq!(
                db.acquire_lock("historical owners remain empty")
                    .unwrap()
                    .query_row::<i64, _, _>(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[tokio::test]
async fn v027_format_one_still_rejects_decision_without_rank_sources() {
    let f = Fixture::new().await;
    let (_, scope, zone) = f.pass.album.private_selection_scope().unwrap().unwrap();
    let key = crate::download::orchestration::generation::pass_key(&f.pass);
    let basis = f.db.selection_basis(owner(), scope.clone()).await.unwrap();
    let root = f.db.begin_selection_generation(owner(),GenerationSpec {
        format:1,scope,zone,config_hash:"a".repeat(64),basis,
        profile:json!({"coverage":"observed_selection_window","passes":[{"key":key,"scope":f.pass.album.selection_scope()}]}),
        metadata_enabled:false,metadata_flags:0,
    },MAX_GENERATION_BYTES).await.unwrap();
    let invalid = ActiveDecision {
        sources: Vec::new(),
        state_id: "asset-m".into(),
        decision: SelectionDecision {
            pass_key: key,
            child: "asset-m".into(),
            master: None,
            confirmation: None,
            outcome: SelectionOutcome::Deferred,
            reason: "retained_current_observation".into(),
            destinations: Vec::new(),
        },
    };
    assert!(
        f.db.project_selection_decision(
            owner(),
            root.id,
            invalid,
            Vec::new(),
            MAX_GENERATION_BYTES
        )
        .await
        .is_err()
    );
    assert_eq!(f.count("provider_active_decisions"), 0);
    f.preserved().await;
}

#[tokio::test]
async fn v027_same_named_provider_identity_in_distinct_scope_must_not_merge() {
    for (other_database, other_owner) in [("shared", "_defaultOwner"), ("private", "other-owner")] {
        let f = Fixture::new().await;
        let mut first_id = None;
        for (index, database, zone_owner) in [
            (0, "private", "_defaultOwner"),
            (1, other_database, other_owner),
        ] {
            let zone = json!({"zoneName":"PrimarySync","ownerRecordName":zone_owner});
            let key = "same-pass".to_owned();
            let spec = GenerationSpec {
                format: 2,
                scope: f.capture.scope(database, &zone).unwrap(),
                zone: zone.clone(),
                config_hash: "a".repeat(64),
                basis: "b".repeat(64),
                profile: json!({"coverage":"selected_downloads","passes":[{"key":key,"scope":{"database":database,"zone":zone}}]}),
                metadata_enabled: false,
                metadata_flags: 0,
            };
            let root =
                f.db.begin_selection_generation(owner(), spec, MAX_GENERATION_BYTES)
                    .await;
            if index == 1 && root.is_err() {
                break;
            }
            let root = root.unwrap();
            let mut values = records("same-provider-name", CURRENT, "same facts");
            for record in &mut values {
                record["zoneID"] = zone.clone();
            }
            values[1]["fields"]["masterRef"]["value"]["zoneID"] = zone.clone();
            let body = serde_json::to_vec(&json!({"records":values})).unwrap();
            let asset = crate::icloud::photos::current_asset(
                &body,
                "asset-same-provider-name",
                "same-provider-name",
                &zone,
            )
            .unwrap();
            let mut config = f.config.clone();
            config.library = Arc::from("PrimarySync");
            config.folder_structure = "none".into();
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            let normalized = plan
                .tasks
                .iter()
                .map(|task| pending_record_for_task(&config, &asset, task))
                .collect();
            let destinations = plan
                .tasks
                .iter()
                .map(|task| SelectionDestination {
                    version_size: task.version_size.as_str().into(),
                    path: SelectionPath::from_path(
                        &std::path::absolute(&task.download_path).unwrap(),
                    ),
                    checksum: task.checksum.to_string(),
                    size: task.size,
                    metadata_hash: crate::download::filter::metadata_for_selected_version(
                        &asset,
                        &config,
                        task.version_size,
                    )
                    .metadata_hash
                    .clone()
                    .unwrap(),
                })
                .collect();
            let decision = ActiveDecision {
                sources: Vec::new(),
                state_id: "asset-same-provider-name".into(),
                decision: SelectionDecision {
                    pass_key: key.clone(),
                    child: "asset-same-provider-name".into(),
                    master: Some("same-provider-name".into()),
                    confirmation: Some(body),
                    outcome: SelectionOutcome::Selected,
                    reason: String::new(),
                    destinations,
                },
            };
            let admitted =
                f.db.project_selection_decision(
                    owner(),
                    root.id.clone(),
                    decision.clone(),
                    normalized,
                    MAX_GENERATION_BYTES,
                )
                .await;
            if index == 0 {
                assert!(admitted.unwrap().admitted);
                first_id = Some((root.id, decision));
            } else {
                assert!(
                    admitted.as_ref().map_or(true, |result| !result.admitted),
                    "same textual child from a different full provider scope must fail closed"
                );
            }
        }
        let (root, decision) = first_id.unwrap();
        let reopened = SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
            .await
            .unwrap();
        assert_eq!(
            reopened
                .selection_decision(
                    owner(),
                    root,
                    "same-pass".into(),
                    "asset-same-provider-name".into()
                )
                .await
                .unwrap(),
            Some(decision)
        );
        assert_eq!(reopened.get_pending().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn explicit_metadata_refresh_profile_version_is_validated_without_reinterpreting_old_roots() {
    let f = Fixture::new().await;
    let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    let base = GenerationSpec {
        format: 2,
        scope: f.capture.scope("private", &zone).unwrap(),
        zone: zone.clone(),
        config_hash: "a".repeat(64),
        basis: "b".repeat(64),
        profile: json!({"coverage":"selected_downloads","passes":[{"key":"selected","scope":{
            "database":"private","zone":zone,"kind":"unfiled"}}]}),
        metadata_enabled: false,
        metadata_flags: 0,
    };
    let old =
        f.db.begin_selection_generation(owner(), base.clone(), MAX_GENERATION_BYTES)
            .await
            .unwrap();
    assert!(old.spec.profile.get("explicit_metadata_refresh").is_none());
    for unsupported in [json!(null), json!(0), json!(2), json!("1")] {
        let mut spec = base.clone();
        spec.config_hash = "c".repeat(64);
        spec.profile["explicit_metadata_refresh"] = unsupported;
        assert!(matches!(
            f.db.begin_selection_generation(owner(), spec, MAX_GENERATION_BYTES)
                .await,
            Err(crate::state::error::StateError::ProviderSelectionInvalid)
        ));
    }
    let mut explicit = base;
    explicit.config_hash = "d".repeat(64);
    explicit.profile["explicit_metadata_refresh"] = json!(1);
    let new =
        f.db.begin_selection_generation(owner(), explicit, MAX_GENERATION_BYTES)
            .await
            .unwrap();
    assert_eq!(new.spec.profile["explicit_metadata_refresh"], json!(1));
    assert!(old.spec.profile.get("explicit_metadata_refresh").is_none());
}
