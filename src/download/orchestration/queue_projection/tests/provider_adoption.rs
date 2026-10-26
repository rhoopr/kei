//! Provider-signature adoption must commit held-file proof with durable work.
use super::{CURRENT, Fixture, owner, records};
use crate::download::baseline_matching::{BaselineMatch, BaselineMatcher};
use crate::download::planner::{TaskPlanner, pending_record_for_task};
use crate::state::SqliteStateDb;
use crate::state::db::provider_generations::{
    ActiveDecision, GenerationSpec, MAX_GENERATION_BYTES,
};
use crate::state::db::provider_selection::{
    SelectionDecision, SelectionDestination, SelectionOutcome, SelectionPath,
};
use crate::state::error::StateError;
use rusqlite::types::Value;
use serde_json::json;
use sha1::{Digest, Sha1};

fn signature(bytes: &[u8]) -> String {
    let mut hash = Sha1::new();
    hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
    hash.update(bytes);
    data_encoding::BASE64.encode(&[vec![1], hash.finalize().to_vec()].concat())
}

fn snapshot(db: &SqliteStateDb) -> Vec<Vec<Vec<Value>>> {
    let conn = db
        .acquire_lock("snapshot provider adoption ownership")
        .unwrap();
    [
        "assets",
        "asset_metadata_paths",
        "asset_verifications",
        "provider_active_generations",
        "provider_active_decisions",
        "provider_active_destinations",
        "metadata",
    ]
    .iter()
    .map(|table| {
        let columns = conn
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap()
            .column_count();
        let ordering = (1..=columns)
            .map(|column| column.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut query = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY {ordering}"))
            .unwrap();
        query
            .query_map([], |row| {
                (0..columns).map(|column| row.get(column)).collect()
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
    .collect()
}

async fn assert_provider_adoption_interruption(
    fault: &'static str,
    raw_relative_publication: bool,
) {
    let fixture = Fixture::new().await;
    let checksum = signature(b"good");
    let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    let scope = fixture.capture.scope("private", &zone).unwrap();
    let key = "provider-adoption-pass";
    let root = fixture
        .db
        .begin_selection_generation(
            owner(),
            GenerationSpec {
                format: 2,
                scope,
                zone: zone.clone(),
                config_hash: "a".repeat(64),
                basis: "b".repeat(64),
                profile: json!({"coverage":"selected_downloads","current_download_metadata":1,"passes":[{
                    "key":key,"scope":{"database":"private","zone":zone,"kind":"unfiled"}
                }]}),
                metadata_enabled: false,
                metadata_flags: 0,
            },
            MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    let master = "provider-adoption-master";
    let child = format!("asset-{master}");
    let mut values = records(master, &checksum, "synthetic provider metadata");
    for record in &mut values {
        record["zoneID"] = zone.clone();
    }
    values[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(4);
    values[1]["fields"]["masterRef"]["value"]["zoneID"] = zone.clone();
    let confirmation = serde_json::to_vec(&json!({"records":values})).unwrap();
    let asset = crate::icloud::photos::current_asset(&confirmation, &child, master, &zone).unwrap();
    let mut config = fixture.config.clone();
    // Keep publication's existing absolute-path contract. Separately exercise
    // diagnostic capture of the same destination expressed relative to cwd.
    let relative_media = tempfile::tempdir_in(".").unwrap();
    config.directory = std::path::absolute(relative_media.path().join("media"))
        .unwrap()
        .into();
    if raw_relative_publication {
        config.directory = relative_media.path().join("media").into();
    }
    config.folder_structure = "none".into();
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
    let mut tasks = planner
        .plan_download_asset(&asset, &config)
        .await
        .unwrap()
        .tasks;
    assert_eq!(tasks.len(), 1);
    let task = tasks.remove(0);
    assert_eq!(task.size, 4);
    let mut record = pending_record_for_task(&config, &asset, &task);
    let manifest = ActiveDecision {
        decision: SelectionDecision {
            pass_key: key.into(),
            child: child.clone(),
            master: Some(master.into()),
            confirmation: Some(confirmation),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations: vec![SelectionDestination {
                version_size: task.version_size.as_str().into(),
                path: SelectionPath::from_path(&std::path::absolute(&task.download_path).unwrap()),
                checksum: checksum.clone(),
                size: 4,
                metadata_hash: record.metadata.metadata_hash.clone().unwrap(),
            }],
        },
        sources: Vec::new(),
        state_id: child.clone(),
    };
    fixture
        .db
        .project_selection_decision(
            owner(),
            root.id.clone(),
            manifest,
            vec![record.clone()],
            MAX_GENERATION_BYTES,
        )
        .await
        .unwrap();
    use crate::state::db::match_diagnostics::MatchDiagnostics;
    std::fs::create_dir_all(task.download_path.parent().unwrap()).unwrap();
    std::fs::write(&task.download_path, b"evil").unwrap();
    let BaselineMatch::Uncertain(explanation) = BaselineMatcher::default()
        .match_task(&config.directory, &task, &task.download_path)
        .await
    else {
        panic!("wrong same-size bytes must remain uncertain");
    };
    let mut diagnostic_task = task.clone();
    diagnostic_task.download_path = relative_media
        .path()
        .join("media")
        .join(task.download_path.file_name().unwrap());
    let captured = crate::download::baseline_matching::pending_record(
        &config,
        &asset,
        &diagnostic_task,
        explanation,
    );
    let mut report = MatchDiagnostics::decode(captured.last_error.as_deref().unwrap()).unwrap();
    assert_eq!(
        report.attempts[0].destination.to_path(),
        std::path::absolute(&task.download_path).unwrap()
    );
    report.attempts[0].generation = Some(root.id.clone());
    report.attempts[0].pass = Some(key.into());
    report.attempts[0].observed_at = 123;
    record.last_error = report.encode();
    fixture.db.defer_baseline_match(&record).await.unwrap();
    fixture
        .db
        .defer_selection_confirmation(owner(), root.id.clone(), key.into(), child.clone())
        .await
        .unwrap();
    let diagnostics = fixture.db.pending_match_status(10).await.unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].schedule.is_some());
    let work = fixture.db.recorded_selection_work().await.unwrap();
    assert_eq!(work.invalid_roots, 0);
    assert_eq!(work.profiles.len(), 1);
    assert_eq!(work.profiles[0].pending_media, 1);
    assert_eq!(work.profiles[0].verified_destinations, 0);
    assert_eq!(work.profiles[0].unsealed_or_held_roots, 1);

    assert_eq!(
        diagnostics[0].attempt.explanation.candidates[0].reason,
        "candidate_bytes_not_verified_for_current_resource"
    );

    std::fs::create_dir_all(task.download_path.parent().unwrap()).unwrap();
    std::fs::write(&task.download_path, b"good").unwrap();
    let BaselineMatch::ProviderPresent { receipt, file } = BaselineMatcher::default()
        .match_task(&config.directory, &task, &task.download_path)
        .await
    else {
        panic!("supported complete signature must yield held provider proof");
    };
    let before = snapshot(&fixture.db);
    let path = task.download_path.clone();
    let expected = checksum.clone();
    let result = fixture
        .db
        .adopt_provider_signature(
            &record,
            &receipt,
            Box::new(move || {
                if fault == "callback" {
                    return Err(StateError::ProviderSelectionInvalid);
                }
                if fault == "same-size" {
                    std::fs::write(&path, b"evil").unwrap();
                } else {
                    assert_eq!(fault, "inode");
                    let replacement = path.with_extension("replacement");
                    std::fs::write(&replacement, b"good").unwrap();
                    std::fs::rename(replacement, &path).unwrap();
                }
                if file.fingerprint.size != 4
                    || !matches!(file.matches_provider_v1_blocking(&expected), Ok(Some(true)))
                {
                    return Err(StateError::ProviderSelectionInvalid);
                }
                Ok(())
            }),
        )
        .await;
    assert!(
        result.is_err(),
        "changed held-file proof must roll back adoption"
    );
    assert!(
        snapshot(&fixture.db) == before,
        "pending ownership and receipts must roll back together"
    );
    let state = fixture.dir.path().join("state.db");
    let media_root = config.directory.clone();
    let media_path = task.download_path.clone();
    drop(planner);
    drop(config);
    drop(fixture.config);
    drop(fixture.capture);
    drop(fixture.pass);
    drop(fixture.db);

    let db = SqliteStateDb::open_owned(&state, &owner()).await.unwrap();
    assert!(snapshot(&db) == before);
    let reopened_report = db.pending_match_status(10).await.unwrap();
    assert_eq!(reopened_report[0].schedule, diagnostics[0].schedule);
    assert_eq!(reopened_report[0].attempt.observed_at, 123);

    let progress: (String, i64, i64, i64) = db.acquire_lock("read retained adoption retry").unwrap()
        .query_row(
            "SELECT a.status,w.admitted,w.verified_media,
                (SELECT COUNT(*) FROM asset_metadata_paths p WHERE p.library=a.library AND p.id=a.id)
             FROM assets a JOIN provider_active_destinations w
               ON w.library=a.library AND w.asset_id=a.id AND w.version_size=a.version_size
             WHERE w.generation=?1",
            [&root.id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).unwrap();
    assert_eq!(progress, ("pending".into(), 1, 0, 0));
    if fault == "same-size" {
        std::fs::write(&media_path, b"good").unwrap();
    }
    let mtime = std::fs::metadata(&media_path).unwrap().modified().unwrap();
    let BaselineMatch::ProviderPresent { receipt, file } = BaselineMatcher::default()
        .match_task(&media_root, &task, &task.download_path)
        .await
    else {
        panic!("fresh proof must support an independent retry");
    };
    let expected = checksum.clone();
    db.adopt_provider_signature(
        &record,
        &receipt,
        Box::new(move || {
            if file.fingerprint.size == 4
                && matches!(file.matches_provider_v1_blocking(&expected), Ok(Some(true)))
            {
                Ok(())
            } else {
                Err(StateError::ProviderSelectionInvalid)
            }
        }),
    )
    .await
    .unwrap();
    assert!(db.pending_match_status(10).await.unwrap().is_empty());
    let completed = snapshot(&db);
    let progress: (String, i64, i64) = db
        .acquire_lock("read committed provider adoption")
        .unwrap()
        .query_row(
            "SELECT a.status,w.verified_media,
                (SELECT COUNT(*) FROM asset_metadata_paths p WHERE p.library=a.library
                 AND p.id=a.id AND p.source_checksum=p.local_checksum)
             FROM assets a JOIN provider_active_destinations w
               ON w.library=a.library AND w.asset_id=a.id AND w.version_size=a.version_size
             WHERE w.generation=?1",
            [&root.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let work = db.recorded_selection_work().await.unwrap();
    assert_eq!(work.invalid_roots, 0);
    assert_eq!(work.profiles[0].pending_media, 0);
    assert_eq!(work.profiles[0].verified_destinations, 1);
    assert_eq!(progress, ("downloaded".into(), 1, 1));
    drop(db);
    for _ in 0..2 {
        let db = SqliteStateDb::open_owned(&state, &owner()).await.unwrap();
        assert!(snapshot(&db) == completed);
        assert_eq!(std::fs::read(&media_path).unwrap(), b"good");
        assert_eq!(
            std::fs::metadata(&media_path).unwrap().modified().unwrap(),
            mtime
        );
    }
}

#[tokio::test]
async fn v027_provider_adoption_callback_failure_retains_durable_retry() {
    assert_provider_adoption_interruption("callback", false).await;
}

#[tokio::test]
async fn v027_provider_adoption_same_size_mutation_rolls_back_before_commit() {
    assert_provider_adoption_interruption("same-size", false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn v027_provider_adoption_inode_replacement_rolls_back_before_commit() {
    assert_provider_adoption_interruption("inode", false).await;
}

#[tokio::test]
async fn v027_relative_provider_adoption_settles_exact_destination_after_reopen() {
    assert_provider_adoption_interruption("callback", true).await;
}

#[tokio::test]
async fn coverage_keeps_profiles_unknown_windows_and_invalid_evidence_separate() {
    let fixture = Fixture::new().await;
    let zone = json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
    let scope = fixture.capture.scope("private", &zone).unwrap();
    let mut ids = Vec::new();
    let mut evidence = records("report-master", CURRENT, "synthetic report evidence");
    for record in &mut evidence {
        record["zoneID"] = zone.clone();
    }
    evidence[1]["fields"]["masterRef"]["value"]["zoneID"] = zone.clone();
    let confirmation = serde_json::to_vec(&json!({"records": evidence})).unwrap();
    for (hash, recognized) in [('a', true), ('c', true), ('d', false)] {
        let mut profile = json!({"coverage":"selected_downloads","passes":[{"key":"unfiled","scope":{"database":"private","zone":zone,"kind":"unfiled"}}]});
        if recognized {
            profile["current_download_metadata"] = json!(1);
        }
        ids.push(
            fixture
                .db
                .begin_selection_generation(
                    owner(),
                    GenerationSpec {
                        format: 2,
                        scope: scope.clone(),
                        zone: zone.clone(),
                        config_hash: hash.to_string().repeat(64),
                        basis: "b".repeat(64),
                        profile,
                        metadata_enabled: false,
                        metadata_flags: 0,
                    },
                    MAX_GENERATION_BYTES,
                )
                .await
                .unwrap()
                .id,
        );
        fixture
            .db
            .project_selection_decision(
                owner(),
                ids.last().unwrap().clone(),
                ActiveDecision {
                    sources: Vec::new(),
                    state_id: "asset-report-master".into(),
                    decision: SelectionDecision {
                        pass_key: "unfiled".into(),
                        child: "asset-report-master".into(),
                        master: Some("report-master".into()),
                        confirmation: Some(confirmation.clone()),
                        outcome: SelectionOutcome::Deferred,
                        reason: "current_lookup_unresolved".into(),
                        destinations: Vec::new(),
                    },
                },
                Vec::new(),
                MAX_GENERATION_BYTES,
            )
            .await
            .unwrap();
    }
    let work = fixture.db.recorded_selection_work().await.unwrap();
    assert_eq!(
        work.profiles.len(),
        2,
        "different retained settings cannot become one current total"
    );
    assert_eq!(work.unsupported_profiles, 1);
    assert_eq!(work.invalid_roots, 0);
    assert!(
        work.profiles.iter().all(
            |profile| profile.unsealed_or_held_roots == 1 && profile.verified_destinations == 0
        )
    );
    fixture
        .db
        .acquire_lock("corrupt reporting proof")
        .unwrap()
        .execute(
            "UPDATE provider_active_generations SET specification_hash='invalid' WHERE id=?1",
            [&ids[0]],
        )
        .unwrap();
    let invalid = fixture.db.recorded_selection_work().await.unwrap();
    assert_eq!(invalid.invalid_roots, 1);
    assert_eq!(invalid.profiles.len(), 1);
    assert_eq!(invalid.unsupported_profiles, 1);
    assert_eq!(invalid.stored_media_receipts, 0);
}

#[tokio::test]
async fn v027_relative_provider_adoption_mutation_rolls_back_before_settlement() {
    assert_provider_adoption_interruption("same-size", true).await;
}
#[tokio::test]
async fn v027_relative_provider_adoption_inode_replacement_rolls_back_before_settlement() {
    assert_provider_adoption_interruption("inode", true).await;
}
