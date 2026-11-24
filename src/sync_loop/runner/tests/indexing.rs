//! Provisional provider inventories through real cycles, files, and SQLite reopen.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

use crate::icloud::photos::{PhotoLibrary, PhotosSession};
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, indexing_reconciliation_key, run_cycle};
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_captured_cycle_album,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_album, make_shared_session_for_run_cycle,
};
use crate::{download, retry, state};

const ZONE: &str = "PrimarySync";
const CURSOR: &str = "sync_token:PrimarySync";
const BYTES: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];

fn indexing_owner() -> state::db::account::AccountOwner {
    state::db::account::AccountOwner::authenticated(
        "indexing@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo": {"dsid": "indexing-fixture"}})).unwrap(),
    )
    .unwrap()
}

async fn indexing_db(path: &std::path::Path) -> Arc<state::SqliteStateDb> {
    Arc::new(
        state::SqliteStateDb::open_owned(path, &indexing_owner())
            .await
            .unwrap(),
    )
}

fn indexing_capture(db: &Arc<state::SqliteStateDb>) -> crate::icloud::photos::inbox::ShadowCapture {
    crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), indexing_owner(), "com")
}

#[derive(Clone)]
struct IndexingSession {
    readiness: Arc<Mutex<&'static str>>,
    readiness_probes: Arc<AtomicUsize>,
    unknown_observed: Arc<tokio::sync::Notify>,
    records: Arc<Vec<Value>>,
    queries: Arc<AtomicUsize>,
    changes: Arc<Mutex<Vec<String>>>,
    interrupt: Option<CancellationToken>,
    regress_after_query: bool,
    catalog: Option<Arc<AtomicBool>>,
    change_records: Arc<Vec<Value>>,
    reject_cursor: bool,
    successor: &'static str,
    query_token: &'static str,
}

#[async_trait::async_trait]
impl PhotosSession for IndexingSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
            self.readiness_probes.fetch_add(1, Ordering::SeqCst);
            if *self.readiness.lock().unwrap() == "UNKNOWN" {
                self.unknown_observed.notify_one();
            }
            return Ok(
                json!({"records":[{"fields":{"state":{"value":*self.readiness.lock().unwrap()}}}]}),
            );
        }
        if request["query"]["recordType"] == "CPLAlbumByPositionLive" {
            let finished = *self.readiness.lock().unwrap() == "FINISHED";
            let names = if finished { vec!["A", "B"] } else { vec!["A"] };
            let records: Vec<_> = names.into_iter().map(|name| json!({
                "recordName":format!("album-{name}"),"recordType":"CPLAlbum",
                "fields":{"albumNameEnc":{"value":base64::engine::general_purpose::STANDARD.encode(name)}}
            })).collect();
            if let Some(catalog) = &self.catalog
                && catalog.swap(false, Ordering::SeqCst)
            {
                *self.readiness.lock().unwrap() = "FINISHED";
            }
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            let scope = &request["zones"][0]["zoneID"];
            let cursor = request["zones"][0]["syncToken"]
                .as_str()
                .unwrap_or("<bootstrap>")
                .to_owned();
            self.changes.lock().unwrap().push(cursor);
            if request["zones"][0]["syncToken"].is_null() {
                return Ok(json!({"zones": [{
                    "zoneID": scope,
                    "records": self.records.as_ref(), "syncToken": "query-candidate", "moreComing": false
                }]}));
            }
            if self.reject_cursor {
                return Ok(json!({"zones":[{"zoneID":scope,
                    "serverErrorCode":"BAD_REQUEST","reason":"expired retained cursor","syncToken":"","moreComing":false}]}));
            }
            // Deliberately no events for omitted assets: only a full inventory
            // can recover them, even though every incremental request succeeds.
            return Ok(json!({"zones":[{"zoneID":scope,
                "records":self.change_records.as_ref(),"syncToken":self.successor,"moreComing":false}]}));
        }
        if url.contains("/records/lookup?") {
            let records: Vec<_> = request["records"].as_array().unwrap().iter().map(|wanted| {
                self.change_records.iter().chain(self.records.iter())
                    .find(|record| record["recordName"] == wanted["recordName"])
                    .cloned().unwrap_or_else(|| json!({"recordName": wanted["recordName"], "serverErrorCode": "UNKNOWN_ITEM"}))
            }).collect();
            return Ok(json!({"records": records}));
        }
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response(
                if self.catalog.is_some() && body.contains("album-A") {
                    0
                } else {
                    (self.records.len() / 2) as u64
                },
            ));
        }
        assert!(url.contains("/records/query?"), "{url}");
        self.queries.fetch_add(1, Ordering::SeqCst);
        if let Some(cancel) = &self.interrupt {
            cancel.cancel();
        }
        if self.regress_after_query {
            *self.readiness.lock().unwrap() = "RUNNING";
        }
        let offset = request["query"]["filterBy"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|f| f["fieldName"] == "startRank")
            .and_then(|f| f["fieldValue"]["value"].as_u64())
            .unwrap_or(0);
        Ok(
            json!({"records":if offset == 0 && !(self.catalog.is_some() && body.contains("album-A")) { self.records.as_ref().clone() } else { Vec::new() },
            "syncToken":self.query_token}),
        )
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn provider(readiness: &'static str, ids: &[&str], base: &str) -> IndexingSession {
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(BYTES));
    let records = ids
        .iter()
        .flat_map(|id| {
            let mut page = full_album_page_with_download(
                ZONE,
                id,
                "query-candidate",
                &format!("{base}/{id}.jpg"),
                BYTES.len() as u64,
                &checksum,
            );
            page["records"][0]["fields"]["filenameEnc"]["value"] =
                json!(base64::engine::general_purpose::STANDARD.encode(format!("{id}.jpg")));
            page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                json!("_defaultOwner");
            page["records"].as_array().unwrap().clone()
        })
        .collect();
    IndexingSession {
        readiness: Arc::new(Mutex::new(readiness)),
        readiness_probes: Arc::default(),
        unknown_observed: Arc::default(),
        records: Arc::new(records),
        queries: Arc::new(AtomicUsize::new(0)),
        changes: Arc::new(Mutex::new(Vec::new())),
        interrupt: None,
        regress_after_query: false,
        catalog: None,
        change_records: Arc::new(Vec::new()),
        reject_cursor: false,
        successor: "trusted-after",
        query_token: "query-candidate",
    }
}

async fn library(
    provider: &IndexingSession,
    db: &Arc<state::SqliteStateDb>,
) -> crate::sync_cycle::LibraryState {
    let mut library = make_run_cycle_library_state_with_album(
        ZONE,
        CURSOR,
        make_captured_cycle_album(ZONE, indexing_capture(db), Box::new(provider.clone())),
    );
    // Use the real readiness constructor and request path, not the legacy stub.
    library.library = PhotoLibrary::new(
        "https://example.com".into(),
        Arc::new(HashMap::new()),
        Box::new(provider.clone()),
        Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
        "private".into(),
        retry::RetryConfig {
            max_retries: 0,
            ..retry::RetryConfig::default()
        },
    )
    .await
    .unwrap();
    library.library.shadow_capture = Some(indexing_capture(db));
    library
}

async fn library_with_album_token(
    source: &IndexingSession,
    db: &Arc<state::SqliteStateDb>,
    token: &'static str,
) -> crate::sync_cycle::LibraryState {
    let mut selected = library(source, db).await;
    let mut album = source.clone();
    album.query_token = token;
    selected.plan.passes.insert(
        0,
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: library(&album, db)
                .await
                .library
                .albums()
                .await
                .unwrap()
                .remove("A")
                .unwrap()
                .pop()
                .unwrap(),
            exclude_ids: Arc::default(),
        },
    );
    selected.pass_scope.include_albums = true;
    selected
}

#[tokio::test]
async fn cursorless_split_tokens_require_a_fresh_unanimous_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let media = tmp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    for round in 0..3 {
        let db = indexing_db(&tmp.path().join("state.db")).await;
        let source = provider("FINISHED", &[], "");
        let token = if round == 0 {
            "split"
        } else {
            source.query_token
        };
        let selected = library_with_album_token(&source, &db, token).await;
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store.clone());
        let result = run_cycle(
            &[&selected],
            &config,
            Some(store.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let cursor = db.get_metadata(CURSOR).await.unwrap();
        match round {
            0 => {
                assert_eq!(cursor, None);
                assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
                assert!(!result.can_advance_database_checkpoint());
            }
            1 => {
                assert!(source.queries.load(Ordering::SeqCst) > 0);
                assert_eq!(
                    cursor.as_deref(),
                    Some("query-candidate"),
                    "{:?}",
                    result.stats
                );
            }
            _ => {
                assert_eq!(source.queries.load(Ordering::SeqCst), 0);
                assert_eq!(*source.changes.lock().unwrap(), ["query-candidate"]);
                assert!(result.can_advance_database_checkpoint());
            }
        }
    }
}

#[tokio::test]
async fn indexing_split_pass_tokens_require_quiet_inventory_then_replay_retained_cursor() {
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("state.db");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let hold = indexing_reconciliation_key(ZONE);
    let replay = crate::sync_cycle::indexing_replay_key(ZONE);
    for round in 0..8 {
        let db = indexing_db(&database).await;
        if round == 0 {
            db.set_metadata(CURSOR, "trusted-before").await.unwrap();
            db.set_metadata(&hold, "1").await.unwrap();
            db.set_metadata(
                ENUM_CONFIG_HASH_KEY,
                &download::compute_config_hash(&config),
            )
            .await
            .unwrap();
        }
        let cancel = CancellationToken::new();
        let mut source = provider("FINISHED", &[], "");
        if round == 0 || round == 5 {
            // Raw provider activity matters even when no asset event is emitted.
            source.change_records =
                Arc::new(vec![json!({"recordType":"Unknown","recordName":"other"})]);
        } else if round == 2 {
            source.interrupt = Some(cancel.clone());
        }
        let token = if matches!(round, 1 | 4) {
            ""
        } else {
            "other-query"
        };
        let selected = library_with_album_token(&source, &db, token).await;
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store.clone());
        let result = run_cycle(
            &[&selected],
            &config,
            Some(store.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(
            db.get_metadata(&hold).await.unwrap().is_some(),
            round < 3,
            "round {round}: {:?}",
            result.stats
        );
        assert_eq!(
            db.get_metadata(&replay).await.unwrap().is_some(),
            (3..=5).contains(&round),
            "round {round}: {:?}, requests: {:?}",
            result.stats,
            source.changes.lock().unwrap()
        );
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some(if round < 6 {
                "trusted-before"
            } else {
                "trusted-after"
            })
        );
        if round == 3 {
            assert_eq!(
                *source.changes.lock().unwrap(),
                ["trusted-before", "trusted-after"]
            );
        } else if round == 4 {
            // Even the same-cycle retry lacked a token. Keep replay debt on disk.
            assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
            assert_eq!(*source.changes.lock().unwrap(), ["trusted-before"]);
            assert!(!result.can_advance_database_checkpoint());
        } else if round == 5 || round == 6 {
            assert_eq!(
                *source.changes.lock().unwrap(),
                ["trusted-before", "trusted-after"]
            );
            assert_eq!(result.can_advance_database_checkpoint(), round == 6);
        } else if round == 7 {
            assert_eq!(*source.changes.lock().unwrap(), ["trusted-after"]);
            assert_eq!(source.queries.load(Ordering::SeqCst), 0);
            assert!(result.can_advance_database_checkpoint());
        }
    }
}

#[tokio::test]
async fn indexing_restart_recovers_omitted_assets_and_reuses_downloaded_files() {
    let server = wiremock::MockServer::start().await;
    for id in ["existing", "provisional", "omitted"] {
        Mock::given(method("GET"))
            .and(path(format!("/{id}.jpg")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
            .expect(1)
            .mount(&server)
            .await;
    }
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("state.db");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let config = make_run_cycle_config();
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let hold = indexing_reconciliation_key(ZONE);
    let mut existing_path = None;
    for (step, readiness, ids) in [
        (0, "FINISHED", vec!["existing"]),
        (1, "RUNNING", vec!["provisional"]),
        (2, "RUNNING", vec![]),
        (3, "FINISHED", vec!["existing", "provisional", "omitted"]),
        (4, "FINISHED", vec![]),
    ] {
        let inner = indexing_db(&database).await;
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let provider = provider(readiness, &ids, &server.uri());
        let library = library(&provider, &inner).await;
        let build = make_run_cycle_download_config_builder(&media, db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "step {step}: {:?}", result.stats);
        let rows = inner.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(
            rows.len(),
            if step == 0 {
                1
            } else if step < 3 {
                2
            } else {
                3
            },
            "step {step}"
        );
        for row in &rows {
            assert_eq!(
                std::fs::read(row.local_path.as_ref().unwrap()).unwrap(),
                BYTES
            );
        }
        if step == 0 {
            existing_path = rows[0].local_path.clone();
        }
        assert_eq!(
            std::fs::read(existing_path.as_ref().unwrap()).unwrap(),
            BYTES
        );
        assert_eq!(
            inner.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some(if step < 4 {
                "query-candidate"
            } else {
                "trusted-after"
            })
        );
        assert_eq!(
            inner.get_metadata(&hold).await.unwrap().is_some(),
            step == 1 || step == 2
        );
        assert_eq!(
            result.can_advance_database_checkpoint(),
            step == 0 || step == 4
        );
        if step == 1 || step == 2 {
            assert!(provider.changes.lock().unwrap().is_empty());
        }
        if step == 3 {
            assert!(provider.queries.load(Ordering::SeqCst) > 0);
            assert!(
                provider.changes.lock().unwrap().is_empty(),
                "held inventory must defer its bridge"
            );
            assert!(
                inner
                    .get_metadata(&crate::sync_cycle::indexing_replay_key(ZONE))
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(
                result.stats.downloaded, 1,
                "reuse both previously downloaded assets"
            );
        }
        if step == 4 {
            assert_eq!(
                provider.queries.load(Ordering::SeqCst),
                0,
                "steady state uses incremental sync"
            );
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(*provider.changes.lock().unwrap(), ["query-candidate"]);
            assert!(
                inner
                    .get_metadata(&crate::sync_cycle::indexing_replay_key(ZONE))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
    server.verify().await;
}

#[tokio::test(start_paused = true)]
async fn indexing_unknown_recovers_in_cycle_or_cancels_during_backoff() {
    for cancel_during_backoff in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = indexing_db(&root.path().join("state.db")).await;
        let config = make_run_cycle_config();
        db.set_metadata(CURSOR, "trusted-before").await.unwrap();
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .unwrap();
        let provider = provider("FINISHED", &[], "");
        let selected = library(&provider, &db).await;
        *provider.readiness.lock().unwrap() = "UNKNOWN";
        let (_dir, session) = make_shared_session_for_run_cycle().await;
        let build = make_run_cycle_download_config_builder(root.path(), db.clone());
        let cancel = CancellationToken::new();
        let libraries = [&selected];
        let cycle = run_cycle(
            &libraries,
            &config,
            Some(db.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &cancel,
        );
        let (result, ()) = tokio::join!(cycle, async {
            provider.unknown_observed.notified().await;
            if cancel_during_backoff {
                cancel.cancel();
            } else {
                *provider.readiness.lock().unwrap() = "FINISHED";
            }
        });
        let result = result.unwrap();
        if cancel_during_backoff {
            assert_eq!(provider.readiness_probes.load(Ordering::SeqCst), 2);
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert!(provider.changes.lock().unwrap().is_empty());
            assert_eq!(
                db.get_metadata(CURSOR).await.unwrap().as_deref(),
                Some("trusted-before")
            );
            assert!(!result.can_advance_database_checkpoint());
        } else {
            assert_eq!(result.failed_count, 0);
            assert_eq!(*provider.changes.lock().unwrap(), ["trusted-before"]);
            assert_eq!(
                db.get_metadata(CURSOR).await.unwrap().as_deref(),
                Some("trusted-after")
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn indexing_failed_interrupted_or_uncommitted_reconciliation_retains_hold_and_cursor() {
    for fault in [
        "malformed",
        "interrupted",
        "checkpoint_write",
        "regressed",
        "unknown",
    ] {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let old_file = media.join("old.jpg");
        std::fs::write(&old_file, BYTES).unwrap();
        let config = make_run_cycle_config();
        let hold = indexing_reconciliation_key(ZONE);
        {
            let db = indexing_db(&database).await;
            db.set_metadata(CURSOR, "trusted-before").await.unwrap();
            db.set_metadata(
                ENUM_CONFIG_HASH_KEY,
                &download::compute_config_hash(&config),
            )
            .await
            .unwrap();
            // Unknown marker payloads remain obligations; they are not ignored.
            db.set_metadata(&hold, "future-version").await.unwrap();
            let row = crate::test_helpers::TestAssetRecord::new("old")
                .library(ZONE)
                .build();
            db.upsert_seen(&row).await.unwrap();
            db.mark_downloaded(ZONE, "old", "original", &old_file, "old-checksum", None)
                .await
                .unwrap();
            db.set_metadata_capture_revision_for_test(
                ZONE,
                "old",
                state::METADATA_CAPTURE_REVISION,
            );
        }
        let inner = indexing_db(&database).await;
        if fault == "checkpoint_write" {
            inner
                .acquire_lock("inject inventory qualification failure")
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER indexing_replay_fault BEFORE INSERT ON metadata
                 WHEN NEW.key = '{}'
                 BEGIN SELECT RAISE(ABORT, 'injected inventory qualification failure'); END;",
                    crate::sync_cycle::indexing_replay_key(ZONE),
                ))
                .unwrap();
        }
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let mut provider = provider("FINISHED", &[], "http://unused.invalid");
        let cancel = CancellationToken::new();
        if fault == "malformed" {
            let mut bad = full_album_page_with_download(
                ZONE,
                "bad",
                "query-candidate",
                "http://unused.invalid",
                8,
                "checksum",
            );
            bad["records"][1]["fields"]["assetDate"]["value"] = Value::Null;
            bad["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
                json!("_defaultOwner");
            provider.records = Arc::new(bad["records"].as_array().unwrap().clone());
        }
        if fault == "interrupted" {
            provider.interrupt = Some(cancel.clone());
        }
        if fault == "regressed" {
            provider.regress_after_query = true;
        }
        let library = library(&provider, &inner).await;
        if fault == "unknown" {
            *provider.readiness.lock().unwrap() = "UNKNOWN";
        }
        let build = make_run_cycle_download_config_builder(&media, db.clone());
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await;
        let result = result.unwrap();
        assert!(!result.can_advance_database_checkpoint(), "{fault}");
        if fault == "unknown" {
            assert!(result.failed_count > 0);
            assert_eq!(provider.readiness_probes.load(Ordering::SeqCst), 4);
            assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
            assert!(provider.changes.lock().unwrap().is_empty());
        }
        if fault == "checkpoint_write" {
            assert!(provider.queries.load(Ordering::SeqCst) > 0);
            assert!(
                capture
                    .events()
                    .iter()
                    .any(|event| event.field("error").is_some_and(
                        |error| error.contains("injected inventory qualification failure")
                    )),
                "qualification trigger must actually execute"
            );
            assert!(
                inner
                    .get_metadata(&crate::sync_cycle::indexing_replay_key(ZONE))
                    .await
                    .unwrap()
                    .is_none()
            );
        }
        drop(build);
        drop(db);
        drop(inner);
        let reopened = indexing_db(&database).await;
        assert_eq!(
            reopened.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("trusted-before"),
            "{fault}"
        );
        assert!(
            reopened.get_metadata(&hold).await.unwrap().is_some(),
            "{fault}"
        );
        assert_eq!(std::fs::read(&old_file).unwrap(), BYTES, "{fault}");
        assert_eq!(
            reopened.get_downloaded_page(0, 10).await.unwrap().len(),
            1,
            "{fault}"
        );
    }
}

#[tokio::test]
async fn indexing_provisional_listing_preserves_membership_pending_rows_and_media() {
    for scenario in ["RUNNING", "REGRESSED", "QUALIFIED"] {
        let readiness = if scenario == "RUNNING" {
            "RUNNING"
        } else {
            "FINISHED"
        };
        use crate::commands::{AlbumPass, PassKind};
        use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig};
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let media_path = media.join("old.jpg");
        std::fs::write(&media_path, BYTES).unwrap();
        let db = indexing_db(&database).await;
        let config = make_run_cycle_config();
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .unwrap();
        db.set_metadata(CURSOR, "trusted-before").await.unwrap();
        db.upsert_album_container(ZONE, "trip", "Trip", "album")
            .await
            .unwrap();
        let generation = db
            .start_album_membership_snapshot(ZONE, "trip", None)
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            ZONE,
            "trip",
            generation,
            "asset-old",
            Some("old"),
            "full",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot(ZONE, "trip", generation)
            .await
            .unwrap();
        for id in ["old", "pending-omitted"] {
            db.upsert_seen(
                &crate::test_helpers::TestAssetRecord::new(id)
                    .library(ZONE)
                    .build(),
            )
            .await
            .unwrap();
        }
        db.mark_downloaded(ZONE, "old", "original", &media_path, "local-checksum", None)
            .await
            .unwrap();
        db.set_metadata_capture_revision_for_test(ZONE, "old", state::METADATA_CAPTURE_REVISION);
        db.set_metadata(&indexing_reconciliation_key(ZONE), "1")
            .await
            .unwrap();
        let mut provider = provider(readiness, &[], "http://unused.invalid");
        provider.regress_after_query = scenario == "REGRESSED";
        provider.change_records = Arc::new(vec![json!({
            "recordName":"asset-old-IN-trip","recordType":"CPLContainerRelation","deleted":true
        })]);
        let mut library = library(&provider, &db).await;
        library.plan.passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::new(
                PhotoAlbumConfig {
                    params: Arc::new(HashMap::new()),
                    service_endpoint: Arc::from("https://example.com"),
                    name: Arc::from("Trip"),
                    list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
                    obj_type: Arc::from("CPLContainerRelationNotDeletedByAssetDate:trip"),
                    query_filter: Some(Arc::new(
                        json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":"trip"}}]),
                    )),
                    page_size: 100,
                    zone_id: Arc::new(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"})),
                    retry_config: retry::RetryConfig::default(),
                    container_id: Some(Arc::from("trip")),
                    cross_zone_sources: Vec::new(),
                },
                Box::new(provider.clone()),
            ),
            exclude_ids: Arc::default(),
        }];
        library.plan.passes[0]
            .album
            .set_shadow_capture(indexing_capture(&db), Arc::from("private"));
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store.clone());
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let result = run_cycle(
            &[&library],
            &config,
            Some(store.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!result.can_advance_database_checkpoint());
        assert_eq!(std::fs::read(&media_path).unwrap(), BYTES);
        assert_eq!(
            db.get_pending()
                .await
                .unwrap()
                .iter()
                .map(|row| row.id.as_ref())
                .collect::<Vec<_>>(),
            ["pending-omitted"]
        );
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("trusted-before")
        );
        assert_eq!(
            db.get_metadata(&indexing_reconciliation_key(ZONE))
                .await
                .unwrap()
                .is_some(),
            scenario != "QUALIFIED"
        );
        assert_eq!(
            db.get_metadata(&crate::sync_cycle::indexing_replay_key(ZONE))
                .await
                .unwrap()
                .is_some(),
            scenario == "QUALIFIED"
        );
        assert!(provider.changes.lock().unwrap().is_empty());
        let before = rusqlite::Connection::open(&database).unwrap().query_row::<i64,_,_>(
            "SELECT is_deleted FROM asset_album_memberships WHERE asset_record_name='asset-old'",[],|r|r.get(0)).unwrap();
        assert_eq!(
            before, 0,
            "even a qualified inventory defers its deletion-capable replay"
        );
        if scenario == "QUALIFIED" {
            let completed: i64 = rusqlite::Connection::open(&database)
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM album_membership_snapshots WHERE status='complete'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                completed, 1,
                "inventory alone cannot certify the new snapshot"
            );
            run_cycle(
                &[&library],
                &config,
                Some(store.as_ref()),
                &build,
                download::DownloadControls::download_hidden(),
                &shared,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(*provider.changes.lock().unwrap(), ["trusted-before"]);
            assert_eq!(std::fs::read(&media_path).unwrap(), BYTES);
        }
        drop(build);
        drop(store);
        drop(db);
        let connection = rusqlite::Connection::open(&database).unwrap();
        let membership:(i64,i64) = connection.query_row("SELECT generation,is_deleted FROM asset_album_memberships WHERE asset_record_name='asset-old'", [], |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(
            membership,
            (generation, i64::from(scenario == "QUALIFIED")),
            "deletion is deferred until a previously qualified inventory permits trusted replay"
        );
        let snapshots: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM album_membership_snapshots WHERE status='complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            snapshots,
            1 + i64::from(scenario == "QUALIFIED"),
            "only the subsequent trusted replay may certify a new album snapshot: {scenario}"
        );
    }
}

#[tokio::test]
async fn indexing_hold_write_failure_stops_before_listing_or_downloading() {
    let root = tempfile::tempdir().unwrap();
    let inner = indexing_db(&root.path().join("state.db")).await;
    inner.set_metadata(CURSOR, "trusted-before").await.unwrap();
    inner
        .acquire_lock("inject indexing hold failure")
        .unwrap()
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER indexing_hold_fault BEFORE INSERT ON metadata
         WHEN NEW.key = '{}'
         BEGIN SELECT RAISE(ABORT, 'injected indexing hold failure'); END;",
            indexing_reconciliation_key(ZONE),
        ))
        .unwrap();
    let db: Arc<dyn download::DownloadStore> = inner.clone();
    let provider = provider("RUNNING", &["new-photo"], "http://unused.invalid");
    let library = library(&provider, &inner).await;
    let config = make_run_cycle_config();
    let build = make_run_cycle_download_config_builder(root.path(), db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        &build,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    let error = result.expect_err("native indexing hold write must fail");
    assert!(
        format!("{error:#}").contains("injected indexing hold failure"),
        "{error:#}"
    );
    assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
    assert!(provider.changes.lock().unwrap().is_empty());
    assert_eq!(
        inner.get_metadata(CURSOR).await.unwrap().as_deref(),
        Some("trusted-before")
    );
    assert!(inner.get_downloaded_page(0, 10).await.unwrap().is_empty());
    assert!(
        inner
            .get_metadata(&indexing_reconciliation_key(ZONE))
            .await
            .unwrap()
            .is_none()
    );
    assert!(inner.get_pending().await.unwrap().is_empty());
}

#[tokio::test]
async fn indexing_cross_zone_source_requires_reconciliation_without_permanent_hold() {
    let root = tempfile::tempdir().unwrap();
    let db = indexing_db(&root.path().join("state.db")).await;
    let primary = provider("FINISHED", &[], "http://unused.invalid");
    let shared = provider("RUNNING", &[], "http://unused.invalid");
    let mut library = library(&primary, &db).await;
    library.cross_zone_libraries.push(
        PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(shared.clone()),
            Arc::new(json!({"zoneName":"SharedSync-TEST","ownerRecordName":"_defaultOwner"})),
            "shared".into(),
            retry::RetryConfig::default(),
        )
        .await
        .unwrap(),
    );
    let (finished, hold) =
        crate::sync_cycle::prepare_library_indexing(&library, Some(db.as_ref()), false)
            .await
            .unwrap();
    assert!(!finished);
    assert!(hold.is_some());
    *shared.readiness.lock().unwrap() = "FINISHED";
    let (finished, hold) =
        crate::sync_cycle::prepare_library_indexing(&library, Some(db.as_ref()), false)
            .await
            .unwrap();
    assert!(finished);
    assert!(
        hold.is_some(),
        "the selected zone must still reconcile after its source finishes"
    );
    db.delete_metadata_by_prefix(&indexing_reconciliation_key(ZONE))
        .await
        .unwrap();
    let (finished, hold) =
        crate::sync_cycle::prepare_library_indexing(&library, Some(db.as_ref()), false)
            .await
            .unwrap();
    assert!(finished);
    assert!(
        hold.is_none(),
        "an unrelated source marker cannot recreate a completed selected-zone hold"
    );
}

#[tokio::test]
async fn indexing_normal_recovery_restores_failed_and_omitted_photos_across_restart() {
    let server = wiremock::MockServer::start().await;
    for id in ["known", "omitted", "late"] {
        Mock::given(method("GET"))
            .and(path(format!("/{id}.jpg")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
            .expect(1)
            .mount(&server)
            .await;
    }
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("state.db");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let config = make_run_cycle_config();
    let hold = indexing_reconciliation_key(ZONE);
    {
        let db = indexing_db(&database).await;
        db.set_metadata(CURSOR, "trusted-before").await.unwrap();
        db.set_metadata(
            ENUM_CONFIG_HASH_KEY,
            &download::compute_config_hash(&config),
        )
        .await
        .unwrap();
        db.set_metadata(&hold, "1").await.unwrap();
        db.upsert_seen(
            &crate::test_helpers::TestAssetRecord::new("asset-known")
                .library(ZONE)
                .build(),
        )
        .await
        .unwrap();
        db.mark_failed(ZONE, "asset-known", "original", "prior failed transfer")
            .await
            .unwrap();
    }
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    for round in 0..4 {
        let inner = indexing_db(&database).await;
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let ids = if round == 0 {
            vec!["known", "omitted"]
        } else {
            vec!["known", "omitted", "late"]
        };
        let mut session = provider("FINISHED", &ids, &server.uri());
        if round == 1 {
            session.change_records = provider("FINISHED", &["late"], &server.uri()).records;
        }
        let provider = session;
        let library = library(&provider, &inner).await;
        let build = make_run_cycle_download_config_builder(&media, db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_eq!(result.can_advance_database_checkpoint(), round > 0);
        assert_eq!(
            inner.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some(if round == 0 {
                "trusted-before"
            } else {
                "trusted-after"
            })
        );
        assert!(inner.get_metadata(&hold).await.unwrap().is_none());
        let downloaded = inner.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(downloaded.len(), if round == 0 { 2 } else { 3 });
        for record in downloaded {
            assert_eq!(std::fs::read(record.local_path.unwrap()).unwrap(), BYTES);
        }
        assert!(inner.get_pending().await.unwrap().is_empty());
        assert_eq!(
            inner
                .get_metadata(&crate::sync_cycle::indexing_replay_key(ZONE))
                .await
                .unwrap()
                .is_some(),
            round == 0
        );
        if round == 0 {
            assert!(
                provider.changes.lock().unwrap().is_empty(),
                "qualification inventory must retain old cursor"
            );
        } else {
            assert_eq!(
                *provider.changes.lock().unwrap(),
                [if round == 1 {
                    "trusted-before"
                } else {
                    "trusted-after"
                }]
            );
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn indexing_ready_peer_reconciles_and_advances_while_neighbor_is_unknown() {
    use crate::icloud::photos::PhotosService;
    use crate::sync_loop::{planning, precheck};

    const PEER: &str = "SharedSync-TEST";
    const PEER_CURSOR: &str = "sync_token:SharedSync-TEST";
    let root = tempfile::tempdir().unwrap();
    let db = indexing_db(&root.path().join("state.db")).await;
    let mut config = make_run_cycle_config();
    config.filters.selection.albums = crate::selection::AlbumSelector::None;
    config.filters.selection.unfiled = true;
    config.filters.selection.libraries.shared_all = true;
    let peer_hold = indexing_reconciliation_key(PEER);
    let peer_replay = crate::sync_cycle::indexing_replay_key(PEER);
    let primary_hold = indexing_reconciliation_key(ZONE);
    for (key, value) in [
        (CURSOR, "primary-before"),
        (PEER_CURSOR, "peer-before"),
        (&peer_hold, "1"),
    ] {
        db.set_metadata(key, value).await.unwrap();
    }
    let enum_hash = download::compute_config_hash(&config);
    db.set_metadata(ENUM_CONFIG_HASH_KEY, &enum_hash)
        .await
        .unwrap();

    let unknown = provider("FINISHED", &[], "");
    let primary = library(&unknown, &db).await.library;
    let ready = provider("FINISHED", &[], "");
    let peer = PhotoLibrary::new(
        "https://example.com".into(),
        Arc::default(),
        Box::new(ready.clone()),
        Arc::new(json!({"zoneName":PEER,"ownerRecordName":"_defaultOwner"})),
        "shared".into(),
        retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    let libraries = vec![primary.clone(), peer.clone()];
    let mut service = PhotosService::for_testing_with_libraries(
        Box::new(unknown.clone()),
        primary.clone(),
        HashMap::from([(ZONE.to_owned(), primary)]),
        HashMap::from([(PEER.to_owned(), peer)]),
    );
    service.set_shadow_capture(indexing_capture(&db));
    let (mut states, mut context) =
        planning::resolve_library_plans(&mut service, &libraries, &config)
            .await
            .unwrap();
    assert_eq!(states.len(), 2);
    assert!(states.iter().all(|s| s.cross_zone_libraries.is_empty()));
    let (_dir, session) = make_shared_session_for_run_cycle().await;
    let build = make_run_cycle_download_config_builder(root.path(), db.clone());
    let scope =
        precheck::DbPrecheckScope::from_config(&config, &states, &build, &enum_hash).unwrap();

    *unknown.readiness.lock().unwrap() = "UNKNOWN";
    tokio::time::pause();
    let mut inventory_queries = 0;
    for round in 0..3 {
        if round > 0 {
            for state in &mut states {
                state.plan_needs_refresh = true;
            }
        }
        planning::refresh_indexing_library_plans(
            &mut service,
            &libraries,
            &config,
            Some(db.as_ref()),
            &mut states,
            &mut context,
        )
        .await
        .unwrap();
        let precheck =
            precheck::check_changes_database(Some(db.as_ref()), &states, &mut service, &scope)
                .await;
        assert_eq!(precheck, precheck::WatchPrecheck::proceed_all());
        planning::refresh_needed_library_plans(
            &mut states,
            &config.filters.selection,
            &context,
            precheck.changed_zones(),
            &mut 0,
        )
        .await;

        let result = run_cycle(
            &states.iter().collect::<Vec<_>>(),
            &config,
            Some(db.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 1, "only the unknown library failed");
        assert!(!result.can_advance_database_checkpoint());
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("primary-before")
        );
        assert!(db.get_metadata(&primary_hold).await.unwrap().is_some());
        assert_eq!(unknown.queries.load(Ordering::SeqCst), 0);
        assert!(unknown.changes.lock().unwrap().is_empty());
        assert!(db.get_metadata(&peer_hold).await.unwrap().is_none());
        assert_eq!(
            db.get_metadata(&peer_replay).await.unwrap().is_some(),
            round == 0
        );
        assert_eq!(
            db.get_metadata(PEER_CURSOR).await.unwrap().as_deref(),
            Some(if round == 0 {
                "peer-before"
            } else {
                "trusted-after"
            })
        );
        let queries = ready.queries.load(Ordering::SeqCst);
        if round == 0 {
            assert!(queries > 0, "the peer qualified a full inventory");
        } else if round == 1 {
            // The held inventory can't activate a selection generation. The
            // first trusted replay can build one for subsequent quiet wakes.
            inventory_queries = queries;
        } else {
            assert_eq!(
                queries, inventory_queries,
                "no peer inventory on a quiet wake"
            );
        }
    }
    assert_eq!(
        *ready.changes.lock().unwrap(),
        ["peer-before", "trusted-after"]
    );
}

#[tokio::test]
async fn indexing_running_plan_cannot_qualify_after_late_finished_probe() {
    use crate::icloud::photos::PhotosService;
    use crate::selection::AlbumSelector;
    let server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/omitted.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
        .expect(1)
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let db = indexing_db(&root.path().join("state.db")).await;
    let mut config = make_run_cycle_config();
    config.filters.selection.albums = AlbumSelector::All {
        excluded: Default::default(),
    };
    config.filters.selection.unfiled = false;
    config.filters.selection.albums_explicit = false;
    db.set_metadata(CURSOR, "trusted-before").await.unwrap();
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .unwrap();
    let mut provider = provider("RUNNING", &["omitted"], &server.uri());
    let transition = Arc::new(AtomicBool::new(false));
    provider.catalog = Some(transition.clone());
    let primary = library(&provider, &db).await.library;
    let libraries = vec![primary.clone()];
    let mut service = PhotosService::for_testing_with_libraries(
        Box::new(provider.clone()),
        primary.clone(),
        HashMap::from([(ZONE.to_owned(), primary)]),
        HashMap::new(),
    );
    service.set_shadow_capture(indexing_capture(&db));
    let (mut states, mut context) =
        crate::sync_loop::planning::resolve_library_plans(&mut service, &libraries, &config)
            .await
            .unwrap();
    assert_eq!(states[0].plan.passes.len(), 1);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    *provider.readiness.lock().unwrap() = "UNKNOWN";
    tokio::time::pause();
    let probes = provider.readiness_probes.load(Ordering::SeqCst);
    crate::sync_loop::planning::refresh_indexing_library_plans(
        &mut service,
        &libraries,
        &config,
        Some(db.as_ref()),
        &mut states,
        &mut context,
    )
    .await
    .unwrap();
    assert!(states[0].indexing_deferred);
    let build = make_run_cycle_download_config_builder(&media, db.clone());
    let other_zone = "SharedSync-TEST";
    let other_cursor = "sync_token:SharedSync-TEST";
    db.set_metadata(other_cursor, "other-before").await.unwrap();
    let independent = make_run_cycle_library_state_with_album(
        other_zone,
        other_cursor,
        make_captured_cycle_album(
            other_zone,
            indexing_capture(&db),
            Box::new(
                crate::test_helpers::DynamicRecentPhotosSession::new(0)
                    .with_zone(other_zone)
                    .with_token("other-after"),
            ),
        ),
    );
    let deferred = run_cycle(
        &[&states[0], &independent],
        &config,
        Some(db.as_ref()),
        &build,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        db.get_metadata(other_cursor).await.unwrap().as_deref(),
        Some("other-after")
    );
    assert!(deferred.failed_count > 0);
    assert!(!deferred.can_advance_database_checkpoint());
    assert_eq!(provider.readiness_probes.load(Ordering::SeqCst), probes + 3);
    assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
    assert!(provider.changes.lock().unwrap().is_empty());
    assert_eq!(
        db.get_metadata(CURSOR).await.unwrap().as_deref(),
        Some("trusted-before")
    );
    tokio::time::resume();
    *provider.readiness.lock().unwrap() = "RUNNING";
    transition.store(true, Ordering::SeqCst);
    for round in 0..2 {
        crate::sync_loop::planning::refresh_indexing_library_plans(
            &mut service,
            &libraries,
            &config,
            Some(db.as_ref()),
            &mut states,
            &mut context,
        )
        .await
        .unwrap();
        assert_eq!(states[0].plan.passes.len(), if round == 0 { 1 } else { 2 });
        assert_eq!(
            states[0].plan_is_stale,
            round == 0,
            "scope must have FINISHED provenance"
        );
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store.clone());
        let result = run_cycle(
            &states.iter().collect::<Vec<_>>(),
            &config,
            Some(store.as_ref()),
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(!result.can_advance_database_checkpoint());
        assert_eq!(
            db.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some("trusted-before")
        );
        assert_eq!(
            db.get_metadata(&indexing_reconciliation_key(ZONE))
                .await
                .unwrap()
                .is_some(),
            round == 0
        );
        assert_eq!(db.get_downloaded_page(0, 10).await.unwrap().len(), round);
        if round == 0 {
            assert!(provider.changes.lock().unwrap().is_empty());
        } else {
            assert_eq!(
                *provider.changes.lock().unwrap(),
                ["trusted-before"],
                "only the fresh FINISHED plan can observe the starting boundary"
            );
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn indexing_qualified_replay_keeps_expired_trusted_cursor_and_recovery_debt() {
    let root = tempfile::tempdir().unwrap();
    let db = indexing_db(&root.path().join("state.db")).await;
    let config = make_run_cycle_config();
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .unwrap();
    db.set_metadata(CURSOR, "trusted-before").await.unwrap();
    let replay = crate::sync_cycle::indexing_replay_key(ZONE);
    db.set_metadata(&replay, "1").await.unwrap();
    let mut provider = provider("FINISHED", &[], "http://unused.invalid");
    provider.reject_cursor = true;
    let library = library(&provider, &db).await;
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let build = make_run_cycle_download_config_builder(root.path(), store.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&library],
        &config,
        Some(store.as_ref()),
        &build,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!result.can_advance_database_checkpoint());
    assert_eq!(
        db.get_metadata(CURSOR).await.unwrap().as_deref(),
        Some("trusted-before")
    );
    assert_eq!(
        db.get_metadata(&replay).await.unwrap().as_deref(),
        Some("1")
    );
    assert!(
        db.get_metadata("retained_checkpoint_hold:PrimarySync")
            .await
            .unwrap()
            .is_some(),
        "qualified inventory cannot waive the retained-history gate when replay rejects its cursor"
    );
}

#[tokio::test]
async fn indexing_materialization_debt_refreshes_finished_scope_after_reopen() {
    use crate::icloud::photos::PhotosService;
    use crate::selection::AlbumSelector;
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("state.db");
    {
        let db = indexing_db(&database).await;
        db.set_metadata(
            &crate::sync_cycle::indexing_materialization_key(ZONE),
            "future-version",
        )
        .await
        .unwrap();
    }
    let db = indexing_db(&database).await;
    let mut config = make_run_cycle_config();
    config.filters.selection.albums = AlbumSelector::All {
        excluded: Default::default(),
    };
    config.filters.selection.unfiled = false;
    config.filters.selection.albums_explicit = false;
    let mut provider = provider("FINISHED", &[], "http://unused.invalid");
    provider.catalog = Some(Arc::new(AtomicBool::new(false)));
    let primary = library(&provider, &db).await.library;
    let libraries = vec![primary.clone()];
    let mut service = PhotosService::for_testing_with_libraries(
        Box::new(provider),
        primary.clone(),
        HashMap::from([(ZONE.to_owned(), primary)]),
        HashMap::new(),
    );
    service.set_shadow_capture(indexing_capture(&db));
    let (mut states, mut context) =
        crate::sync_loop::planning::resolve_library_plans(&mut service, &libraries, &config)
            .await
            .unwrap();
    assert_eq!(states[0].plan.passes.len(), 2);
    states[0].plan.passes.truncate(1);
    crate::sync_loop::planning::refresh_indexing_library_plans(
        &mut service,
        &libraries,
        &config,
        Some(db.as_ref()),
        &mut states,
        &mut context,
    )
    .await
    .unwrap();
    assert_eq!(
        states[0].plan.passes.len(),
        2,
        "full materialization must rebuild the selected scope even without a reconciliation marker"
    );
    assert!(!states[0].plan_is_stale);
    assert_eq!(
        db.get_metadata(&crate::sync_cycle::indexing_materialization_key(ZONE))
            .await
            .unwrap()
            .as_deref(),
        Some("future-version")
    );
}

/// The released hold must compose with main's Hidden-only source replay and
/// diagnostic token groups. A healthy query token cannot replace a blank delta.
#[tokio::test]
async fn indexing_hidden_recovery_keeps_query_diagnostics_separate_from_source_successor() {
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig};
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::layer::SubscriberExt;

    // A worker may first register this callsite without our scoped subscriber.
    // Keep a second dispatch alive so cold registration retains dynamic interest.
    let _worker_callsite_registration = tracing::Dispatch::new(tracing_subscriber::registry());
    for bad_successor in ["", " "] {
        let server = wiremock::MockServer::start().await;
        for id in ["provisional", "omitted"] {
            Mock::given(method("GET"))
                .and(path(format!("/{id}.jpg")))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(BYTES))
                .expect(1)
                .mount(&server)
                .await;
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let retained = media.join("retained.jpg");
        std::fs::write(&retained, BYTES).unwrap();
        let config = make_run_cycle_config();
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        {
            let db = indexing_db(&path).await;
            db.set_metadata(CURSOR, "trusted-before").await.unwrap();
            db.set_metadata(
                ENUM_CONFIG_HASH_KEY,
                &download::compute_config_hash(&config),
            )
            .await
            .unwrap();
        }
        for step in 0..5 {
            let db = indexing_db(&path).await;
            let readiness = if step == 0 { "RUNNING" } else { "FINISHED" };
            let ids = if step == 0 {
                vec!["provisional"]
            } else {
                vec!["provisional", "omitted"]
            };
            let mut source = provider(readiness, &ids, &server.uri());
            if step == 2 {
                source.successor = bad_successor;
            }
            for record in Arc::make_mut(&mut source.records) {
                if record["recordType"] == "CPLAsset" {
                    record["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
                }
            }
            let mut selected = library(&source, &db).await;
            let hidden = crate::icloud::photos::smart_folders::smart_folders()
                .into_iter()
                .find(|(name, _)| *name == "Hidden")
                .unwrap()
                .1;
            selected.plan.passes = vec![AlbumPass {
                kind: PassKind::SmartFolder,
                album: PhotoAlbum::new(
                    PhotoAlbumConfig {
                        params: Arc::new(HashMap::new()),
                        service_endpoint: Arc::from("https://example.com"),
                        name: Arc::from("Hidden"),
                        list_type: Arc::from(hidden.list_type),
                        obj_type: Arc::from(hidden.obj_type),
                        query_filter: hidden.query_filter,
                        page_size: 100,
                        zone_id: Arc::new(
                            json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"}),
                        ),
                        retry_config: retry::RetryConfig::default(),
                        container_id: None,
                        cross_zone_sources: Vec::new(),
                    },
                    Box::new(source.clone()),
                ),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            }];
            selected.plan.passes[0]
                .album
                .set_shadow_capture(indexing_capture(&db), Arc::from("private"));
            selected.pass_scope = crate::commands::PassScope {
                include_albums: false,
                include_smart_folders: true,
                include_unfiled: false,
            };
            let store: Arc<dyn download::DownloadStore> = db.clone();
            let build = make_run_cycle_download_config_builder(&media, store.clone());
            let (recorder, diagnostics) = crate::support::history::test_layer();
            let result = run_cycle(
                &[&selected],
                &config,
                Some(store.as_ref()),
                &build,
                download::DownloadControls::download_hidden(),
                &shared,
                &CancellationToken::new(),
            )
            .with_subscriber(tracing_subscriber::registry().with(recorder))
            .await;
            let result = if step == 2 {
                let error = result.expect_err("blank source successor must be refused");
                assert!(format!("{error:#}").contains("Missing changes/zone successor"));
                None
            } else {
                Some(result.unwrap())
            };
            assert_eq!(std::fs::read(&retained).unwrap(), BYTES, "step {step}");
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            assert_eq!(rows.len(), if step == 0 { 1 } else { 2 }, "step {step}");
            for row in rows {
                assert_eq!(std::fs::read(row.local_path.unwrap()).unwrap(), BYTES);
            }
            assert_eq!(
                db.get_metadata(CURSOR).await.unwrap().as_deref(),
                Some(if step < 3 {
                    "trusted-before"
                } else {
                    "trusted-after"
                }),
                "step {step}"
            );
            assert_eq!(
                db.get_metadata(&indexing_reconciliation_key(ZONE))
                    .await
                    .unwrap()
                    .is_some(),
                step == 0
            );
            assert_eq!(
                db.get_metadata(&crate::sync_cycle::indexing_replay_key(ZONE))
                    .await
                    .unwrap()
                    .is_some(),
                step == 1 || step == 2
            );
            let Some(result) = result else {
                assert_eq!(*source.changes.lock().unwrap(), ["trusted-before"]);
                assert_eq!(
                    source.queries.load(Ordering::SeqCst),
                    0,
                    "a refused source successor cannot be replaced with a fresh query token"
                );
                continue;
            };
            assert_eq!(
                result.can_advance_database_checkpoint(),
                step >= 3,
                "step {step}: {:?}",
                result.stats
            );
            if step < 2 {
                assert!(source.changes.lock().unwrap().is_empty());
            } else {
                let cursor = if step == 4 {
                    "trusted-after"
                } else {
                    "trusted-before"
                };
                assert_eq!(*source.changes.lock().unwrap(), [cursor]);
            }
            assert_eq!(result.stats.downloaded, usize::from(step < 2));
            let events = diagnostics();
            assert!(
                events
                    .iter()
                    .any(|event| event["kind"] == "support_token_equivalence_v1"
                        && event["fields"]["rows"][0]["initial_status"] == "present"),
                "query token evidence must survive provisional/recovery composition: step {step}: {events:?}"
            );
            assert!(
                !serde_json::to_string(&events)
                    .unwrap()
                    .contains("query-candidate")
            );
        }
        server.verify().await;
    }
}
