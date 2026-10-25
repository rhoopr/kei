//! Normal configuration expansion must preserve prior files and pending identities.
use super::{MEDIA, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::state::SparseIdentityStore;
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

#[derive(Clone)]
struct Session {
    records: Arc<Vec<Value>>,
    cycle: usize,
    unresolved: bool,
    observed_historical: bool,
    omit_after_first: bool,
    inventories: Arc<AtomicUsize>,
    omissions: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(),"syncToken":"expansion-boundary","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(2));
        }
        if url.contains("/records/query?") {
            self.inventories.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0) as usize;
            let records: Vec<_> = self
                .records
                .chunks(2)
                .skip(offset)
                .flat_map(|p| p.iter().cloned())
                .collect();
            return Ok(json!({"records":records,"syncToken":"query-not-checkpoint"}));
        }
        if url.contains("/records/lookup?") {
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .filter(|record| {
                    let selected = requested
                        .iter()
                        .any(|r| r["recordName"] == record["recordName"]);
                    if selected && self.unresolved && record["recordName"] == "asset-january" {
                        self.omissions.fetch_add(1, Ordering::SeqCst);
                        false
                    } else {
                        selected
                    }
                })
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"], zone());
            let records = if self.observed_historical
                && self.cycle > 0
                && (!self.omit_after_first || self.cycle == 1)
            {
                let mut record = crate::test_helpers::sparse_shared_asset_record();
                record["recordName"] = json!("historical-source");
                record["fields"]["linkedShareRecordName"]["value"] = json!("historical-child");
                record["fields"]["linkedShareZoneName"]["value"] = json!("SharedSync-absent");
                record["fields"]["linkedShareZoneOwner"]["value"]["recordName"] =
                    json!("historical-owner");
                vec![record]
            } else {
                Vec::new()
            };
            return Ok(json!({"zones":[{"zoneID":zone(),"records":records,
                "syncToken":format!("expanded-after-{}",self.cycle),"moreComing":false}]}));
        }
        anyhow::bail!("unexpected expansion fixture endpoint: {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn v027_date_expansion_downloads_new_selection_then_two_quiet_reopens() {
    expansion(false, false, false, false).await;
}
#[tokio::test]
async fn v027_date_expansion_unconfirmed_identity_holds_then_recovers() {
    expansion(true, false, false, false).await;
}
#[tokio::test]
#[ignore = "known blocker: historical sparse obligations lack durable origin separation; do not bypass current replay safety"]
async fn v027_date_expansion_preserves_historical_uncertainty_without_global_hold() {
    expansion(false, true, false, false).await;
}

#[tokio::test]
async fn v027_current_sparse_source_observed_again_holds_current_cursor() {
    expansion(false, false, true, false).await;
}
#[tokio::test]
async fn v027_current_sparse_source_omitted_on_replay_stays_pending() {
    expansion(false, false, true, true).await;
}
async fn expansion(
    unavailable: bool,
    historical: bool,
    observed_historical: bool,
    omit_after_first: bool,
) {
    let server = crate::start_wiremock_or_skip!();
    for name in ["april", "january"] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let mut records = Vec::new();
    for (name, month, tag) in [("april", 4, 21), ("january", 1, 22)] {
        let mut entry = pair(
            name,
            &format!("{name}.JPG"),
            &format!("{}/{name}", server.uri()),
            tag,
        );
        let date = chrono::NaiveDate::from_ymd_opt(2026, month, 15)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc();
        entry[1]["fields"]["assetDate"]["value"] = json!(date.timestamp_millis());
        entry[1]["fields"]["addedDate"]["value"] = json!(date.timestamp_millis());
        records.extend(entry);
    }
    let records = Arc::new(records);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    let mut april_mtime = None;
    let mut january_mtime = None;
    let mut historical_before = None;
    for cycle in 0..if unavailable { 5 } else { 4 } {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if cycle == 0 && historical {
            use crate::state::{SparseAttemptOutcome, SparseEvidence, SparseSourceId};
            db.set_metadata(&crate::state::unresolved_identity_key(ZONE), "1")
                .await
                .unwrap();
            let evidence = SparseEvidence::new(
                r#"[1,"historical-child","SharedSync-absent","historical-owner"]"#.into(),
            );
            let row = db
                .observe_sparse_identity(
                    ZONE,
                    &SparseSourceId::new("historical-source"),
                    &evidence,
                    chrono::Utc::now(),
                )
                .await
                .unwrap();
            db.record_sparse_attempt(
                &row,
                SparseAttemptOutcome::Unresolved(evidence),
                chrono::Utc::now(),
            )
            .await
            .unwrap();
        }
        if cycle == 0 && historical {
            historical_before = db.sparse_identities(ZONE).await.unwrap().into_iter().next();
        }
        let prior_cursor = db.get_metadata(&key).await.unwrap();
        let session = Session {
            records: records.clone(),
            cycle,
            unresolved: unavailable && cycle == 1,
            observed_historical,
            omit_after_first,
            inventories: Arc::new(AtomicUsize::new(0)),
            omissions: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session.clone()),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: library.all(),
            exclude_ids: Arc::default(),
        };
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &key, vec![pass]);
        selected.library = library;
        let lower = crate::config::CreatedDateFilter::CaptureDate(
            chrono::NaiveDate::from_ymd_opt(2026, if cycle == 0 { 4 } else { 1 }, 1).unwrap(),
        );
        let mut config = make_run_cycle_config();
        config.filters.skip_created_before = Some(lower);
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut download = (*base(mode, exclude, groups, zone)).clone();
            download.folder_structure = "none".into();
            download.skip_created_before = Some(lower);
            download.retry.max_retries = 0;
            Arc::new(download)
        };
        let result = run_cycle(
            &[&selected],
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let april = root.join("april.JPG");
        assert_eq!(std::fs::read(&april).unwrap(), MEDIA);
        let mtime = std::fs::metadata(&april).unwrap().modified().unwrap();
        if cycle == 0 {
            april_mtime = Some(mtime);
        }
        assert_eq!(Some(mtime), april_mtime);
        let january = root.join("january.JPG");
        if observed_historical && cycle > 0 {
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert!(
                result.stats.identity_incomplete,
                "current sparse source must not be ignored"
            );
            assert_eq!(db.get_metadata(&key).await.unwrap(), prior_cursor);
            assert_eq!(
                std::fs::read(&january).unwrap(),
                MEDIA,
                "healthy newly selected item still completes"
            );
        } else if unavailable && cycle == 1 {
            assert!(
                session.omissions.load(Ordering::SeqCst) > 0,
                "current lookup fault must execute"
            );
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(
                db.get_metadata(&key).await.unwrap(),
                prior_cursor,
                "unconfirmed expansion must preserve exact prior cursor"
            );
            assert!(
                !january.exists(),
                "unconfirmed identity must not be guessed or downloaded"
            );
        } else {
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert!(
                result.can_advance_database_checkpoint(),
                "cycle {cycle}: {result:?}"
            );
            assert_eq!(
                result.stats.downloaded,
                usize::from(cycle == 0 || cycle == if unavailable { 2 } else { 1 }),
                "{result:?}"
            );
            assert_eq!(
                db.get_metadata(&key).await.unwrap(),
                Some(format!("expanded-after-{cycle}"))
            );
        }
        if cycle == 0 {
            assert!(!january.exists());
        }
        if january.exists() {
            assert_eq!(std::fs::read(&january).unwrap(), MEDIA);
            let mtime = std::fs::metadata(&january).unwrap().modified().unwrap();
            if let Some(prior) = january_mtime {
                assert_eq!(mtime, prior);
            } else {
                january_mtime = Some(mtime);
            }
        }
        if cycle == 1 {
            assert!(
                session.inventories.load(Ordering::SeqCst) > 0,
                "expanded selection needs current inventory"
            );
        }
        if !observed_historical && cycle >= if unavailable { 3 } else { 2 } {
            assert_eq!(
                session.inventories.load(Ordering::SeqCst),
                0,
                "quiet reopened configuration must not replay inventory"
            );
        }
        if historical {
            let rows = db.sparse_identities(ZONE).await.unwrap();
            assert_eq!(rows.len(), 1);
            let before = historical_before.as_ref().unwrap();
            let after = &rows[0];
            assert_eq!(after.library, before.library);
            assert!(
                after.source == before.source
                    && after.original == before.original
                    && after.observed == before.observed
                    && after.lookup == before.lookup
                    && after.generation == before.generation
                    && after.deletion_checkpoint == before.deletion_checkpoint,
                "historical identity evidence and ownership must survive unchanged"
            );
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn v027_destination_root_replaced_during_transfer_preserves_work_and_reopens() {
    let server = crate::start_wiremock_or_skip!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    let held_root = dir.path().join("interrupted-media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    Mock::given(method("GET"))
        .and(path("/april"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    let next_gets = Arc::new(AtomicUsize::new(0));
    let counter = next_gets.clone();
    let original_root = root.clone();
    let displaced_root = held_root.clone();
    Mock::given(method("GET"))
        .and(path("/january"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                // A single worker has published April before starting January.
                assert_eq!(
                    std::fs::read(original_root.join("april.JPG")).unwrap(),
                    MEDIA
                );
                std::fs::rename(&original_root, &displaced_root).unwrap();
                std::fs::write(&original_root, b"external replacement, never overwrite").unwrap();
            }
            ResponseTemplate::new(200).set_body_bytes(MEDIA)
        })
        .mount(&server)
        .await;
    let mut records = pair("april", "april.JPG", &format!("{}/april", server.uri()), 21);
    records.extend(pair(
        "january",
        "january.JPG",
        &format!("{}/january", server.uri()),
        22,
    ));
    let records = Arc::new(records);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let key = sync_token_key(ZONE);
    let mut april_mtime = None;
    let mut completed_gets = None;
    for cycle in 0..3 {
        if cycle == 1 {
            assert_eq!(
                std::fs::read(&root).unwrap(),
                b"external replacement, never overwrite"
            );
            std::fs::remove_file(&root).unwrap();
            std::fs::rename(&held_root, &root).unwrap();
        }
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = Session {
            records: records.clone(),
            cycle,
            unresolved: false,
            observed_historical: false,
            omit_after_first: false,
            inventories: Arc::new(AtomicUsize::new(0)),
            omissions: Arc::new(AtomicUsize::new(0)),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session.clone()),
            Arc::new(zone()),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let mut selected = make_run_cycle_library_state_with_passes(
            ZONE,
            &key,
            vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::default(),
            }],
        );
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.concurrent_downloads = 1;
            config.retry.max_retries = 0;
            Arc::new(config)
        };
        if cycle == 1 {
            db.make_selection_retry_due(baseline_owner(), "asset-january".into())
                .await
                .unwrap();
        }
        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await;
        let original = if cycle == 0 {
            held_root.join("april.JPG")
        } else {
            root.join("april.JPG")
        };
        assert_eq!(
            std::fs::read(&original).unwrap(),
            MEDIA,
            "replacement must not destroy completed bytes"
        );
        let mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
        if cycle == 0 {
            april_mtime = Some(mtime);
        }
        assert_eq!(Some(mtime), april_mtime);
        if cycle == 0 {
            assert_eq!(
                next_gets.load(Ordering::SeqCst),
                1,
                "replacement hook must fire during the real transfer"
            );
            match &result {
                Ok(result) => assert!(!result.can_advance_database_checkpoint(), "{result:?}"),
                Err(error) => assert!(
                    format!("{error:#}").contains("Not a directory")
                        || format!("{error:#}").contains("not a directory"),
                    "{error:#}"
                ),
            }
            assert_eq!(
                std::fs::read(&root).unwrap(),
                b"external replacement, never overwrite"
            );
            let pending:i64=db.acquire_lock("prove interrupted destination ownership").unwrap().query_row(
                "SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND asset_id='asset-january' AND admitted=1 AND verified_media=0",
                [ZONE],|r|r.get(0)).unwrap();
            assert!(
                pending > 0,
                "interrupted destination must retain its admitted owner"
            );
            let cursor = db.get_metadata(&key).await.unwrap();
            assert!(
                cursor.is_none() || cursor.as_deref() == Some("expanded-after-0"),
                "only the accounted source successor can be saved"
            );
            if result.is_err() {
                assert!(cursor.is_none(), "an aborted cycle cannot commit a cursor");
            }
        } else {
            let result = result.unwrap();
            assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
            assert!(result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(std::fs::read(root.join("january.JPG")).unwrap(), MEDIA);
            let pending:i64=db.acquire_lock("prove interrupted destination resolved").unwrap().query_row(
                "SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND asset_id='asset-january' AND admitted=1 AND verified_media=0",
                [ZONE],|r|r.get(0)).unwrap();
            assert_eq!(pending, 0);
            if cycle == 1 {
                completed_gets = Some(next_gets.load(Ordering::SeqCst));
                assert!(
                    completed_gets.unwrap() <= 2,
                    "at most one recovery request after the interrupted transfer"
                );
            } else {
                assert_eq!(Some(next_gets.load(Ordering::SeqCst)), completed_gets);
                assert_eq!(result.stats.downloaded, 0);
                assert_eq!(
                    session.inventories.load(Ordering::SeqCst),
                    0,
                    "quiet recovery must not repeat baseline"
                );
            }
        }
    }
    server.verify().await;
}
