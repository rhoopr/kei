use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, ResponseTemplate};

use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoAsset, PhotosSession};
use crate::state::SqliteStateDb;
use crate::state::db::account::AccountOwner;

use super::super::dispatch::download_photos_with_sync;
use super::super::models::{DownloadControls, DownloadOutcome, SyncMode};
use super::super::test_support::{
    incremental_photo_records, incremental_photo_records_with_url, relation_delta_record,
    seed_complete_album_snapshot, seed_downloaded_metadata_asset, test_config,
};

const MEDIA: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
];

#[derive(Clone)]
struct SelectionSession {
    inventory: Arc<Vec<Vec<Value>>>,
    changed: bool,
    cancel_at_selection: Option<CancellationToken>,
    queries: Arc<Mutex<Vec<String>>>,
}

impl SelectionSession {
    fn inventory_for(&self, scope: &str) -> Vec<&Vec<Value>> {
        self.inventory
            .iter()
            .filter(|pair| {
                let id = pair[0]["recordName"].as_str().unwrap();
                match scope {
                    "A" => ["alpha", "delta"].contains(&id),
                    "B" => ["alpha", "beta", "epsilon"].contains(&id),
                    _ => true,
                }
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl PhotosSession for SelectionSession {
    async fn post(&self, url: &str, body: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/changes/zone?") {
            let continued = request["zones"][0]["syncToken"] == "delta-page-1";
            let records = if !self.changed {
                Vec::new()
            } else if !continued {
                [self.inventory[4].clone(), self.inventory[2].clone()].concat()
            } else {
                let mut records = [
                    self.inventory[3].clone(),
                    self.inventory[1].clone(),
                    self.inventory[0].clone(),
                ]
                .concat();
                for (scope, names) in [
                    ("A", &["alpha", "delta"][..]),
                    ("B", &["alpha", "beta", "epsilon"][..]),
                ] {
                    for name in names {
                        records.push(relation_delta_record(scope, &format!("asset-{name}")));
                    }
                }
                records.push(
                    json!({"recordName":"future-retained", "recordType":"FutureSelectionRecord",
                    "fields":{"futureRelationship":{"value":{"recordName":"future-target"}}},
                    "opaque":{"keep":[1,2,3]}}),
                );
                records
            };
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                "records":records,"syncToken":if self.changed && !continued {"delta-page-1"} else {"delta-successor"},
                "moreComing":self.changed && !continued}]}),
            );
        }
        if url.contains("/internal/records/query/batch?") {
            if let Some(cancel) = &self.cancel_at_selection {
                cancel.cancel();
            }
            let batch: Vec<_> = request["batch"].as_array().unwrap().iter().map(|query| {
                let object = query["query"]["filterBy"]["fieldValue"]["value"][0].as_str().unwrap();
                let scope = object.rsplit(':').next().unwrap();
                json!({"records":[{"fields":{"itemCount":{"value":self.inventory_for(scope).len()}}}]})
            }).collect();
            return Ok(json!({"batch":batch}));
        }
        if url.contains("/records/query?") {
            if let Some(cancel) = &self.cancel_at_selection {
                cancel.cancel();
            }
            let filters = request["query"]["filterBy"].as_array().unwrap();
            let field = |name: &str| {
                filters
                    .iter()
                    .find(|f| f["fieldName"] == name)
                    .map(|f| &f["fieldValue"]["value"])
            };
            let scope = field("parentId")
                .and_then(Value::as_str)
                .unwrap_or("library");
            self.queries.lock().unwrap().push(scope.to_string());
            let offset = field("startRank").and_then(Value::as_u64).unwrap_or(0) as usize;
            let limit = request["resultsLimit"].as_u64().unwrap() as usize / 2;
            let records: Vec<_> = self
                .inventory_for(scope)
                .into_iter()
                .skip(offset)
                .take(limit)
                .flat_map(|pair| pair.iter().cloned())
                .collect();
            return Ok(json!({"records":records,"syncToken":"rank-query-token"}));
        }
        if url.contains("/records/lookup?") {
            return Ok(
                json!({"records":self.inventory.iter().flatten().cloned().collect::<Vec<_>>()}),
            );
        }
        anyhow::bail!("Unexpected fixture owner endpoint")
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "recent-fixture@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"recent-synthetic-account"}})).unwrap(),
    )
    .unwrap()
}

fn passes(
    session: impl PhotosSession + Clone + 'static,
    db: &Arc<SqliteStateDb>,
) -> Vec<AlbumPass> {
    [Some("A"), Some("B"), None].into_iter().map(|scope| {
        let mut album = PhotoAlbum::new(PhotoAlbumConfig {
            params: Arc::new(HashMap::new()), service_endpoint: Arc::from("https://example.invalid"),
            name: Arc::from(scope.unwrap_or("")),
            list_type: Arc::from(if scope.is_some() { "CPLContainerRelationLiveByAssetDate" } else { "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted" }),
            obj_type: Arc::from(scope.map_or_else(|| "CPLAssetByAssetDateWithoutHiddenOrDeleted".to_string(), |id| format!("CPLContainerRelationNotDeletedByAssetDate:{id}"))),
            query_filter: scope.map(|id| Arc::new(json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":id}}]))),
            page_size: 2, zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(), container_id: scope.map(Arc::from), cross_zone_sources: Vec::new(),
        }, Box::new(session.clone()));
        album.set_shadow_capture(ShadowCapture::new(db.clone(), owner(), "com"), Arc::from("private"));
        AlbumPass {
            kind: if scope.is_some() {PassKind::Album} else {PassKind::Unfiled},
            album,
            exclude_ids: Arc::new(if scope.is_none() {
                ["alpha","beta","delta","epsilon"].into_iter().flat_map(|id| [id.to_string(),format!("asset-{id}")]).collect()
            } else {FxHashSet::default()}),
        }
    }).collect()
}

fn paths(db: &SqliteStateDb) -> Vec<String> {
    let conn = db
        .acquire_lock("recent independent publication oracle")
        .unwrap();
    let mut statement = conn
        .prepare("SELECT local_path FROM asset_metadata_paths ORDER BY local_path")
        .unwrap();
    statement
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| {
            let path = std::path::PathBuf::from(r.unwrap());
            format!(
                "{}/{}",
                path.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap(),
                path.file_name().unwrap().to_str().unwrap()
            )
        })
        .collect()
}

fn inventory(uri: &str) -> Arc<Vec<Vec<Value>>> {
    Arc::new(
        ["alpha", "beta", "gamma", "delta", "epsilon"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                let mut records = incremental_photo_records_with_url(
                    id,
                    &format!("{id}.jpg"),
                    &format!("{}/{id}.jpg", uri),
                    MEDIA.len() as u64,
                );
                records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                    json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MEDIA)));
                records[1]["fields"]["assetDate"]["value"] =
                    json!(1_700_000_000_000i64 - index as i64 * 1_000_000);
                records
            })
            .collect::<Vec<_>>(),
    )
}

#[tokio::test]
async fn recent_scope_overflow_reopens_then_increases_removes_and_converges() {
    for scope in [
        crate::cli::RecentScope::Global,
        crate::cli::RecentScope::PerFilter,
    ] {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .mount(&server)
            .await;
        let inventory = inventory(&server.uri());
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("owned.db");
        let mut config = test_config();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("{album}");
        config.recent_scope = scope;
        config.recent = Some(2);
        let previous_path;
        {
            let db = Arc::new(
                SqliteStateDb::open_owned(&database, &owner())
                    .await
                    .unwrap(),
            );
            let session = SelectionSession {
                inventory: inventory.clone(),
                changed: false,
                cancel_at_selection: None,
                queries: Arc::default(),
            };
            let passes = passes(session, &db);
            for (album, names) in [
                ("A", &["alpha", "delta"][..]),
                ("B", &["alpha", "beta", "epsilon"][..]),
            ] {
                let memberships: Vec<_> = names
                    .iter()
                    .map(|name| (format!("asset-{name}"), *name))
                    .collect();
                let refs: Vec<_> = memberships
                    .iter()
                    .map(|(asset, master)| (asset.as_str(), *master))
                    .collect();
                seed_complete_album_snapshot(&db, album, album, &refs).await;
            }
            let old = incremental_photo_records("previous");
            previous_path = seed_downloaded_metadata_asset(
                &db,
                &config,
                &passes[2],
                &PhotoAsset::new(old[0].clone(), old[1].clone()),
            )
            .await;
            db.set_metadata("sync_token:PrimarySync", "saved-cursor")
                .await
                .unwrap();
        }
        let before = tokio::fs::read(&previous_path).await.unwrap();
        for cycle in 0..7 {
            let db = Arc::new(
                SqliteStateDb::open_owned(&database, &owner())
                    .await
                    .unwrap(),
            );
            config.state_db = Some(db.clone());
            config.recent = match cycle {
                0 => Some(2),
                1..=3 => Some(5),
                _ => None,
            };
            let cursor = db
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .unwrap();
            config.sync_mode = SyncMode::Incremental {
                zone_sync_token: cursor.clone(),
            };
            let queries = Arc::new(Mutex::new(Vec::<String>::new()));
            let session = SelectionSession {
                inventory: inventory.clone(),
                changed: cycle == 0,
                cancel_at_selection: None,
                queries: queries.clone(),
            };
            let passes = passes(session, &db);
            let result = download_photos_with_sync(
                &Client::new(),
                &passes,
                Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                matches!(result.outcome, DownloadOutcome::Success),
                "{scope:?} cycle{cycle}: {result:?}"
            );
            if cycle == 0 {
                let mut expected = vec![
                    "A/alpha.JPG",
                    "B/alpha.JPG",
                    "B/beta.JPG",
                    "media/changed.JPG",
                ];
                if scope == crate::cli::RecentScope::PerFilter {
                    expected.push("A/delta.JPG");
                }
                expected.sort_unstable();
                assert_eq!(paths(&db), expected, "{scope:?}");
                assert_eq!(
                    result.stats.downloaded,
                    if scope == crate::cli::RecentScope::Global {
                        3
                    } else {
                        4
                    }
                );
                assert!(result.sync_token.is_none());
                assert!(result.checkpoint.sync_token_blocked);
                let raw = db
                    .get_metadata("recent_selection_recovery:PrimarySync")
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    serde_json::from_str::<Value>(&raw).unwrap()["complete"],
                    false
                );
            } else {
                assert_eq!(
                    paths(&db),
                    [
                        "A/alpha.JPG",
                        "A/delta.JPG",
                        "B/alpha.JPG",
                        "B/beta.JPG",
                        "B/epsilon.JPG",
                        "media/changed.JPG",
                        "media/gamma.JPG"
                    ]
                );
                assert_eq!(
                    result.sync_token.as_deref(),
                    Some("delta-successor"),
                    "rank query token is never the source cursor"
                );
                if cycle == 1 {
                    assert!(result.checkpoint.completed_delta_replay);
                }
                if cycle == 3 {
                    assert!(
                        queries.lock().unwrap().is_empty(),
                        "completed quiet selection must not re-query inventory"
                    );
                }
                if cycle >= 2 {
                    assert_eq!(result.stats.downloaded, 0);
                }
                let transition = || crate::state::CheckpointTransition {
                    legacy_preservation_proofs: Vec::new(),
                    legacy_config_hash: None,
                    sparse_identity_proofs: Vec::new(),
                    metadata_updates: vec![(
                        "sync_token:PrimarySync".to_string(),
                        result.sync_token.clone().unwrap(),
                    )],
                    metadata_deletes: Vec::new(),
                };
                if cycle == 1 {
                    db.acquire_lock("recent successor commit interruption")
                        .unwrap()
                        .execute_batch(
                            "CREATE TRIGGER fault_recent_commit BEFORE UPDATE ON metadata
                         WHEN NEW.key='sync_token:PrimarySync'
                         BEGIN SELECT RAISE(ABORT,'synthetic recent successor interruption'); END;",
                        )
                        .unwrap();
                    assert!(db.commit_checkpoint_transition(transition()).await.is_err());
                    assert_eq!(
                        db.get_metadata("sync_token:PrimarySync")
                            .await
                            .unwrap()
                            .as_deref(),
                        Some("saved-cursor")
                    );
                    db.acquire_lock("remove recent interruption")
                        .unwrap()
                        .execute_batch("DROP TRIGGER fault_recent_commit")
                        .unwrap();
                } else {
                    if cycle == 2 {
                        assert!(
                            !queries.lock().unwrap().is_empty(),
                            "an early receipt cannot skip recovery before its successor commits"
                        );
                    }
                    db.commit_checkpoint_transition(transition()).await.unwrap();
                }
            }
            assert_eq!(tokio::fs::read(&previous_path).await.unwrap(), before);
            assert!(db.get_pending().await.unwrap().is_empty());
            let conn = db
                .acquire_lock("recent source evidence remains retained")
                .unwrap();
            assert_eq!(conn.query_row::<i64,_,_>("SELECT count(*) FROM provider_catalog_records WHERE record_name='future-retained'",[],|r|r.get(0)).unwrap(),1);
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_catalog_debt WHERE reason='unclassified_record'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                1
            );
            drop(conn);
            config.state_db = None;
        }
    }
}

#[tokio::test]
async fn recent_recovery_receipt_task_fault_and_cancellation_preserve_then_reopen() {
    for fault in ["receipt", "task", "cancel"] {
        let server = crate::start_wiremock_or_skip!();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .mount(&server)
            .await;
        let inventory = inventory(&server.uri());
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("owned.db");
        let mut config = test_config();
        config.directory = Arc::from(directory.path().join("media"));
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("{album}");
        config.recent = Some(5);
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "saved-cursor".to_string(),
        };
        let kept;
        {
            let db = Arc::new(
                SqliteStateDb::open_owned(&database, &owner())
                    .await
                    .unwrap(),
            );
            let session = SelectionSession {
                inventory: inventory.clone(),
                changed: false,
                cancel_at_selection: None,
                queries: Arc::default(),
            };
            let selected = passes(session, &db);
            for (album, names) in [
                ("A", &["alpha", "delta"][..]),
                ("B", &["alpha", "beta", "epsilon"][..]),
            ] {
                let owned: Vec<_> = names
                    .iter()
                    .map(|name| (format!("asset-{name}"), *name))
                    .collect();
                let refs: Vec<_> = owned
                    .iter()
                    .map(|(asset, master)| (asset.as_str(), *master))
                    .collect();
                seed_complete_album_snapshot(&db, album, album, &refs).await;
            }
            let old = incremental_photo_records("previous");
            kept = seed_downloaded_metadata_asset(
                &db,
                &config,
                &selected[2],
                &PhotoAsset::new(old[0].clone(), old[1].clone()),
            )
            .await;
            db.set_metadata("sync_token:PrimarySync", "saved-cursor")
                .await
                .unwrap();
        }
        let before = tokio::fs::read(&kept).await.unwrap();
        for cycle in 0..3 {
            let db = Arc::new(
                SqliteStateDb::open_owned(&database, &owner())
                    .await
                    .unwrap(),
            );
            config.state_db = Some(db.clone());
            let cursor = db
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .unwrap();
            config.sync_mode = SyncMode::Incremental {
                zone_sync_token: cursor,
            };
            if cycle == 0 && fault != "cancel" {
                let sql = if fault == "receipt" {
                    "CREATE TRIGGER recent_fault BEFORE INSERT ON metadata
                     WHEN NEW.key='recent_selection_recovery:PrimarySync'
                     BEGIN SELECT RAISE(ABORT,'synthetic recent receipt failure'); END;"
                } else {
                    "CREATE TRIGGER recent_fault BEFORE INSERT ON assets
                     WHEN NEW.id LIKE 'asset-%'
                     BEGIN SELECT RAISE(ABORT,'synthetic recent task failure'); END;"
                };
                db.acquire_lock("inject recent selection fault")
                    .unwrap()
                    .execute_batch(sql)
                    .unwrap();
            }
            let cancel = CancellationToken::new();
            let queries = Arc::new(Mutex::new(Vec::new()));
            let session = SelectionSession {
                inventory: inventory.clone(),
                changed: cycle == 0,
                cancel_at_selection: (cycle == 0 && fault == "cancel").then(|| cancel.clone()),
                queries: queries.clone(),
            };
            let selected = passes(session, &db);
            let result = download_photos_with_sync(
                &Client::new(),
                &selected,
                Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                cancel,
            )
            .await;
            if cycle == 0 {
                if fault == "receipt" {
                    assert!(result.is_err(), "{result:?}");
                } else {
                    let result = result.unwrap();
                    assert!(result.sync_token.is_none(), "{fault}: {result:?}");
                    if fault == "task" {
                        assert!(result.checkpoint.state_write_failures > 0);
                    } else {
                        assert!(result.checkpoint.interrupted);
                    }
                }
                assert_eq!(
                    db.get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("saved-cursor")
                );
                assert_eq!(paths(&db), ["media/changed.JPG"]);
                assert!(
                    server.received_requests().await.unwrap().is_empty(),
                    "fault/cancellation must prevent byte requests"
                );
                if fault != "cancel" {
                    db.acquire_lock("remove recent selection fault")
                        .unwrap()
                        .execute_batch("DROP TRIGGER recent_fault")
                        .unwrap();
                }
            } else {
                let result = result.unwrap();
                assert!(
                    matches!(result.outcome, DownloadOutcome::Success),
                    "{fault} cycle{cycle}: {result:?}"
                );
                assert_eq!(result.sync_token.as_deref(), Some("delta-successor"));
                db.commit_checkpoint_transition(crate::state::CheckpointTransition {
                    legacy_preservation_proofs: Vec::new(),
                    legacy_config_hash: None,
                    sparse_identity_proofs: Vec::new(),
                    metadata_updates: vec![(
                        "sync_token:PrimarySync".to_string(),
                        result.sync_token.unwrap(),
                    )],
                    metadata_deletes: Vec::new(),
                })
                .await
                .unwrap();
                if cycle == 1 {
                    assert_eq!(result.stats.downloaded, 6);
                } else {
                    assert_eq!(result.stats.downloaded, 0);
                    assert!(queries.lock().unwrap().is_empty());
                }
            }
            assert_eq!(tokio::fs::read(&kept).await.unwrap(), before);
            config.state_db = None;
        }
    }
}

#[tokio::test]
async fn recent_one_asset_keeps_both_companion_renditions_and_recovers_unchanged_tail() {
    use wiremock::matchers::path;
    let server = crate::start_wiremock_or_skip!();
    let movie = include_bytes!("../../../../tests/data/media/pattern.mov");
    Mock::given(method("GET"))
        .and(path("/alpha.mov"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(movie.as_slice()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/alpha.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;
    for name in ["beta", "gamma", "delta", "epsilon"] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}.jpg")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut source = (*inventory(&server.uri())).clone();
    source[0][0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
        "downloadURL":format!("{}/alpha.mov",server.uri()),"size":movie.len(),
        "fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))}});
    source[0][0]["fields"]["resOriginalVidComplFileType"] =
        json!({"value":"com.apple.quicktime-movie"});
    let inventory = Arc::new(source);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("owned.db");
    let mut config = test_config();
    config.directory = Arc::from(directory.path().join("media"));
    config.folder_structure = String::new();
    config.recent_scope = crate::cli::RecentScope::Global;
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        if cycle == 0 {
            db.set_metadata("sync_token:PrimarySync", "saved-cursor")
                .await
                .unwrap();
        }
        config.state_db = Some(db.clone());
        config.recent = Some(if cycle == 0 { 1 } else { 5 });
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: db
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .unwrap(),
        };
        let queries = Arc::new(Mutex::new(Vec::new()));
        let mut selected = passes(
            SelectionSession {
                inventory: inventory.clone(),
                changed: cycle == 0,
                cancel_at_selection: None,
                queries: queries.clone(),
            },
            &db,
        );
        let mut pass = selected.pop().unwrap();
        pass.exclude_ids = Arc::default();
        let result = download_photos_with_sync(
            &Client::new(),
            &[pass],
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "{result:?}"
        );
        if cycle == 0 {
            assert_eq!(paths(&db), ["media/alpha.JPG", "media/alpha.MOV"]);
            assert_eq!(result.stats.photos_downloaded, 1);
            assert_eq!(result.stats.videos_downloaded, 1);
            assert!(result.sync_token.is_none());
            assert_eq!(
                tokio::fs::read(directory.path().join("media/alpha.MOV"))
                    .await
                    .unwrap(),
                movie
            );
        } else {
            assert_eq!(result.sync_token.as_deref(), Some("delta-successor"));
            assert_eq!(paths(&db).len(), 6);
            db.commit_checkpoint_transition(crate::state::CheckpointTransition {
                legacy_preservation_proofs: Vec::new(),
                legacy_config_hash: None,
                sparse_identity_proofs: Vec::new(),
                metadata_updates: vec![(
                    "sync_token:PrimarySync".into(),
                    result.sync_token.unwrap(),
                )],
                metadata_deletes: Vec::new(),
            })
            .await
            .unwrap();
            if cycle == 2 {
                assert_eq!(result.stats.downloaded, 0);
                assert!(queries.lock().unwrap().is_empty());
            }
        }
    }
}

#[tokio::test]
async fn recent_receipt_roundtrip_binds_scope_exclusions_and_future_versions() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("owned.db");
    let mut config = test_config();
    config.recent = Some(5);
    {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        config.state_db = Some(db.clone());
        let selected = passes(
            SelectionSession {
                inventory: inventory("https://example.invalid"),
                changed: false,
                cancel_at_selection: None,
                queries: Arc::default(),
            },
            &db,
        );
        super::record(&selected, &config, "successor", true)
            .await
            .unwrap();
    }
    let db = Arc::new(
        SqliteStateDb::open_owned(&database, &owner())
            .await
            .unwrap(),
    );
    config.state_db = Some(db.clone());
    let mut selected = passes(
        SelectionSession {
            inventory: inventory("https://example.invalid"),
            changed: false,
            cancel_at_selection: None,
            queries: Arc::default(),
        },
        &db,
    );
    assert!(
        !super::needs_inventory(&selected, &config, "successor", false)
            .await
            .unwrap()
    );
    selected[2].exclude_ids = Arc::default();
    assert!(
        super::needs_inventory(&selected, &config, "successor", false)
            .await
            .unwrap()
    );
    super::record(&selected, &config, "successor", true)
        .await
        .unwrap();
    selected.remove(0);
    assert!(
        super::needs_inventory(&selected, &config, "successor", false)
            .await
            .unwrap()
    );
    let raw = db
        .get_metadata("recent_selection_recovery:PrimarySync")
        .await
        .unwrap()
        .unwrap();
    let mut future: Value = serde_json::from_str(&raw).unwrap();
    future["version"] = json!(2);
    future["opaque_future"] = json!({"keep":true});
    let future = serde_json::to_string(&future).unwrap();
    db.set_metadata("recent_selection_recovery:PrimarySync", &future)
        .await
        .unwrap();
    config.recent = None;
    assert!(super::active(&config).await.unwrap());
    assert!(
        super::needs_inventory(&selected, &config, "successor", false)
            .await
            .unwrap()
    );
    assert_eq!(
        db.get_metadata("recent_selection_recovery:PrimarySync")
            .await
            .unwrap()
            .unwrap(),
        future
    );
}

#[tokio::test]
async fn recent_mixed_identity_and_source_write_vetoes_keep_healthy_jobs_and_debt() {
    #[derive(Clone)]
    struct MixedSession(SelectionSession);
    #[async_trait::async_trait]
    impl PhotosSession for MixedSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/lookup?") {
                let request: Value = serde_json::from_str(&body)?;
                if request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["recordName"] == "unresolved-child")
                {
                    return Ok(json!({"records":[]}));
                }
            }
            let mut page = self.0.post(url, body, headers).await?;
            if self.0.changed
                && url.contains("/changes/zone?")
                && page["zones"][0]["moreComing"] == true
            {
                let records = page["zones"][0]["records"].as_array_mut().unwrap();
                records.insert(
                    0,
                    json!({"recordName":"unresolved-child","recordType":"CPLAsset","fields":{}}),
                );
                records
                    .push(json!({"recordName":"previous","recordType":"CPLMaster","deleted":true}));
            }
            Ok(page)
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(5)
        .mount(&server)
        .await;
    let inventory = inventory(&server.uri());
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("owned.db");
    let mut config = test_config();
    config.directory = Arc::from(directory.path().join("media"));
    config.folder_structure = String::new();
    config.recent = Some(5);
    let kept;
    {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        let mut selected = passes(
            SelectionSession {
                inventory: inventory.clone(),
                changed: false,
                cancel_at_selection: None,
                queries: Arc::default(),
            },
            &db,
        );
        let mut pass = selected.pop().unwrap();
        pass.exclude_ids = Arc::default();
        let old = incremental_photo_records("previous");
        kept = seed_downloaded_metadata_asset(
            &db,
            &config,
            &pass,
            &PhotoAsset::new(old[0].clone(), old[1].clone()),
        )
        .await;
        db.set_metadata("sync_token:PrimarySync", "saved-cursor")
            .await
            .unwrap();
        db.acquire_lock("inject unrelated source-state fault")
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER mixed_source_fault BEFORE UPDATE ON assets
             WHEN NEW.filename='changed.JPG'
             BEGIN SELECT RAISE(ABORT,'synthetic unrelated source write'); END;",
            )
            .unwrap();
    }
    let bytes = tokio::fs::read(&kept).await.unwrap();
    for cycle in 0..2 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Incremental {
            zone_sync_token: "saved-cursor".into(),
        };
        let mut selected = passes(
            MixedSession(SelectionSession {
                inventory: inventory.clone(),
                changed: true,
                cancel_at_selection: None,
                queries: Arc::default(),
            }),
            &db,
        );
        let mut pass = selected.pop().unwrap();
        pass.exclude_ids = Arc::default();
        let result = download_photos_with_sync(
            &Client::new(),
            &[pass],
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(result.checkpoint.identity_incomplete);
        assert!(result.checkpoint.state_write_failures > 0, "{result:?}");
        assert!(result.checkpoint.sync_token_blocked);
        assert!(result.sync_token.is_none());
        assert!(matches!(
            result.outcome,
            DownloadOutcome::PartialFailure { .. }
        ));
        assert_eq!(result.stats.downloaded, if cycle == 0 { 5 } else { 0 });
        assert_eq!(paths(&db).len(), 6);
        assert_eq!(tokio::fs::read(&kept).await.unwrap(), bytes);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("saved-cursor")
        );
        let raw = db
            .get_metadata("recent_selection_recovery:PrimarySync")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&raw).unwrap()["complete"],
            false
        );
        let connection = db
            .acquire_lock("verify unresolved source retention")
            .unwrap();
        assert_eq!(connection.query_row::<i64,_,_>("SELECT count(*) FROM provider_catalog_records WHERE record_name='unresolved-child'",[],|r|r.get(0)).unwrap(),1);
    }
}
