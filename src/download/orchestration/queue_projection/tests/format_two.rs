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
async fn publication_choice_pending_companions_are_path_local() {
    use crate::state::error::StateError;
    for (companion, allowed) in [
        ("companion.JPG", true),
        ("Photo 10 AM.JPG", false),
        ("photo 10AM.jpg", false),
        ("Sibling 10 AM.JPG", false),
        ("sibling 10AM.jpg", false),
    ] {
        let f = publication_choice_fixture(companion, false).await;
        let result = f
            .fixture
            .db
            .reserve_bound_publication_choices(f.binding, vec![(f.reservation, f.requested)])
            .await;
        if allowed {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(StateError::ProviderWorkConflict)),
                "{companion}: {result:?}"
            );
        }
        let conn = f
            .fixture
            .db
            .acquire_lock("assert path-local pending branch")
            .unwrap();
        let counts: (i64, i64) = conn.query_row("SELECT (SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=0),(SELECT count(*) FROM reconciliation_paths)", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(counts, (2, i64::from(allowed)), "{companion}");
    }
}

struct PublicationChoiceFixture {
    fixture: Fixture,
    binding: crate::state::db::PublicationBinding,
    reservation: crate::state::db::ReconciliationReservation,
    requested: SelectionPath,
    generation: String,
    asset: crate::icloud::photos::PhotoAsset,
    config: crate::download::DownloadConfig,
    rows: Vec<crate::state::AssetRecord>,
}

async fn publication_choice_fixture(
    companion: &str,
    metadata_enabled: bool,
) -> PublicationChoiceFixture {
    use crate::state::VersionSizeKey;
    use crate::state::db::{
        DownloadStateStore, PreparedPublication, ReconciliationContent, ReconciliationPathKey,
        ReconciliationReservation, TempFileOwnershipStore,
    };
    use sha2::{Digest, Sha256};
    let f = Fixture::new().await;
    let db = &f.db;
    let p = f.config.directory.join("Photo 10 AM.JPG");
    let s = f.config.directory.join("Sibling 10 AM.JPG");
    let q = f.config.directory.join(companion);
    let digest = Sha256::digest(b"old");
    let checksum = data_encoding::BASE64.encode(&digest);
    let mut values = records("m", &checksum, "current");
    values[0]["fields"]["filenameEnc"]["value"] = json!("Photo 10 AM.JPG");
    values[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(3);
    values[1]["fields"]["resJPEGFullRes"] = json!({"value":{"size":4,"downloadURL":"https://p01.icloud-content.com/edit","fileChecksum":CURRENT}});
    values[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
    values[0]["fields"]["adjustmentRenderType"] = json!({"value":1});
    let (_, scope, zone) = f.pass.album.private_selection_scope().unwrap().unwrap();
    let key = crate::download::orchestration::generation::pass_key(&f.pass);
    let body = serde_json::to_vec(&json!({"records":values})).unwrap();
    let asset = crate::icloud::photos::current_asset(&body, "asset-m", "m", &zone).unwrap();
    let mut config = f.config.with_pass(&f.pass);
    config.edited = true;
    config.metadata.set_exif_datetime = metadata_enabled;
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
    let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    assert_eq!(plan.tasks.len(), 2);
    for task in &mut plan.tasks {
        task.download_path = if task.version_size == VersionSizeKey::Original {
            p.clone()
        } else {
            q.clone()
        };
    }
    let original = &plan.tasks[0];
    assert_eq!(original.version_size, VersionSizeKey::Original);
    let binding = original.publication_binding.as_deref().unwrap().clone();
    db.upsert_seen(&pending_record_for_task(&config, &asset, original))
        .await
        .unwrap();
    let part = f.config.directory.join("source.part");
    std::fs::write(&part, b"old").unwrap();
    let sha = data_encoding::HEXLOWER.encode(&digest);
    db.claim_temp_file(&part).await.unwrap();
    db.prepare_temp_publication(
        &part,
        &PreparedPublication {
            binding: Some(binding.clone()),
            library: "PrimarySync".into(),
            id: "asset-m".into(),
            version: "original".into(),
            provider_checksum: original.checksum.to_string(),
            provider_size: 3,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&p),
            local_checksum: sha.clone(),
            source_checksum: sha.clone(),
        },
    )
    .await
    .unwrap();
    std::fs::rename(&part, &p).unwrap();
    db.mark_verified_download(
        "PrimarySync",
        "asset-m",
        "original",
        &p,
        &sha,
        Some(&sha),
        false,
    )
    .await
    .unwrap();
    let root = db.begin_selection_generation(owner(), GenerationSpec {
            format: 2, scope, zone, config_hash: "a".repeat(64), basis: "b".repeat(64),
            profile: json!({"coverage":"selected_downloads","passes":[{"key":key,"scope":f.pass.album.selection_scope()}]}),
            metadata_enabled, metadata_flags: crate::download::pipeline::MetadataFlags::from(&config).bits(),
        }, MAX_GENERATION_BYTES).await.unwrap();
    let rows: Vec<_> = plan
        .tasks
        .iter()
        .map(|t| pending_record_for_task(&config, &asset, t))
        .collect();
    let destinations = plan
        .tasks
        .iter()
        .zip(&rows)
        .map(|(t, r)| SelectionDestination {
            version_size: t.version_size.as_str().into(),
            path: SelectionPath::from_path(&t.download_path),
            checksum: t.checksum.to_string(),
            size: t.size,
            metadata_hash: r.metadata.metadata_hash.clone().unwrap(),
        })
        .collect();
    let decision = ActiveDecision {
        state_id: "asset-m".into(),
        sources: Vec::new(),
        decision: SelectionDecision {
            pass_key: key,
            child: "asset-m".into(),
            master: Some("m".into()),
            confirmation: Some(body),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations,
        },
    };
    assert!(
        db.project_selection_decision(
            owner(),
            root.id.clone(),
            decision,
            rows.clone(),
            MAX_GENERATION_BYTES
        )
        .await
        .unwrap()
        .admitted
    );
    let reservation = ReconciliationReservation {
        library: "PrimarySync".into(),
        asset_id: "asset-m".into(),
        version_size: VersionSizeKey::Original,
        requested_path_key: ReconciliationPathKey(crate::fs_util::confined_path_key(&p).unwrap()),
        destination_path_key: ReconciliationPathKey(crate::fs_util::confined_path_key(&s).unwrap()),
        destination_path: s,
        content: Some(ReconciliationContent {
            checksum: super::OLD.into(),
            size: 5,
        }),
    };
    PublicationChoiceFixture {
        fixture: f,
        binding,
        reservation,
        requested: SelectionPath::from_path(&p),
        generation: root.id,
        asset,
        config,
        rows,
    }
}

#[tokio::test]
async fn publication_choice_exact_replay_is_intent_not_historical_byte_proof() {
    use crate::download::baseline_matching::BaselineMatch;
    use crate::state::VersionSizeKey;
    use crate::state::db::{
        DownloadStateStore, PreparedPublication, ReconciliationContent, TempFileOwnershipStore,
    };
    use sha2::{Digest, Sha256};
    let mut f = publication_choice_fixture("Sibling 10 AM.JPG", false).await;
    let mut db = f.fixture.db.clone();
    let p = f.requested.to_path();
    let s = f.reservation.destination_path.clone();
    f.reservation.version_size = VersionSizeKey::Adjusted;
    f.reservation.content = Some(ReconciliationContent {
        checksum: CURRENT.into(),
        size: 4,
    });
    db.reserve_bound_publication_choices(
        f.binding.clone(),
        vec![(f.reservation.clone(), f.requested.clone())],
    )
    .await
    .unwrap();
    let part = s.with_extension("part");
    std::fs::write(&part, b"edit").unwrap();
    let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(b"edit"));
    db.claim_temp_file(&part).await.unwrap();
    db.prepare_temp_publication(
        &part,
        &PreparedPublication {
            binding: Some(f.binding.clone()),
            library: "PrimarySync".into(),
            id: "asset-m".into(),
            version: "adjusted".into(),
            provider_checksum: CURRENT.into(),
            provider_size: 4,
            temporary: SelectionPath::from_path(&part),
            destination: SelectionPath::from_path(&s),
            local_checksum: sha.clone(),
            source_checksum: sha.clone(),
        },
    )
    .await
    .unwrap();
    std::fs::rename(&part, &s).unwrap();
    db.mark_verified_download(
        "PrimarySync",
        "asset-m",
        "adjusted",
        &s,
        &sha,
        Some(&sha),
        false,
    )
    .await
    .unwrap();
    std::fs::write(&p, b"changed original after S completion").unwrap();
    assert!(
        db.bound_historical_downloads(f.binding.clone(), "adjusted".into(), CURRENT.into(), 4)
            .await
            .unwrap()
            .is_empty()
    );
    let original: String = db.acquire_lock("exact persisted choice snapshot").unwrap()
        .query_row("SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths", [], |r| r.get(0)).unwrap();
    db.acquire_lock("reject replay writes").unwrap().execute_batch("CREATE TRIGGER no_replay_insert BEFORE INSERT ON reconciliation_paths BEGIN SELECT RAISE(ABORT,'replay rewrote intent'); END; CREATE TRIGGER no_replay_update BEFORE UPDATE ON reconciliation_paths BEGIN SELECT RAISE(ABORT,'replay rewrote intent'); END;").unwrap();
    let mut config = f.config.clone();
    config.resolution = crate::types::PhotoResolution::None;
    config.edited_naming = crate::types::EditedNaming::Primary;
    config.folder_structure = "none".into();
    for round in 0..3 {
        if round == 2 {
            db = Arc::new(
                SqliteStateDb::open_owned(&f.fixture.dir.path().join("state.db"), &owner())
                    .await
                    .unwrap(),
            );
            config.state_db = Some(db.clone());
            let mut album = config.primary_layout_source.as_deref().unwrap().clone();
            album.set_shadow_capture(
                crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner(), "com"),
                "private".into(),
            );
            config.primary_layout_source = Some(Arc::new(album));
        }
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let plan = planner
            .plan_download_asset(&f.asset, &config)
            .await
            .unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].download_path, s);
        assert!(plan.tasks[0].superseded_requested_path.is_none());
        assert!(
            planner
                .persist_execution_publication_choices(&f.asset, &config, &plan.tasks)
                .await
                .unwrap()
        );
        assert!(
            matches!(planner.match_existing(&f.asset, &config, &plan.tasks[0]).await,
            BaselineMatch::RecordedPresent { receipt, .. } if receipt.native_path.to_path() == s)
        );
        let manifest = db
            .selection_decision(
                owner(),
                f.generation.clone(),
                f.binding.origin_pass.clone(),
                "asset-m".into(),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            db.project_selection_decision(
                owner(),
                f.generation.clone(),
                manifest,
                f.rows.clone(),
                MAX_GENERATION_BYTES
            )
            .await
            .unwrap()
            .admitted
        );
        assert_eq!(db.acquire_lock("unchanged replay row").unwrap().query_row("SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths", [], |r| r.get::<_,String>(0)).unwrap(), original);
    }
    db.acquire_lock("allow synthetic negative mutations")
        .unwrap()
        .execute_batch("DROP TRIGGER no_replay_insert; DROP TRIGGER no_replay_update")
        .unwrap();
    for case in [
        "binding",
        "pass",
        "source",
        "resource",
        "requested",
        "destination",
        "missing",
        "conflict",
        "incomplete",
        "malformed",
        "read_error",
    ] {
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let mut plan = planner
            .plan_download_asset(&f.asset, &config)
            .await
            .unwrap();
        let mut current = config.clone();
        db.acquire_lock("begin replay negative")
            .unwrap()
            .execute_batch("SAVEPOINT replay_negative")
            .unwrap();
        match case {
            "binding" => {
                Arc::make_mut(plan.tasks[0].publication_binding.as_mut().unwrap()).account_key =
                    "foreign".into()
            }
            "pass" => current.primary_layout_pass = Some("wrong-pass".into()),
            "source" => current.state_db = None,
            "resource" => plan.tasks[0].checksum = "different-resource".into(),
            "requested" => current.folder_structure = "different-request".into(),
            "destination" => plan.tasks[0].download_path = s.with_file_name("different-final.JPG"),
            "missing" => {
                db.acquire_lock("missing persisted row")
                    .unwrap()
                    .execute("DELETE FROM reconciliation_paths", [])
                    .unwrap();
            }
            "conflict" => {
                db.acquire_lock("conflicting saved S").unwrap().execute("UPDATE reconciliation_paths SET destination_path_key='other',destination_path='other'", []).unwrap();
            }
            "incomplete" => {
                db.acquire_lock("incomplete saved content")
                    .unwrap()
                    .execute(
                        "UPDATE reconciliation_paths SET provider_checksum='',provider_size=-1",
                        [],
                    )
                    .unwrap();
            }
            "malformed" => {
                db.acquire_lock("inconsistent native S")
                    .unwrap()
                    .execute(
                        "UPDATE reconciliation_paths SET destination_path='other'",
                        [],
                    )
                    .unwrap();
            }
            "read_error" => {
                db.acquire_lock("missing choice table")
                    .unwrap()
                    .execute_batch(
                        "ALTER TABLE reconciliation_paths RENAME TO hidden_reconciliation_paths",
                    )
                    .unwrap();
            }
            _ => panic!("unexpected replay case {case}"),
        }
        let result = planner
            .persist_execution_publication_choices(&f.asset, &current, &plan.tasks)
            .await;
        assert!(!result.unwrap_or(false), "{case} received replay exemption");
        db.acquire_lock("restore replay negative")
            .unwrap()
            .execute_batch("ROLLBACK TO replay_negative; RELEASE replay_negative")
            .unwrap();
    }
    // Storage intent cannot certify S bytes either.
    std::fs::write(&s, b"changed current S").unwrap();
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
    let plan = planner
        .plan_download_asset(&f.asset, &config)
        .await
        .unwrap();
    assert!(
        planner
            .persist_execution_publication_choices(&f.asset, &config, &plan.tasks)
            .await
            .unwrap()
    );
    assert!(matches!(
        planner
            .match_existing(&f.asset, &config, &plan.tasks[0])
            .await,
        BaselineMatch::Uncertain(_)
    ));
}

#[tokio::test]
async fn publication_choice_media_complete_metadata_owners_are_path_local() {
    use crate::state::db::{DownloadStateStore, ReconciliationContent};
    use crate::state::{VersionSizeKey, error::StateError};
    use sha2::{Digest, Sha256};
    #[derive(Clone, Copy, Debug)]
    enum Protection {
        None,
        Frozen,
        Prepared,
        SourceOnly,
    }
    for (protection, disjoint) in [
        (Protection::None, false),
        (Protection::Frozen, false),
        (Protection::Prepared, false),
        (Protection::Frozen, true),
        (Protection::Prepared, true),
        (Protection::SourceOnly, false),
    ] {
        let companion = match (protection, disjoint) {
            (_, true) => "companion.JPG",
            (Protection::Frozen, false) => "sibling 10AM.jpg",
            _ => "Sibling 10 AM.JPG",
        };
        let metadata = !matches!(protection, Protection::None);
        let mut f = publication_choice_fixture(companion, metadata).await;
        let db = &f.fixture.db;
        let p = f.requested.to_path();
        let source = data_encoding::HEXLOWER.encode(&Sha256::digest(b"old"));
        db.mark_verified_download(
            "PrimarySync",
            "asset-m",
            "original",
            &p,
            &source,
            Some(&source),
            false,
        )
        .await
        .unwrap();
        let q = f.fixture.config.directory.join(companion);
        if !matches!(protection, Protection::SourceOnly) {
            std::fs::write(&q, b"edit").unwrap();
            let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(b"edit"));
            db.mark_verified_download(
                "PrimarySync",
                "asset-m",
                "adjusted",
                &q,
                &sha,
                Some(&sha),
                false,
            )
            .await
            .unwrap();
        }
        if matches!(protection, Protection::Prepared) {
            let pending = db
                .pending_selection_metadata(owner(), f.generation.clone(), 10)
                .await
                .unwrap()
                .into_iter()
                .find(|p| p.asset.version_size == VersionSizeKey::Adjusted)
                .unwrap();
            let output = data_encoding::HEXLOWER.encode(&Sha256::digest(b"prepared metadata"));
            assert!(
                db.record_selection_metadata_prepared(&pending, &output, 17)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        {
            let mut conn = db
                .acquire_lock("assert real metadata owner and media-only readers")
                .unwrap();
            let tx = conn.transaction().unwrap();
            let bits: (bool,bool,bool,bool) = tx.query_row("SELECT verified_media,verified_metadata,frozen_metadata IS NOT NULL,prepared_hash IS NOT NULL FROM provider_active_destinations WHERE generation=?1 AND version_size='adjusted'", [&f.generation], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
            let complete = !matches!(protection, Protection::SourceOnly);
            assert_eq!(
                bits,
                (
                    complete,
                    false,
                    complete && metadata,
                    matches!(protection, Protection::Prepared)
                ),
                "{protection:?} disjoint={disjoint}"
            );
            if complete {
                assert!(
                    crate::state::db::provider_generations::root_test_pending_path(&tx, &q)
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    crate::state::db::provider_generations::root_test_pending_resource(
                        &tx,
                        "PrimarySync",
                        "asset-m",
                        "adjusted"
                    )
                    .unwrap()
                    .is_empty()
                );
                crate::state::db::provider_generations::root_test_guard_pending_resource(
                    &tx,
                    "PrimarySync",
                    "asset-m",
                    "adjusted",
                    &f.reservation.destination_path,
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        f.reservation.version_size = VersionSizeKey::Adjusted;
        f.reservation.content = Some(ReconciliationContent {
            checksum: CURRENT.into(),
            size: 4,
        });
        let result = db
            .reserve_bound_publication_choices(f.binding, vec![(f.reservation, f.requested)])
            .await;
        let allowed = disjoint || matches!(protection, Protection::None | Protection::SourceOnly);
        if allowed {
            result.unwrap();
        } else {
            assert!(
                matches!(result, Err(StateError::ProviderWorkConflict)),
                "{protection:?}: {result:?}"
            );
        }
        assert_eq!(
            db.acquire_lock("assert metadata refusal is not publication")
                .unwrap()
                .query_row("SELECT count(*) FROM reconciliation_paths", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            i64::from(allowed)
        );
    }
}

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

#[tokio::test]
async fn v027_deferred_observation_requires_fresh_proof_and_atomic_first_destinations() {
    let f = Fixture::new().await;
    let (_, scope, zone) = f.pass.album.private_selection_scope().unwrap().unwrap();
    let key = crate::download::orchestration::generation::pass_key(&f.pass);
    let root = f.db.begin_selection_generation(owner(), GenerationSpec {
        format: 2, scope, zone: zone.clone(), config_hash: "a".repeat(64),
        basis: "b".repeat(64), metadata_enabled: false, metadata_flags: 0,
        profile: json!({"format":2,"coverage":"selected_downloads","passes":[{"key":key,"scope":f.pass.album.selection_scope()}]}),
    }, MAX_GENERATION_BYTES).await.unwrap();
    let body = serde_json::to_vec(&json!({"records":records("m", CURRENT, "observed")})).unwrap();
    let asset = crate::icloud::photos::current_asset(&body, "asset-m", "m", &zone).unwrap();
    let observed = ActiveDecision {
        state_id: "asset-m".into(),
        sources: Vec::new(),
        decision: SelectionDecision {
            pass_key: key.clone(),
            child: "asset-m".into(),
            master: Some("m".into()),
            confirmation: Some(body.clone()),
            outcome: SelectionOutcome::Deferred,
            reason: "current_lookup_unresolved".into(),
            destinations: Vec::new(),
        },
    };
    let queue = f.queue();
    assert!(
        f.db.project_selection_decision(owner(), root.id.clone(), observed.clone(), Vec::new(), 0)
            .await
            .is_err()
    );
    assert_eq!(f.count("provider_active_decisions"), 0);
    f.db.acquire_lock("inject initial observation rollback").unwrap().execute_batch(
        "CREATE TRIGGER initial_observation_fault BEFORE INSERT ON provider_active_decisions BEGIN SELECT RAISE(ABORT,'synthetic observation admission'); END;"
    ).unwrap();
    assert!(
        f.db.project_selection_decision(
            owner(),
            root.id.clone(),
            observed.clone(),
            Vec::new(),
            MAX_GENERATION_BYTES
        )
        .await
        .is_err()
    );
    assert_eq!(f.count("provider_active_decisions"), 0);
    assert_eq!(f.count("provider_active_destinations"), 0);
    assert_eq!(f.queue(), queue);
    f.db.acquire_lock("remove initial observation fault")
        .unwrap()
        .execute_batch("DROP TRIGGER initial_observation_fault;")
        .unwrap();
    assert!(
        !f.db
            .project_selection_decision(
                owner(),
                root.id.clone(),
                observed.clone(),
                Vec::new(),
                MAX_GENERATION_BYTES
            )
            .await
            .unwrap()
            .admitted
    );
    assert_eq!(f.count("provider_active_destinations"), 0);
    assert_eq!(f.queue(), queue);
    f.db.seal_selection_generation(owner(), root.id.clone(), true, None)
        .await
        .unwrap();
    let seal: String =
        f.db.acquire_lock("saved accounted seal")
            .unwrap()
            .query_row(
                "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
                [&root.id],
                |r| r.get(0),
            )
            .unwrap();
    let reopened = SqliteStateDb::open_owned(&f.dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    assert_eq!(
        reopened
            .selection_decision(owner(), root.id.clone(), key.clone(), "asset-m".into())
            .await
            .unwrap(),
        Some(observed.clone())
    );
    let mut planner = TaskPlanner::for_download(f.config.state_db.as_deref())
        .await
        .unwrap();
    let plan = planner
        .plan_download_asset(&asset, &f.config)
        .await
        .unwrap();
    let mut selected = observed.clone();
    selected.decision.outcome = SelectionOutcome::Selected;
    selected.decision.reason.clear();
    selected.decision.destinations = plan
        .tasks
        .iter()
        .map(|task| SelectionDestination {
            version_size: task.version_size.as_str().into(),
            path: SelectionPath::from_path(&std::path::absolute(&task.download_path).unwrap()),
            checksum: task.checksum.to_string(),
            size: task.size,
            metadata_hash: crate::download::filter::metadata_for_selected_version(
                &asset,
                &f.config,
                task.version_size,
            )
            .metadata_hash
            .clone()
            .unwrap(),
        })
        .collect();
    let pending: Vec<_> = plan
        .tasks
        .iter()
        .map(|task| pending_record_for_task(&f.config, &asset, task))
        .collect();
    assert!(
        reopened
            .project_selection_decision(
                owner(),
                root.id.clone(),
                selected.clone(),
                pending.clone(),
                MAX_GENERATION_BYTES
            )
            .await
            .is_err(),
        "saved observation is not fresh confirmation"
    );
    let mut wrong: serde_json::Value = serde_json::from_slice(&body).unwrap();
    wrong
        .pointer_mut("/records/1/fields/masterRef/value/recordName")
        .map(|value| *value = json!("other-master"))
        .unwrap();
    assert!(
        reopened
            .project_selection_decision_with_proof(
                owner(),
                root.id.clone(),
                selected.clone(),
                pending.clone(),
                Some(serde_json::to_vec(&wrong).unwrap()),
                MAX_GENERATION_BYTES
            )
            .await
            .is_err()
    );
    let mut foreign: serde_json::Value = serde_json::from_slice(&body).unwrap();
    foreign["records"][0]["zoneID"] = json!({"zoneName":"OtherSync","ownerRecordName":"foreign"});
    assert!(
        reopened
            .project_selection_decision_with_proof(
                owner(),
                root.id.clone(),
                selected.clone(),
                pending.clone(),
                Some(serde_json::to_vec(&foreign).unwrap()),
                MAX_GENERATION_BYTES,
            )
            .await
            .is_err(),
        "foreign source cannot confirm the retained observation"
    );
    let foreign_owner = crate::state::db::account::AccountOwner::authenticated(
        "other@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"other-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(
        reopened
            .project_selection_decision_with_proof(
                foreign_owner,
                root.id.clone(),
                selected.clone(),
                pending.clone(),
                Some(body.clone()),
                MAX_GENERATION_BYTES,
            )
            .await
            .is_err(),
        "another authenticated account cannot resolve this obligation"
    );
    // The same valid synthetic confirmation cannot give a foreign account
    // authority over this root. The attempted admission above reaches the
    // owner and must leave the original deferred decision and queue intact.
    assert_eq!(
        reopened
            .selection_decision(owner(), root.id.clone(), key.clone(), "asset-m".into())
            .await
            .unwrap(),
        Some(observed.clone())
    );
    assert_eq!(f.count("provider_active_destinations"), 0);
    assert_eq!(f.queue(), queue);
    let mut missing_master: serde_json::Value = serde_json::from_slice(&body).unwrap();
    missing_master["records"]
        .as_array_mut()
        .unwrap()
        .retain(|record| record["recordType"] != "CPLMaster");
    assert!(
        reopened
            .project_selection_decision_with_proof(
                owner(),
                root.id.clone(),
                selected.clone(),
                pending.clone(),
                Some(serde_json::to_vec(&missing_master).unwrap()),
                MAX_GENERATION_BYTES,
            )
            .await
            .is_err(),
        "omitted master cannot certify a selected resource"
    );
    assert_eq!(
        reopened
            .selection_decision(owner(), root.id.clone(), key.clone(), "asset-m".into())
            .await
            .unwrap(),
        Some(observed.clone())
    );
    assert_eq!(f.count("provider_active_destinations"), 0);
    assert_eq!(f.queue(), queue);
    let after_refusal: String = reopened
        .acquire_lock("foreign and missing-master refusal preserves seal")
        .unwrap()
        .query_row(
            "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
            [&root.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after_refusal, seal);
    reopened.acquire_lock("inject destination rollback").unwrap().execute_batch(
        "CREATE TRIGGER observation_destination_fault BEFORE INSERT ON provider_active_destinations BEGIN SELECT RAISE(ABORT,'synthetic first destination'); END;"
    ).unwrap();
    assert!(
        reopened
            .project_selection_decision_with_proof(
                owner(),
                root.id.clone(),
                selected.clone(),
                pending.clone(),
                Some(body.clone()),
                MAX_GENERATION_BYTES
            )
            .await
            .is_err()
    );
    assert_eq!(
        reopened
            .selection_decision(owner(), root.id.clone(), key.clone(), "asset-m".into())
            .await
            .unwrap(),
        Some(observed)
    );
    assert_eq!(f.count("provider_active_destinations"), 0);
    assert_eq!(f.queue(), queue);
    assert_eq!(
        reopened
            .acquire_lock("rollback preserves accounted seal")
            .unwrap()
            .query_row::<String, _, _>(
                "SELECT seal_hash FROM provider_active_generations WHERE id=?1",
                [&root.id],
                |r| r.get(0)
            )
            .unwrap(),
        seal
    );
    reopened
        .acquire_lock("remove destination fault")
        .unwrap()
        .execute_batch("DROP TRIGGER observation_destination_fault;")
        .unwrap();
    assert!(
        reopened
            .project_selection_decision_with_proof(
                owner(),
                root.id.clone(),
                selected.clone(),
                pending,
                Some(body),
                MAX_GENERATION_BYTES
            )
            .await
            .unwrap()
            .admitted
    );
    assert_eq!(
        reopened
            .selection_decision(owner(), root.id, key, "asset-m".into())
            .await
            .unwrap(),
        Some(selected)
    );
    let current = reopened
        .current_download_generation(
            owner(),
            root.spec.scope.clone(),
            root.spec.config_hash.clone(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(current.sealed && current.checkpoint_ready);
    assert!(f.count("provider_active_destinations") > 0);
    f.preserved().await;
}
