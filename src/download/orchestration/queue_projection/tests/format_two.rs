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

async fn named_admission_declaration(
    f: &Fixture,
    kind: Option<&str>,
    name: &str,
    metadata_enabled: bool,
) -> (
    crate::state::db::provider_generations::ActiveGeneration,
    ActiveDecision,
    Vec<crate::state::AssetRecord>,
) {
    let (_, scope, zone) = f.pass.album.private_selection_scope().unwrap().unwrap();
    let current = f.pass.album.confirm_catalog_asset("asset-m").await.unwrap();
    let mut config = f.config.with_pass(&f.pass);
    config.metadata.set_exif_datetime = metadata_enabled;
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
    let plan = planner
        .plan_download_asset(&current.asset, &config)
        .await
        .unwrap();
    assert_eq!(plan.tasks.len(), 1);
    let records: Vec<_> = plan
        .tasks
        .iter()
        .map(|task| pending_record_for_task(&config, &current.asset, task))
        .collect();
    let mut selected_pass = json!({"key":"named-admission","scope":f.pass.album.selection_scope()});
    selected_pass["scope"]["name"] = json!(name);
    if let Some(kind) = kind {
        selected_pass["kind"] = json!(kind);
    }
    let root = f.db.begin_selection_generation(owner(), GenerationSpec {
        format:2, scope, zone, config_hash:"a".repeat(64), basis:"b".repeat(64),
        profile:json!({"coverage":"selected_downloads","current_download_metadata":1,"passes":[selected_pass]}),
        metadata_enabled, metadata_flags:crate::download::pipeline::MetadataFlags::from(&config).bits(),
    }, MAX_GENERATION_BYTES).await.unwrap();
    let decision = ActiveDecision {
        state_id: "asset-m".into(),
        sources: Vec::new(),
        decision: SelectionDecision {
            pass_key: "named-admission".into(),
            child: "asset-m".into(),
            master: Some("m".into()),
            confirmation: Some(current.body),
            outcome: SelectionOutcome::Selected,
            reason: String::new(),
            destinations: plan
                .tasks
                .iter()
                .zip(&records)
                .map(|(task, record)| SelectionDestination {
                    version_size: task.version_size.as_str().into(),
                    path: SelectionPath::from_path(&task.download_path),
                    checksum: task.checksum.to_string(),
                    size: task.size,
                    metadata_hash: record.metadata.metadata_hash.clone().unwrap(),
                })
                .collect(),
        },
    };
    (root, decision, records)
}

#[tokio::test]
async fn named_admission_unfiled_label_creates_neither_membership_nor_retry_marker() {
    for metadata_enabled in [false, true] {
        for (kind, name, expected) in [
            (Some("Unfiled"), "TestAlbum", false),
            (Some("Album"), "TestAlbum", true),
            (Some("SmartFolder"), "TestAlbum", true),
            (None, "TestAlbum", true),
            (Some("FutureKind"), "TestAlbum", true),
            (Some("Album"), "", false),
        ] {
            let f = Fixture::new().await;
            let (root, decision, records) =
                named_admission_declaration(&f, kind, name, metadata_enabled).await;
            let projection =
                f.db.project_selection_decision(
                    owner(),
                    root.id.clone(),
                    decision.clone(),
                    records.clone(),
                    MAX_GENERATION_BYTES,
                )
                .await
                .unwrap();
            assert!(projection.admitted, "{kind:?}: {}", projection.reason);
            // This is the downstream owner that inherited the invented association.
            f.db.upsert_seen(&records[0]).await.unwrap();
            f.preserved().await;
            let Fixture {
                dir,
                db,
                capture,
                session,
                pass,
                config,
            } = f;
            drop((db, capture, session, pass, config));
            let db = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
                .await
                .unwrap();
            assert_eq!(
                db.selection_decision(
                    owner(),
                    root.id.clone(),
                    "named-admission".into(),
                    "asset-m".into()
                )
                .await
                .unwrap(),
                Some(decision)
            );
            assert!(
                !db.selection_destination_verified(
                    owner(),
                    root.id,
                    "named-admission".into(),
                    "asset-m".into(),
                    "original".into()
                )
                .await
                .unwrap()
            );
            let conn = db
                .acquire_lock("reopened name-derived admission evidence")
                .unwrap();
            let (associations, marked):(i64,bool) = conn.query_row("SELECT (SELECT count(*) FROM asset_albums WHERE library='PrimarySync' AND asset_id='asset-m'),metadata_write_failed_at IS NOT NULL FROM assets WHERE library='PrimarySync' AND id='asset-m'",[],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
            assert_eq!(
                (associations, marked),
                (i64::from(expected), expected),
                "kind={kind:?}, metadata={metadata_enabled}"
            );
        }
    }
}

#[tokio::test]
async fn named_admission_unfiled_label_cannot_block_on_unfinished_groupings() {
    for kind in [
        Some("Unfiled"),
        Some("Album"),
        Some("SmartFolder"),
        None,
        Some("FutureKind"),
    ] {
        let f = Fixture::new().await;
        let (old, old_decision, records) =
            named_admission_declaration(&f, Some("Unfiled"), "", true).await;
        assert!(
            f.db.project_selection_decision(
                owner(),
                old.id.clone(),
                old_decision,
                records,
                MAX_GENERATION_BYTES
            )
            .await
            .unwrap()
            .admitted
        );
        let (root, decision, records) =
            named_admission_declaration(&f, kind, "TestAlbum", true).await;
        let protected = || {
            f.db.acquire_lock("unfinished grouping intent").unwrap().query_row("SELECT grouping_hash,intent_hash,verified_media,verified_metadata,frozen_metadata FROM provider_active_destinations WHERE generation=?1",[&old.id],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,bool>(2)?,row.get::<_,bool>(3)?,row.get::<_,Option<Vec<u8>>>(4)?))).unwrap()
        };
        let before = protected();
        assert!(!before.2 && !before.3 && before.4.is_none());
        let projection =
            f.db.project_selection_decision(
                owner(),
                root.id.clone(),
                decision.clone(),
                records,
                MAX_GENERATION_BYTES,
            )
            .await
            .unwrap();
        assert_eq!(
            projection.admitted,
            kind == Some("Unfiled"),
            "{kind:?}: {}",
            projection.reason
        );
        assert_eq!(
            projection.reason,
            if kind == Some("Unfiled") {
                ""
            } else {
                "unfinished_grouping_conflict"
            }
        );
        assert_eq!(
            protected(),
            before,
            "independent writer intent must survive either outcome"
        );
        f.preserved().await;
        let Fixture {
            dir,
            db,
            capture,
            session,
            pass,
            config,
        } = f;
        drop((db, capture, session, pass, config));
        let db = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
            .await
            .unwrap();
        assert_eq!(
            db.selection_decision(owner(), root.id, "named-admission".into(), "asset-m".into())
                .await
                .unwrap(),
            Some(decision)
        );
        let conn = db
            .acquire_lock("reopened grouping admission state")
            .unwrap();
        let (admission, reason): (String, String) = conn
            .query_row(
                "SELECT admission,reason FROM provider_active_decisions WHERE generation<>?1",
                [old.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            admission,
            if kind == Some("Unfiled") {
                "admitted"
            } else {
                "deferred"
            }
        );
        assert_eq!(reason, projection.reason);
    }
}

#[tokio::test]
async fn named_admission_does_not_clean_up_existing_unfiled_label_association() {
    let f = Fixture::new().await;
    f.db.add_asset_album("PrimarySync", "asset-m", "TestAlbum", "icloud")
        .await
        .unwrap();
    let (root, decision, records) =
        named_admission_declaration(&f, Some("Unfiled"), "TestAlbum", false).await;
    assert!(
        f.db.project_selection_decision(
            owner(),
            root.id,
            decision,
            records.clone(),
            MAX_GENERATION_BYTES
        )
        .await
        .unwrap()
        .admitted
    );
    f.db.upsert_seen(&records[0]).await.unwrap();
    let conn =
        f.db.acquire_lock("existing label is not cleanup authority")
            .unwrap();
    let (associations, marked): (i64, bool) = conn.query_row(
        "SELECT (SELECT count(*) FROM asset_albums WHERE library='PrimarySync' AND asset_id='asset-m' AND album_name='TestAlbum'),metadata_write_failed_at IS NOT NULL FROM assets WHERE library='PrimarySync' AND id='asset-m'",
        [], |row| Ok((row.get(0)?, row.get(1)?))
    ).unwrap();
    assert_eq!((associations, marked), (1, true));
}

#[tokio::test]
async fn named_admission_preserves_real_membership_and_prepared_capture_debt() {
    use crate::state::db::MetadataRewriteQueue;
    use crate::state::{METADATA_CAPTURE_REVISION, MetadataCapture};
    let f = Fixture::new().await;
    f.db.upsert_album_container("PrimarySync", "real-container", "RealAlbum", "Album")
        .await
        .unwrap();
    assert!(
        f.db.upsert_album_membership_delta(
            "PrimarySync",
            "real-container",
            "asset-m",
            Some("m"),
            "icloud"
        )
        .await
        .unwrap()
    );
    let peer = crate::test_helpers::TestAssetRecord::new("capture-peer").build();
    f.db.upsert_seen(&peer).await.unwrap();
    let path = f.config.directory.join("existing.jpg");
    f.db.mark_downloaded(
        "PrimarySync",
        "capture-peer",
        "original",
        &path,
        "retained-local",
        None,
    )
    .await
    .unwrap();
    let capture = MetadataCapture {
        shared: peer.metadata.clone(),
        renditions: Default::default(),
    };
    assert_eq!(
        f.db.refresh_downloaded_asset_metadata(
            "PrimarySync",
            "capture-peer",
            (&capture, peer.created_at, peer.added_at),
            true,
            true,
            METADATA_CAPTURE_REVISION
        )
        .await
        .unwrap(),
        1
    );
    let pending =
        f.db.get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        f.db.record_capture_repair_prepared(&pending[0], "prepared-capture", 42)
            .await
            .unwrap()
            .is_some()
    );
    let before = noncommit_choice_snapshot(&f.db);
    let (root, decision, records) =
        named_admission_declaration(&f, Some("Unfiled"), "TestAlbum", false).await;
    assert!(
        f.db.project_selection_decision(
            owner(),
            root.id.clone(),
            decision.clone(),
            records.clone(),
            MAX_GENERATION_BYTES
        )
        .await
        .unwrap()
        .admitted
    );
    f.db.upsert_seen(&records[0]).await.unwrap();
    f.preserved().await;
    let Fixture {
        dir,
        db,
        capture,
        session,
        pass,
        config,
    } = f;
    drop((db, capture, session, pass, config));
    let db = SqliteStateDb::open_owned(&dir.path().join("state.db"), &owner())
        .await
        .unwrap();
    assert_eq!(
        db.selection_decision(owner(), root.id, "named-admission".into(), "asset-m".into())
            .await
            .unwrap(),
        Some(decision)
    );
    let conn = db
        .acquire_lock("real membership and capture debt survive Unfiled admission")
        .unwrap();
    let names:Vec<String>=conn.prepare("SELECT album_name FROM asset_albums WHERE library='PrimarySync' AND asset_id='asset-m' ORDER BY album_name").unwrap().query_map([],|row|row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(names, ["RealAlbum"]);
    assert!(
        conn.query_row(
            "SELECT metadata_write_failed_at IS NOT NULL FROM assets WHERE id='asset-m'",
            [],
            |row| row.get::<_, bool>(0)
        )
        .unwrap()
    );
    assert_eq!(conn.query_row("SELECT count(*) FROM asset_album_memberships WHERE container_id='real-container' AND asset_record_name='asset-m'",[],|row|row.get::<_,i64>(0)).unwrap(),1);
    let debt:(String,i64,bool)=conn.query_row("SELECT capture_repair_output_checksum,capture_repair_output_size,metadata_write_failed_at IS NOT NULL FROM assets WHERE id='capture-peer'",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(debt, ("prepared-capture".into(), 42, true));
    drop(conn);
    let after = noncommit_choice_snapshot(&db);
    assert_eq!(
        before
            .iter()
            .find(|(table, _)| table == "asset_metadata_paths"),
        after
            .iter()
            .find(|(table, _)| table == "asset_metadata_paths"),
        "prepared per-path capture packet unchanged"
    );
}

fn noncommit_choice_snapshot(db: &SqliteStateDb) -> Vec<(String, Vec<String>)> {
    let conn = db
        .acquire_lock("noncommit choice and admission snapshot")
        .unwrap();
    [
        ("reconciliation_paths", "SELECT * FROM reconciliation_paths"),
        ("provider_active_generations", "SELECT * FROM provider_active_generations"),
        ("provider_active_decisions", "SELECT * FROM provider_active_decisions"),
        ("provider_active_destinations", "SELECT * FROM provider_active_destinations"),
        ("provider_selection_generations", "SELECT * FROM provider_selection_generations"),
        ("provider_selection_decisions", "SELECT * FROM provider_selection_decisions"),
        ("provider_selection_destinations", "SELECT * FROM provider_selection_destinations"),
        ("provider_work_receipts", "SELECT * FROM provider_work_receipts"),
        ("provider_work_obligations", "SELECT * FROM provider_work_obligations"),
        ("download_receipts", "SELECT * FROM download_receipts"),
        ("asset_metadata_paths", "SELECT * FROM asset_metadata_paths"),
        // Source observations can update seen/metadata fields or retain a pending
        // asset without a path. Compare materialized queue destinations, not telemetry.
        ("assets_with_destinations", "SELECT library,id,version_size,status,local_path,checksum,size_bytes,local_checksum FROM assets WHERE local_path IS NOT NULL"),
    ].into_iter().map(|(name, sql)| {
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        let mut rows: Vec<String> = statement.query_map([], |row| {
            (0..columns).map(|index| row.get::<_,rusqlite::types::Value>(index)).collect::<Result<Vec<_>,_>>()
                .map(|values| format!("{values:?}"))
        }).unwrap().collect::<Result<_,_>>().unwrap();
        rows.sort();
        (name.to_owned(), rows)
    }).collect()
}

#[tokio::test]
async fn collecting_dry_run_mixed_task_observation_failure_preserves_successful_preview() {
    use crate::download::orchestration::models::{
        DownloadControls, DownloadReporting, DownloadRunMode,
    };
    use sha2::{Digest, Sha256};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    for fail_b in [false, true] {
        let server = wiremock::MockServer::start().await;
        let mut f = Fixture::new().await;
        f.config.folder_structure = "none".into();
        let mut source = Vec::new();
        for (name, filename, bytes) in [
            (
                "a",
                "preview-a.JPG",
                b"\xff\xd8\xff\xe0eligible preview A".as_slice(),
            ),
            (
                "b",
                "preview-b.JPG",
                b"\xff\xd8\xff\xe0eligible preview B".as_slice(),
            ),
        ] {
            let url = format!("{}/{name}", server.uri());
            Mock::given(method("GET"))
                .and(path(format!("/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .expect(0)
                .mount(&server)
                .await;
            let mut pair = super::eligible_records(name, &url);
            pair[0]["fields"]["filenameEnc"]["value"] = json!(filename);
            pair[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(data_encoding::BASE64.encode(&Sha256::digest(bytes)));
            pair[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(bytes.len());
            source.extend(pair);
        }
        *f.session.records.lock().unwrap() = source.clone();
        *f.session.changes_records.lock().unwrap() = source;
        assert!(f.dir.path().join("state.db").is_file());
        let concrete: Arc<dyn crate::download::DownloadStore> = f.db.clone();
        assert!(Arc::ptr_eq(f.config.state_db.as_ref().unwrap(), &concrete));
        let (capture, _, _) = f
            .pass
            .album
            .publication_scope("PrimarySync")
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&capture.db, &f.db));
        for child in ["asset-a", "asset-b"] {
            let asset = f
                .pass
                .album
                .confirm_catalog_asset(child)
                .await
                .unwrap()
                .asset;
            assert_eq!(asset.state_id(), child);
        }

        let file_snapshot = || {
            let mut files: Vec<_> = std::fs::read_dir(&f.config.directory)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    assert!(entry.file_type().unwrap().is_file());
                    let handle = std::fs::File::open(entry.path()).unwrap();
                    (
                        entry.file_name(),
                        std::fs::read(entry.path()).unwrap(),
                        crate::fs_util::file_identity(&handle).unwrap(),
                        handle.metadata().unwrap().modified().unwrap(),
                    )
                })
                .collect();
            files.sort_by(|left, right| left.0.cmp(&right.0));
            files
        };
        let retained_snapshot = || {
            let conn =
                f.db.acquire_lock("collecting dry-run retained checkpoint and temp evidence")
                    .unwrap();
            let metadata = conn
                .prepare("SELECT key,value FROM metadata ORDER BY key")
                .unwrap()
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            let unknown: Vec<u8> = conn
                .query_row("SELECT payload FROM unknown_future", [], |row| row.get(0))
                .unwrap();
            let temps: i64 = conn
                .query_row("SELECT count(*) FROM owned_temp_files", [], |row| {
                    row.get(0)
                })
                .unwrap();
            (metadata, unknown, temps)
        };
        let before = noncommit_choice_snapshot(&f.db);
        let before_files = file_snapshot();
        assert_eq!(
            before_files.len(),
            2,
            "nonvacuous existing media and sidecar protection"
        );
        let before_retained = retained_snapshot();
        assert!(!before_retained.0.is_empty());
        assert_eq!(before_retained.1, [0, 255, 7]);
        assert_eq!(before_retained.2, 0);
        {
            let conn =
                f.db.acquire_lock("install only B task observation fault")
                    .unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM assets WHERE id IN ('asset-a','asset-b')",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            if fail_b {
                conn.execute_batch("CREATE TRIGGER dryrun_reject_b_task_observation BEFORE INSERT ON assets WHEN NEW.library='PrimarySync' AND NEW.id='asset-b' AND NEW.version_size='original' AND NEW.filename='preview-b.JPG' BEGIN SELECT RAISE(ABORT,'dryrun B task observation rejected'); END;").unwrap();
            }
        }

        let (trace, _guard) = crate::test_helpers::TracingCapture::install();
        let result =
            crate::download::orchestration::incremental::download_photos_incremental_collecting(
                &reqwest::Client::new(),
                std::slice::from_ref(&f.pass),
                &Arc::new(f.config.clone()),
                "mixed-preview-source",
                DownloadControls::new(DownloadRunMode::DryRun, DownloadReporting::hidden()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let would_download = if fail_b { 1 } else { 2 };
        assert_eq!(
            result.stats.downloaded, would_download,
            "fail_b={fail_b}: {result:?}"
        );
        assert_eq!(result.stats.state_write_failures, usize::from(fail_b));
        assert_eq!(
            (
                result.stats.enumeration_errors,
                result.stats.exif_failures,
                result.stats.metadata_capture_failures
            ),
            (0, 0, 0)
        );
        assert_eq!(
            result.stats.failed, 0,
            "this is an observation-write failure, not a transfer failure"
        );
        assert!(!matches!(
            &result.outcome,
            crate::download::DownloadOutcome::SessionExpired { .. }
        ));
        assert!(!result.stats.enumeration_incomplete && !result.stats.sync_token_blocked);
        assert!(!result.stats.identity_incomplete && !result.stats.interrupted);
        assert!(result.sync_token.is_none());
        assert_eq!(
            (
                result.stats.bytes_downloaded,
                result.stats.disk_bytes_written,
                result.stats.photos_downloaded,
                result.stats.videos_downloaded
            ),
            (0, 0, 0, 0)
        );
        assert_eq!(
            f.session.changes_cursors.lock().unwrap().as_slice(),
            &[Some("mixed-preview-source".to_owned())]
        );

        let events = trace.events();
        let planning: Vec<_> = events
            .iter()
            .filter(|event| event.message() == Some("Incremental task planning phase complete"))
            .collect();
        assert_eq!(planning.len(), 1);
        assert_eq!(
            planning[0].field("planned_tasks"),
            Some(would_download.to_string().as_str())
        );
        assert!(!trace.contains_event(|event| event.message()
            == Some("All incremental assets already downloaded or filtered")));
        let a = f.config.directory.join("preview-a.JPG");
        let b = f.config.directory.join("preview-b.JPG");
        assert!(
            trace.contains_event(|event| event.message() == Some("[DRY RUN] Would download")
                && event.field("path") == Some(a.to_str().unwrap()))
        );
        assert_eq!(
            trace.contains_event(|event| event.message() == Some("[DRY RUN] Would download")
                && event.field("path") == Some(b.to_str().unwrap())),
            !fail_b
        );
        let failed: Vec<_> = events
            .iter()
            .filter(|event| event.message() == Some("Failed to record asset in state DB"))
            .collect();
        assert_eq!(failed.len(), usize::from(fail_b));
        if fail_b {
            assert_eq!(failed[0].field("asset_id"), Some("asset-b"));
            assert!(
                failed[0]
                    .field("error")
                    .unwrap()
                    .contains("dryrun B task observation rejected")
            );
        }
        {
            let conn =
                f.db.acquire_lock("successful observation survives B-only abort")
                    .unwrap();
            let observed: Vec<(String, String, Option<String>)> = conn.prepare("SELECT id,status,local_path FROM assets WHERE id IN ('asset-a','asset-b') ORDER BY id").unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
                .collect::<rusqlite::Result<Vec<_>>>().unwrap();
            let mut expected = vec![("asset-a".to_owned(), "pending".to_owned(), None)];
            if !fail_b {
                expected.push(("asset-b".to_owned(), "pending".to_owned(), None));
            }
            assert_eq!(observed, expected);
        }
        assert_eq!(noncommit_choice_snapshot(&f.db), before);
        assert_eq!(retained_snapshot(), before_retained);
        assert_eq!(file_snapshot(), before_files);
        assert!(!a.exists() && !b.exists());
        assert!(server.received_requests().await.unwrap().is_empty());

        // Keep the outcome oracle last: all branch/safety preconditions above
        // must pass before exposing the mixed-task Success misclassification.
        if fail_b {
            assert!(
                matches!(
                    result.outcome,
                    crate::download::DownloadOutcome::PartialFailure { failed_count: 1 }
                ),
                "mixed dry-run planning failure must not report Success: {result:?}"
            );
        } else {
            assert!(
                matches!(result.outcome, crate::download::DownloadOutcome::Success),
                "no-fault two-preview control: {result:?}"
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn noncommitting_modes_preserve_positively_nominated_publication_choice() {
    use crate::download::baseline_matching::BaselineMatch;
    use crate::download::orchestration::models::{
        DownloadControls, DownloadReporting, DownloadRunMode,
    };
    use crate::download::pipeline::{StreamRuntime, stream_and_download_from_stream};
    use futures_util::StreamExt;
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let original = b"\xff\xd8\xff\xe0unchanged original";
    let edited = b"\xff\xd8\xff\xe0new adjusted bytes";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    for (collecting, mode) in [
        (false, DownloadRunMode::PrintFilenames),
        (false, DownloadRunMode::DryRun),
        (true, DownloadRunMode::PrintFilenames),
        (true, DownloadRunMode::DryRun),
    ] {
        let case = format!(
            "{}-{mode:?}",
            if collecting {
                "collecting"
            } else {
                "streaming"
            }
        );
        let server = wiremock::MockServer::start().await;
        for (name, bytes) in [
            ("original", original.as_slice()),
            ("edited", edited.as_slice()),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .mount(&server)
                .await;
        }
        let mut f = Fixture::new().await;
        f.config.folder_structure = "none".into();
        f.config.edited = true;
        f.config.edited_naming = crate::types::EditedNaming::Primary;
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
            values[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/original",server.uri()),"size":original.len(),"fileChecksum":checksum(original)});
        }
        let baseline = stream_collision_child(&f, "asset-m", false).await;
        assert_eq!(
            (baseline.downloaded, baseline.state_write_failures),
            (1, 0),
            "{case}: {baseline:?}"
        );
        let p = f.config.directory.join("photo.JPG");
        let metadata = std::fs::metadata(&p).unwrap();
        let p_identity = (metadata.ino(), metadata.mtime(), metadata.mtime_nsec());
        {
            let mut values = f.session.records.lock().unwrap();
            values[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edited",server.uri()),"size":edited.len(),"fileChecksum":checksum(edited)}});
            values[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
            values[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
        }
        f.config.resolution = crate::types::PhotoResolution::None;
        *f.session.changes_records.lock().unwrap() = f.session.records.lock().unwrap().clone();
        let config = Arc::new(f.config.with_pass(&f.pass));
        let concrete: Arc<dyn crate::download::DownloadStore> = f.db.clone();
        assert!(Arc::ptr_eq(config.state_db.as_ref().unwrap(), &concrete));
        let before = noncommit_choice_snapshot(&f.db);
        let asset = f
            .pass
            .album
            .confirm_catalog_asset("asset-m")
            .await
            .unwrap()
            .asset;
        let s = {
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            assert!(plan.filter_reason.is_none() && plan.malformed_resource.is_none());
            assert_eq!(plan.tasks.len(), 1);
            let task = &plan.tasks[0];
            assert_eq!(task.version_size, crate::state::VersionSizeKey::Adjusted);
            assert_eq!(
                task.superseded_requested(&config.directory, &p),
                Some(p.as_path())
            );
            assert_ne!(task.download_path, p);
            assert!(!task.download_path.exists());
            let reservations: Vec<_> = planner.reconciliation_reservations().collect();
            assert_eq!(reservations.len(), 1);
            assert_eq!(
                reservations[0].requested_path_key.0,
                crate::fs_util::confined_path_key(&p).unwrap()
            );
            assert_eq!(reservations[0].destination_path, task.download_path);
            assert!(
                matches!(
                    planner.match_existing(&asset, &config, task).await,
                    BaselineMatch::Missing
                ),
                "{case}: nomination is not eligible work"
            );
            let binding = task.publication_binding.as_deref().unwrap().clone();
            let receipts =
                f.db.bound_download_receipts(
                    binding,
                    "original".into(),
                    checksum(original),
                    original.len() as u64,
                )
                .await
                .unwrap();
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].native_path.to_path(), p);
            task.download_path.clone()
        };
        assert_eq!(
            noncommit_choice_snapshot(&f.db),
            before,
            "{case}: pure nomination committed state"
        );
        let before_files: Vec<_> = std::fs::read_dir(&config.directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let gets_before = server.received_requests().await.unwrap().len();
        assert_eq!(gets_before, 1);
        let cursors_before = f.session.changes_cursors.lock().unwrap().len();
        let controls = DownloadControls::new(mode, DownloadReporting::hidden());
        let (trace, _guard) = crate::test_helpers::TracingCapture::install();
        if collecting {
            let result = crate::download::orchestration::incremental::download_photos_incremental_collecting(
                &reqwest::Client::new(), std::slice::from_ref(&f.pass), &Arc::new(f.config.clone()),
                "noncommit-source", controls, CancellationToken::new(),
            ).await.unwrap();
            eprintln!("NONCOMMIT_ENTRY case={case} result={result:?}");
            assert!(
                trace.contains_event(|event| event.message()
                    == Some("Incremental task planning phase complete")
                    && event.field("planned_tasks") == Some("1")),
                "{case}: did not plan eligible collected task; events={:?}",
                trace.events()
            );
            assert_eq!(result.stats.enumeration_errors, 0);
            assert_eq!(result.stats.state_write_failures, 0);
            assert!(matches!(
                result.outcome,
                crate::download::DownloadOutcome::Success
            ));
            assert!(
                result.sync_token.is_none(),
                "{case}: preview offered a checkpoint"
            );
            assert_eq!(
                result.stats.downloaded,
                usize::from(mode == DownloadRunMode::DryRun)
            );
            assert_eq!(
                (
                    result.stats.bytes_downloaded,
                    result.stats.disk_bytes_written,
                    result.stats.photos_downloaded,
                    result.stats.videos_downloaded
                ),
                (0, 0, 0, 0)
            );
        } else {
            let (changes, successor, _) = f
                .pass
                .album
                .changes_stream_observed("noncommit-source", true);
            let observed = Arc::new(AtomicUsize::new(0));
            let enumerated = observed.clone();
            let combined = changes.filter_map(move |change| {
                futures_util::future::ready(match change {
                    Ok(change) => change.asset.map(|asset| {
                        assert_eq!(asset.asset_record_name(), "asset-m");
                        enumerated.fetch_add(1, Ordering::SeqCst);
                        Ok(asset)
                    }),
                    Err(error) => Some(Err(error)),
                })
            });
            let result = stream_and_download_from_stream(
                &reqwest::Client::new(),
                combined,
                &config,
                controls,
                1,
                CancellationToken::new(),
                StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(
                observed.load(Ordering::SeqCst),
                1,
                "{case}: source not consumed"
            );
            assert_eq!(successor.await.unwrap(), "quiet-zone-cursor");
            assert!(result.enumeration_complete);
            assert_eq!(
                (
                    result.enumeration_errors,
                    result.provider_auth_errors,
                    result.state_write_failures
                ),
                (0, 0, 0)
            );
            match mode {
                DownloadRunMode::PrintFilenames => {
                    assert_eq!(result.printed_filenames, vec![s.clone()])
                }
                DownloadRunMode::DryRun => {
                    assert_eq!(result.downloaded, 1, "{case}: no eligible task counted")
                }
                DownloadRunMode::Download => panic!("unexpected download control"),
            }
        }
        assert_eq!(
            f.session.changes_cursors.lock().unwrap().len(),
            cursors_before + 1,
            "{case}: actual source enumeration did not run"
        );
        let after = noncommit_choice_snapshot(&f.db);
        let changed: Vec<_> = before
            .iter()
            .zip(&after)
            .filter(|(a, b)| a != b)
            .map(|(a, b)| format!("{} BEFORE={:?} AFTER={:?}", a.0, a.1, b.1))
            .collect();
        let media = server.received_requests().await.unwrap();
        let mut after_files: Vec<_> = std::fs::read_dir(&config.directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let mut before_files = before_files;
        before_files.sort();
        after_files.sort();
        if !changed.is_empty()
            || media.len() != gets_before
            || s.exists()
            || before_files != after_files
        {
            let retained = f.dir.keep();
            panic!(
                "NONCOMMIT_WRITE case={case} changed={changed:?} MEDIA_GET_before={gets_before} after={} S_exists={} before_files={before_files:?} after_files={after_files:?} retained={}",
                media.len(),
                s.exists(),
                retained.display()
            );
        }
        assert_eq!(std::fs::read(&p).unwrap(), original);
        let metadata = std::fs::metadata(&p).unwrap();
        assert_eq!(
            (metadata.ino(), metadata.mtime(), metadata.mtime_nsec()),
            p_identity
        );
        eprintln!(
            "NONCOMMIT_PROVED case={case} eligible_nomination=1 actual_source_pages=1 intent_and_admission_unchanged=1 media_GET=0 siblings_absent=1 P_identity_and_scoped_proof_unchanged=1"
        );
        if collecting && mode == DownloadRunMode::DryRun {
            crate::download::orchestration::test_support::seed_complete_album_snapshot(
                f.db.as_ref(),
                "preview-album",
                "preview-album",
                &[],
            )
            .await;
            let passes = [super::destination_pass(&f, "preview-album"), f.pass.clone()];
            assert!(
                f.db.selected_album_containers_have_complete_snapshots(
                    "PrimarySync",
                    &["preview-album"]
                )
                .await
                .unwrap()
            );
            f.config.sync_mode = crate::download::SyncMode::Incremental {
                zone_sync_token: "noncommit-source".into(),
            };
            let plans_before = trace
                .events()
                .iter()
                .filter(|event| event.message() == Some("Incremental task planning phase complete"))
                .count();
            let pages_before = f.session.changes_cursors.lock().unwrap().len();
            let ranks_before = f.session.rank_calls.load(Ordering::SeqCst);
            let routed = crate::download::download_photos_with_sync(
                &reqwest::Client::new(),
                &passes,
                Arc::new(f.config.clone()),
                controls,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(routed.outcome, crate::download::DownloadOutcome::Success),
                "{routed:?}"
            );
            assert!(routed.sync_token.is_none());
            // The public cycle also composes targeted retry previews. The
            // collecting event below must still contain exactly one task.
            assert!(routed.stats.downloaded >= 1);
            assert_eq!(
                (
                    routed.stats.bytes_downloaded,
                    routed.stats.disk_bytes_written
                ),
                (0, 0)
            );
            assert!(!routed.full_enumeration_ran);
            assert_eq!(
                f.session.changes_cursors.lock().unwrap().len(),
                pages_before + 1
            );
            assert_eq!(
                trace
                    .events()
                    .iter()
                    .filter(|event| event.message()
                        == Some("Incremental task planning phase complete")
                        && event.field("planned_tasks") == Some("1"))
                    .count(),
                plans_before + 1
            );
            assert_eq!(noncommit_choice_snapshot(&f.db), before);
            assert_eq!(server.received_requests().await.unwrap().len(), gets_before);
            assert!(!s.exists());
            assert_eq!(std::fs::read(&p).unwrap(), original);
            let metadata = std::fs::metadata(&p).unwrap();
            assert_eq!(
                (metadata.ino(), metadata.mtime(), metadata.mtime_nsec()),
                p_identity
            );
            eprintln!(
                "COLLECTING_DRY_RUN_PUBLIC_ROUTE named_album_plus_unfiled=1 complete_snapshot=1 collected_tasks=1 aggregate_would_download={} retry_query_calls={} bytes=0 token=None source_pages=1 full_enumeration=0 protected_snapshot_unchanged=1",
                routed.stats.downloaded,
                f.session.rank_calls.load(Ordering::SeqCst) - ranks_before
            );

            let downloaded = crate::download::orchestration::incremental::download_photos_incremental_collecting(
                &reqwest::Client::new(), std::slice::from_ref(&f.pass), &Arc::new(f.config.clone()),
                "noncommit-source", super::controls(), CancellationToken::new(),
            ).await.unwrap();
            assert!(
                matches!(
                    downloaded.outcome,
                    crate::download::DownloadOutcome::Success
                ),
                "{downloaded:?}"
            );
            assert_eq!(downloaded.sync_token.as_deref(), Some("quiet-zone-cursor"));
            assert_eq!(downloaded.stats.downloaded, 1);
            assert_eq!(downloaded.stats.bytes_downloaded, edited.len() as u64);
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                gets_before + 1
            );
            assert_eq!(std::fs::read(&s).unwrap(), edited);
            assert_eq!(std::fs::read(&p).unwrap(), original);
            let conn =
                f.db.acquire_lock("real collecting download remains enabled")
                    .unwrap();
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM download_receipts WHERE version_size='adjusted'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1
            );
            assert_eq!(conn.query_row("SELECT count(*) FROM assets WHERE version_size='adjusted' AND status='downloaded' AND local_path=?1",[s.to_str().unwrap()],|r|r.get::<_,i64>(0)).unwrap(),1);
            eprintln!(
                "COLLECTING_DOWNLOAD_POSITIVE downloaded=1 media_GET=1 adjusted_receipt=1 S_published=1 token=Some"
            );
        }
    }
}

#[tokio::test]
async fn collecting_dry_run_empty_and_noop_withhold_tokens_but_download_keeps_them() {
    use crate::download::orchestration::models::{
        DownloadControls, DownloadReporting, DownloadRunMode,
    };
    use sha2::{Digest, Sha256};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let media = b"\xff\xd8\xff\xe0unchanged original";
    for empty in [true, false] {
        for mode in [
            DownloadRunMode::PrintFilenames,
            DownloadRunMode::DryRun,
            DownloadRunMode::Download,
        ] {
            let server = wiremock::MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/original"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(media))
                .expect(1)
                .mount(&server)
                .await;
            let mut f = Fixture::new().await;
            f.config.folder_structure = "none".into();
            {
                let mut records = f.session.records.lock().unwrap();
                records[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
                records[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/original",server.uri()),"size":media.len(),"fileChecksum":data_encoding::BASE64.encode(&Sha256::digest(media))});
            }
            let seeded = stream_collision_child(&f, "asset-m", false).await;
            assert_eq!((seeded.downloaded, seeded.state_write_failures), (1, 0));
            if !empty {
                *f.session.changes_records.lock().unwrap() =
                    f.session.records.lock().unwrap().clone();
            }
            let before = noncommit_choice_snapshot(&f.db);
            let (trace, _guard) = crate::test_helpers::TracingCapture::install();
            let result=crate::download::orchestration::incremental::download_photos_incremental_collecting(
                &reqwest::Client::new(),std::slice::from_ref(&f.pass),&Arc::new(f.config.clone()),
                "noncommit-source",DownloadControls::new(mode,DownloadReporting::hidden()),CancellationToken::new(),
            ).await.unwrap();
            assert!(
                matches!(result.outcome, crate::download::DownloadOutcome::Success),
                "empty={empty} mode={mode:?}: {result:?}"
            );
            assert_eq!(
                result.sync_token.as_deref(),
                mode.downloads_files().then_some("quiet-zone-cursor"),
                "empty={empty} mode={mode:?}: readonly checkpoint leak"
            );
            assert_eq!(
                (
                    result.stats.downloaded,
                    result.stats.bytes_downloaded,
                    result.stats.disk_bytes_written
                ),
                (0, 0, 0)
            );
            assert_eq!(
                (
                    result.stats.enumeration_errors,
                    result.stats.state_write_failures
                ),
                (0, 0)
            );
            assert_eq!(f.session.changes_cursors.lock().unwrap().len(), 1);
            let expected = if empty {
                "No new photos to download from incremental sync"
            } else {
                "All incremental assets already downloaded or filtered"
            };
            assert!(
                trace.contains_event(|event| event.message() == Some(expected)),
                "empty={empty} mode={mode:?}: wrong branch"
            );
            if !empty {
                assert_eq!(result.stats.skipped.by_state, 1);
            }
            let after = noncommit_choice_snapshot(&f.db);
            if mode.downloads_files() {
                // Real Download adoption can fill its missing preserved-path
                // key. Readonly modes must preserve even that field exactly.
                assert_eq!(
                    after
                        .iter()
                        .filter(|(table, _)| table != "asset_metadata_paths")
                        .collect::<Vec<_>>(),
                    before
                        .iter()
                        .filter(|(table, _)| table != "asset_metadata_paths")
                        .collect::<Vec<_>>()
                );
            } else {
                assert_eq!(after, before);
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            assert_eq!(
                std::fs::read(f.config.directory.join("photo.JPG")).unwrap(),
                media
            );
            eprintln!(
                "COLLECTING_NOOP_TOKEN empty={empty} mode={mode:?} token={:?} source_pages=1 would_download=0 media_GET=0 snapshot_unchanged=1",
                result.sync_token
            );
        }
    }
}

async fn stream_collision_child(
    f: &Fixture,
    child: &str,
    selected: bool,
) -> crate::download::pipeline::StreamingResult {
    use crate::download::orchestration::generation;
    use crate::download::pipeline::{StreamRuntime, stream_and_download_from_stream};
    let mut config = f.config.with_pass(&f.pass);
    let mut asset = f
        .pass
        .album
        .confirm_catalog_asset(child)
        .await
        .unwrap()
        .asset;
    if selected {
        let context = generation::context_for_format(
            std::slice::from_ref(&f.pass),
            &f.config,
            super::controls(),
            2,
        )
        .await
        .unwrap()
        .unwrap();
        config.selection_run = Some(
            context
                .start_for_test(std::slice::from_ref(&f.pass), &f.config)
                .await
                .unwrap(),
        );
        config.selection_pass = Some(generation::pass_key(&f.pass));
        let run = config.selection_run.as_ref().unwrap();
        let key = config.selection_pass.as_deref().unwrap();
        let (current, confirmation) = run.confirm(key, asset).await.unwrap();
        asset = current;
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        if child == "asset-b" {
            assert_eq!(plan.tasks.len(), 1);
            assert!(plan.tasks[0].superseded_requested_path.is_none());
        }
        eprintln!(
            "COLLISION_PLAN child={child} tasks={:?} reservations={:?}",
            plan.tasks
                .iter()
                .map(|t| (&t.download_path, &t.superseded_requested_path))
                .collect::<Vec<_>>(),
            planner.reconciliation_reservations().collect::<Vec<_>>()
        );
        let admitted = run
            .project(
                key,
                &asset,
                confirmation,
                &mut plan,
                &mut planner,
                &config,
                &tokio_util::sync::CancellationToken::new(),
            )
            .await;
        assert!(
            admitted.as_ref().is_ok_and(|v| *v),
            "{child} actual projection: {admitted:?}; plan={plan:?}"
        );
        if child == "asset-b" {
            assert_eq!(
                f.db.acquire_lock("ordinary B keeps post-Missing bookmark timing")
                    .unwrap()
                    .query_row(
                        "SELECT count(*) FROM reconciliation_paths WHERE id='asset-b'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
        }
    }
    stream_and_download_from_stream(
        &reqwest::Client::new(),
        futures_util::stream::iter([Ok(asset)]),
        &Arc::new(config),
        super::controls(),
        1,
        tokio_util::sync::CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap()
}

#[cfg(unix)]
async fn ordinary_collision_after_publication(bookmark: bool) {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::Ordering;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = wiremock::MockServer::start().await;
    let original = b"\xff\xd8\xff\xe0original child A";
    let edited = b"\xff\xd8\xff\xe0adjusted child A";
    let independent = b"\xff\xd8\xff\xe0independent child B";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    for (name, bytes) in [
        ("original", original.as_slice()),
        ("edited", edited.as_slice()),
        ("independent", independent.as_slice()),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
    }
    let mut f = Fixture::new().await;
    f.config.folder_structure = "none".into();
    f.config.edited = true;
    f.config.edited_naming = crate::types::EditedNaming::Primary;
    {
        let mut values = f.session.records.lock().unwrap();
        values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
        values[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/original",server.uri()),"size":original.len(),"fileChecksum":checksum(original)});
    }
    let first = stream_collision_child(&f, "asset-m", false).await;
    assert_eq!(
        (first.downloaded, first.state_write_failures),
        (1, 0),
        "{first:?}"
    );
    let p = f.config.directory.join("photo.JPG");
    assert_eq!(std::fs::read(&p).unwrap(), original);
    if bookmark {
        {
            let mut values = f.session.records.lock().unwrap();
            values[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edited",server.uri()),"size":edited.len(),"fileChecksum":checksum(edited)}});
            values[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
            values[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
        }
        f.config.resolution = crate::types::PhotoResolution::None;
        let adjusted = stream_collision_child(&f, "asset-m", true).await;
        assert_eq!(
            (adjusted.downloaded, adjusted.state_write_failures),
            (1, 0),
            "{adjusted:?}"
        );
    }
    let (a_paths, proofs, mapping) = {
        let conn = f.db.acquire_lock("snapshot A scoped publications").unwrap();
        let paths: Vec<String> = conn.prepare("SELECT destination_path FROM reconciliation_paths WHERE id='asset-m' AND version_size='adjusted'").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(paths.len(), usize::from(bookmark));
        let mut a_paths = vec![(p.clone(), original.as_slice())];
        if bookmark {
            a_paths.push((std::path::PathBuf::from(&paths[0]), edited.as_slice()));
        }
        let a_paths: Vec<_> = a_paths
            .into_iter()
            .map(|(path, bytes)| {
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                let m = std::fs::metadata(&path).unwrap();
                (path, bytes.to_vec(), (m.ino(), m.mtime(), m.mtime_nsec()))
            })
            .collect();
        let proofs: Vec<String> = conn.prepare("SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-m' ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(proofs.len(), 1 + usize::from(bookmark));
        let mapping: Vec<String> = conn.prepare("SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths ORDER BY id,version_size").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(mapping.len(), usize::from(bookmark));
        (a_paths, proofs, mapping)
    };
    let old_state = Arc::downgrade(&f.db);
    f = super::reopen_streaming_fixture(f).await;
    assert!(old_state.upgrade().is_none());
    let mut b = records("b", &checksum(independent), "independent child");
    b[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
    b[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/independent",server.uri()),"size":independent.len(),"fileChecksum":checksum(independent)});
    f.session.records.lock().unwrap().extend(b);
    f.config.resolution = crate::types::PhotoResolution::Original;
    // Exercise the generic new-collision producer, not Primary's managed path policy.
    f.config.edited_naming = crate::types::EditedNaming::Suffix;
    assert_eq!(f.db.acquire_lock("no B history before admission").unwrap().query_row("SELECT (SELECT count(*) FROM assets WHERE id='asset-b')+(SELECT count(*) FROM download_receipts WHERE child='asset-b')+(SELECT count(*) FROM reconciliation_paths WHERE id='asset-b')+(SELECT count(*) FROM provider_active_destinations WHERE child='asset-b')", [], |r|r.get::<_,i64>(0)).unwrap(), 0);
    let next = stream_collision_child(&f, "asset-b", true).await;
    eprintln!(
        "ORDINARY_COLLISION bookmark={bookmark} downloaded={} state_failures={}",
        next.downloaded, next.state_write_failures
    );
    assert_eq!(next.state_write_failures, 0, "{next:?}");
    assert_eq!(next.downloaded, 1, "{next:?}");
    assert!(next.failed.is_empty());
    let b_path: String = f.db.acquire_lock("B independent publication").unwrap().query_row("SELECT local_path FROM assets WHERE id='asset-b' AND version_size='original' AND status='downloaded'", [], |r|r.get(0)).unwrap();
    let b_path = std::path::PathBuf::from(b_path);
    assert!(a_paths.iter().all(|(path, _, _)| path != &b_path));
    assert_eq!(std::fs::read(&b_path).unwrap(), independent);
    let gets = server.received_requests().await.unwrap();
    assert_eq!(gets.len(), 2 + usize::from(bookmark));
    assert_eq!(
        gets.iter()
            .filter(|r| r.url.path() == "/independent")
            .count(),
        1
    );
    let lookups = f.session.calls.load(Ordering::SeqCst);
    f = super::reopen_streaming_fixture(f).await;
    let quiet = crate::download::download_photos_with_sync(
        &reqwest::Client::new(),
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        super::controls(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(quiet.outcome, crate::download::DownloadOutcome::Success),
        "{quiet:?}"
    );
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(f.session.calls.load(Ordering::SeqCst), lookups);
    assert_eq!(server.received_requests().await.unwrap().len(), gets.len());
    let conn =
        f.db.acquire_lock("A remains unchanged after B and quiet reopen")
            .unwrap();
    let after: Vec<String> = conn.prepare("SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-m' ORDER BY id").unwrap()
        .query_map([], |r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(after, proofs);
    let after_mapping: Vec<String> = conn.prepare("SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths WHERE id='asset-m' ORDER BY id,version_size").unwrap()
        .query_map([], |r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(after_mapping, mapping);
    for (path, bytes, identity) in a_paths {
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let m = std::fs::metadata(path).unwrap();
        assert_eq!((m.ino(), m.mtime(), m.mtime_nsec()), identity);
    }
    assert_eq!(std::fs::read(b_path).unwrap(), independent);
    eprintln!(
        "ORDINARY_COLLISION_COMPLETE bookmark={bookmark} independent_GET=1 A_proofs={} quiet_downloaded=0",
        proofs.len()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ordinary_collision_with_unrelated_publication_bookmark_progresses() {
    ordinary_collision_after_publication(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn ordinary_collision_without_unrelated_bookmark_progresses() {
    ordinary_collision_after_publication(false).await;
}

#[tokio::test]
#[cfg(unix)]
async fn admitted_publication_choice_prefix_reopens_before_missing_and_dispatch() {
    use crate::download::orchestration::generation;
    use crate::download::pipeline::{StreamRuntime, stream_and_download_from_stream};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::Ordering;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let original = b"\xff\xd8\xff\xe0unchanged original";
    let edited = b"\xff\xd8\xff\xe0current adjusted";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    let server = wiremock::MockServer::start().await;
    for (name, bytes) in [
        ("original", original.as_slice()),
        ("edited", edited.as_slice()),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut f = Fixture::new().await;
    f.config.folder_structure = "none".into();
    f.config.edited = true;
    f.config.edited_naming = crate::types::EditedNaming::Primary;
    {
        let mut values = f.session.records.lock().unwrap();
        values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
        values[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/original",server.uri()),"size":original.len(),"fileChecksum":checksum(original)});
    }
    let client = reqwest::Client::new();
    {
        let asset = f
            .pass
            .album
            .confirm_catalog_asset("asset-m")
            .await
            .unwrap()
            .asset;
        let result = stream_and_download_from_stream(
            &client,
            futures_util::stream::iter([Ok(asset)]),
            &Arc::new(f.config.with_pass(&f.pass)),
            super::controls(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(result.downloaded, 1);
        assert!(result.failed.is_empty());
        assert_eq!(result.state_write_failures, 0);
    }
    let p = f.config.directory.join("photo.JPG");
    assert_eq!(std::fs::read(&p).unwrap(), original);
    let before = std::fs::metadata(&p).unwrap();
    let proof_sql = "SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-m' AND version_size='original'";
    let proof: String =
        f.db.acquire_lock("prefix original proof")
            .unwrap()
            .query_row(proof_sql, [], |r| r.get(0))
            .unwrap();
    {
        let mut values = f.session.records.lock().unwrap();
        values[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edited",server.uri()),"size":edited.len(),"fileChecksum":checksum(edited)}});
        values[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
        values[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    }
    f.config.resolution = crate::types::PhotoResolution::None;
    let map_sql = "SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths WHERE version_size='adjusted'";
    let (s, mapping, root, pass, binding) = {
        let context = generation::context_for_format(
            std::slice::from_ref(&f.pass),
            &f.config,
            super::controls(),
            2,
        )
        .await
        .unwrap()
        .unwrap();
        let run = context
            .start_for_test(std::slice::from_ref(&f.pass), &f.config)
            .await
            .unwrap();
        assert_eq!(run.root.spec.format, 2);
        let key = generation::pass_key(&f.pass);
        let observed = f
            .pass
            .album
            .confirm_catalog_asset("asset-m")
            .await
            .unwrap()
            .asset;
        let (asset, confirmation) = run.confirm(&key, observed).await.unwrap();
        let mut config = f.config.with_pass(&f.pass);
        config.selection_run = Some(run.clone());
        config.selection_pass = Some(key.clone());
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(
            f.db.acquire_lock("no seeded prefix intent")
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM reconciliation_paths WHERE version_size='adjusted'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert!(
            run.project(
                &key,
                &asset,
                confirmation,
                &mut plan,
                &mut planner,
                &config,
                &CancellationToken::new()
            )
            .await
            .unwrap()
        );
        // Inspect the returned production-owner prefix before caller matching/dispatch.
        let s = plan.tasks[0].download_path.clone();
        assert_ne!(s, p);
        assert!(!s.exists());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let conn = rusqlite::Connection::open_with_flags(
            f.dir.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mapping: String = conn.query_row(map_sql, [], |r| r.get(0)).unwrap();
        assert_eq!(
            mapping,
            json!([
                "PrimarySync",
                "asset-m",
                "adjusted",
                crate::fs_util::confined_path_key(&p).unwrap(),
                crate::fs_util::confined_path_key(&s).unwrap(),
                s.to_str().unwrap(),
                checksum(edited),
                edited.len()
            ])
            .to_string()
        );
        let rows: Vec<String> = conn.prepare("SELECT json_array(d.generation,d.pass_key,d.child,d.library,d.asset_id,d.master,d.version_size,d.path,d.checksum,d.size_bytes,d.verified_media,g.scope,g.account_key,g.provider_key) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.version_size='adjusted' AND d.admitted=1").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(rows.len(), 1);
        let row: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
        let (account, provider) = f.capture.owner.authenticated_keys().unwrap();
        assert_eq!(
            row,
            json!([
                run.root.id,
                key,
                "asset-m",
                "PrimarySync",
                "asset-m",
                "m",
                "adjusted",
                SelectionPath::from_path(&s).key().unwrap(),
                checksum(edited),
                edited.len(),
                0,
                run.root.spec.scope,
                account,
                provider
            ])
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM download_receipts WHERE version_size='adjusted'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row(proof_sql, [], |r| r.get::<_, String>(0))
                .unwrap(),
            proof
        );
        assert_eq!(std::fs::read(&p).unwrap(), original);
        let prefix_metadata = std::fs::metadata(&p).unwrap();
        assert_eq!(
            (before.ino(), before.mtime(), before.mtime_nsec()),
            (
                prefix_metadata.ino(),
                prefix_metadata.mtime(),
                prefix_metadata.mtime_nsec()
            )
        );
        eprintln!(
            "ADMITTED_PREFIX root={} pass={key} scope={} resource={} size={} P={} S={} admitted=1 media=0 edited_GET=0",
            run.root.id,
            run.root.spec.scope,
            checksum(edited),
            edited.len(),
            p.display(),
            s.display()
        );
        (
            s,
            mapping,
            run.root.id.clone(),
            key,
            plan.tasks[0]
                .publication_binding
                .as_deref()
                .unwrap()
                .clone(),
        )
    };
    let old_state = Arc::downgrade(&f.db);
    f = super::reopen_streaming_fixture(f).await;
    assert!(
        old_state.upgrade().is_none(),
        "old planner/run/source/state handle survived reopen"
    );
    {
        let asset = f
            .pass
            .album
            .confirm_catalog_asset("asset-m")
            .await
            .unwrap()
            .asset;
        let config = f.config.with_pass(&f.pass);
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].download_path, s);
        assert!(plan.tasks[0].superseded_requested_path.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let result = stream_and_download_from_stream(
            &client,
            futures_util::stream::iter([Ok(asset)]),
            &Arc::new(config),
            super::controls(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(result.downloaded, 1);
        assert!(result.failed.is_empty());
        assert_eq!(result.state_write_failures, 0);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(std::fs::read(&s).unwrap(), edited);
    let receipts =
        f.db.bound_download_receipts(
            binding,
            "adjusted".into(),
            checksum(edited),
            edited.len() as u64,
        )
        .await
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].native_path.to_path(), s);
    assert!(receipts[0].fulfills_pass(&pass));
    assert_eq!(
        receipts[0].source_checksum,
        data_encoding::HEXLOWER.encode(&Sha256::digest(edited))
    );
    assert!(
        f.db.selection_destination_verified(
            owner(),
            root,
            pass,
            "asset-m".into(),
            "adjusted".into()
        )
        .await
        .unwrap()
    );
    let lookups = f.session.calls.load(Ordering::SeqCst);
    let quiet = crate::download::download_photos_with_sync(
        &client,
        std::slice::from_ref(&f.pass),
        Arc::new(f.config.clone()),
        super::controls(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(quiet.outcome, crate::download::DownloadOutcome::Success),
        "{quiet:?}"
    );
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(f.session.calls.load(Ordering::SeqCst), lookups);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    let conn = f.db.acquire_lock("final admitted prefix proof").unwrap();
    assert_eq!(
        conn.query_row(map_sql, [], |r| r.get::<_, String>(0))
            .unwrap(),
        mapping
    );
    assert_eq!(
        conn.query_row(proof_sql, [], |r| r.get::<_, String>(0))
            .unwrap(),
        proof
    );
    assert_eq!(conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE version_size='adjusted' AND (verified_media=0 OR path<>?1)", [SelectionPath::from_path(&s).key().unwrap()], |r| r.get::<_,i64>(0)).unwrap(), 0);
    assert_eq!(std::fs::read(&p).unwrap(), original);
    let after = std::fs::metadata(&p).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec())
    );
}

#[tokio::test]
async fn stale_prior_publication_nomination_cannot_fall_back_to_ordinary() {
    use crate::download::orchestration::generation;
    use sha2::{Digest, Sha256};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let original = b"\xff\xd8\xff\xe0unchanged original";
    let edited = b"\xff\xd8\xff\xe0current adjusted";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    for case in ["positive", "binding", "history", "history_and_marker"] {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/original"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(original))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/edited"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(edited))
            .expect(0)
            .mount(&server)
            .await;
        let mut f = Fixture::new().await;
        f.config.folder_structure = "none".into();
        f.config.edited = true;
        f.config.edited_naming = crate::types::EditedNaming::Primary;
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
            values[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/original",server.uri()),"size":original.len(),"fileChecksum":checksum(original)});
        }
        let first = stream_collision_child(&f, "asset-m", false).await;
        assert_eq!((first.downloaded, first.state_write_failures), (1, 0));
        {
            let mut values = f.session.records.lock().unwrap();
            values[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edited",server.uri()),"size":edited.len(),"fileChecksum":checksum(edited)}});
            values[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
            values[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
        }
        f.config.resolution = crate::types::PhotoResolution::None;
        let context = generation::context_for_format(
            std::slice::from_ref(&f.pass),
            &f.config,
            super::controls(),
            2,
        )
        .await
        .unwrap()
        .unwrap();
        let run = context
            .start_for_test(std::slice::from_ref(&f.pass), &f.config)
            .await
            .unwrap();
        let key = generation::pass_key(&f.pass);
        let observed = f
            .pass
            .album
            .confirm_catalog_asset("asset-m")
            .await
            .unwrap()
            .asset;
        let (asset, confirmation) = run.confirm(&key, observed).await.unwrap();
        let mut config = f.config.with_pass(&f.pass);
        config.selection_run = Some(run.clone());
        config.selection_pass = Some(key.clone());
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        let p = f.config.directory.join("photo.JPG");
        assert_eq!(plan.tasks.len(), 1);
        let s = plan.tasks[0].download_path.clone();
        assert_ne!(p, s);
        assert!(
            plan.tasks[0]
                .superseded_requested(&config.directory, &p)
                .is_some()
        );
        assert_eq!(
            planner.reconciliation_reservations().count(),
            1,
            "positive producer did not nominate prior publication"
        );
        let count_sql = "SELECT (SELECT count(*) FROM reconciliation_paths),(SELECT count(*) FROM provider_active_destinations WHERE admitted=1)";
        assert_eq!(
            f.db.acquire_lock("before stale nomination")
                .unwrap()
                .query_row(count_sql, [], |r| Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?
                )))
                .unwrap(),
            (0, 0)
        );
        match case {
            "positive" => {}
            "binding" => plan.tasks[0].publication_binding = None,
            "history" | "history_and_marker" => {
                f.db.acquire_lock("invalidate prior evidence after nomination")
                    .unwrap()
                    .execute("DELETE FROM download_receipts", [])
                    .unwrap();
                if case == "history_and_marker" {
                    plan.tasks[0].superseded_requested_path = None;
                }
            }
            _ => panic!("unexpected stale nomination case {case}"),
        }
        let result = planner
            .persist_execution_publication_choices(&asset, &config, &plan.tasks)
            .await;
        if case == "positive" {
            assert!(result.unwrap());
            assert!(
                run.project(
                    &key,
                    &asset,
                    confirmation,
                    &mut plan,
                    &mut planner,
                    &config,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
            );
            assert_eq!(
                f.db.acquire_lock("valid prior progress")
                    .unwrap()
                    .query_row(count_sql, [], |r| Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?
                    )))
                    .unwrap(),
                (1, 1)
            );
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                match case {
                    "binding" => error == "Choice lost publication binding",
                    "history" =>
                        error == crate::state::error::StateError::ProviderWorkConflict.to_string(),
                    "history_and_marker" => error == "Choice lost authenticated historical source",
                    _ => false,
                },
                "{case}: {error}"
            );
            // Deliberately attempt the ordinary writer after rejection. It must
            // neither drain provenance nor rewrite the final task's destination.
            let mut fallback_tasks = plan.tasks.clone();
            fallback_tasks[0].download_path = s.with_file_name("unapproved-final-rewrite.JPG");
            let fallback = planner
                .persist_download_reservations(f.db.as_ref(), &fallback_tasks)
                .await
                .unwrap_err();
            assert_eq!(
                fallback.to_string(),
                "Unconfirmed prior publication cannot use ordinary reservation writer"
            );
            assert!(
                run.project(
                    &key,
                    &asset,
                    confirmation,
                    &mut plan,
                    &mut planner,
                    &config,
                    &CancellationToken::new()
                )
                .await
                .is_err(),
                "{case} reached admission"
            );
            assert_eq!(
                f.db.acquire_lock("stale prior has no intent or admission")
                    .unwrap()
                    .query_row(count_sql, [], |r| Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?
                    )))
                    .unwrap(),
                (0, 0)
            );
            eprintln!(
                "STALE_PRIOR case={case} refused={error} ordinary_fallback=refused intent=0 admitted=0 edited_GET=0"
            );
        }
        assert!(!s.exists());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        assert_eq!(std::fs::read(p).unwrap(), original);
    }
}

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
        if matches!(case, "missing" | "conflict") {
            assert!(
                result.is_err(),
                "{case} silently downgraded replay: {result:?}"
            );
            eprintln!("EXACT_REPLAY case={case} refused_exemption={result:?}");
        }
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

// Both tests need a genuine cross-rendition nomination, not a seeded reservation.
struct IdentityRaceNomination {
    f: Fixture,
    config: crate::download::DownloadConfig,
    asset: crate::icloud::photos::PhotoAsset,
    confirmation: crate::download::orchestration::generation::SelectionConfirmation,
    planner: TaskPlanner,
    plan: crate::download::planner::AssetTaskPlan,
}

async fn identity_race_nomination(server: &wiremock::MockServer) -> IdentityRaceNomination {
    use crate::download::orchestration::generation;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let original = b"\xff\xd8\xff\xe0unchanged original";
    let adjusted = b"\xff\xd8\xff\xe0current adjusted";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    for (name, bytes, expected) in [
        ("original", original.as_slice(), 1),
        ("adjusted", adjusted.as_slice(), 0),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .expect(expected)
            .mount(server)
            .await;
    }
    let mut dir = tempfile::tempdir().unwrap();
    if std::env::var_os("KEI_IDENTITY_RACE_RETAIN_FIXTURE").is_some() {
        dir.disable_cleanup(true);
        eprintln!("IDENTITY_RACE_FIXTURE path={}", dir.path().display());
    }
    let mut f = Fixture::new_in(dir, "state.db").await;
    f.config.folder_structure = "none".into();
    f.config.edited = true;
    f.config.edited_naming = crate::types::EditedNaming::Primary;
    {
        let mut values = f.session.records.lock().unwrap();
        values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
        values[0]["fields"]["resOriginalRes"]["value"] = json!({
            "downloadURL":format!("{}/original", server.uri()),
            "size":original.len(), "fileChecksum":checksum(original)
        });
    }
    let first = stream_collision_child(&f, "asset-m", false).await;
    assert_eq!((first.downloaded, first.state_write_failures), (1, 0));
    assert!(first.failed.is_empty());
    assert_eq!(
        std::fs::read(f.config.directory.join("photo.JPG")).unwrap(),
        original
    );
    {
        let mut values = f.session.records.lock().unwrap();
        values[1]["fields"]["resJPEGFullRes"] = json!({"value":{
            "downloadURL":format!("{}/adjusted", server.uri()),
            "size":adjusted.len(), "fileChecksum":checksum(adjusted)
        }});
        values[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
        values[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    }
    f.config.resolution = crate::types::PhotoResolution::None;
    let context = generation::context_for_format(
        std::slice::from_ref(&f.pass),
        &f.config,
        super::controls(),
        2,
    )
    .await
    .unwrap()
    .unwrap();
    let run = context
        .start_for_test(std::slice::from_ref(&f.pass), &f.config)
        .await
        .unwrap();
    let key = generation::pass_key(&f.pass);
    let observed = f
        .pass
        .album
        .confirm_catalog_asset("asset-m")
        .await
        .unwrap()
        .asset;
    let (asset, confirmation) = run.confirm(&key, observed).await.unwrap();
    let mut config = f.config.with_pass(&f.pass);
    config.selection_run = Some(run.clone());
    config.selection_pass = Some(key.clone());
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
    let plan = planner.plan_download_asset(&asset, &config).await.unwrap();
    assert_eq!(plan.tasks.len(), 1);
    let task = &plan.tasks[0];
    let p = config.directory.join("photo.JPG");
    assert_eq!(task.version_size, crate::state::VersionSizeKey::Adjusted);
    assert_eq!(task.checksum.as_ref(), checksum(adjusted));
    assert_eq!(task.size, adjusted.len() as u64);
    assert_ne!(task.download_path, p);
    assert!(!task.download_path.exists());
    assert!(task.superseded_requested(&config.directory, &p).is_some());
    let binding = task.publication_binding.as_deref().unwrap();
    let (account, provider) = f.capture.owner.authenticated_keys().unwrap();
    assert_eq!(
        json!([
            binding.account_key,
            binding.provider_key,
            binding.scope,
            binding.library,
            binding.child,
            binding.master,
            binding.state_id,
            binding.origin_pass
        ]),
        json!([
            account,
            provider,
            run.root.spec.scope,
            "PrimarySync",
            "asset-m",
            "m",
            "asset-m",
            key
        ])
    );
    let choices: Vec<_> = planner.reconciliation_reservations().collect();
    assert_eq!(choices.len(), 1);
    let choice = choices[0];
    assert_eq!(choice.library.as_ref(), "PrimarySync");
    assert_eq!(choice.asset_id.as_ref(), "asset-m");
    assert_eq!(choice.version_size, task.version_size);
    assert_eq!(
        choice.requested_path_key.0,
        crate::fs_util::confined_path_key(&p).unwrap()
    );
    assert_eq!(
        choice.destination_path_key.0,
        crate::fs_util::confined_path_key(&task.download_path).unwrap()
    );
    assert_eq!(choice.destination_path, task.download_path);
    let content = choice.content.as_ref().unwrap();
    assert_eq!(
        (content.checksum.as_ref(), content.size),
        (task.checksum.as_ref(), task.size)
    );
    assert_eq!(run.root.spec.format, 2);
    assert_eq!(f.count("reconciliation_paths"), 0);
    assert_eq!(f.count("provider_active_destinations"), 0);
    assert!(
        f.db.bound_historical_downloads(
            binding.clone(),
            "adjusted".into(),
            task.checksum.to_string(),
            task.size
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    eprintln!(
        "IDENTITY_RACE_NOMINATION P={} S={} adjusted_history=0 intent=0 admission=0 original_GET=1 adjusted_GET=0",
        p.display(),
        task.download_path.display()
    );
    IdentityRaceNomination {
        f,
        config,
        asset,
        confirmation,
        planner,
        plan,
    }
}

// All-column snapshots retain proof hashes, nullable trust fields and immutable debt.
fn identity_race_rows(
    db: &SqliteStateDb,
    table: &str,
    column: &str,
    value: &str,
) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = db.acquire_lock("identity race immutable rows").unwrap();
    let mut stmt = conn
        .prepare(&format!(
            "SELECT * FROM {table} WHERE {column}=?1 ORDER BY 1"
        ))
        .unwrap();
    let columns = stmt.column_count();
    stmt.query_map([value], |row| {
        (0..columns)
            .map(|i| row.get(i))
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

#[tokio::test]
async fn publication_choice_after_preload_pending_owner_race_is_path_local() {
    use crate::state::error::StateError;
    use tokio_util::sync::CancellationToken;

    for conflict in [false, true] {
        let server = wiremock::MockServer::start().await;
        let IdentityRaceNomination {
            f,
            config,
            asset,
            confirmation,
            mut planner,
            mut plan,
        } = identity_race_nomination(&server).await;
        let run = config.selection_run.as_ref().unwrap();
        let key = config.selection_pass.as_ref().unwrap();
        let p = config.directory.join("photo.JPG");
        let s = plan.tasks[0].download_path.clone();
        let checksum = plan.tasks[0].checksum.to_string();
        let size = plan.tasks[0].size;
        let original = std::fs::read(&p).unwrap();
        let file = std::fs::File::open(&p).unwrap();
        let physical = (
            crate::fs_util::file_identity(&file).unwrap(),
            file.metadata().unwrap().modified().unwrap(),
        );
        drop(file);
        let proof = identity_race_rows(&f.db, "download_receipts", "child", "asset-m");
        assert_eq!(proof.len(), 1);
        let token = identity_race_rows(&f.db, "metadata", "key", "sync_token:PrimarySync");
        let unrelated = identity_race_rows(
            &f.db,
            "metadata",
            "key",
            "pending_sync_token:sparse:OtherZone",
        );
        assert_eq!((token.len(), unrelated.len()), (1, 1));
        // The independent identity appears only after A's actual planner preload.
        let leaf = if conflict {
            s.file_name().unwrap().to_str().unwrap()
        } else {
            "companion.JPG"
        };
        {
            let mut values = f.session.records.lock().unwrap();
            let adjusted = values[1]["fields"]["resJPEGFullRes"].clone();
            let mut b = records("b", CURRENT, "independent equal-resource adjusted child");
            b[0]["fields"]["filenameEnc"]["value"] = json!(leaf);
            b[1]["fields"]["resJPEGFullRes"] = adjusted;
            b[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
            b[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
            values.extend(b);
        }
        let confirmed = f.pass.album.confirm_catalog_asset("asset-b").await.unwrap();
        assert_eq!(confirmed.asset.id(), "b");
        assert_eq!(confirmed.asset.state_id(), "asset-b");
        let b_config = f.config.with_pass(&f.pass);
        let mut b_planner = TaskPlanner::for_download(b_config.state_db.as_deref())
            .await
            .unwrap();
        let b_plan = b_planner
            .plan_download_asset(&confirmed.asset, &b_config)
            .await
            .unwrap();
        assert_eq!(b_plan.tasks.len(), 1);
        let b_task = &b_plan.tasks[0];
        let b_path = b_task.download_path.clone();
        assert_eq!(
            b_path,
            config.directory.join(leaf),
            "real path owner did not derive B's target"
        );
        assert_eq!(b_path == s, conflict);
        assert_ne!(b_path, p);
        assert!(!b_path.exists());
        assert_eq!(
            (b_task.version_size, b_task.checksum.as_ref(), b_task.size),
            (
                plan.tasks[0].version_size,
                plan.tasks[0].checksum.as_ref(),
                plan.tasks[0].size
            )
        );
        let b_binding = b_task.publication_binding.as_deref().unwrap();
        let a_binding = plan.tasks[0].publication_binding.as_deref().unwrap();
        assert_eq!(
            json!([
                b_binding.account_key,
                b_binding.provider_key,
                b_binding.scope,
                b_binding.library,
                b_binding.child,
                b_binding.master,
                b_binding.state_id,
                b_binding.origin_pass
            ]),
            json!([
                a_binding.account_key,
                a_binding.provider_key,
                a_binding.scope,
                "PrimarySync",
                "asset-b",
                "b",
                "asset-b",
                key
            ])
        );
        let pending = pending_record_for_task(&b_config, &confirmed.asset, b_task);
        assert!(pending.local_path.is_none());
        let b_decision = ActiveDecision {
            state_id: confirmed.asset.state_id().into(),
            sources: Vec::new(),
            decision: SelectionDecision {
                pass_key: key.clone(),
                child: confirmed.asset.asset_record_name().into(),
                master: Some(confirmed.asset.id().into()),
                confirmation: Some(confirmed.body),
                outcome: SelectionOutcome::Selected,
                reason: String::new(),
                destinations: vec![SelectionDestination {
                    version_size: b_task.version_size.as_str().into(),
                    path: SelectionPath::from_path(&b_path),
                    checksum: b_task.checksum.to_string(),
                    size: b_task.size,
                    metadata_hash: pending.metadata.metadata_hash.clone().unwrap(),
                }],
            },
        };
        let database = f.dir.path().join("state.db");
        let external = SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap();
        assert!(
            external
                .project_selection_decision(
                    owner(),
                    run.root.id.clone(),
                    b_decision.clone(),
                    vec![pending],
                    MAX_GENERATION_BYTES
                )
                .await
                .unwrap()
                .admitted
        );
        assert_eq!(
            external
                .selection_decision(owner(), run.root.id.clone(), key.clone(), "asset-b".into())
                .await
                .unwrap(),
            Some(b_decision)
        );
        let debt = |db: &SqliteStateDb| {
            [
                identity_race_rows(db, "provider_active_destinations", "child", "asset-b"),
                identity_race_rows(db, "provider_active_decisions", "child", "asset-b"),
                identity_race_rows(db, "assets", "id", "asset-b"),
                identity_race_rows(db, "asset_master_mappings", "asset_record_name", "asset-b"),
                identity_race_rows(db, "asset_metadata_paths", "id", "asset-b"),
            ]
        };
        let saved_debt = debt(&external);
        assert_eq!(
            (
                saved_debt[0].len(),
                saved_debt[1].len(),
                saved_debt[2].len()
            ),
            (1, 1, 1)
        );
        {
            let conn = external
                .acquire_lock("real pending B binding and no publication")
                .unwrap();
            let row: String = conn.query_row("SELECT json_array(d.generation,d.pass_key,d.child,d.library,d.asset_id,d.master,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,d.verified_metadata,d.prepared_hash,d.prepared_checksum,d.source_checksum,d.local_checksum,g.scope,g.account_key,g.provider_key) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-b'", [], |r| r.get(0)).unwrap();
            eprintln!("IDENTITY_RACE_PENDING conflict={conflict} row={row}");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&row).unwrap(),
                json!([
                    run.root.id,
                    key,
                    "asset-b",
                    "PrimarySync",
                    "asset-b",
                    "b",
                    "adjusted",
                    SelectionPath::from_path(&b_path).key().unwrap(),
                    b_task.checksum,
                    b_task.size,
                    1,
                    0,
                    0,
                    null,
                    null,
                    null,
                    null,
                    run.root.spec.scope,
                    b_binding.account_key,
                    b_binding.provider_key
                ])
            );
            assert_eq!(conn.query_row("SELECT count(*) FROM assets WHERE id='asset-b' AND status='pending' AND local_path IS NULL", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
        }
        let preserved = |db: &SqliteStateDb| {
            assert_eq!(debt(db), saved_debt);
            assert_eq!(
                identity_race_rows(db, "download_receipts", "child", "asset-m"),
                proof
            );
            assert_eq!(
                identity_race_rows(db, "metadata", "key", "sync_token:PrimarySync"),
                token
            );
            assert_eq!(
                identity_race_rows(db, "metadata", "key", "pending_sync_token:sparse:OtherZone"),
                unrelated
            );
            assert!(identity_race_rows(db, "download_receipts", "child", "asset-b").is_empty());
            assert!(identity_race_rows(db, "reconciliation_paths", "id", "asset-b").is_empty());
            assert_eq!(
                db.acquire_lock("no prepared publication")
                    .unwrap()
                    .query_row(
                        "SELECT count(*) FROM owned_temp_files WHERE publication IS NOT NULL",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            assert_eq!(std::fs::read(&p).unwrap(), original);
            let file = std::fs::File::open(&p).unwrap();
            assert_eq!(
                (
                    crate::fs_util::file_identity(&file).unwrap(),
                    file.metadata().unwrap().modified().unwrap()
                ),
                physical
            );
            assert!(!s.exists());
            assert!(!b_path.exists());
        };
        let a_state = |db: &SqliteStateDb, admitted: bool| {
            let mappings = identity_race_rows(db, "reconciliation_paths", "id", "asset-m");
            let destinations =
                identity_race_rows(db, "provider_active_destinations", "child", "asset-m");
            assert_eq!(
                (mappings.len(), destinations.len()),
                (usize::from(!conflict), usize::from(admitted))
            );
            assert_eq!(
                identity_race_rows(db, "provider_active_decisions", "child", "asset-m").len(),
                usize::from(admitted)
            );
            let conn = db
                .acquire_lock("exact A helper and projection boundaries")
                .unwrap();
            if !conflict {
                let mapping: String = conn.query_row("SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths WHERE id='asset-m'", [], |r| r.get(0)).unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&mapping).unwrap(),
                    json!([
                        "PrimarySync",
                        "asset-m",
                        "adjusted",
                        crate::fs_util::confined_path_key(&p).unwrap(),
                        crate::fs_util::confined_path_key(&s).unwrap(),
                        s.to_str().unwrap(),
                        checksum,
                        size
                    ])
                );
            }
            if admitted {
                let row: String = conn.query_row("SELECT json_array(d.generation,d.pass_key,d.child,d.library,d.asset_id,d.master,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,g.scope,g.account_key,g.provider_key) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-m'", [], |r| r.get(0)).unwrap();
                let (account, provider) = f.capture.owner.authenticated_keys().unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&row).unwrap(),
                    json!([
                        run.root.id,
                        key,
                        "asset-m",
                        "PrimarySync",
                        "asset-m",
                        "m",
                        "adjusted",
                        SelectionPath::from_path(&s).key().unwrap(),
                        checksum,
                        size,
                        1,
                        0,
                        run.root.spec.scope,
                        account,
                        provider
                    ])
                );
            }
        };
        preserved(&external);
        assert!(identity_race_rows(&external, "reconciliation_paths", "id", "asset-m").is_empty());
        assert!(
            identity_race_rows(
                &external,
                "provider_active_destinations",
                "child",
                "asset-m"
            )
            .is_empty()
        );
        let result = planner
            .persist_execution_publication_choices(&asset, &config, &plan.tasks)
            .await;
        eprintln!("IDENTITY_RACE_HELPER conflict={conflict} result={result:?}");
        if conflict {
            assert!(matches!(
                result.unwrap_err().downcast_ref::<StateError>(),
                Some(StateError::ProviderWorkConflict)
            ));
            assert_eq!(planner.reconciliation_reservations().count(), 1);
        } else {
            assert!(result.unwrap());
        }
        preserved(&f.db);
        a_state(&f.db, false);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].download_path, s);
        assert!(
            plan.tasks[0]
                .superseded_requested(&config.directory, &p)
                .is_some()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let projected = run
            .project(
                key,
                &asset,
                confirmation,
                &mut plan,
                &mut planner,
                &config,
                &CancellationToken::new(),
            )
            .await;
        eprintln!("IDENTITY_RACE_PROJECT conflict={conflict} result={projected:?}");
        if conflict {
            assert!(matches!(
                projected.unwrap_err().downcast_ref::<StateError>(),
                Some(StateError::ProviderWorkConflict)
            ));
            assert_eq!(planner.reconciliation_reservations().count(), 1);
        } else {
            assert!(projected.unwrap());
        }
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].download_path, s);
        assert!(
            plan.tasks[0]
                .superseded_requested(&config.directory, &p)
                .is_some()
        );
        for db in [
            &*f.db,
            &SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        ] {
            preserved(db);
            a_state(db, !conflict);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        eprintln!(
            "IDENTITY_RACE_COMPLETE conflict={conflict} owned_reopen=true P_proof_and_file_unchanged=true B_debt_unchanged=true adjusted_GET=0"
        );
    }
}

#[tokio::test]
async fn publication_choice_zero_history_still_validates_binding() {
    use crate::state::db::account::AccountOwner;
    use crate::state::error::StateError;

    let server = wiremock::MockServer::start().await;
    let nomination = identity_race_nomination(&server).await;
    let task = &nomination.plan.tasks[0];
    let binding_a = task.publication_binding.as_deref().unwrap().clone();
    let reservation = nomination
        .planner
        .reconciliation_reservations()
        .next()
        .unwrap()
        .clone();
    let proof = identity_race_rows(&nomination.f.db, "download_receipts", "child", "asset-m");
    let owner_b = AccountOwner::authenticated(
        "identity-race-b@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"identity-race-b"}})).unwrap(),
    )
    .unwrap();
    let path = nomination.f.dir.path().join("account-b.db");
    let db_b = SqliteStateDb::open_owned(&path, &owner_b).await.unwrap();
    // bind_publication, not the fixture, derives B's actual account/provider keys.
    let binding_b = db_b
        .bind_publication(owner_b.clone(), binding_a.clone())
        .await
        .unwrap();
    assert_ne!(binding_a.account_key, binding_b.account_key);
    assert_ne!(binding_a.provider_key, binding_b.provider_key);
    assert_eq!(
        json!([
            binding_a.scope,
            binding_a.library,
            binding_a.child,
            binding_a.master,
            binding_a.state_id,
            binding_a.origin_pass
        ]),
        json!([
            binding_b.scope,
            binding_b.library,
            binding_b.child,
            binding_b.master,
            binding_b.state_id,
            binding_b.origin_pass
        ])
    );
    let own = db_b
        .bound_historical_downloads(
            binding_b.clone(),
            "adjusted".into(),
            task.checksum.to_string(),
            task.size,
        )
        .await
        .unwrap();
    assert!(own.is_empty());
    assert!(identity_race_rows(&db_b, "download_receipts", "child", "asset-m").is_empty());
    let foreign = db_b
        .bound_historical_downloads(
            binding_a.clone(),
            "adjusted".into(),
            task.checksum.to_string(),
            task.size,
        )
        .await
        .unwrap_err();
    eprintln!("IDENTITY_RACE_ZERO_HISTORY own=empty foreign_read={foreign:?}");
    assert!(matches!(
        foreign,
        StateError::Invariant {
            operation: "prepared_publication",
            ..
        }
    ));
    let requested = SelectionPath::from_path(&nomination.config.directory.join("photo.JPG"));
    let foreign_write = db_b
        .reserve_bound_publication_choices(binding_a, vec![(reservation, requested)])
        .await
        .unwrap_err();
    eprintln!("IDENTITY_RACE_ZERO_HISTORY foreign_write={foreign_write:?}");
    assert!(matches!(
        foreign_write,
        StateError::Invariant {
            operation: "prepared_publication",
            ..
        }
    ));
    for db in [
        &db_b,
        &SqliteStateDb::open_owned(&path, &owner_b).await.unwrap(),
    ] {
        assert_eq!(
            db.acquire_lock("zero foreign intent inserts")
                .unwrap()
                .query_row("SELECT count(*) FROM reconciliation_paths", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(identity_race_rows(db, "reconciliation_paths", "id", "asset-m").is_empty());
        assert!(identity_race_rows(db, "download_receipts", "child", "asset-m").is_empty());
        assert!(
            db.bound_historical_downloads(
                binding_b.clone(),
                "adjusted".into(),
                task.checksum.to_string(),
                task.size
            )
            .await
            .unwrap()
            .is_empty()
        );
    }
    assert_eq!(
        identity_race_rows(&nomination.f.db, "download_receipts", "child", "asset-m"),
        proof
    );
    assert_eq!(nomination.f.count("reconciliation_paths"), 0);
    assert_eq!(nomination.f.count("provider_active_destinations"), 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    eprintln!(
        "IDENTITY_RACE_ZERO_HISTORY_COMPLETE owned_reopen=true own=empty foreign_intent=0 A_proof_unchanged=true adjusted_GET=0"
    );
    // Static immutable-principal boundary only; no concurrent account rebind is attempted.
}

#[tokio::test]
async fn verified_same_pass_receipt_supersedes_new_publication_choice() {
    use crate::download::orchestration::generation;
    use crate::state::VersionSizeKey;
    use sha2::{Digest, Sha256};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let old = b"\xff\xd8\xff\xe0historical original";
    let current = b"\xff\xd8\xff\xe0replacement original";
    let companion = b"\xff\xd8\xff\xe0disjoint pending child";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    let local = |bytes: &[u8]| data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
    let identity = |path: &std::path::Path| {
        let file = std::fs::File::open(path).unwrap();
        let metadata = file.metadata().unwrap();
        (
            crate::fs_util::file_identity(&file).unwrap(),
            metadata.len(),
            metadata.modified().unwrap(),
        )
    };
    // Prove both historical prefixes before testing independent R publication.
    for (publish_r, pending_q, new_child) in [
        (false, false, false),
        (false, true, false),
        (false, false, true),
        (false, true, true),
        (true, false, false),
        (true, true, false),
    ] {
        let server = wiremock::MockServer::start().await;
        for (name, bytes, expected) in [
            ("old", old.as_slice(), 1),
            (
                "current",
                current.as_slice(),
                u64::from(publish_r || new_child),
            ),
            ("companion", companion.as_slice(), 0),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{name}")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
                .expect(expected)
                .mount(&server)
                .await;
        }
        let mut f = Fixture::new().await;
        if std::env::var_os("KEI_FINAL_R_RETAIN_FIXTURE").is_some() {
            f.dir.disable_cleanup(true);
        }
        eprintln!(
            "FINAL_R_CASE publish_r={publish_r} pending_q={pending_q} new_child={new_child} fixture={}",
            f.dir.path().display()
        );
        f.config.folder_structure = "none".into();
        f.config.resolution = crate::types::PhotoResolution::Original;
        f.config.edited = false;
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
            values[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/old", server.uri()),
                "size":old.len(), "fileChecksum":checksum(old)
            });
        }
        let first = stream_collision_child(&f, "asset-m", false).await;
        assert_eq!((first.downloaded, first.state_write_failures), (1, 0));
        assert!(first.failed.is_empty());
        let p = f.config.directory.join("photo.JPG");
        let r = f.config.directory.join("verified.JPG");
        let q = f.config.directory.join("companion.JPG");
        assert_eq!(std::fs::read(&p).unwrap(), old);
        let p_identity = identity(&p);
        // Snapshot every column, including immutable proof and nullable trust fields.
        let rows = |table: &str, column: &str, child: &str| {
            let conn = f.db.acquire_lock("final R complete row snapshot").unwrap();
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT * FROM {table} WHERE {column}=?1 ORDER BY 1"
                ))
                .unwrap();
            let columns = stmt.column_count();
            stmt.query_map([child], |row| {
                (0..columns)
                    .map(|i| row.get::<_, rusqlite::types::Value>(i))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        let p_receipts = rows("download_receipts", "child", "asset-m");
        assert_eq!(p_receipts.len(), 1);
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/current", server.uri()),
                "size":current.len(), "fileChecksum":checksum(current)
            });
            let mut other = records("q", &checksum(companion), "disjoint unfinished child");
            other[0]["fields"]["filenameEnc"]["value"] = json!("companion.JPG");
            other[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/companion", server.uri()),
                "size":companion.len(), "fileChecksum":checksum(companion)
            });
            values.extend(other);
        }
        assert_ne!(checksum(old), checksum(current));
        let context = generation::context_for_format(
            std::slice::from_ref(&f.pass),
            &f.config,
            super::controls(),
            2,
        )
        .await
        .unwrap()
        .unwrap();
        let run = context
            .start_for_test(std::slice::from_ref(&f.pass), &f.config)
            .await
            .unwrap();
        let key = generation::pass_key(&f.pass);
        let mut config = f.config.with_pass(&f.pass);
        config.selection_run = Some(run.clone());
        config.selection_pass = Some(key.clone());
        assert_eq!(run.root.spec.format, 2);
        if pending_q {
            let observed = f
                .pass
                .album
                .confirm_catalog_asset("asset-q")
                .await
                .unwrap()
                .asset;
            let (asset, confirmation) = run.confirm(&key, observed).await.unwrap();
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            assert_eq!(plan.tasks.len(), 1);
            assert_eq!(plan.tasks[0].download_path, q);
            assert!(
                run.project(
                    &key,
                    &asset,
                    confirmation,
                    &mut plan,
                    &mut planner,
                    &config,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
            );
        }
        let admitted_sql = "SELECT json_array(d.generation,d.pass_key,d.child,d.library,d.asset_id,d.master,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,g.scope,g.account_key,g.provider_key) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.generation=?1 AND d.pass_key=?2 AND d.child=?3";
        let admitted = |child: &str| -> serde_json::Value {
            let text: String =
                f.db.acquire_lock("final R exact admission")
                    .unwrap()
                    .query_row(
                        admitted_sql,
                        rusqlite::params![run.root.id, key, child],
                        |row| row.get(0),
                    )
                    .unwrap();
            serde_json::from_str(&text).unwrap()
        };
        let (account, provider) = f.capture.owner.authenticated_keys().unwrap();
        if pending_q {
            assert_eq!(
                admitted("asset-q"),
                json!([
                    run.root.id,
                    key,
                    "asset-q",
                    "PrimarySync",
                    "asset-q",
                    "q",
                    "original",
                    SelectionPath::from_path(&q).key().unwrap(),
                    checksum(companion),
                    companion.len(),
                    1,
                    0,
                    run.root.spec.scope,
                    account,
                    provider
                ])
            );
        }
        let q_destinations = rows("provider_active_destinations", "child", "asset-q");
        let q_decisions = rows("provider_active_decisions", "child", "asset-q");
        let q_queue = rows("assets", "id", "asset-q");
        assert_eq!(
            (q_destinations.len(), q_decisions.len(), q_queue.len()),
            (
                usize::from(pending_q),
                usize::from(pending_q),
                usize::from(pending_q)
            )
        );
        assert!(!q.exists());
        if new_child {
            let mut b = records("b", &checksum(current), "new child without history");
            b[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
            b[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/current", server.uri()),
                "size":current.len(), "fileChecksum":checksum(current)
            });
            f.session.records.lock().unwrap().extend(b);
            for table in ["download_receipts", "provider_active_destinations"] {
                assert!(rows(table, "child", "asset-b").is_empty());
            }
            assert!(rows("reconciliation_paths", "id", "asset-b").is_empty());
            let observed = f
                .pass
                .album
                .confirm_catalog_asset("asset-b")
                .await
                .unwrap()
                .asset;
            let (b, confirmation) = run.confirm(&key, observed).await.unwrap();
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let mut plan = planner.plan_download_asset(&b, &config).await.unwrap();
            assert_eq!(plan.tasks.len(), 1);
            assert_ne!(plan.tasks[0].download_path, p);
            assert!(plan.tasks[0].superseded_requested_path.is_none());
            let binding = plan.tasks[0].publication_binding.as_deref().unwrap();
            assert_eq!(binding.child, "asset-b");
            assert_eq!(binding.master, "b");
            assert!(
                f.db.bound_historical_downloads(
                    binding.clone(),
                    "original".into(),
                    checksum(current),
                    current.len() as u64
                )
                .await
                .unwrap()
                .is_empty()
            );
            f.db.acquire_lock("new B has no early bookmark")
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER no_early_b BEFORE INSERT ON provider_active_destinations
                 WHEN NEW.child='asset-b' BEGIN SELECT CASE WHEN EXISTS(
                 SELECT 1 FROM reconciliation_paths WHERE id='asset-b')
                 THEN RAISE(ABORT,'new B gained early intent') END; END;",
                )
                .unwrap();
            assert!(
                run.project(
                    &key,
                    &b,
                    confirmation,
                    &mut plan,
                    &mut planner,
                    &config,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
            );
            assert!(rows("reconciliation_paths", "id", "asset-b").is_empty());
            assert!(rows("download_receipts", "child", "asset-b").is_empty());
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            f.db.acquire_lock("release B prefix observer")
                .unwrap()
                .execute_batch("DROP TRIGGER no_early_b")
                .unwrap();
            let result = crate::download::pipeline::stream_and_download_from_stream(
                &reqwest::Client::new(),
                futures_util::stream::iter([Ok(b)]),
                &Arc::new(config.clone()),
                super::controls(),
                1,
                CancellationToken::new(),
                crate::download::pipeline::StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(
                (result.downloaded, result.state_write_failures),
                (1, 0),
                "{result:?}"
            );
            assert!(result.failed.is_empty());
            let receipts =
                f.db.acquire_lock("B completed path")
                    .unwrap()
                    .query_row(
                        "SELECT local_path FROM assets WHERE id='asset-b' AND status='downloaded'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap();
            assert_eq!(std::fs::read(receipts).unwrap(), current);
            assert_eq!(rows("download_receipts", "child", "asset-b").len(), 1);
            assert_eq!(rows("download_receipts", "child", "asset-m"), p_receipts);
            assert_eq!(std::fs::read(&p).unwrap(), old);
            assert_eq!(identity(&p), p_identity);
            assert_eq!(
                rows("provider_active_destinations", "child", "asset-q"),
                q_destinations
            );
            assert_eq!(
                rows("provider_active_decisions", "child", "asset-q"),
                q_decisions
            );
            assert_eq!(rows("assets", "id", "asset-q"), q_queue);
            assert!(!q.exists());
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
            eprintln!(
                "ORIGIN_MATRIX_PASS new_child=true pending_q={pending_q} no-early-intent=true published=true P-Q-unchanged=true"
            );
            continue;
        }
        let observed = f
            .pass
            .album
            .confirm_catalog_asset("asset-m")
            .await
            .unwrap()
            .asset;
        let (asset, confirmation) = run.confirm(&key, observed).await.unwrap();
        assert_eq!(asset.filename(), Some("photo.JPG"));
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let task = &plan.tasks[0];
        assert_eq!(task.version_size, VersionSizeKey::Original);
        assert_eq!(task.checksum.as_ref(), checksum(current));
        assert_eq!(task.size, current.len() as u64);
        let s = task.download_path.clone();
        assert_ne!(s, p);
        assert_ne!(s, q);
        assert_ne!(s, r);
        assert!(!s.exists());
        // Ordinary planning already chose S; the bound historical P receipt,
        // not a relocation marker, must authenticate this prior-publication choice.
        assert!(
            task.superseded_requested_path.is_none(),
            "unexpected raw marker: {:?}",
            task.superseded_requested_path
        );
        assert!(task.superseded_requested(&config.directory, &p).is_none());
        let binding = task.publication_binding.as_deref().unwrap().clone();
        assert_eq!(
            json!([
                binding.account_key,
                binding.provider_key,
                binding.scope,
                binding.library,
                binding.child,
                binding.master,
                binding.state_id,
                binding.origin_pass
            ]),
            json!([
                account,
                provider,
                run.root.spec.scope,
                "PrimarySync",
                "asset-m",
                "m",
                "asset-m",
                key
            ])
        );
        let nominated: Vec<_> = planner.reconciliation_reservations().collect();
        assert_eq!(nominated.len(), 1);
        let nomination = nominated[0];
        assert_eq!(nomination.library.as_ref(), "PrimarySync");
        assert_eq!(nomination.asset_id.as_ref(), "asset-m");
        assert_eq!(nomination.version_size, VersionSizeKey::Original);
        assert_eq!(
            nomination.requested_path_key.0,
            crate::fs_util::confined_path_key(&p).unwrap()
        );
        assert_eq!(
            nomination.destination_path_key.0,
            crate::fs_util::confined_path_key(&s).unwrap()
        );
        assert_eq!(nomination.destination_path, s);
        let content = nomination.content.as_ref().unwrap();
        assert_eq!(
            (content.checksum.as_ref(), content.size),
            (checksum(current).as_str(), current.len() as u64)
        );
        assert!(rows("reconciliation_paths", "id", "asset-m").is_empty());
        assert!(rows("provider_active_destinations", "child", "asset-m").is_empty());
        let old_receipts =
            f.db.bound_historical_downloads(
                binding.clone(),
                "original".into(),
                checksum(current),
                current.len() as u64,
            )
            .await
            .unwrap();
        assert_eq!(old_receipts.len(), 1);
        let historical = &old_receipts[0];
        assert_eq!(historical.native_path.to_path(), p);
        assert!(historical.fulfills_pass(&key));
        assert_eq!(
            json!([
                historical.account_key,
                historical.provider_key,
                historical.scope,
                historical.library,
                historical.child,
                historical.master,
                historical.state_id,
                historical.origin_pass,
                historical.version_size,
                historical.provider_checksum,
                historical.provider_size
            ]),
            json!([
                account,
                provider,
                run.root.spec.scope,
                "PrimarySync",
                "asset-m",
                "m",
                "asset-m",
                key,
                "original",
                checksum(old),
                old.len()
            ])
        );
        assert_eq!(
            (
                old_receipts[0].source_checksum.as_str(),
                old_receipts[0].local_checksum.as_str()
            ),
            (local(old).as_str(), local(old).as_str())
        );
        assert_eq!(rows("download_receipts", "child", "asset-m"), p_receipts);
        assert_eq!(std::fs::read(&p).unwrap(), old);
        assert_eq!(identity(&p), p_identity);
        eprintln!(
            "FINAL_R_NOMINATION publish_r={publish_r} pending_q={pending_q} marker-free=true historical-P=true exact-nomination=true"
        );
        let r_identity = if publish_r {
            // Do not start another run, reload the retained planner, or rewrite a receipt/path.
            f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"]["value"] =
                json!("verified.JPG");
            let result = stream_collision_child(&f, "asset-m", false).await;
            assert_eq!(
                (result.downloaded, result.state_write_failures),
                (1, 0),
                "{result:?}"
            );
            assert!(result.failed.is_empty());
            f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"]["value"] =
                json!("photo.JPG");
            assert_eq!(std::fs::read(&r).unwrap(), current);
            let receipts =
                f.db.bound_download_receipts(
                    binding.clone(),
                    "original".into(),
                    checksum(current),
                    current.len() as u64,
                )
                .await
                .unwrap();
            assert_eq!(receipts.len(), 1);
            let receipt = &receipts[0];
            assert_eq!(receipt.native_path, SelectionPath::from_path(&r));
            assert!(receipt.fulfills_pass(&key));
            assert_eq!(
                json!([
                    receipt.account_key,
                    receipt.provider_key,
                    receipt.scope,
                    receipt.library,
                    receipt.child,
                    receipt.master,
                    receipt.state_id,
                    receipt.version_size,
                    receipt.provider_checksum,
                    receipt.provider_size,
                    receipt.source_checksum,
                    receipt.local_checksum
                ]),
                json!([
                    account,
                    provider,
                    run.root.spec.scope,
                    "PrimarySync",
                    "asset-m",
                    "m",
                    "asset-m",
                    "original",
                    checksum(current),
                    current.len(),
                    local(current),
                    local(current)
                ])
            );
            let fingerprint =
                crate::download::file::fingerprint_downloaded_path(&config.directory, &r)
                    .await
                    .unwrap();
            assert_eq!(
                (
                    data_encoding::HEXLOWER.encode(&fingerprint.sha256),
                    fingerprint.size
                ),
                (local(current), current.len() as u64)
            );
            assert!(
                f.db.verified_selection_paths(
                    owner(),
                    run.root.spec.scope.clone(),
                    key.clone(),
                    "asset-m".into(),
                    "m".into(),
                    "original".into(),
                    (checksum(current), current.len() as u64)
                )
                .await
                .unwrap()
                .contains(&SelectionPath::from_path(&r))
            );
            assert!(
                f.db.selection_path_reusable_for_pass(
                    owner(),
                    run.root.id.clone(),
                    run.root.spec.scope.clone(),
                    key.clone(),
                    "asset-m".into(),
                    SelectionPath::from_path(&r)
                )
                .await
                .unwrap()
            );
            assert!(
                f.db.selection_path_matches_local(
                    owner(),
                    run.root.spec.scope.clone(),
                    key.clone(),
                    "asset-m".into(),
                    SelectionPath::from_path(&r),
                    (local(current), current.len() as u64)
                )
                .await
                .unwrap()
            );
            eprintln!("FINAL_R_VERIFIED same-pass-receipt=true local-fingerprint=true");
            Some(identity(&r))
        } else {
            assert!(!r.exists());
            None
        };
        // Any ordinary R-to-R bookmark is legitimate, never an obsolete P-to-R choice.
        let map_sql = "SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths WHERE id='asset-m' ORDER BY requested_path_key";
        let mappings = || -> Vec<serde_json::Value> {
            let conn = f.db.acquire_lock("final R mapping tuples").unwrap();
            conn.prepare(map_sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|text| serde_json::from_str(&text.unwrap()).unwrap())
                .collect()
        };
        let tuple = |requested: &std::path::Path, destination: &std::path::Path| {
            json!([
                "PrimarySync",
                "asset-m",
                "original",
                crate::fs_util::confined_path_key(requested).unwrap(),
                crate::fs_util::confined_path_key(destination).unwrap(),
                destination.to_str().unwrap(),
                checksum(current),
                current.len()
            ])
        };
        let before_maps = mappings();
        assert!(before_maps.len() <= usize::from(publish_r));
        assert!(before_maps.iter().all(|row| *row == tuple(&r, &r)));
        let receipts_before = rows("download_receipts", "child", "asset-m");
        assert_eq!(receipts_before.len(), 1 + usize::from(publish_r));
        assert!(p_receipts.iter().all(|row| receipts_before.contains(row)));
        assert_eq!(plan.tasks[0].download_path, s);
        assert!(
            plan.tasks[0].superseded_requested_path.is_none(),
            "retained plan unexpectedly acquired a raw marker: {:?}",
            plan.tasks[0].superseded_requested_path
        );
        assert!(
            plan.tasks[0]
                .superseded_requested(&config.directory, &p)
                .is_none()
        );
        assert_eq!(
            f.session.records.lock().unwrap()[0]["fields"]["filenameEnc"]["value"],
            json!("photo.JPG")
        );
        assert!(rows("provider_active_destinations", "child", "asset-m").is_empty());
        let gets = server.received_requests().await.unwrap().len();
        assert_eq!(gets, 1 + usize::from(publish_r));
        if !publish_r {
            f.db.acquire_lock("observe exact prior intent before admission").unwrap().execute_batch(&format!(
                "CREATE TRIGGER exact_prior_before_admission BEFORE INSERT ON provider_active_destinations
                 WHEN NEW.child='asset-m' BEGIN SELECT CASE WHEN NOT EXISTS(
                 SELECT 1 FROM reconciliation_paths r WHERE r.library=NEW.library
                 AND r.id=NEW.asset_id AND r.version_size=NEW.version_size
                 AND r.provider_checksum=NEW.checksum AND r.provider_size=NEW.size_bytes
                 AND r.requested_path_key='{}' AND r.destination_path_key='{}'
                 AND r.destination_path=NEW.compat_path AND NEW.admitted=1)
                 THEN RAISE(ABORT,'historical nomination missing before admission') END; END;",
                crate::fs_util::confined_path_key(&p).unwrap().replace('\'', "''"),
                crate::fs_util::confined_path_key(&s).unwrap().replace('\'', "''"),
            )).unwrap();
        }
        assert!(
            run.project(
                &key,
                &asset,
                confirmation,
                &mut plan,
                &mut planner,
                &config,
                &CancellationToken::new()
            )
            .await
            .unwrap()
        );
        let expected = if publish_r { &r } else { &s };
        let manifest =
            f.db.selection_decision(owner(), run.root.id.clone(), key.clone(), "asset-m".into())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(manifest.state_id, "asset-m");
        assert_eq!(manifest.decision.outcome, SelectionOutcome::Selected);
        assert_eq!(manifest.decision.master.as_deref(), Some("m"));
        assert!(manifest.sources.is_empty());
        assert!(manifest.decision.confirmation.is_some());
        assert_eq!(manifest.decision.destinations.len(), 1);
        let destination = &manifest.decision.destinations[0];
        assert_eq!(destination.path, SelectionPath::from_path(expected));
        assert_eq!(destination.version_size, "original");
        assert_eq!(
            (destination.checksum.as_str(), destination.size),
            (checksum(current).as_str(), current.len() as u64)
        );
        assert_eq!(
            admitted("asset-m"),
            json!([
                run.root.id,
                key,
                "asset-m",
                "PrimarySync",
                "asset-m",
                "m",
                "original",
                SelectionPath::from_path(expected).key().unwrap(),
                checksum(current),
                current.len(),
                1,
                i64::from(publish_r),
                run.root.spec.scope,
                account,
                provider
            ])
        );
        assert_eq!(
            rows("provider_active_destinations", "child", "asset-m").len(),
            1
        );
        assert_eq!(plan.tasks.len(), usize::from(!publish_r));
        if !publish_r {
            assert_eq!(plan.tasks[0].download_path, s);
        }
        if publish_r {
            assert_eq!(
                mappings(),
                before_maps,
                "obsolete nomination wrote P-to-S/P-to-R intent"
            );
            assert_eq!(std::fs::read(&r).unwrap(), current);
            assert_eq!(Some(identity(&r)), r_identity);
        } else {
            assert_eq!(
                mappings(),
                vec![tuple(&p, &s)],
                "positive control did not persist exact P-to-S"
            );
        }
        assert!(!s.exists());
        assert!(!q.exists());
        assert_eq!(std::fs::read(&p).unwrap(), old);
        assert_eq!(identity(&p), p_identity);
        assert_eq!(
            rows("download_receipts", "child", "asset-m"),
            receipts_before
        );
        assert_eq!(
            rows("provider_active_destinations", "child", "asset-q"),
            q_destinations
        );
        assert_eq!(
            rows("provider_active_decisions", "child", "asset-q"),
            q_decisions
        );
        assert_eq!(rows("assets", "id", "asset-q"), q_queue);
        assert!(rows("download_receipts", "child", "asset-q").is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), gets);
        eprintln!(
            "FINAL_R_PASS publish_r={publish_r} pending_q={pending_q} destination={} verified_media={} Q-unchanged=true projection-GETs=0",
            if publish_r { "R" } else { "S" },
            i64::from(publish_r)
        );
        // This is only the direct admission prefix, not completed dispatch or checkpoint proof.
    }
}

#[tokio::test]
async fn first_adjusted_suffix_nomination_precedes_admission() {
    use crate::download::orchestration::generation;
    use crate::state::VersionSizeKey;
    use sha2::{Digest, Sha256};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let original = b"\xff\xd8\xff\xe0source original";
    let adjusted = b"\xff\xd8\xff\xe0first adjusted resource";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    let local = |bytes: &[u8]| data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
    let file = |path: &std::path::Path| {
        let handle = std::fs::File::open(path).unwrap();
        let metadata = handle.metadata().unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&handle).unwrap(),
            metadata.len(),
            metadata.modified().unwrap(),
        )
    };
    let mut missing_intent = 0;
    // Prove admission ordering and real publication before testing stale nominations.
    for (pending_q, own_original, invalidation) in [
        (true, true, None),
        (false, true, None),
        (true, false, None),
        (false, false, None),
        (true, true, Some("changed")),
        (true, true, Some("missing")),
    ] {
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/original"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(original))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/adjusted"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(adjusted))
            .expect(u64::from(invalidation.is_none()))
            .mount(&server)
            .await;
        let mut f = Fixture::new().await;
        if std::env::var_os("KEI_FIRST_ADJUSTED_RETAIN_FIXTURE").is_some() {
            f.dir.disable_cleanup(true);
        }
        eprintln!(
            "FIRST_ADJUSTED_CASE pending_q={pending_q} own_original={own_original} invalidation={invalidation:?} fixture={}",
            f.dir.path().display()
        );
        f.config.folder_structure = "none".into();
        f.config.edited_naming = crate::types::EditedNaming::Suffix;
        f.config.resolution = crate::types::PhotoResolution::Original;
        f.config.edited = false;
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["filenameEnc"]["value"] = json!("photo_edited.JPG");
            values[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/original",server.uri()),"size":original.len(),"fileChecksum":checksum(original)
            });
        }
        let initial = stream_collision_child(&f, "asset-m", false).await;
        assert_eq!((initial.downloaded, initial.state_write_failures), (1, 0));
        assert!(initial.failed.is_empty());
        let p = f.config.directory.join("photo_edited.JPG");
        let original_file = file(&p);
        assert_eq!(original_file.0, original);
        let rows = |table: &str, column: &str, child: &str| {
            let conn =
                f.db.acquire_lock("first Adjusted complete evidence")
                    .unwrap();
            let mut statement = conn
                .prepare(&format!(
                    "SELECT * FROM {table} WHERE {column}=?1 ORDER BY 1"
                ))
                .unwrap();
            let columns = statement.column_count();
            statement
                .query_map([child], |row| {
                    (0..columns)
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let original_receipts = rows("download_receipts", "child", "asset-m");
        assert_eq!(original_receipts.len(), 1);
        let target = if own_original { "asset-m" } else { "asset-b" };
        let master = if own_original { "m" } else { "b" };
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
            let mut current = records(master, &checksum(original), "first adjusted");
            current[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
            current[0]["fields"]["resOriginalRes"] = values[0]["fields"]["resOriginalRes"].clone();
            current[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/adjusted",server.uri()),"size":adjusted.len(),"fileChecksum":checksum(adjusted)}});
            current[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
            current[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
            if own_original {
                *values = current;
            } else {
                values.extend(current);
            }
            let mut companion = records("q", &checksum(original), "pending companion");
            companion[0]["fields"]["filenameEnc"]["value"] = json!("companion.JPG");
            companion[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/companion",server.uri()),"size":adjusted.len(),"fileChecksum":checksum(adjusted)}});
            companion[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
            companion[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
            values.extend(companion);
        }
        f.config.resolution = crate::types::PhotoResolution::None;
        f.config.edited = true;
        let context = generation::context_for_format(
            std::slice::from_ref(&f.pass),
            &f.config,
            super::controls(),
            2,
        )
        .await
        .unwrap()
        .unwrap();
        let run = context
            .start_for_test(std::slice::from_ref(&f.pass), &f.config)
            .await
            .unwrap();
        let key = generation::pass_key(&f.pass);
        let mut config = f.config.with_pass(&f.pass);
        config.selection_run = Some(run.clone());
        config.selection_pass = Some(key.clone());
        if pending_q {
            let observed = f
                .pass
                .album
                .confirm_catalog_asset("asset-q")
                .await
                .unwrap()
                .asset;
            let (asset, confirmation) = run.confirm(&key, observed).await.unwrap();
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            assert_eq!(plan.tasks.len(), 1);
            assert_eq!(plan.tasks[0].version_size, VersionSizeKey::Adjusted);
            assert!(
                run.project(
                    &key,
                    &asset,
                    confirmation,
                    &mut plan,
                    &mut planner,
                    &config,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
            );
        }
        let q_destinations = rows("provider_active_destinations", "child", "asset-q");
        let q_decisions = rows("provider_active_decisions", "child", "asset-q");
        let q_queue = rows("assets", "id", "asset-q");
        assert_eq!(
            (q_destinations.len(), q_decisions.len(), q_queue.len()),
            (
                usize::from(pending_q),
                usize::from(pending_q),
                usize::from(pending_q)
            )
        );
        if pending_q {
            assert_eq!(f.db.acquire_lock("Q really unfinished").unwrap().query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-q' AND admitted=1 AND verified_media=0", [], |row|row.get::<_,i64>(0)).unwrap(), 1);
        }
        let observed = f
            .pass
            .album
            .confirm_catalog_asset(target)
            .await
            .unwrap()
            .asset;
        let (asset, confirmation) = run.confirm(&key, observed).await.unwrap();
        assert_eq!(asset.filename(), Some("photo.JPG"));
        let expected = crate::download::filter::expected_paths_for(&asset, &config);
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].version_size, VersionSizeKey::Adjusted);
        assert_eq!(
            expected[0].path, p,
            "actual Suffix requested path must be occupied Original P"
        );
        let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
            .await
            .unwrap();
        let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
        assert_eq!(plan.tasks.len(), 1);
        let task = &plan.tasks[0];
        assert_eq!(task.version_size, VersionSizeKey::Adjusted);
        assert_eq!(
            (task.checksum.as_ref(), task.size),
            (checksum(adjusted).as_str(), adjusted.len() as u64)
        );
        let s = task.download_path.clone();
        assert_ne!(s, p, "collision must choose S before late chooser");
        assert!(!s.exists());
        assert!(task.superseded_requested_path.is_none());
        assert!(task.superseded_requested(&config.directory, &p).is_none());
        let binding = task.publication_binding.as_deref().unwrap().clone();
        let (account, provider) = f.capture.owner.authenticated_keys().unwrap();
        assert_eq!(
            json!([
                binding.account_key,
                binding.provider_key,
                binding.scope,
                binding.library,
                binding.child,
                binding.master,
                binding.state_id,
                binding.origin_pass
            ]),
            json!([
                account,
                provider,
                run.root.spec.scope,
                "PrimarySync",
                target,
                master,
                target,
                key
            ])
        );
        assert!(
            f.db.bound_historical_downloads(
                binding.clone(),
                "adjusted".into(),
                checksum(adjusted),
                adjusted.len() as u64
            )
            .await
            .unwrap()
            .is_empty()
        );
        let original_key = crate::download::filter::provider_version_for_selected(
            &asset,
            &config,
            VersionSizeKey::Original,
        );
        let (_, resource) = asset
            .versions()
            .iter()
            .find(|(version, _)| VersionSizeKey::from(*version) == original_key)
            .unwrap();
        assert_eq!(
            (resource.checksum.as_ref(), resource.size),
            (checksum(original).as_str(), original.len() as u64)
        );
        let fingerprint = crate::download::file::fingerprint_downloaded_path(&config.directory, &p)
            .await
            .unwrap();
        assert_eq!(
            (
                data_encoding::HEXLOWER.encode(&fingerprint.sha256),
                fingerprint.size
            ),
            (local(original), original.len() as u64)
        );
        let proof =
            f.db.bound_cross_rendition_original(
                binding.clone(),
                VersionSizeKey::Adjusted,
                crate::state::db::CrossRenditionOriginal {
                    path: SelectionPath::from_path(&p),
                    provider_checksum: resource.checksum.to_string(),
                    provider_size: resource.size,
                    local_checksum: Some(data_encoding::HEXLOWER.encode(&fingerprint.sha256)),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            proof.is_some(),
            own_original,
            "existing scoped Original witness eligibility"
        );
        if let Some(proof) = proof {
            let receipt = proof.receipt;
            assert!(receipt.fulfills_pass(&key));
            assert_eq!(
                json!([
                    receipt.account_key,
                    receipt.provider_key,
                    receipt.scope,
                    receipt.library,
                    receipt.child,
                    receipt.master,
                    receipt.state_id,
                    receipt.origin_pass,
                    receipt.version_size,
                    receipt.provider_checksum,
                    receipt.provider_size,
                    receipt.native_path,
                    receipt.source_checksum,
                    receipt.local_checksum
                ]),
                json!([
                    account,
                    provider,
                    run.root.spec.scope,
                    "PrimarySync",
                    target,
                    master,
                    target,
                    key,
                    "original",
                    checksum(original),
                    original.len(),
                    SelectionPath::from_path(&p),
                    local(original),
                    local(original)
                ])
            );
        }
        assert_eq!(
            rows("download_receipts", "child", "asset-m"),
            original_receipts
        );
        assert_eq!(file(&p), original_file);
        assert!(rows("reconciliation_paths", "id", target).is_empty());
        assert!(rows("provider_active_destinations", "child", target).is_empty());
        if own_original {
            let nominations = planner.reconciliation_reservations().collect::<Vec<_>>();
            assert_eq!(nominations.len(), 1);
            let nomination = nominations[0];
            assert_eq!(nomination.library.as_ref(), "PrimarySync");
            assert_eq!(nomination.asset_id.as_ref(), target);
            assert_eq!(nomination.version_size, VersionSizeKey::Adjusted);
            assert_eq!(
                nomination.requested_path_key.0,
                crate::fs_util::confined_path_key(&p).unwrap()
            );
            assert_eq!(
                nomination.destination_path_key.0,
                crate::fs_util::confined_path_key(&s).unwrap()
            );
            assert_eq!(nomination.destination_path, s);
            let content = nomination.content.as_ref().unwrap();
            assert_eq!(
                (content.checksum.as_ref(), content.size),
                (checksum(adjusted).as_str(), adjusted.len() as u64)
            );
        }
        eprintln!(
            "FIRST_ADJUSTED_PREMISES pending_q={pending_q} own_original={own_original} suffix=true collision_before_project=true marker_absent=true adjusted_history=0 original_witness={}",
            own_original
        );
        let (condition, diagnostic) = if own_original {
            (
                "NOT EXISTS",
                "first Adjusted intent missing before admission",
            )
        } else {
            ("EXISTS", "new child gained early Adjusted intent")
        };
        f.db.acquire_lock("first Adjusted actual admission observer").unwrap().execute_batch(&format!(
            "CREATE TRIGGER first_adjusted_order BEFORE INSERT ON provider_active_destinations
             WHEN NEW.child='{target}' AND NEW.version_size='adjusted' BEGIN SELECT CASE WHEN {condition}(
             SELECT 1 FROM reconciliation_paths r WHERE r.library=NEW.library AND r.id=NEW.asset_id
             AND r.version_size=NEW.version_size AND r.provider_checksum=NEW.checksum AND r.provider_size=NEW.size_bytes
             AND r.requested_path_key='{}' AND r.destination_path_key='{}'
             AND r.destination_path=NEW.compat_path AND NEW.admitted=1)
             THEN RAISE(ABORT,'{diagnostic}') END; END;",
            crate::fs_util::confined_path_key(&p).unwrap().replace('\'', "''"),
            crate::fs_util::confined_path_key(&s).unwrap().replace('\'', "''"),
        )).unwrap();
        let invalidated_file = invalidation.map(|change| {
            let path = if change == "changed" {
                std::fs::write(&p, b"concurrent synthetic local change").unwrap();
                p.clone()
            } else {
                let retained = p.with_file_name("retained-original.JPG");
                std::fs::rename(&p, &retained).unwrap();
                assert_eq!(file(&retained), original_file);
                retained
            };
            let snapshot = file(&path);
            (path, snapshot)
        });
        let projected = run
            .project(
                &key,
                &asset,
                confirmation,
                &mut plan,
                &mut planner,
                &config,
                &CancellationToken::new(),
            )
            .await;
        if let Some((retained, snapshot)) = invalidated_file {
            let reason = format!("{:#}", projected.expect_err("invalidated positive nomination must refuse, not fall back to ordinary admission"));
            assert!(
                reason.contains("Choice lost authenticated historical source"),
                "unexpected owner obstacle: {reason}"
            );
            assert!(rows("reconciliation_paths", "id", target).is_empty());
            assert!(rows("provider_active_destinations", "child", target).is_empty());
            assert_eq!(
                rows("download_receipts", "child", "asset-m"),
                original_receipts
            );
            assert_eq!(file(&retained), snapshot);
            if invalidation == Some("missing") {
                assert!(!p.exists());
            }
            assert_eq!(
                rows("provider_active_destinations", "child", "asset-q"),
                q_destinations
            );
            assert_eq!(
                rows("provider_active_decisions", "child", "asset-q"),
                q_decisions
            );
            assert_eq!(rows("assets", "id", "asset-q"), q_queue);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            eprintln!(
                "FIRST_ADJUSTED_STALE_PASS invalidation={invalidation:?} prior-nomination=true refused-before-intent-admission-GET=true reason={reason}"
            );
            continue;
        }
        let prefix_admitted = match projected {
            Err(error) if own_original => {
                let reason = format!("{error:#}");
                assert!(
                    reason.contains("first Adjusted intent missing before admission"),
                    "unexpected owner obstacle: {reason}"
                );
                missing_intent += 1;
                assert!(rows("provider_active_destinations", "child", target).is_empty());
                assert!(rows("reconciliation_paths", "id", target).is_empty());
                eprintln!(
                    "FIRST_ADJUSTED_RED pending_q={pending_q} unique_insert_tripwire=true reason={reason}"
                );
                false
            }
            result => {
                assert!(result.unwrap());
                assert_eq!(
                    rows("provider_active_destinations", "child", target).len(),
                    1
                );
                assert_eq!(
                    rows("reconciliation_paths", "id", target).len(),
                    usize::from(own_original)
                );
                let admitted: String = f.db.acquire_lock("first Adjusted exact admitted prefix").unwrap().query_row(
                    "SELECT json_array(d.generation,d.pass_key,d.child,d.library,d.asset_id,d.master,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,g.scope,g.account_key,g.provider_key) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child=?1",
                    [target], |row| row.get(0),
                ).unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&admitted).unwrap(),
                    json!([
                        run.root.id,
                        key,
                        target,
                        "PrimarySync",
                        target,
                        master,
                        "adjusted",
                        SelectionPath::from_path(&s).key().unwrap(),
                        checksum(adjusted),
                        adjusted.len(),
                        1,
                        0,
                        run.root.spec.scope,
                        account,
                        provider
                    ])
                );
                eprintln!(
                    "FIRST_ADJUSTED_CONTROL pending_q={pending_q} own_original={own_original} admitted=true early_intent={own_original}"
                );
                true
            }
        };
        assert_eq!(
            rows("download_receipts", "child", "asset-m"),
            original_receipts
        );
        assert_eq!(file(&p), original_file);
        assert_eq!(
            rows("provider_active_destinations", "child", "asset-q"),
            q_destinations
        );
        assert_eq!(
            rows("provider_active_decisions", "child", "asset-q"),
            q_decisions
        );
        assert_eq!(rows("assets", "id", "asset-q"), q_queue);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        eprintln!(
            "FIRST_ADJUSTED_CUSTODY pending_q={pending_q} own_original={own_original} P_Q_exact=true adjusted_GET=0"
        );
        if !prefix_admitted {
            continue;
        }
        if !own_original {
            f.db.acquire_lock("release ordinary prefix observer")
                .unwrap()
                .execute_batch("DROP TRIGGER first_adjusted_order")
                .unwrap();
        }
        let published = crate::download::pipeline::stream_and_download_from_stream(
            &reqwest::Client::new(),
            futures_util::stream::iter([Ok(asset)]),
            &Arc::new(config.clone()),
            super::controls(),
            1,
            CancellationToken::new(),
            crate::download::pipeline::StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert_eq!(
            (published.downloaded, published.state_write_failures),
            (1, 0),
            "{published:?}"
        );
        assert!(published.failed.is_empty());
        assert_eq!(std::fs::read(&s).unwrap(), adjusted);
        let completed =
            f.db.bound_download_receipts(
                binding,
                "adjusted".into(),
                checksum(adjusted),
                adjusted.len() as u64,
            )
            .await
            .unwrap();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].native_path, SelectionPath::from_path(&s));
        assert!(completed[0].fulfills_pass(&key));
        assert_eq!(
            (&completed[0].source_checksum, &completed[0].local_checksum),
            (&local(adjusted), &local(adjusted))
        );
        let after = rows("download_receipts", "child", "asset-m");
        assert!(
            original_receipts
                .iter()
                .all(|receipt| after.contains(receipt))
        );
        assert_eq!(after.len(), 1 + usize::from(own_original));
        assert_eq!(file(&p), original_file);
        assert_eq!(
            rows("provider_active_destinations", "child", "asset-q"),
            q_destinations
        );
        assert_eq!(
            rows("provider_active_decisions", "child", "asset-q"),
            q_decisions
        );
        assert_eq!(rows("assets", "id", "asset-q"), q_queue);
        let gets = server.received_requests().await.unwrap();
        assert_eq!(gets.len(), 2);
        assert_eq!(
            gets.iter()
                .filter(|request| request.url.path() == "/adjusted")
                .count(),
            1
        );
        eprintln!(
            "FIRST_ADJUSTED_MEDIA_PASS pending_q={pending_q} own_original={own_original} saved-S=true adjusted_GET=1 P_Q_exact=true"
        );
    }
    assert_eq!(
        missing_intent, 0,
        "eligible first-Adjusted intent must precede admission"
    );
}

fn marker_free_history_rows(
    db: &SqliteStateDb,
    table: &str,
    column: &str,
    value: &str,
) -> Vec<Vec<rusqlite::types::Value>> {
    let conn = db.acquire_lock("marker-free complete evidence").unwrap();
    let mut statement = conn
        .prepare(&format!(
            "SELECT * FROM {table} WHERE {column}=?1 ORDER BY 1"
        ))
        .unwrap();
    let columns = statement.column_count();
    statement
        .query_map([value], |row| {
            (0..columns)
                .map(|index| row.get(index))
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn marker_free_history_file(
    path: &std::path::Path,
) -> (Vec<u8>, crate::fs_util::FileIdentity, std::time::SystemTime) {
    let file = std::fs::File::open(path).unwrap();
    (
        std::fs::read(path).unwrap(),
        crate::fs_util::file_identity(&file).unwrap(),
        file.metadata().unwrap().modified().unwrap(),
    )
}

#[tokio::test]
async fn marker_free_same_rendition_history_controls_saved_destination() {
    use crate::download::baseline_matching::BaselineMatch;
    use crate::download::orchestration::generation;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    // DEPENDENCY: genuine historical nominations in both download modes must
    // qualify first. Never remove Q or force the planner's mode/origin/marker.
    let c0 = b"\xff\xd8\xff\xe0old original";
    let c1 = b"\xff\xd8\xff\xe0a distinctly longer replacement original";
    let checksum = |bytes: &[u8]| data_encoding::BASE64.encode(&Sha256::digest(bytes));
    assert_ne!(c0.len(), c1.len());
    assert!(!crate::download::file::supported_provider_signature(
        &checksum(c1)
    ));
    for case in [
        "unchanged",
        "absent",
        "changed",
        "directory",
        "missing_receipt",
    ] {
        let positive = matches!(case, "unchanged" | "absent");
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/original"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(c0))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/q"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(c1))
            .expect(0)
            .mount(&server)
            .await;
        let mut f = Fixture::new().await;
        f.dir.disable_cleanup(true);
        eprintln!(
            "MARKER_FREE_HISTORY_FIXTURE case={case} retained={}",
            f.dir.path().display()
        );
        f.config.folder_structure = "none".into();
        f.config.resolution = crate::types::PhotoResolution::Original;
        f.config.edited = false;
        f.config.retry = crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
            values[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/original", server.uri()), "size":c0.len(), "fileChecksum":checksum(c0)
            });
        }
        let published = stream_collision_child(&f, "asset-m", false).await;
        assert_eq!(
            (published.downloaded, published.state_write_failures),
            (1, 0)
        );
        assert!(published.failed.is_empty());
        drop(published);
        let p = f.config.directory.join("photo.JPG");
        let q = f.config.directory.join("companion.JPG");
        let backup = f.dir.path().join("historical-original-backup.JPG");
        assert!(!backup.starts_with(&*f.config.directory));
        let old_file = marker_free_history_file(&p);
        assert_eq!(old_file.0, c0);
        let mut initial_entries: Vec<_> = std::fs::read_dir(&*f.config.directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        initial_entries.sort();
        let retained_files: Vec<_> = initial_entries
            .iter()
            .filter(|path| *path != &p)
            .map(|path| (path.clone(), marker_free_history_file(path)))
            .collect();
        let old_proof = marker_free_history_rows(&f.db, "download_receipts", "child", "asset-m");
        assert_eq!(old_proof.len(), 1);
        let old_paths = marker_free_history_rows(
            &f.db,
            "asset_metadata_paths",
            "local_path",
            p.to_str().unwrap(),
        );
        assert!(!old_paths.is_empty());
        let old_claims = marker_free_history_rows(
            &f.db,
            "legacy_master_state_owners",
            "asset_record_name",
            "asset-m",
        );
        let token = marker_free_history_rows(&f.db, "metadata", "key", "sync_token:PrimarySync");
        let other_token = marker_free_history_rows(
            &f.db,
            "metadata",
            "key",
            "pending_sync_token:sparse:OtherZone",
        );
        assert_eq!((token.len(), other_token.len()), (1, 1));
        {
            let mut values = f.session.records.lock().unwrap();
            values[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/current", server.uri()), "size":c1.len(), "fileChecksum":checksum(c1)
            });
            let mut companion = records("q", &checksum(c1), "genuine unfinished disjoint work");
            companion[0]["fields"]["filenameEnc"]["value"] = json!("companion.JPG");
            companion[0]["fields"]["resOriginalRes"]["value"] = json!({
                "downloadURL":format!("{}/q", server.uri()), "size":c1.len(), "fileChecksum":checksum(c1)
            });
            values.extend(companion);
        }
        let q_rows = |db: &SqliteStateDb| {
            [
                marker_free_history_rows(db, "provider_active_destinations", "child", "asset-q"),
                marker_free_history_rows(db, "provider_active_decisions", "child", "asset-q"),
                marker_free_history_rows(db, "assets", "id", "asset-q"),
                marker_free_history_rows(db, "asset_metadata_paths", "id", "asset-q"),
                marker_free_history_rows(
                    db,
                    "asset_master_mappings",
                    "asset_record_name",
                    "asset-q",
                ),
                marker_free_history_rows(db, "reconciliation_paths", "id", "asset-q"),
            ]
        };
        let (s, root, pass, binding, q_debt) = {
            let context = generation::context_for_format(
                std::slice::from_ref(&f.pass),
                &f.config,
                super::controls(),
                2,
            )
            .await
            .unwrap()
            .unwrap();
            let run = context
                .start_for_test(std::slice::from_ref(&f.pass), &f.config)
                .await
                .unwrap();
            let pass = generation::pass_key(&f.pass);
            let mut config = f.config.with_pass(&f.pass);
            config.selection_run = Some(run.clone());
            config.selection_pass = Some(pass.clone());
            {
                let observed = f
                    .pass
                    .album
                    .confirm_catalog_asset("asset-q")
                    .await
                    .unwrap()
                    .asset;
                let (asset, proof) = run.confirm(&pass, observed).await.unwrap();
                let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                    .await
                    .unwrap();
                let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
                assert_eq!(plan.tasks.len(), 1);
                assert_eq!(plan.tasks[0].download_path, q);
                assert!(
                    run.project(
                        &pass,
                        &asset,
                        proof,
                        &mut plan,
                        &mut planner,
                        &config,
                        &CancellationToken::new()
                    )
                    .await
                    .unwrap()
                );
            }
            let q_debt = q_rows(&f.db);
            assert_eq!(
                (q_debt[0].len(), q_debt[1].len(), q_debt[2].len()),
                (1, 1, 1)
            );
            assert_eq!(f.db.acquire_lock("actual unverified Q").unwrap().query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-q' AND admitted=1 AND verified_media=0", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
            let observed = f
                .pass
                .album
                .confirm_catalog_asset("asset-m")
                .await
                .unwrap()
                .asset;
            let (asset, proof) = run.confirm(&pass, observed).await.unwrap();
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let mut plan = planner.plan_download_asset(&asset, &config).await.unwrap();
            assert_eq!(plan.tasks.len(), 1);
            let task = &plan.tasks[0];
            assert!(
                task.superseded_requested_path.is_none(),
                "{case}: marker was not naturally absent"
            );
            assert_eq!(task.version_size, crate::state::VersionSizeKey::Original);
            assert_eq!(
                (task.checksum.as_ref(), task.size),
                (checksum(c1).as_str(), c1.len() as u64)
            );
            let s = task.download_path.clone();
            assert_ne!(s, p);
            assert_ne!(s, q);
            assert!(!s.exists());
            let binding = task.publication_binding.as_deref().unwrap().clone();
            let (account, provider) = f.capture.owner.authenticated_keys().unwrap();
            assert_eq!(
                json!([
                    binding.account_key,
                    binding.provider_key,
                    binding.scope,
                    binding.library,
                    binding.child,
                    binding.master,
                    binding.state_id,
                    binding.origin_pass
                ]),
                json!([
                    account,
                    provider,
                    run.root.spec.scope,
                    "PrimarySync",
                    "asset-m",
                    "m",
                    "asset-m",
                    pass
                ])
            );
            let historical =
                f.db.bound_historical_downloads(
                    binding.clone(),
                    "original".into(),
                    checksum(c1),
                    c1.len() as u64,
                )
                .await
                .unwrap();
            assert_eq!(historical.len(), 1);
            assert_eq!(historical[0].native_path, SelectionPath::from_path(&p));
            assert_eq!(
                (
                    historical[0].version_size.as_str(),
                    historical[0].provider_checksum.as_str(),
                    historical[0].provider_size
                ),
                ("original", checksum(c0).as_str(), c0.len() as u64)
            );
            assert_eq!(
                (
                    historical[0].source_checksum.as_str(),
                    historical[0].local_checksum.as_str()
                ),
                (
                    data_encoding::HEXLOWER.encode(&Sha256::digest(c0)).as_str(),
                    data_encoding::HEXLOWER.encode(&Sha256::digest(c0)).as_str()
                )
            );
            assert!(historical[0].fulfills_pass(&pass));
            assert!(
                marker_free_history_rows(&f.db, "reconciliation_paths", "id", "asset-m").is_empty()
            );
            assert!(
                marker_free_history_rows(&f.db, "provider_active_destinations", "child", "asset-m")
                    .is_empty()
            );
            // Existing projection INSERT is the assertion boundary, not an admission seed.
            f.db.acquire_lock("intent must precede real admission").unwrap().execute_batch(
                "CREATE TEMP TRIGGER marker_free_before_admission BEFORE INSERT ON provider_active_destinations WHEN NEW.child='asset-m' BEGIN SELECT RAISE(ABORT,'historical intent missing before admission') WHERE NOT EXISTS(SELECT 1 FROM reconciliation_paths r WHERE r.library=NEW.library AND r.id=NEW.asset_id AND r.version_size=NEW.version_size AND r.provider_checksum=NEW.checksum AND r.provider_size=NEW.size_bytes AND r.requested_path_key<>r.destination_path_key); END;"
            ).unwrap();
            assert!(
                run.project(
                    &pass,
                    &asset,
                    proof,
                    &mut plan,
                    &mut planner,
                    &config,
                    &CancellationToken::new()
                )
                .await
                .unwrap(),
                "{case}"
            );
            f.db.acquire_lock("remove scoped ordering assertion")
                .unwrap()
                .execute_batch("DROP TRIGGER marker_free_before_admission")
                .unwrap();
            assert_eq!(plan.tasks[0].download_path, s);
            assert!(plan.tasks[0].superseded_requested_path.is_none());
            assert_eq!(q_rows(&f.db), q_debt);
            (s, run.root.id.clone(), pass, binding, q_debt)
        };
        let mapping_sql = "SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths WHERE id='asset-m'";
        let expected_mapping = json!([
            "PrimarySync",
            "asset-m",
            "original",
            crate::fs_util::confined_path_key(&p).unwrap(),
            crate::fs_util::confined_path_key(&s).unwrap(),
            s.to_str().unwrap(),
            checksum(c1),
            c1.len()
        ])
        .to_string();
        let choice = marker_free_history_rows(&f.db, "reconciliation_paths", "id", "asset-m");
        assert_eq!(choice.len(), 1);
        assert_eq!(
            f.db.acquire_lock("real project wrote exact history choice")
                .unwrap()
                .query_row(mapping_sql, [], |r| r.get::<_, String>(0))
                .unwrap(),
            expected_mapping
        );
        let admission_sql = "SELECT json_array(generation,pass_key,child,library,asset_id,master,version_size,path,checksum,size_bytes,admitted,verified_media) FROM provider_active_destinations WHERE child='asset-m'";
        let expected_admission = json!([
            root,
            pass,
            "asset-m",
            "PrimarySync",
            "asset-m",
            "m",
            "original",
            SelectionPath::from_path(&s).key().unwrap(),
            checksum(c1),
            c1.len(),
            1,
            0
        ])
        .to_string();
        assert_eq!(
            f.db.acquire_lock("exact pending S")
                .unwrap()
                .query_row(admission_sql, [], |r| r.get::<_, String>(0))
                .unwrap(),
            expected_admission
        );
        assert_eq!(
            f.db.acquire_lock("actual catalogue replacement")
                .unwrap()
                .query_row(
                    "SELECT checksum FROM assets WHERE id='asset-m' AND version_size='original'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            checksum(c1)
        );
        eprintln!(
            "MARKER_FREE_HISTORY_ADMITTED case={case} P={} S={} Q={} marker=None intent=exact admission=exact",
            p.display(),
            s.display(),
            q.display()
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let database = f.dir.path().join("state.db");
        let callback_database = database.clone();
        let callback_mapping = expected_mapping.clone();
        let callback_admission = expected_admission.clone();
        let failure = Mock::given(method("GET"))
            .and(path("/current"))
            .respond_with(move |_: &wiremock::Request| {
                let conn = rusqlite::Connection::open_with_flags(
                    &callback_database,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
                .unwrap();
                assert_eq!(
                    conn.query_row(mapping_sql, [], |r| r.get::<_, String>(0))
                        .unwrap(),
                    callback_mapping
                );
                assert_eq!(
                    conn.query_row(admission_sql, [], |r| r.get::<_, String>(0))
                        .unwrap(),
                    callback_admission
                );
                seen.fetch_add(1, Ordering::SeqCst);
                eprintln!("MARKER_FREE_HISTORY_FAILED_GET case={case} intent=exact admission=exact status=500");
                ResponseTemplate::new(500)
            })
            .expect(1)
            .mount_as_scoped(&server)
            .await;
        let failed = stream_collision_child(&f, "asset-m", false).await;
        assert_eq!(
            (
                failed.downloaded,
                failed.state_write_failures,
                failed.failed.len()
            ),
            (0, 0, 1)
        );
        assert_eq!(failed.failed[0].download_path, s);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        drop(failed);
        drop(failure);
        assert!(!s.exists());
        let stage =
            crate::download::file::temp_download_path(&s, &checksum(c1), &f.config.temp_suffix)
                .unwrap();
        let stage_file = match std::fs::symlink_metadata(&stage) {
            Ok(metadata) => {
                assert!(metadata.is_file());
                Some(marker_free_history_file(&stage))
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
                None
            }
        };
        assert_eq!(marker_free_history_file(&p), old_file);
        assert_eq!(
            marker_free_history_rows(&f.db, "download_receipts", "child", "asset-m"),
            old_proof
        );
        let old_state = Arc::downgrade(&f.db);
        f = super::reopen_streaming_fixture(f).await;
        assert!(
            old_state.upgrade().is_none(),
            "old planner/run/store handle survived reopen"
        );
        f.db.make_selection_retry_due(owner(), "asset-m".into())
            .await
            .unwrap();
        if matches!(case, "absent" | "changed" | "directory") {
            std::fs::rename(&p, &backup).unwrap();
            assert_eq!(marker_free_history_file(&backup), old_file);
            if case == "changed" {
                std::fs::write(&p, b"changed synthetic historical occupant").unwrap();
            } else if case == "directory" {
                std::fs::create_dir(&p).unwrap();
            }
        }
        if case == "missing_receipt" {
            // Fault only the actual compact source receipt. No owner API forgets
            // just this proof while preserving the genuine current choice/Q debt.
            let receipt_id: String =
                f.db.acquire_lock("identify real proof fault")
                    .unwrap()
                    .query_row(
                        "SELECT id FROM download_receipts WHERE child='asset-m' AND native_path=?1",
                        [SelectionPath::from_path(&p).key().unwrap()],
                        |r| r.get(0),
                    )
                    .unwrap();
            assert_eq!(f.db.acquire_lock("scoped missing-proof fault").unwrap().execute("DELETE FROM download_receipts WHERE id=?1 AND child='asset-m' AND native_path=?2", rusqlite::params![receipt_id, SelectionPath::from_path(&p).key().unwrap()]).unwrap(), 1);
        }
        let expected_proof = if case == "missing_receipt" {
            Vec::new()
        } else {
            old_proof.clone()
        };
        let changed_file = (case == "changed").then(|| marker_free_history_file(&p));
        let directory_time =
            (case == "directory").then(|| std::fs::metadata(&p).unwrap().modified().unwrap());
        let physical = || {
            for (path, expected) in &retained_files {
                assert_eq!(marker_free_history_file(path), *expected);
            }
            if case == "absent" {
                assert_eq!(
                    std::fs::symlink_metadata(&p).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
            } else if case == "directory" {
                assert!(std::fs::metadata(&p).unwrap().is_dir());
                assert_eq!(
                    std::fs::metadata(&p).unwrap().modified().unwrap(),
                    directory_time.unwrap()
                );
                assert_eq!(std::fs::read_dir(&p).unwrap().count(), 0);
            } else {
                assert_eq!(
                    marker_free_history_file(&p),
                    changed_file.as_ref().unwrap_or(&old_file).clone()
                );
            }
            if matches!(case, "absent" | "changed" | "directory") {
                assert_eq!(marker_free_history_file(&backup), old_file);
            } else {
                assert!(!backup.exists());
            }
            let mut expected_entries = initial_entries.clone();
            if case == "absent" {
                expected_entries.retain(|path| path != &p);
            }
            if s.exists() {
                assert!(positive, "negative case published a saved destination");
                assert!(!stage.exists());
                expected_entries.push(s.clone());
            } else {
                let current_stage = match std::fs::symlink_metadata(&stage) {
                    Ok(metadata) => {
                        assert!(metadata.is_file());
                        Some(marker_free_history_file(&stage))
                    }
                    Err(error) => {
                        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
                        None
                    }
                };
                assert_eq!(current_stage, stage_file);
                if stage_file.is_some() {
                    expected_entries.push(stage.clone());
                }
            }
            let media = p.parent().unwrap();
            let mut actual_entries: Vec<_> = std::fs::read_dir(media)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            actual_entries.sort();
            expected_entries.sort();
            assert_eq!(
                actual_entries, expected_entries,
                "extra media, sibling or stage: {case}"
            );
        };
        let preserved = |db: &SqliteStateDb| {
            assert_eq!(q_rows(db), q_debt);
            assert_eq!(
                marker_free_history_rows(db, "reconciliation_paths", "id", "asset-m"),
                choice
            );
            assert_eq!(
                marker_free_history_rows(
                    db,
                    "asset_metadata_paths",
                    "local_path",
                    p.to_str().unwrap()
                ),
                old_paths
            );
            assert_eq!(
                marker_free_history_rows(
                    db,
                    "legacy_master_state_owners",
                    "asset_record_name",
                    "asset-m"
                ),
                old_claims
            );
            assert_eq!(
                marker_free_history_rows(db, "metadata", "key", "sync_token:PrimarySync"),
                token
            );
            assert_eq!(
                marker_free_history_rows(
                    db,
                    "metadata",
                    "key",
                    "pending_sync_token:sparse:OtherZone"
                ),
                other_token
            );
            assert!(!q.exists());
            physical();
        };
        Mock::given(method("GET"))
            .and(path("/current"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(c1))
            .expect(u64::from(positive))
            .mount(&server)
            .await;
        {
            let confirmation = f.pass.album.confirm_catalog_asset("asset-m").await.unwrap();
            let config = f.config.with_pass(&f.pass);
            assert!(config.selection_run.is_none());
            let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
                .await
                .unwrap();
            let plan = planner
                .plan_download_asset(&confirmation.asset, &config)
                .await
                .unwrap();
            assert_eq!(plan.tasks.len(), 1);
            assert_eq!(plan.tasks[0].download_path, s);
            assert!(plan.tasks[0].superseded_requested_path.is_none());
            assert!(
                f.db.bound_download_receipts(
                    binding.clone(),
                    "original".into(),
                    checksum(c1),
                    c1.len() as u64
                )
                .await
                .unwrap()
                .is_empty()
            );
            let historical =
                f.db.bound_historical_downloads(
                    binding.clone(),
                    "original".into(),
                    checksum(c1),
                    c1.len() as u64,
                )
                .await
                .unwrap();
            assert_eq!(historical.len(), usize::from(case != "missing_receipt"));
            let matched = planner
                .match_existing(&confirmation.asset, &config, &plan.tasks[0])
                .await;
            eprintln!(
                "MARKER_FREE_HISTORY_MATCH case={case} actual={matched:?} selection_run=None current_receipts=0"
            );
            if positive {
                assert!(
                    matches!(matched, BaselineMatch::Missing),
                    "{case}: {matched:?}"
                );
            } else {
                assert!(
                    matches!(matched, BaselineMatch::Uncertain(_)),
                    "{case}: {matched:?}"
                );
            }
            if case == "unchanged" {
                let c2 = b"\xff\xd8\xff\xe0genuinely newer provider original resource";
                let descriptor =
                    f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"]
                        .clone();
                f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] = json!({
                    "downloadURL":format!("{}/never-fetch-stale", server.uri()), "size":c2.len(), "fileChecksum":checksum(c2)
                });
                let newer = f.pass.album.confirm_catalog_asset("asset-m").await.unwrap();
                let error = planner
                    .persist_execution_publication_choices(&newer.asset, &config, &plan.tasks)
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "Choice no longer matches current requested resource"
                );
                f.session.records.lock().unwrap()[0]["fields"]["resOriginalRes"]["value"] =
                    descriptor;
                preserved(&f.db);
                assert_eq!(server.received_requests().await.unwrap().len(), 2);
            }
        }
        let lookups = f.session.calls.load(Ordering::SeqCst);
        let result = stream_collision_child(&f, "asset-m", false).await;
        assert_eq!(f.session.calls.load(Ordering::SeqCst), lookups + 2);
        assert_eq!(result.state_write_failures, 0);
        assert_eq!(result.downloaded, usize::from(positive));
        assert!(result.failed.is_empty());
        if !positive {
            assert_eq!(result.skip_summary.uncertain, 1);
        }
        drop(result);
        preserved(&f.db);
        let s_physical = if positive {
            Some(marker_free_history_file(&s))
        } else {
            None
        };
        if positive {
            assert_eq!(s_physical.as_ref().unwrap().0, c1);
            let receipts =
                f.db.bound_download_receipts(
                    binding.clone(),
                    "original".into(),
                    checksum(c1),
                    c1.len() as u64,
                )
                .await
                .unwrap();
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].native_path, SelectionPath::from_path(&s));
            assert!(receipts[0].fulfills_pass(&pass));
            assert_eq!(
                (
                    receipts[0].source_checksum.as_str(),
                    receipts[0].local_checksum.as_str()
                ),
                (
                    data_encoding::HEXLOWER.encode(&Sha256::digest(c1)).as_str(),
                    data_encoding::HEXLOWER.encode(&Sha256::digest(c1)).as_str()
                )
            );
            assert!(
                f.db.selection_destination_verified(
                    owner(),
                    root.clone(),
                    pass.clone(),
                    "asset-m".into(),
                    "original".into()
                )
                .await
                .unwrap()
            );
        } else {
            assert!(!s.exists());
            assert!(
                !f.db
                    .selection_destination_verified(
                        owner(),
                        root.clone(),
                        pass.clone(),
                        "asset-m".into(),
                        "original".into()
                    )
                    .await
                    .unwrap()
            );
        }
        let receipt_rows = marker_free_history_rows(&f.db, "download_receipts", "child", "asset-m");
        assert_eq!(
            receipt_rows.len(),
            expected_proof.len() + usize::from(positive)
        );
        assert!(expected_proof.iter().all(|row| receipt_rows.contains(row)));
        let requests = server.received_requests().await.unwrap().len();
        assert_eq!(requests, 2 + usize::from(positive));
        for round in 0..2 {
            let old_state = Arc::downgrade(&f.db);
            f = super::reopen_streaming_fixture(f).await;
            assert!(old_state.upgrade().is_none());
            let lookups = f.session.calls.load(Ordering::SeqCst);
            let quiet = stream_collision_child(&f, "asset-m", false).await;
            assert_eq!(f.session.calls.load(Ordering::SeqCst), lookups + 2);
            assert_eq!((quiet.downloaded, quiet.state_write_failures), (0, 0));
            assert!(quiet.failed.is_empty());
            if !positive {
                assert_eq!(quiet.skip_summary.uncertain, 1);
            }
            drop(quiet);
            preserved(&f.db);
            assert_eq!(
                marker_free_history_rows(&f.db, "download_receipts", "child", "asset-m"),
                receipt_rows
            );
            if let Some(expected) = &s_physical {
                assert_eq!(marker_free_history_file(&s), *expected);
            } else {
                assert!(!s.exists());
            }
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            eprintln!(
                "MARKER_FREE_HISTORY_REDISCOVERY case={case} round={round} fresh_lookups=2 additional_GETs=0 physical_and_all_rows=unchanged"
            );
        }
        eprintln!(
            "MARKER_FREE_HISTORY_CASE_COMPLETE case={case} successful_C1_GETs={} retained_C0_receipts={} rediscoveries=2",
            usize::from(positive),
            expected_proof.len()
        );
        // Bounded A rediscovery only. Q remains unfinished; no seal/token/config
        // promotion or globally quiet cycle is inferred from these direct calls.
    }
}

#[tokio::test]
async fn missing_original_receipt_cannot_classify_discovered_original() {
    use crate::download::baseline_matching::BaselineMatch;
    use crate::types::{EditedNaming, PhotoResolution};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::Ordering;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    fn signature(bytes: &[u8]) -> String {
        use sha1::{Digest as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        data_encoding::BASE64.encode(&[&[1][..], &hash.finalize()[..]].concat())
    }
    let c0 = b"\xff\xd8\xff\xe0original source";
    let c1 = b"\xff\xd8\xff\xe0first adjusted resource bytes";
    let c2 = b"\xff\xd8\xff\xe0newer adjusted resource with a different size";
    assert_ne!(c0.len(), c1.len());
    assert_ne!(c1.len(), c2.len());
    assert_ne!(c0.len(), c2.len());
    let server = wiremock::MockServer::start().await;
    for (name, bytes, expected) in [
        ("original", c0.as_slice(), 1_u64),
        ("adjusted", c1.as_slice(), 1),
        ("current", c2.as_slice(), 0),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .expect(expected)
            .mount(&server)
            .await;
    }
    let mut f = Fixture::new().await;
    f.dir.disable_cleanup(true);
    eprintln!(
        "DISCOVERED_ORIGINAL_DIRECT_FIXTURE retained={}",
        f.dir.path().display()
    );
    f.config.folder_structure = "none".into();
    f.config.resolution = PhotoResolution::Original;
    f.config.edited = true;
    f.config.edited_naming = EditedNaming::Suffix;
    f.config.refresh_metadata = false;
    {
        let mut values = f.session.records.lock().unwrap();
        values[0]["fields"]["filenameEnc"]["value"] = json!("photo.JPG");
        values[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/original",server.uri()),"fileChecksum":signature(c0),"size":c0.len()});
        values[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/adjusted",server.uri()),"fileChecksum":signature(c1),"size":c1.len()}});
        values[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
        values[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    }
    let initial = stream_collision_child(&f, "asset-m", false).await;
    assert_eq!((initial.downloaded, initial.state_write_failures), (2, 0));
    assert!(initial.failed.is_empty());
    drop(initial);
    let p = f.config.directory.join("photo.JPG");
    let e = f.config.directory.join("photo_edited.JPG");
    assert_eq!(marker_free_history_file(&p).0, c0);
    assert_eq!(marker_free_history_file(&e).0, c1);
    let physical = || {
        let mut files: Vec<_> = std::fs::read_dir(&f.config.directory)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                assert!(entry.file_type().unwrap().is_file());
                (entry.path(), marker_free_history_file(&entry.path()))
            })
            .collect();
        files.sort_by(|left, right| left.0.cmp(&right.0));
        files
    };
    let old_files = physical();
    f.session.records.lock().unwrap()[1]["fields"]["resJPEGFullRes"]["value"] = json!({"downloadURL":format!("{}/current",server.uri()),"fileChecksum":signature(c2),"size":c2.len()});
    let confirmation = f.pass.album.confirm_catalog_asset("asset-m").await.unwrap();
    let config = f.config.with_pass(&f.pass);
    assert!(config.selection_run.is_none());
    let mut planner = TaskPlanner::for_download(config.state_db.as_deref())
        .await
        .unwrap();
    let plan = planner
        .plan_download_asset(&confirmation.asset, &config)
        .await
        .unwrap();
    let adjusted: Vec<_> = plan
        .tasks
        .iter()
        .filter(|task| task.version_size == crate::state::VersionSizeKey::Adjusted)
        .collect();
    assert_eq!(adjusted.len(), 1);
    let task = adjusted[0];
    assert_eq!(
        (task.checksum.as_ref(), task.size),
        (signature(c2).as_str(), c2.len() as u64)
    );
    assert!(task.superseded_requested_path.is_none());
    let s = task.download_path.clone();
    assert_ne!(s, p);
    assert_ne!(s, e);
    assert!(!s.exists());
    let binding = task.publication_binding.as_deref().unwrap().clone();
    let (account, provider) = f.capture.owner.authenticated_keys().unwrap();
    assert_eq!(
        (binding.account_key.as_str(), binding.provider_key.as_str()),
        (account, provider)
    );
    assert_eq!(
        (
            binding.child.as_str(),
            binding.master.as_str(),
            binding.state_id.as_str()
        ),
        ("asset-m", "m", "asset-m")
    );
    assert_eq!(
        binding.origin_pass,
        crate::download::orchestration::generation::pass_key(&f.pass)
    );
    assert!(
        f.db.bound_download_receipts(
            binding.clone(),
            "adjusted".into(),
            signature(c2),
            c2.len() as u64
        )
        .await
        .unwrap()
        .is_empty()
    );
    let historical =
        f.db.bound_historical_downloads(
            binding.clone(),
            "adjusted".into(),
            signature(c2),
            c2.len() as u64,
        )
        .await
        .unwrap();
    assert_eq!(historical.len(), 1);
    assert_eq!(historical[0].native_path, SelectionPath::from_path(&e));
    assert_eq!(
        (
            historical[0].provider_checksum.as_str(),
            historical[0].provider_size
        ),
        (signature(c1).as_str(), c1.len() as u64)
    );
    let healthy = planner
        .match_existing(&confirmation.asset, &config, task)
        .await;
    assert!(
        matches!(healthy, BaselineMatch::Missing),
        "healthy discovered Original must classify history: {healthy:?}"
    );
    let calls = f.session.calls.load(Ordering::SeqCst);
    let gets = server.received_requests().await.unwrap().len();
    assert_eq!(gets, 2);
    let receipts = marker_free_history_rows(&f.db, "download_receipts", "child", "asset-m");
    assert_eq!(receipts.len(), 2);
    let original = marker_free_history_rows(&f.db, "download_receipts", "version_size", "original");
    assert_eq!(original.len(), 1);
    let (id, proof_hash): (String, String) = f.db.acquire_lock("save genuine Original proof before corruption").unwrap().query_row("SELECT id,proof_hash FROM download_receipts WHERE child='asset-m' AND version_size='original' AND native_path=?1", [SelectionPath::from_path(&p).key().unwrap()], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert!(!proof_hash.is_empty());
    let surviving: Vec<_> = receipts
        .iter()
        .filter(|row| !original.contains(row))
        .cloned()
        .collect();
    assert_eq!(surviving.len(), 1);
    let snapshot = || {
        [
            marker_free_history_rows(&f.db, "assets", "id", "asset-m"),
            marker_free_history_rows(&f.db, "asset_metadata_paths", "id", "asset-m"),
            marker_free_history_rows(
                &f.db,
                "asset_master_mappings",
                "asset_record_name",
                "asset-m",
            ),
            marker_free_history_rows(
                &f.db,
                "legacy_master_state_owners",
                "asset_record_name",
                "asset-m",
            ),
            marker_free_history_rows(&f.db, "provider_active_destinations", "child", "asset-m"),
            marker_free_history_rows(&f.db, "provider_active_decisions", "child", "asset-m"),
            marker_free_history_rows(&f.db, "reconciliation_paths", "id", "asset-m"),
        ]
    };
    let before = snapshot();
    assert_eq!(
        (before[0].len(), before[1].len(), before[2].len()),
        (2, 2, 1)
    );
    let stage =
        crate::download::file::temp_download_path(&s, &signature(c2), &config.temp_suffix).unwrap();
    assert!(!stage.exists());
    eprintln!(
        "DISCOVERED_ORIGINAL_DIRECT_HEALTHY result=Missing S={} marker=None selection_run=None GETs={gets} saved_original_id={id} proof_hash={proof_hash} deleted_row_backup={original:?}",
        s.display()
    );
    assert_eq!(f.db.acquire_lock("explicit exact compact-receipt corruption").unwrap().execute("DELETE FROM download_receipts WHERE id=?1 AND child='asset-m' AND version_size='original' AND native_path=?2",rusqlite::params![id,SelectionPath::from_path(&p).key().unwrap()]).unwrap(),1);
    let original_after =
        f.db.bound_cross_rendition_original(
            binding.clone(),
            crate::state::VersionSizeKey::Adjusted,
            crate::state::db::CrossRenditionOriginal {
                path: SelectionPath::from_path(&p),
                provider_checksum: signature(c0),
                provider_size: c0.len() as u64,
                local_checksum: Some(data_encoding::HEXLOWER.encode(&Sha256::digest(c0))),
            },
        )
        .await
        .unwrap();
    assert!(original_after.is_none());
    assert_eq!(
        f.db.bound_historical_downloads(
            binding.clone(),
            "adjusted".into(),
            signature(c2),
            c2.len() as u64
        )
        .await
        .unwrap(),
        historical
    );
    let matched = planner
        .match_existing(&confirmation.asset, &config, task)
        .await;
    let BaselineMatch::Uncertain(explanation) = matched else {
        panic!(
            "missing Original proof must not retain cached positive classification: {matched:?}"
        );
    };
    assert!(
        explanation
            .candidates
            .iter()
            .any(
                |candidate| candidate.path.as_ref() == Some(&SelectionPath::from_path(&p))
                    && candidate.reason == "candidate_bytes_not_verified_for_current_resource"
            ),
        "actual P candidate refusal required: {explanation:?}"
    );
    assert!(
        !explanation
            .candidates
            .iter()
            .any(|candidate| candidate.reason == "scoped_receipt_or_reservation_validation_failed")
    );
    assert_eq!(
        f.session.calls.load(Ordering::SeqCst),
        calls,
        "matcher must not reconfirm or replan after damage"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), gets);
    assert!(!s.exists() && !stage.exists());
    assert_eq!(physical(), old_files);
    assert_eq!(snapshot(), before);
    assert_eq!(
        marker_free_history_rows(&f.db, "download_receipts", "child", "asset-m"),
        surviving
    );
    assert!(
        marker_free_history_rows(&f.db, "download_receipts", "version_size", "original").is_empty()
    );
    assert!(
        f.db.bound_download_receipts(binding, "adjusted".into(), signature(c2), c2.len() as u64)
            .await
            .unwrap()
            .is_empty()
    );
    eprintln!(
        "DISCOVERED_ORIGINAL_DIRECT_COMPLETE actual={explanation:?} fresh_original_reader=None same_planner_asset_config_task=true provider_lookups=unchanged GETs={gets} files_surviving_rows=unchanged no_current_completion=true"
    );
    // This is the read-only matcher boundary, not a healthy damaged-root replay,
    // current selection admission, source checkpoint or whole-cycle proof.
}
