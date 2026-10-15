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
    FailingMetadataSetDb, MetadataSetFailure, album_count_response, full_album_page_with_download,
    make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_album,
    make_shared_session_for_run_cycle,
};
use crate::{download, retry, state};

const ZONE: &str = "PrimarySync";
const CURSOR: &str = "sync_token:PrimarySync";
const BYTES: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46];

#[derive(Clone)]
struct IndexingSession {
    readiness: Arc<Mutex<&'static str>>,
    records: Arc<Vec<Value>>,
    queries: Arc<AtomicUsize>,
    changes: Arc<Mutex<Vec<String>>>,
    interrupt: Option<CancellationToken>,
    regress_after_query: bool,
    catalog: Option<Arc<AtomicBool>>,
    change_records: Arc<Vec<Value>>,
    reject_cursor: bool,
    successor: &'static str,
}

#[async_trait::async_trait]
impl PhotosSession for IndexingSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if request["query"]["recordType"] == "CheckIndexingState" {
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
            let cursor = request["zones"][0]["syncToken"]
                .as_str()
                .unwrap()
                .to_owned();
            self.changes.lock().unwrap().push(cursor);
            if self.reject_cursor {
                return Ok(
                    json!({"zones":[{"zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},
                    "serverErrorCode":"BAD_REQUEST","reason":"expired retained cursor","syncToken":"","moreComing":false}]}),
                );
            }
            // Deliberately no events for omitted assets: only a full inventory
            // can recover them, even though every incremental request succeeds.
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":ZONE,"ownerRecordName":"_defaultOwner"},
                "records":self.change_records.as_ref(),"syncToken":self.successor,"moreComing":false}]}),
            );
        }
        if url.contains("/records/lookup?") {
            return Ok(json!({"records":self.records.as_ref()}));
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
            "syncToken":"query-candidate"}),
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
            page["records"][0]["fields"]["filenameEnc"]["value"] = json!(format!("{id}.jpg"));
            page["records"].as_array().unwrap().clone()
        })
        .collect();
    IndexingSession {
        readiness: Arc::new(Mutex::new(readiness)),
        records: Arc::new(records),
        queries: Arc::new(AtomicUsize::new(0)),
        changes: Arc::new(Mutex::new(Vec::new())),
        interrupt: None,
        regress_after_query: false,
        catalog: None,
        change_records: Arc::new(Vec::new()),
        reject_cursor: false,
        successor: "trusted-after",
    }
}

async fn library(provider: &IndexingSession) -> crate::sync_cycle::LibraryState {
    let mut library = make_run_cycle_library_state_with_album(
        ZONE,
        CURSOR,
        make_full_album_with_boxed_session(ZONE, Box::new(provider.clone())),
    );
    // Use the real readiness constructor and request path, not the legacy stub.
    library.library = PhotoLibrary::new(
        "https://example.com".into(),
        Arc::new(HashMap::new()),
        Box::new(provider.clone()),
        Arc::new(json!({"zoneName":ZONE})),
        "private".into(),
        retry::RetryConfig {
            max_retries: 0,
            ..retry::RetryConfig::default()
        },
    )
    .await
    .unwrap();
    library
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
        let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let provider = provider(readiness, &ids, &server.uri());
        let library = library(&provider).await;
        let build = make_run_cycle_download_config_builder(&media, db.clone());
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            false,
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

#[tokio::test]
async fn indexing_failed_interrupted_or_uncommitted_reconciliation_retains_hold_and_cursor() {
    for fault in [
        "malformed",
        "interrupted",
        "checkpoint_write",
        "regressed",
        "unknown",
    ] {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.db");
        let media = root.path().join("media");
        std::fs::create_dir(&media).unwrap();
        let old_file = media.join("old.jpg");
        std::fs::write(&old_file, BYTES).unwrap();
        let config = make_run_cycle_config();
        let hold = indexing_reconciliation_key(ZONE);
        {
            let db = state::SqliteStateDb::open(&database).await.unwrap();
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
        let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let raw: Arc<dyn download::DownloadStore> = inner.clone();
        let db: Arc<dyn download::DownloadStore> = if fault == "checkpoint_write" {
            Arc::new(FailingMetadataSetDb::new(
                raw,
                MetadataSetFailure::Prefix(crate::sync_cycle::INDEXING_REPLAY_PREFIX),
                "injected inventory qualification failure",
            ))
        } else {
            raw
        };
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
            provider.records = Arc::new(bad["records"].as_array().unwrap().clone());
        }
        if fault == "interrupted" {
            provider.interrupt = Some(cancel.clone());
        }
        if fault == "regressed" {
            provider.regress_after_query = true;
        }
        let library = library(&provider).await;
        if fault == "unknown" {
            *provider.readiness.lock().unwrap() = "UNKNOWN";
        }
        let build = make_run_cycle_download_config_builder(&media, db.clone());
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &cancel,
        )
        .await;
        if fault == "unknown" {
            assert!(result.is_err());
        } else {
            assert!(
                !result.unwrap().can_advance_database_checkpoint(),
                "{fault}"
            );
        }
        drop(build);
        drop(db);
        drop(inner);
        let reopened = state::SqliteStateDb::open(&database).await.unwrap();
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
        let db = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
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
        let mut library = library(&provider).await;
        library.plan.passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::new(
                PhotoAlbumConfig {
                    params: Arc::new(HashMap::new()),
                    service_endpoint: Arc::from("https://example.com"),
                    name: Arc::from("Trip"),
                    list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                    obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                    query_filter: None,
                    page_size: 100,
                    zone_id: Arc::new(json!({"zoneName":ZONE})),
                    retry_config: retry::RetryConfig::default(),
                    container_id: Some(Arc::from("trip")),
                    cross_zone_sources: Vec::new(),
                },
                Box::new(provider.clone()),
            ),
            exclude_ids: Arc::default(),
        }];
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store.clone());
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
        let result = run_cycle(
            &[&library],
            &config,
            Some(store.as_ref()),
            false,
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
            run_cycle(
                &[&library],
                &config,
                Some(store.as_ref()),
                false,
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
            .query_row("SELECT COUNT(*) FROM album_membership_snapshots", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            snapshots, 1,
            "provisional queries do not certify album snapshots"
        );
    }
}

#[tokio::test]
async fn indexing_hold_write_failure_stops_before_listing_or_downloading() {
    let root = tempfile::tempdir().unwrap();
    let inner = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    inner.set_metadata(CURSOR, "trusted-before").await.unwrap();
    let raw: Arc<dyn download::DownloadStore> = inner.clone();
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        raw,
        MetadataSetFailure::Prefix(crate::sync_cycle::INDEXING_RECONCILIATION_PREFIX),
        "injected indexing hold failure",
    ));
    let provider = provider("RUNNING", &["new-photo"], "http://unused.invalid");
    let library = library(&provider).await;
    let config = make_run_cycle_config();
    let build = make_run_cycle_download_config_builder(root.path(), db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(provider.queries.load(Ordering::SeqCst), 0);
    assert!(provider.changes.lock().unwrap().is_empty());
    assert_eq!(
        inner.get_metadata(CURSOR).await.unwrap().as_deref(),
        Some("trusted-before")
    );
    assert!(inner.get_downloaded_page(0, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn indexing_cross_zone_source_requires_reconciliation_without_permanent_hold() {
    let root = tempfile::tempdir().unwrap();
    let db = state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    let primary = provider("FINISHED", &[], "http://unused.invalid");
    let shared = provider("RUNNING", &[], "http://unused.invalid");
    let mut library = library(&primary).await;
    library.cross_zone_libraries.push(
        PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(shared.clone()),
            Arc::new(json!({"zoneName":"SharedSync-TEST"})),
            "shared".into(),
            retry::RetryConfig::default(),
        )
        .await
        .unwrap(),
    );
    let (finished, hold) = crate::sync_cycle::prepare_library_indexing(&library, Some(&db), false)
        .await
        .unwrap();
    assert!(!finished);
    assert!(hold.is_some());
    *shared.readiness.lock().unwrap() = "FINISHED";
    let (finished, hold) = crate::sync_cycle::prepare_library_indexing(&library, Some(&db), false)
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
    let (finished, hold) = crate::sync_cycle::prepare_library_indexing(&library, Some(&db), false)
        .await
        .unwrap();
    assert!(finished);
    assert!(
        hold.is_none(),
        "an unrelated source marker cannot recreate a completed selected-zone hold"
    );
}

#[tokio::test]
async fn indexing_retry_only_keeps_hold_until_normal_inventory_recovers_omitted_photo() {
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
        let db = state::SqliteStateDb::open(&database).await.unwrap();
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
    for (round, retry_only) in [true, false, true, false].into_iter().enumerate() {
        let inner = Arc::new(state::SqliteStateDb::open(&database).await.unwrap());
        let db: Arc<dyn download::DownloadStore> = inner.clone();
        let mut session = provider("FINISHED", &["known", "omitted"], &server.uri());
        if round >= 2 {
            session.change_records = provider("FINISHED", &["late"], &server.uri()).records;
        }
        let provider = session;
        let library = library(&provider).await;
        let base = make_run_cycle_download_config_builder(&media, db.clone());
        let build = |mode, exclude, groups, zone| {
            let mut config = base(mode, exclude, groups, zone);
            Arc::make_mut(&mut config).retry_only = retry_only;
            config
        };
        let result = run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            retry_only,
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_eq!(result.can_advance_database_checkpoint(), round == 3);
        assert_eq!(
            inner.get_metadata(CURSOR).await.unwrap().as_deref(),
            Some(if round < 3 {
                "trusted-before"
            } else {
                "trusted-after"
            })
        );
        assert_eq!(
            inner.get_metadata(&hold).await.unwrap().is_some(),
            round == 0
        );
        assert_eq!(
            inner.get_downloaded_page(0, 10).await.unwrap().len(),
            if round == 0 {
                1
            } else if round == 3 {
                3
            } else {
                2
            }
        );
        assert_eq!(
            inner
                .get_metadata(&crate::sync_cycle::indexing_replay_key(ZONE))
                .await
                .unwrap()
                .is_some(),
            round == 1 || round == 2
        );
        if round < 2 {
            assert!(
                provider.changes.lock().unwrap().is_empty(),
                "held/retry inventory must retain old cursor"
            );
        } else {
            assert_eq!(*provider.changes.lock().unwrap(), ["trusted-before"]);
        }
    }
    server.verify().await;
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
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
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
    let primary = library(&provider).await.library;
    let libraries = vec![primary.clone()];
    let mut service = PhotosService::for_testing_with_libraries(
        Box::new(provider.clone()),
        primary.clone(),
        HashMap::from([(ZONE.to_owned(), primary)]),
        HashMap::new(),
    );
    let (mut states, mut context) =
        crate::sync_loop::planning::resolve_library_plans(&mut service, &libraries, &config)
            .await
            .unwrap();
    assert_eq!(states[0].plan.passes.len(), 1);
    transition.store(true, Ordering::SeqCst);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
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
            false,
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
        assert!(provider.changes.lock().unwrap().is_empty());
    }
    server.verify().await;
}

#[tokio::test]
async fn indexing_qualified_replay_keeps_expired_trusted_cursor_and_recovery_debt() {
    let root = tempfile::tempdir().unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
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
    let library = library(&provider).await;
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let build = make_run_cycle_download_config_builder(root.path(), store.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&library],
        &config,
        Some(store.as_ref()),
        false,
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
        let db = state::SqliteStateDb::open(&database).await.unwrap();
        db.set_metadata(
            &crate::sync_cycle::indexing_materialization_key(ZONE),
            "future-version",
        )
        .await
        .unwrap();
    }
    let db = state::SqliteStateDb::open(&database).await.unwrap();
    let mut config = make_run_cycle_config();
    config.filters.selection.albums = AlbumSelector::All {
        excluded: Default::default(),
    };
    config.filters.selection.unfiled = false;
    config.filters.selection.albums_explicit = false;
    let mut provider = provider("FINISHED", &[], "http://unused.invalid");
    provider.catalog = Some(Arc::new(AtomicBool::new(false)));
    let primary = library(&provider).await.library;
    let libraries = vec![primary.clone()];
    let mut service = PhotosService::for_testing_with_libraries(
        Box::new(provider),
        primary.clone(),
        HashMap::from([(ZONE.to_owned(), primary)]),
        HashMap::new(),
    );
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
        Some(&db),
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
            let db = state::SqliteStateDb::open(&path).await.unwrap();
            db.set_metadata(CURSOR, "trusted-before").await.unwrap();
            db.set_metadata(
                ENUM_CONFIG_HASH_KEY,
                &download::compute_config_hash(&config),
            )
            .await
            .unwrap();
        }
        for step in 0..5 {
            let db = Arc::new(state::SqliteStateDb::open(&path).await.unwrap());
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
            let mut selected = library(&source).await;
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
                false,
                &build,
                download::DownloadControls::download_hidden(),
                &shared,
                &CancellationToken::new(),
            )
            .with_subscriber(tracing_subscriber::registry().with(recorder))
            .await
            .unwrap();
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
                // Main's missing-successor fallback performs the initial
                // replay and one retained-cursor bridge after its inventory.
                let calls = if step == 2 { 2 } else { 1 };
                assert_eq!(*source.changes.lock().unwrap(), vec![cursor; calls]);
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
