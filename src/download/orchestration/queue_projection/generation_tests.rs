//! Count-independent generation histories retained from the former recent owner.

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
#[cfg(feature = "xmp")]
use super::super::test_support::relation_delta_record;
use super::super::test_support::{
    incremental_photo_records, incremental_photo_records_with_url, seed_downloaded_metadata_asset,
    test_config,
};

const MEDIA: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
];

#[derive(Clone)]
struct SelectionSession {
    inventory: Arc<Vec<Vec<Value>>>,
    memberships: Option<Arc<HashMap<String, FxHashSet<String>>>>,
    queries: Arc<Mutex<Vec<String>>>,
}

impl SelectionSession {
    fn inventory_for(&self, scope: &str) -> Vec<&Vec<Value>> {
        self.inventory
            .iter()
            .filter(|pair| {
                let id = pair[0]["recordName"].as_str().unwrap();
                if let Some(memberships) = &self.memberships {
                    return memberships
                        .get(scope)
                        .is_none_or(|names| names.contains(id));
                }
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
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
                "records":[],"syncToken":"delta-successor","moreComing":false}]}),
            );
        }
        if url.contains("/internal/records/query/batch?") {
            let batch: Vec<_> = request["batch"].as_array().unwrap().iter().map(|query| {
                let object = query["query"]["filterBy"]["fieldValue"]["value"][0].as_str().unwrap();
                let scope = object.rsplit(':').next().unwrap();
                json!({"records":[{"fields":{"itemCount":{"value":self.inventory_for(scope).len()}}}]})
            }).collect();
            return Ok(json!({"batch":batch}));
        }
        if url.contains("/records/query?") {
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
            let names: FxHashSet<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|record| record["recordName"].as_str())
                .collect();
            return Ok(json!({"records":self.inventory.iter().flatten()
                .filter(|record|names.contains(record["recordName"].as_str().unwrap()))
                .cloned().collect::<Vec<_>>()}));
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

fn passes(session: SelectionSession, db: &Arc<SqliteStateDb>) -> Vec<AlbumPass> {
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
            kind: if scope.is_some() {PassKind::Album} else {PassKind::Unfiled}, album,
            exclude_ids: Arc::new(if scope.is_none() {
                ["alpha","beta","delta","epsilon"].into_iter().flat_map(|id| [id.to_string(),format!("asset-{id}")]).collect()
            } else {FxHashSet::default()}),
        }
    }).collect()
}

fn paths(db: &SqliteStateDb) -> Vec<String> {
    let conn = db.acquire_lock("independent publication oracle").unwrap();
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
                    &format!("{uri}/{id}.jpg"),
                    MEDIA.len() as u64,
                );
                records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                    json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(MEDIA)));
                records[1]["fields"]["assetDate"]["value"] =
                    json!(1_700_000_000_000i64 - index as i64 * 1_000_000);
                records
            })
            .collect(),
    )
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn private_master_metadata_and_relationship_only_invalidation_reseeds_current_facts_without_deletion()
 {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("dependencies.db");
    let mut config = test_config();
    config.directory = Arc::from(directory.path().join("media"));
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("{album}");
    config.metadata.xmp_sidecar = true;
    let mut current = (*inventory(&server.uri())).clone();
    let mut stable_sidecars = None;
    for cycle in 0..7 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        config.state_db = Some(db.clone());
        if cycle == 1 {
            current[0][0]["futureMasterDependency"] = json!({"unknown":[0,255,9]});
        }
        if cycle == 2 {
            current[0][1]["fields"]["captionEnc"] =
                json!({"value":"Current alpha title","type":"STRING"});
        }
        let memberships: HashMap<String, FxHashSet<String>> = [
            (
                "A",
                if cycle < 3 {
                    vec!["alpha", "delta"]
                } else {
                    vec!["alpha"]
                },
            ),
            (
                "B",
                if cycle < 3 {
                    vec!["alpha", "beta", "epsilon"]
                } else {
                    vec!["alpha", "beta", "epsilon", "gamma"]
                },
            ),
        ]
        .into_iter()
        .map(|(name, ids)| {
            (
                name.to_owned(),
                ids.into_iter().map(str::to_owned).collect(),
            )
        })
        .collect();
        let queries = Arc::new(Mutex::new(Vec::new()));
        let session = SelectionSession {
            inventory: Arc::new(current.clone()),
            memberships: Some(Arc::new(memberships.clone())),
            queries: queries.clone(),
        };
        let mut selected = passes(session, &db);
        selected[2].exclude_ids = Arc::new(
            memberships
                .values()
                .flatten()
                .flat_map(|name| [name.clone(), format!("asset-{name}")])
                .collect(),
        );
        let (_, scope, zone) = selected[0].album.owned_private_scope().unwrap().unwrap();
        let capture = ShadowCapture::new(db.clone(), owner(), "com");
        let observed = match cycle {
            1 => vec![current[0][0].clone()],
            2 => vec![current[0][1].clone()],
            3 => {
                let add = relation_delta_record("B", "asset-gamma");
                let mut remove = relation_delta_record("A", "asset-delta");
                remove["deleted"] = json!(true);
                vec![add, remove]
            }
            4 => vec![
                current[0][1].clone(),
                current[2][0].clone(),
                current[0][1].clone(),
            ],
            _ => Vec::new(),
        };
        if !observed.is_empty() {
            let raw = serde_json::to_vec(&json!({"zones":[{"zoneID":zone,"records":observed,"syncToken":format!("dependency-{cycle}-successor"),"moreComing":false}]})).unwrap();
            capture
                .capture(
                    crate::icloud::photos::catalog_observed_page(
                        raw,
                        &scope,
                        &format!("dependency-{cycle}"),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        if cycle == 4 {
            let raw = serde_json::to_vec(&json!({"zones":[{"zoneID":zone,"records":[current[0][0].clone(),current[2][1].clone()],"syncToken":"split-dependency-successor","moreComing":false}]})).unwrap();
            capture
                .capture(
                    crate::icloud::photos::catalog_observed_page(raw, &scope, "split-dependency")
                        .unwrap(),
                )
                .await
                .unwrap();
        }
        if cycle > 0 {
            config.sync_mode = SyncMode::Incremental {
                zone_sync_token: db
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .unwrap(),
            };
        }
        let result = download_photos_with_sync(
            &Client::new(),
            &selected,
            Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "cycle{cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded,
            match cycle {
                0 => 6,
                3 => 2,
                _ => 0,
            },
            "cycle{cycle}: {result:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            if cycle < 3 { 6 } else { 8 }
        );
        let mut expected = vec![
            "A/alpha.JPG",
            "A/delta.JPG",
            "B/alpha.JPG",
            "B/beta.JPG",
            "B/epsilon.JPG",
            "media/gamma.JPG",
        ];
        if cycle >= 3 {
            expected.extend(["B/gamma.JPG", "media/delta.JPG"]);
        }
        expected.sort_unstable();
        assert_eq!(
            paths(&db),
            expected,
            "historical local files are not absence-authorized deletions"
        );
        for relative in &expected {
            let relative = relative.strip_prefix("media/").unwrap_or(relative);
            assert_eq!(
                std::fs::read(config.directory.join(relative)).unwrap(),
                MEDIA
            );
        }
        if cycle >= 2 {
            for album in ["A", "B"] {
                assert!(
                    std::fs::read_to_string(config.directory.join(album).join("alpha.JPG.xmp"))
                        .unwrap()
                        .contains("Current alpha title")
                );
            }
        }
        if cycle == 1 || cycle == 2 || cycle == 3 {
            assert!(
                !queries.lock().unwrap().is_empty(),
                "dependency-only source must dirty and reseed scoped selection"
            );
        }
        let sidecars: Vec<_> = expected
            .iter()
            .map(|relative| {
                let relative = relative.strip_prefix("media/").unwrap_or(relative);
                let media = config.directory.join(relative);
                let path = media.with_file_name(format!(
                    "{}.xmp",
                    media.file_name().unwrap().to_str().unwrap()
                ));
                (
                    path.clone(),
                    std::fs::read(&path).unwrap(),
                    std::fs::metadata(path).unwrap().modified().unwrap(),
                )
            })
            .collect();
        if cycle >= 5 {
            assert!(queries.lock().unwrap().is_empty());
            assert_eq!(
                stable_sidecars.as_ref().unwrap(),
                &sidecars,
                "quiet cycles cannot repeat completed metadata writes"
            );
        }
        if cycle == 4 {
            stable_sidecars = Some(sidecars);
        }
        let successor = result
            .sync_token
            .expect("complete current coverage preserves source checkpoint gate");
        db.commit_checkpoint_transition(crate::state::CheckpointTransition {
            album_snapshots: Vec::new(),
            expected_metadata: Vec::new(),
            legacy_preservation_proofs: Vec::new(),
            legacy_config_hash: None,
            sparse_identity_proofs: Vec::new(),
            metadata_updates: vec![("sync_token:PrimarySync".into(), successor)],
            metadata_deletes: Vec::new(),
        })
        .await
        .unwrap();
        assert!(
            db.acquire_lock("active dependency history")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations",
                    [],
                    |r| r.get(0)
                )
                .unwrap()
                > 0
        );
        config.state_db = None;
    }
}

#[tokio::test]
async fn private_filter_expansion_reopens_retained_coverage_and_converges_without_source_changes() {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let inventory = inventory(&server.uri());
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("filter-history.db");
    let mut config = test_config();
    config.directory = Arc::from(directory.path().join("media"));
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("{album}");
    config.filename_exclude = Arc::from([glob::Pattern::new("epsilon.*").unwrap()]);
    let existing;
    {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        let session = SelectionSession {
            inventory: inventory.clone(),
            memberships: None,
            queries: Arc::default(),
        };
        let passes = passes(session, &db);
        let previous = incremental_photo_records("previous");
        existing = seed_downloaded_metadata_asset(
            &db,
            &config,
            &passes[2],
            &PhotoAsset::new(previous[0].clone(), previous[1].clone()),
        )
        .await;
        std::fs::write(existing.with_extension("xmp"), b"retained-old-sidecar").unwrap();
        db.set_metadata("sync_token:PrimarySync", "saved-cursor")
            .await
            .unwrap();
    }
    let old_media = std::fs::read(&existing).unwrap();
    for cycle in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &owner())
                .await
                .unwrap(),
        );
        config.state_db = Some(db.clone());
        if cycle > 0 {
            config.filename_exclude = Arc::from([]);
            config.sync_mode = SyncMode::Incremental {
                zone_sync_token: db
                    .get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .unwrap(),
            };
        }
        let queries = Arc::new(Mutex::new(Vec::new()));
        let session = SelectionSession {
            inventory: inventory.clone(),
            memberships: None,
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
            "cycle={cycle}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded,
            match cycle {
                0 => 5,
                1 => 1,
                _ => 0,
            }
        );
        let mut expected = vec![
            "A/alpha.JPG",
            "A/delta.JPG",
            "B/alpha.JPG",
            "B/beta.JPG",
            "media/changed.JPG",
            "media/gamma.JPG",
        ];
        if cycle > 0 {
            expected.push("B/epsilon.JPG");
        }
        expected.sort_unstable();
        assert_eq!(paths(&db), expected, "cycle={cycle}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            if cycle == 0 { 5 } else { 6 }
        );
        if cycle >= 2 {
            assert!(
                queries.lock().unwrap().is_empty(),
                "unchanged complete coverage must not reselect inventory"
            );
        }
        let successor = result
            .sync_token
            .expect("complete unbounded selection retains the existing source bridge");
        assert_eq!(
            successor,
            if cycle == 0 {
                "rank-query-token"
            } else {
                "delta-successor"
            },
            "retain the existing full-query and incremental source checkpoint owners"
        );
        db.commit_checkpoint_transition(crate::state::CheckpointTransition {
            album_snapshots: Vec::new(),
            expected_metadata: Vec::new(),
            legacy_preservation_proofs: Vec::new(),
            legacy_config_hash: None,
            sparse_identity_proofs: Vec::new(),
            metadata_updates: vec![("sync_token:PrimarySync".into(), successor)],
            metadata_deletes: Vec::new(),
        })
        .await
        .unwrap();
        assert!(
            db.acquire_lock("active filtered and expanded generations")
                .unwrap()
                .query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_active_generations",
                    [],
                    |row| row.get(0)
                )
                .unwrap()
                > 0
        );
        assert_eq!(std::fs::read(&existing).unwrap(), old_media);
        assert_eq!(
            std::fs::read(existing.with_extension("xmp")).unwrap(),
            b"retained-old-sidecar"
        );
        config.state_db = None;
    }
}
