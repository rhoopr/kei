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
