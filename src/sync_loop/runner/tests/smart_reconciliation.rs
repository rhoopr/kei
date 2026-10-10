//! Durable smart-folder path configuration transitions through the production cycle.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::PhotosSession;
use crate::sync_cycle::{ENUM_CONFIG_HASH_KEY, PENDING_DOWNLOAD_CONFIG_HASH_KEY, run_cycle};
use crate::sync_loop::test_support::{
    album_count_response, full_album_page_with_download, make_named_full_album_with_boxed_session,
    make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use crate::{download, state};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct SmartReconciliationSession {
    zone: &'static str,
    records: Arc<Value>,
    lookups: Arc<AtomicUsize>,
    queries: Arc<AtomicUsize>,
    fault: Arc<std::sync::Mutex<Option<SmartReconciliationFault>>>,
    cancel: Arc<std::sync::Mutex<Option<CancellationToken>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SmartReconciliationFault {
    UnknownCatalogIdentity,
    MalformedQuery,
    MissingQueryToken,
    StalePlan,
    Interrupted,
    RecentBound,
    LowerDateBound,
    PromotionWrite,
    DownloadWrite,
    IncompleteAlbumSnapshot,
    OtherLibraryFailure,
}
#[async_trait::async_trait]
impl PhotosSession for SmartReconciliationSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            if *self.fault.lock().unwrap() == Some(SmartReconciliationFault::UnknownCatalogIdentity)
            {
                return Ok(json!({"records": []}));
            }
            return Ok(self.records.as_ref().clone());
        }
        if url.contains("/records/query/batch?") {
            return Ok(album_count_response(
                self.records["records"].as_array().unwrap().len() as u64 / 2,
            ));
        }
        if url.contains("/records/query?") {
            self.queries.fetch_add(1, Ordering::SeqCst);
            if let Some(cancel) = self.cancel.lock().unwrap().as_ref() {
                cancel.cancel();
            }
            let mut response = self.records.as_ref().clone();
            match *self.fault.lock().unwrap() {
                Some(SmartReconciliationFault::MalformedQuery) => {
                    response["records"][1]["fields"]["assetDate"]["value"] = Value::Null;
                }
                Some(SmartReconciliationFault::MissingQueryToken) => {
                    response.as_object_mut().unwrap().remove("syncToken");
                }
                Some(SmartReconciliationFault::OtherLibraryFailure)
                    if self.zone == "SharedSync-TEST" =>
                {
                    response.as_object_mut().unwrap().remove("syncToken");
                }
                _ => {}
            }
            return Ok(response);
        }
        if url.contains("/changes/zone?") {
            return Ok(
                json!({"zones": [{"zoneID": {"zoneName": self.zone, "ownerRecordName": "_defaultOwner"}, "syncToken": "smart-current", "moreComing": false, "records": []}]}),
            );
        }
        anyhow::bail!("unexpected synthetic Photos request: {url}")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_promotes_after_fresh_query_and_stays_quiet() {
    Box::pin(smart_reconciliation_lifecycle("Hidden", false, false, None)).await;
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_favorites_mixed_selection_stays_quiet() {
    Box::pin(smart_reconciliation_lifecycle(
        "Favorites",
        true,
        false,
        None,
    ))
    .await;
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn run_cycle_smart_reconciliation_sidecars_survive_transition_and_restart() {
    Box::pin(smart_reconciliation_lifecycle("Hidden", false, true, None)).await;
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_other_empty_library_failure_holds_promotion() {
    Box::pin(smart_reconciliation_lifecycle(
        "Hidden",
        false,
        false,
        Some(SmartReconciliationFault::OtherLibraryFailure),
    ))
    .await;
}

#[tokio::test]
async fn run_cycle_smart_reconciliation_retains_incomplete_work_then_recovers() {
    for fault in [
        SmartReconciliationFault::UnknownCatalogIdentity,
        SmartReconciliationFault::MalformedQuery,
        SmartReconciliationFault::MissingQueryToken,
        SmartReconciliationFault::StalePlan,
        SmartReconciliationFault::Interrupted,
        SmartReconciliationFault::RecentBound,
        SmartReconciliationFault::LowerDateBound,
        SmartReconciliationFault::PromotionWrite,
        SmartReconciliationFault::DownloadWrite,
        SmartReconciliationFault::IncompleteAlbumSnapshot,
    ] {
        Box::pin(smart_reconciliation_lifecycle(
            "Hidden",
            false,
            false,
            Some(fault),
        ))
        .await;
    }
}

async fn smart_reconciliation_lifecycle(
    smart_name: &str,
    mixed: bool,
    sidecars: bool,
    failure: Option<SmartReconciliationFault>,
) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let server = crate::start_wiremock_or_skip!();
    let bytes = include_bytes!("../../../../tests/data/media/pattern.jpg");
    let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes));
    Mock::given(method("GET"))
        .and(path("/smart.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let config = make_run_cycle_config();
    let lookups = Arc::new(AtomicUsize::new(0));
    let queries = Arc::new(AtomicUsize::new(0));
    let mut records = full_album_page_with_download(
        "PrimarySync",
        "SMART",
        "smart-current",
        &format!("{}/smart.jpg", server.uri()),
        bytes.len() as u64,
        &checksum,
    );
    records["records"][1]["fields"]["isHidden"] =
        json!({"value": i32::from(smart_name == "Hidden"), "type": "INT64"});
    records["records"][1]["fields"]["isFavorite"] = json!({"value": 1, "type": "INT64"});
    let fault = Arc::new(std::sync::Mutex::new(None));
    let cancel = Arc::new(std::sync::Mutex::new(None));
    let records = Arc::new(records);
    let primary_session = SmartReconciliationSession {
        zone: "PrimarySync",
        records: records.clone(),
        lookups: lookups.clone(),
        queries: queries.clone(),
        fault: fault.clone(),
        cancel: cancel.clone(),
    };
    let album = smart_album("PrimarySync", smart_name, Box::new(primary_session.clone()));
    let mut lib = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![AlbumPass {
            kind: PassKind::SmartFolder,
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    if mixed {
        lib.plan.passes.insert(
            0,
            AlbumPass {
                kind: PassKind::Unfiled,
                album: make_named_full_album_with_boxed_session(
                    "PrimarySync",
                    "",
                    Box::new(primary_session),
                ),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
        );
    }
    let other_library = make_run_cycle_library_state_with_passes(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        vec![AlbumPass {
            kind: PassKind::SmartFolder,
            album: smart_album(
                "SharedSync-TEST",
                smart_name,
                Box::new(SmartReconciliationSession {
                    zone: "SharedSync-TEST",
                    records: Arc::new(json!({"records": [], "syncToken": "smart-current"})),
                    lookups: Arc::new(AtomicUsize::new(0)),
                    queries: Arc::new(AtomicUsize::new(0)),
                    fault: fault.clone(),
                    cancel: Arc::new(std::sync::Mutex::new(None)),
                }),
            ),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    let mut original_path = None;
    let mut initial_hash = String::new();
    let mut current_hash = String::new();
    let mut previous_files = std::collections::BTreeMap::new();
    let mut original_sidecar = None;
    let final_cycle = if failure.is_some() { 3 } else { 2 };
    for cycle in 0..=final_cycle {
        let held = cycle == 1 && failure.is_some();
        *fault.lock().unwrap() = if held { failure } else { None };
        let shutdown = CancellationToken::new();
        *cancel.lock().unwrap() = if held && failure == Some(SmartReconciliationFault::Interrupted)
        {
            Some(shutdown.clone())
        } else {
            None
        };
        lib.plan_is_stale = held && failure == Some(SmartReconciliationFault::StalePlan);
        if held && failure == Some(SmartReconciliationFault::IncompleteAlbumSnapshot) {
            let mut album = lib.plan.passes[0].clone();
            album.kind = PassKind::Album;
            lib.plan.passes.push(album);
        } else if cycle > 1 && failure == Some(SmartReconciliationFault::IncompleteAlbumSnapshot) {
            lib.plan.passes.retain(|pass| pass.kind != PassKind::Album);
        }
        let db = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
        if held
            && matches!(
                failure,
                Some(
                    SmartReconciliationFault::PromotionWrite
                        | SmartReconciliationFault::DownloadWrite
                )
            )
        {
            let sql = if failure == Some(SmartReconciliationFault::PromotionWrite) {
                "CREATE TRIGGER smart_fail BEFORE UPDATE ON metadata WHEN NEW.key = 'config_hash' BEGIN SELECT RAISE(FAIL, 'synthetic promotion failure'); END;"
            } else {
                "CREATE TRIGGER smart_fail BEFORE UPDATE ON assets WHEN NEW.status = 'downloaded' BEGIN SELECT RAISE(FAIL, 'synthetic downloaded write failure'); END;"
            };
            rusqlite::Connection::open(&db_path)
                .unwrap()
                .execute_batch(sql)
                .unwrap();
        }
        let base = make_run_cycle_download_config_builder(&media, db.clone());
        let builder = |mode, excludes, groupings, library| {
            let mut result = (*base(mode, excludes, groupings, library)).clone();
            result.folder_structure_smart_folders = Arc::from(if cycle == 0 {
                "old/{smart-folder}"
            } else {
                "new/{smart-folder}"
            });
            #[cfg(feature = "xmp")]
            {
                result.metadata.xmp_sidecar = sidecars;
            }
            #[cfg(not(feature = "xmp"))]
            assert!(!sidecars);
            if held && failure == Some(SmartReconciliationFault::RecentBound) {
                result.recent = Some(1);
            }
            if held && failure == Some(SmartReconciliationFault::LowerDateBound) {
                result.skip_created_before = Some(crate::config::CreatedDateFilter::CaptureDate(
                    chrono::NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                ));
            }
            Arc::new(result)
        };
        let candidate = builder(
            download::SyncMode::Full,
            Arc::new(rustc_hash::FxHashSet::default()),
            Arc::new(download::AssetGroupings::default()),
            Arc::from("PrimarySync"),
        );
        let hash = download::hash_download_config(&candidate);
        current_hash = hash.clone();
        let before = lookups.load(Ordering::SeqCst);
        let requests_before = server.received_requests().await.unwrap().len();
        let states = if failure == Some(SmartReconciliationFault::OtherLibraryFailure) {
            vec![&lib, &other_library]
        } else {
            vec![&lib]
        };
        let result = run_cycle(
            &states,
            &config,
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &session,
            &shutdown,
        )
        .await
        .unwrap();
        if !held {
            assert_eq!(
                result.failed_count, 0,
                "{failure:?} cycle {cycle}: {result:?}"
            );
        }
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        if !(held && failure == Some(SmartReconciliationFault::DownloadWrite)) {
            assert_eq!(rows.len(), 1, "{failure:?} cycle {cycle}: {rows:?}");
        }
        let current = rows
            .first()
            .and_then(|row| row.local_path.as_ref())
            .cloned()
            .or_else(|| original_path.clone())
            .unwrap();
        assert_eq!(std::fs::read(&current).unwrap(), bytes);
        if cycle == 0 {
            original_path = Some(current.clone());
            initial_hash = hash.clone();
            if sidecars {
                let path = current.with_file_name(format!(
                    "{}.xmp",
                    current.file_name().unwrap().to_string_lossy()
                ));
                original_sidecar = Some((path.clone(), std::fs::read(path).unwrap()));
            }
        } else if held {
            assert_eq!(
                db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(initial_hash.as_str()),
                "{failure:?}"
            );
            assert_eq!(
                db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(hash.as_str()),
                "{failure:?}"
            );
            assert_eq!(
                std::fs::read(original_path.as_ref().unwrap()).unwrap(),
                bytes
            );
        } else {
            assert!(current.starts_with(media.join("new")));
            assert_eq!(
                std::fs::read(original_path.as_ref().unwrap()).unwrap(),
                bytes
            );
            assert_eq!(
                db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(hash.as_str())
            );
            assert_eq!(
                db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                lookups.load(Ordering::SeqCst) - before,
                usize::from(cycle < final_cycle)
            );
        }
        if let Some((path, bytes)) = &original_sidecar {
            assert_eq!(std::fs::read(path).unwrap(), *bytes);
        }
        let files = media_snapshot(&media);
        if cycle == final_cycle {
            assert_eq!(
                result.stats.downloaded, 0,
                "unchanged final cycle: {failure:?}"
            );
            assert_eq!(
                files, previous_files,
                "no duplicate or temporary files: {failure:?}"
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                requests_before
            );
        }
        previous_files = files;
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("smart-current")
        );
        assert_eq!(
            db.get_metadata(ENUM_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(download::compute_config_hash(&config).as_str())
        );
        if held
            && matches!(
                failure,
                Some(
                    SmartReconciliationFault::PromotionWrite
                        | SmartReconciliationFault::DownloadWrite
                )
            )
        {
            rusqlite::Connection::open(&db_path)
                .unwrap()
                .execute_batch("DROP TRIGGER smart_fail;")
                .unwrap();
        }
        eprintln!(
            "#770 acceptance case={failure:?} smart={smart_name} mixed={mixed} cycle={cycle} failed={} downloaded={} lookups={} queries={} active_hash_old={} pending={} reason={:?}",
            result.failed_count,
            result.stats.downloaded,
            lookups.load(Ordering::SeqCst),
            queries.load(Ordering::SeqCst),
            db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref()
                == Some(initial_hash.as_str()),
            db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .is_some(),
            result.stats.full_enumeration_reason
        );
    }
    let reopened = state::SqliteStateDb::open(&db_path).await.unwrap();
    assert_eq!(
        reopened
            .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(current_hash.as_str())
    );
    assert_eq!(
        reopened
            .get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        None
    );
    assert_eq!(reopened.get_downloaded_page(0, 10).await.unwrap().len(), 1);
    assert!(reopened.get_pending().await.unwrap().is_empty());
    // Ordinary mixed selection also lands its own unfiled copy. Failures may
    // repeat a transfer whose durable write failed, but the final cycle is quiet.
    let expected_minimum = if mixed { 3 } else { 2 };
    assert!(server.received_requests().await.unwrap().len() >= expected_minimum);
}

fn media_snapshot(
    root: &std::path::Path,
) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(
        root: &std::path::Path,
        result: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, result);
            } else {
                result.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    visit(root, &mut result);
    result
}

fn smart_album(
    zone: &str,
    name: &str,
    session: Box<dyn PhotosSession>,
) -> crate::icloud::photos::PhotoAlbum {
    let def = crate::icloud::photos::smart_folders::smart_folders()
        .into_iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap()
        .1;
    crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from(def.list_type),
            obj_type: Arc::from(def.obj_type),
            query_filter: def.query_filter,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": zone})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}
