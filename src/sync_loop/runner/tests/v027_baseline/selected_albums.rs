//! Selected album additions and removals use current membership after reopen.
use super::{MEDIA, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoLibrary, PhotosSession};
use crate::state::DownloadStateStore;
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    make_run_cycle_config, make_run_cycle_download_config_builder,
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
    cycle: usize,
    unavailable_membership: bool,
    incomplete_inventory: bool,
    incomplete_hits: Arc<AtomicUsize>,
    records: Arc<Vec<Vec<Value>>>,
    rank_queries: Arc<AtomicUsize>,
    lookup_calls: Arc<AtomicUsize>,
}
fn parent(request: &Value) -> Option<&str> {
    request
        .pointer("/query/filterBy")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|f| f["fieldName"] == "parentId")
        .and_then(|f| f.pointer("/fieldValue/value"))
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .pointer("/query/filterBy/fieldValue/value/0")
                .and_then(Value::as_str)
                .and_then(|kind| kind.rsplit_once(':').map(|(_, name)| name))
        })
        .or_else(|| {
            request
                .pointer("/query/recordType")
                .and_then(Value::as_str)
                .and_then(|t| t.rsplit_once(':').map(|(_, name)| name))
        })
}
impl Session {
    fn members(&self, album: Option<&str>) -> Vec<Value> {
        let indices: Vec<usize> = match (album, self.cycle) {
            (Some("A"), 0) => vec![0],
            (Some("A"), _) => vec![],
            (Some("B"), 0) => vec![1],
            (Some("B"), _) => vec![0],
            _ => vec![0, 1, 2],
        };
        indices
            .into_iter()
            .flat_map(|index| self.records[index].clone())
            .collect()
    }
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
                json!({"zones":[{"zoneID":zone(),"syncToken":"album-boundary","deleted":false}]}),
            );
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            let batch:Vec<_>=request["batch"].as_array().unwrap().iter().map(|query|
                json!({"records":[{"fields":{"itemCount":{"value":self.members(parent(query)).len()/2}}}]})).collect();
            return Ok(json!({"batch":batch}));
        }
        if url.contains("/records/query?") {
            if self.incomplete_inventory && parent(&request) == Some("B") {
                self.incomplete_hits.fetch_add(1, Ordering::SeqCst);
                return Ok(json!({"records":"incomplete selected album inventory"}));
            }
            self.rank_queries.fetch_add(1, Ordering::SeqCst);
            let offset = request["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.members(parent(&request))
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":"album-query-proof"}));
        }
        if url.contains("/records/lookup?") {
            self.lookup_calls.fetch_add(1, Ordering::SeqCst);
            if self.unavailable_membership {
                return Ok(json!({"records":[]}));
            }
            let requested = request["records"].as_array().unwrap();
            let mut returned = std::collections::HashSet::new();
            let records: Vec<_> = self
                .records
                .iter()
                .flatten()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
                .filter(|record| returned.insert(record["recordName"].as_str().unwrap()))
                .cloned()
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/changes/zone?") {
            let relation = |album: &str, id: &str, deleted: bool| {
                json!({
                    "recordName":format!("asset-{id}-IN-{album}"),"recordType":"CPLContainerRelation",
                    "recordChangeTag":"membership-change",
                    "fields":{"containerId":{"value":album,"type":"STRING"},
                        "itemId":{"value":format!("asset-{id}"),"type":"STRING"},
                        "isDeleted":{"value":i64::from(deleted),"type":"INT64"}}
                })
            };
            let records = if self.cycle == 1 {
                vec![
                    relation("A", "album-a", true),
                    relation("B", "album-a", false),
                    relation("B", "album-b", true),
                ]
            } else {
                Vec::new()
            };
            return Ok(json!({"zones":[{"zoneID":zone(),"records":records,
                "syncToken":format!("album-after-{}",self.cycle+1),"moreComing":false}]}));
        }
        anyhow::bail!("unexpected album coverage endpoint {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
fn selected_album(
    name: &str,
    session: &Session,
    capture: &crate::icloud::photos::inbox::ShadowCapture,
) -> AlbumPass {
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: "https://synthetic.example/database/1/photos/production/private"
                .into(),
            name: name.into(),
            list_type: "CPLContainerRelationLiveByAssetDate".into(),
            obj_type: format!("CPLContainerRelationNotDeletedByAssetDate:{name}").into(),
            query_filter: Some(Arc::new(
                json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":name}}]),
            )),
            page_size: 100,
            zone_id: Arc::new(zone()),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: Some(name.into()),
            cross_zone_sources: Vec::new(),
        },
        Box::new(session.clone()),
    );
    album.set_shadow_capture(capture.clone(), "private".into());
    AlbumPass {
        kind: PassKind::Album,
        album,
        exclude_ids: Arc::default(),
    }
}

#[tokio::test]
async fn v027_album_relation_only_changes_route_new_copies_and_unfiled_after_reopen() {
    album_contract(false, false).await;
}
#[tokio::test]
async fn v027_album_membership_confirmation_failure_holds_cursor_then_recovers() {
    album_contract(true, false).await;
}
#[tokio::test]
async fn v027_incomplete_selected_album_inventory_holds_baseline_then_recovers() {
    album_contract(false, true).await;
}
async fn album_contract(interrupt_membership: bool, interrupt_inventory: bool) {
    let server = crate::start_wiremock_or_skip!();
    for (id, gets) in [("album-a", 2), ("album-b", 1), ("unfiled", 1)] {
        Mock::given(method("GET"))
            .and(path(format!("/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(gets)
            .mount(&server)
            .await;
    }
    let records = Arc::new(
        ["album-a", "album-b", "unfiled"]
            .into_iter()
            .enumerate()
            .map(|(i, id)| {
                pair(
                    id,
                    &format!("{id}.JPG"),
                    &format!("{}/{id}", server.uri()),
                    30 + u8::try_from(i).unwrap(),
                )
            })
            .collect(),
    );
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let token_key = sync_token_key(ZONE);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut originals = Vec::new();
    let mut interrupted_downloads = 0;
    for iteration in 0..if interrupt_membership || interrupt_inventory {
        4
    } else {
        3
    } {
        let held =
            (interrupt_membership && iteration == 1) || (interrupt_inventory && iteration == 0);
        let cycle =
            if (interrupt_membership && iteration > 1) || (interrupt_inventory && iteration > 0) {
                iteration - 1
            } else {
                iteration
            };
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = Session {
            cycle,
            unavailable_membership: held && interrupt_membership,
            incomplete_inventory: held && interrupt_inventory,
            incomplete_hits: Arc::new(AtomicUsize::new(0)),
            records: Arc::clone(&records),
            rank_queries: Arc::new(AtomicUsize::new(0)),
            lookup_calls: Arc::new(AtomicUsize::new(0)),
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
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
        library.shadow_capture = Some(capture.clone());
        let passes = vec![
            selected_album("A", &session, &capture),
            selected_album("B", &session, &capture),
            AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::default(),
            },
        ];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "unfiled".into();
            config.folder_structure_albums = "albums/{album}".into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = false;
            }
            Arc::new(config)
        };
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
        .await
        .unwrap();
        if held {
            if interrupt_inventory {
                assert!(
                    session.incomplete_hits.load(Ordering::SeqCst) > 0,
                    "malformed album inventory must be requested"
                );
                interrupted_downloads = result.stats.downloaded;
                for relative in ["albums/A/album-a.JPG", "unfiled/unfiled.JPG"] {
                    let path = root.join(relative);
                    if path.exists() {
                        originals.push((
                            path.clone(),
                            std::fs::read(&path).unwrap(),
                            std::fs::metadata(&path).unwrap().modified().unwrap(),
                        ));
                    }
                }
            }
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert_eq!(
                db.get_metadata(&token_key).await.unwrap().as_deref(),
                if interrupt_inventory {
                    None
                } else {
                    Some("album-after-1")
                }
            );
            assert!(!root.join("albums/B/album-a.JPG").exists());
            for (path, bytes, mtime) in &originals {
                assert_eq!(&std::fs::read(path).unwrap(), bytes);
                assert_eq!(&std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
            }
            continue;
        }
        assert_eq!(result.failed_count, 0, "cycle {cycle}: {result:?}");
        assert_eq!(
            result.stats.downloaded,
            match cycle {
                0 => 3 - interrupted_downloads,
                1 => 1,
                _ => 0,
            },
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap(),
            Some(format!("album-after-{}", cycle + 1)),
            "cycle {cycle}: {result:?}"
        );
        if cycle == 0 {
            for relative in [
                "albums/A/album-a.JPG",
                "albums/B/album-b.JPG",
                "unfiled/unfiled.JPG",
            ] {
                let path = root.join(relative);
                originals.push((
                    path.clone(),
                    std::fs::read(&path).unwrap(),
                    std::fs::metadata(&path).unwrap().modified().unwrap(),
                ));
            }
            assert!(!root.join("unfiled/album-a.JPG").exists());
            assert!(!root.join("unfiled/album-b.JPG").exists());
        } else {
            assert_eq!(
                std::fs::read(root.join("albums/B/album-a.JPG")).unwrap(),
                MEDIA
            );
            assert!(
                !root.join("unfiled/album-b.JPG").exists(),
                "new Unfiled membership must reuse the recorded B copy without layout convergence"
            );
            {
                let conn = db
                    .acquire_lock("current Unfiled membership has positive durable completion")
                    .unwrap();
                let key:String=conn.query_row(
                    "SELECT json_extract(p.value,'$.key') FROM provider_active_generations g,json_each(CAST(g.specification AS TEXT),'$.profile.passes') p WHERE g.scope LIKE ?1 AND json_extract(p.value,'$.kind')='Unfiled' ORDER BY g.created_at DESC LIMIT 1",
                    [format!("%{ZONE}%")],|row|row.get(0)).unwrap();
                let native = crate::state::db::provider_selection::SelectionPath::from_path(
                    &root.join("albums/B/album-b.JPG"),
                )
                .key()
                .unwrap();
                let aliases:Vec<String>=conn.prepare("SELECT origin_pass FROM download_receipts WHERE library=?1 AND child='asset-album-b' AND native_path=?2").unwrap()
                    .query_map(rusqlite::params![ZONE,native],|row|row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
                assert!(
                    aliases.into_iter().any(|alias| alias == key
                        || serde_json::from_str::<Vec<String>>(&alias)
                            .is_ok_and(|keys| keys.contains(&key))),
                    "retaining B/b alone does not account for current Unfiled membership"
                );
            }
            assert!(
                !root.join("unfiled/album-a.JPG").exists(),
                "A removal must respect current B membership"
            );
            assert_eq!(
                session.rank_queries.load(Ordering::SeqCst),
                0,
                "relation hydration must not replay rank inventory"
            );
        }
        for (path, bytes, mtime) in &originals {
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
            assert_eq!(&std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
        }
    }
    server.verify().await;
}

#[tokio::test]
async fn v027_shared_asset_in_two_selected_albums_keeps_historical_paths_on_quiet_reopen() {
    let (trace, _guard) = crate::test_helpers::TracingCapture::install();
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/shared"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(0)
        .mount(&server)
        .await;

    let shared = pair(
        "shared",
        "shared.JPG",
        &format!("{}/shared", server.uri()),
        70,
    );
    let records = Arc::new(vec![
        shared.clone(),
        shared,
        pair(
            "unselected",
            "unselected.JPG",
            &format!("{}/unselected", server.uri()),
            71,
        ),
    ]);

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    let a_path = root.join("albums/A/shared.JPG");
    let b_path = root.join("albums/B/shared.JPG");
    std::fs::create_dir_all(a_path.parent().unwrap()).unwrap();
    std::fs::create_dir_all(b_path.parent().unwrap()).unwrap();
    std::fs::write(&a_path, MEDIA).unwrap();
    std::fs::write(&b_path, MEDIA).unwrap();

    let snapshots = [&a_path, &b_path].map(|path| {
        (
            path.to_path_buf(),
            std::fs::read(path).unwrap(),
            std::fs::metadata(path).unwrap().modified().unwrap(),
        )
    });

    let database = dir.path().join("state.db");
    let token_key = sync_token_key(ZONE);
    let checksum = {
        use sha2::Digest as _;
        sha2::Sha256::digest(MEDIA)
            .iter()
            .map(|byte| format!("{:02x}", *byte))
            .collect::<String>()
    };
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    for iteration in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if iteration == 0 {
            seed_shared_historical_pair(db.as_ref(), &records[0], &a_path, &b_path, &checksum)
                .await
                .unwrap();
        }

        let session = Session {
            cycle: 0,
            unavailable_membership: false,
            incomplete_inventory: false,
            incomplete_hits: Arc::new(AtomicUsize::new(0)),
            records: Arc::clone(&records),
            rank_queries: Arc::new(AtomicUsize::new(0)),
            lookup_calls: Arc::new(AtomicUsize::new(0)),
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
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
        library.shadow_capture = Some(capture.clone());
        let passes = vec![
            selected_album("A", &session, &capture),
            selected_album("B", &session, &capture),
        ];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
        selected.library = library;

        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure_albums = "albums/{album}".into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = false;
            }
            Arc::new(config)
        };

        let result = run_cycle(
            &[&selected],
            &make_run_cycle_config(),
            Some(db.as_ref()),
            false,
            &builder,
            download::DownloadControls::download_hidden(),
            &shared_session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        if result.failed_count != 0 {
            eprintln!("TRACE {:?}", trace.events());
        }
        assert_eq!(result.failed_count, 0, "iteration {iteration}: {result:?}");
        assert_eq!(
            result.stats.downloaded, 0,
            "iteration {iteration}: {result:?}"
        );
        assert!(
            result.can_advance_database_checkpoint(),
            "iteration {iteration}: {result:?}"
        );
        assert_eq!(
            db.get_metadata(&token_key).await.unwrap().as_deref(),
            Some("album-after-1"),
            "iteration {iteration}: {result:?}"
        );
        if iteration > 0 {
            assert_eq!(
                session.rank_queries.load(Ordering::SeqCst),
                0,
                "quiet reopen must not replay rank inventory: {result:?}"
            );
        }

        for (path, old_bytes, old_mtime) in &snapshots {
            assert!(
                path.exists(),
                "historical path vanished: {}",
                path.display()
            );
            assert_eq!(
                &std::fs::read(path).unwrap(),
                old_bytes,
                "historical bytes changed: {}",
                path.display()
            );
            assert_eq!(
                &std::fs::metadata(path).unwrap().modified().unwrap(),
                old_mtime,
                "historical mtime changed: {}",
                path.display()
            );
        }
        assert!(!root.join("albums/A/unselected.JPG").exists());
        assert!(!root.join("albums/B/unselected.JPG").exists());

        // Each selected destination must have its own settled receipt, not
        // merely an empty work queue or an untouched unaccounted old copy.
        {
            let conn = db
                .acquire_lock("historical selected destination receipts")
                .unwrap();
            for path in [&a_path, &b_path] {
                let key = crate::state::db::provider_selection::SelectionPath::from_path(path)
                    .key()
                    .unwrap();
                let receipts: i64 = conn.query_row(
                "SELECT COUNT(*) FROM download_receipts WHERE child='asset-shared' AND native_path=?1",
                [key], |row| row.get(0)).unwrap();
                assert!(
                    receipts > 0,
                    "selected historical path lacks completion: {}",
                    path.display()
                );
            }
        }
        let observed = db.observed_selection_status().await.unwrap();
        assert_eq!(observed.len(), 1, "{result:?}");
        assert!(
            !observed[0].settlement_unknown,
            "iteration {iteration}: {result:?}"
        );
        assert!(
            !observed[0].pending_media,
            "iteration {iteration}: {result:?}"
        );
        assert!(
            !observed[0].pending_metadata,
            "iteration {iteration}: {result:?}"
        );
    }

    server.verify().await;
}

async fn seed_shared_historical_pair(
    db: &SqliteStateDb,
    records: &[Value],
    a_path: &std::path::Path,
    b_path: &std::path::Path,
    checksum: &str,
) -> anyhow::Result<()> {
    let asset = crate::icloud::photos::PhotoAsset::new(records[0].clone(), records[1].clone());
    let record = crate::test_helpers::TestAssetRecord::new(asset.state_id())
        .library(ZONE)
        .filename("shared.JPG")
        .checksum(&super::generation(70))
        .size(MEDIA.len() as u64)
        .created_at(asset.created())
        .added_at(asset.added_date())
        .build();
    db.upsert_seen(&record).await?;
    for path in [a_path, b_path] {
        db.mark_verified_download(
            ZONE,
            asset.state_id(),
            "original",
            path,
            checksum,
            Some(checksum),
            false,
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn v027_initial_album_lookup_refusal_accounts_baseline_and_retries_after_reopen() {
    initial_album_confirmation_retry(false).await;
}

#[tokio::test]
async fn v027_initial_album_refusal_recovers_persisted_legacy_owner_without_duplicate() {
    initial_album_confirmation_retry(true).await;
}

async fn initial_album_confirmation_retry(legacy: bool) {
    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/album-a"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(u64::from(!legacy))
        .mount(&server)
        .await;
    let records: Arc<Vec<Vec<Value>>> = Arc::new(
        ["album-a", "album-b", "unfiled"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                pair(
                    id,
                    &format!("{id}.JPG"),
                    &format!("{}/{id}", server.uri()),
                    30 + u8::try_from(index).unwrap(),
                )
            })
            .collect(),
    );
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let database = dir.path().join("state.db");
    let legacy_path = root.join("A/album-a.JPG");
    let legacy_time = if legacy {
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        std::fs::write(&legacy_path, MEDIA).unwrap();
        let db = SqliteStateDb::open_owned(&database, &baseline_owner())
            .await
            .unwrap();
        let asset =
            crate::icloud::photos::PhotoAsset::new(records[0][0].clone(), records[0][1].clone());
        let row = crate::test_helpers::TestAssetRecord::new("album-a")
            .library(ZONE)
            .filename("album-a.JPG")
            .checksum(&super::generation(30))
            .size(MEDIA.len() as u64)
            .created_at(asset.created())
            .added_at(asset.added_date())
            .build();
        db.upsert_seen(&row).await.unwrap();
        let hash = download::file::compute_sha256(&legacy_path).await.unwrap();
        db.mark_verified_download(
            ZONE,
            "album-a",
            "original",
            &legacy_path,
            &hash,
            Some(&hash),
            false,
        )
        .await
        .unwrap();
        assert!(
            db.claim_legacy_master_state_owner(ZONE, "album-a", "asset-album-a")
                .await
                .unwrap()
        );
        Some(std::fs::metadata(&legacy_path).unwrap().modified().unwrap())
    } else {
        None
    };
    let token_key = sync_token_key(ZONE);
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let ranks = Arc::new(AtomicUsize::new(0));
    let lookups = Arc::new(AtomicUsize::new(0));
    let mut baseline_ranks = 0;
    let mut failed_lookups = 0;
    let mut completed = None;
    for iteration in 0..4 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        if iteration == 2 {
            db.make_selection_retry_due(baseline_owner(), "asset-album-a".into())
                .await
                .unwrap();
        }
        let session = Session {
            cycle: 0,
            unavailable_membership: iteration < 2,
            incomplete_inventory: false,
            incomplete_hits: Arc::new(AtomicUsize::new(0)),
            records: records.clone(),
            rank_queries: ranks.clone(),
            lookup_calls: lookups.clone(),
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
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
        library.shadow_capture = Some(capture.clone());
        let passes = vec![selected_album("A", &session, &capture)];
        let mut selected = make_run_cycle_library_state_with_passes(ZONE, &token_key, passes);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure_albums = "{album}".into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = false;
            }
            Arc::new(config)
        };
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
        .await
        .unwrap();
        assert!(
            db.get_metadata(&token_key).await.unwrap().is_some(),
            "accounted baseline has a cursor: {result:?}"
        );
        assert!(
            db.get_metadata(&format!("observed_selection:v27:{ZONE}"))
                .await
                .unwrap()
                .is_some()
        );
        let outstanding=db.acquire_lock("actual runner deferred observations").unwrap().query_row::<i64,_,_>(
            "SELECT count(*) FROM provider_active_decisions WHERE child='asset-album-a' AND admission='deferred'",[],|r|r.get(0)).unwrap();
        if iteration < 2 {
            assert_eq!(outstanding, 1);
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(root.join("A/album-a.JPG").exists(), legacy);
            if iteration == 0 {
                baseline_ranks = ranks.load(Ordering::SeqCst);
                failed_lookups = lookups.load(Ordering::SeqCst);
                assert!(baseline_ranks > 0 && failed_lookups > 0);
            } else {
                assert_eq!(
                    lookups.load(Ordering::SeqCst),
                    failed_lookups,
                    "backoff survives reopen"
                );
            }
        } else {
            assert_eq!(outstanding, 0);
            assert_eq!(
                result.stats.downloaded,
                usize::from(iteration == 2 && !legacy)
            );
            let path = root.join("A/album-a.JPG");
            assert_eq!(std::fs::read(&path).unwrap(), MEDIA);
            let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
            if let Some(before) = completed {
                assert_eq!(modified, before);
            } else {
                completed = Some(modified);
            }
        }
        if let Some(time) = legacy_time {
            assert_eq!(std::fs::read(&legacy_path).unwrap(), MEDIA);
            assert_eq!(
                std::fs::metadata(&legacy_path).unwrap().modified().unwrap(),
                time
            );
            assert_eq!(
                db.acquire_lock("legacy identity remains singular")
                    .unwrap()
                    .query_row::<i64, _, _>(
                        "SELECT count(*) FROM assets WHERE library=?1 AND id='asset-album-a'",
                        [ZONE],
                        |r| r.get(0)
                    )
                    .unwrap(),
                0
            );
        }
        if iteration > 0 {
            assert_eq!(
                ranks.load(Ordering::SeqCst),
                baseline_ranks,
                "pending work must not force another inventory"
            );
        }
    }
}

/// Reporter C16 prerequisite, unchanged production baseline.
///
/// The actual resolver for M1 (`libraries = shared`, no albums, `unfiled =
/// true`, `--smart-folder Hidden`) widens collection enumeration to every
/// visible library while Unfiled stays selected-zone scoped. That produces
/// `PrimarySync` Hidden plus `SharedSync` Hidden+Unfiled. A still+motion pair
/// downloaded before this ExistingUpgrade baseline boundary is omitted from
/// the fresh inventory and from the empty delta bridge, yet an exact strict
/// paired lookup proves both family records deleted. The mixed plan must still
/// census per authenticated source instead of requiring exactly one
/// unrestricted Unfiled pass. Local bytes, paths, and mtimes stay untouched.
#[tokio::test]
async fn v027_mixed_shared_hidden_baseline_censuses_preboundary_deleted_live_pair() {
    use crate::commands::{
        build_collection_context, collection_libraries, pass_scope_for_zone,
        resolve_passes_for_scope, zone_name_set,
    };
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    const PRIMARY: &str = "PrimarySync";
    const DELETED_MASTER: &str = "shared-live-master";
    const DELETED_CHILD: &str = "asset-shared-live-master";

    fn mixed_pair(
        zone_id: &Value,
        record_name: &str,
        filename: &str,
        url: &str,
        tag: u8,
        hidden: bool,
    ) -> Vec<Value> {
        let zone = zone_id["zoneName"].as_str().unwrap();
        let mut records = crate::sync_loop::test_support::full_album_page_with_download(
            zone,
            record_name,
            "inventory-last-page",
            url,
            MEDIA.len() as u64,
            &super::generation(tag),
        )["records"]
            .as_array()
            .unwrap()
            .clone();
        records[0]["zoneID"] = zone_id.clone();
        records[0]["fields"]["filenameEnc"] = json!({"value":filename,"type":"STRING"});
        records[1]["zoneID"] = zone_id.clone();
        records[1]["fields"]["masterRef"]["value"]["zoneID"] = zone_id.clone();
        if hidden {
            records[1]["fields"]["isHidden"] = json!({"value":1,"type":"INT64"});
        }
        records
    }

    fn deleted_lookup_records(zone_id: &Value) -> Vec<Value> {
        vec![
            json!({
                "recordName":DELETED_MASTER,"recordType":"CPLMaster","zoneID":zone_id,
                "fields":{
                    "isDeleted":{"value":1,"type":"INT64"},
                    "isExpunged":{"value":1,"type":"INT64"},
                    "dateExpunged":{"value":1792541032000_i64,"type":"TIMESTAMP"}
                }
            }),
            json!({
                "recordName":DELETED_CHILD,"recordType":"CPLAsset","zoneID":zone_id,
                "fields":{
                    "masterRef":{"value":{"recordName":DELETED_MASTER,"zoneID":zone_id},"type":"REFERENCE"},
                    "isDeleted":{"value":1,"type":"INT64"},
                    "isExpunged":{"value":1,"type":"INT64"},
                    "dateExpunged":{"value":1792541032000_i64,"type":"TIMESTAMP"}
                }
            }),
        ]
    }

    #[derive(Clone)]
    struct MixedSession {
        zone_id: Value,
        zone: String,
        boundary: String,
        current: Arc<Vec<Value>>,
        current_on_hidden: bool,
        deleted: Arc<Vec<Value>>,
        lookups: Arc<AtomicUsize>,
        requested: Arc<Mutex<Vec<Vec<String>>>>,
        changes: Arc<AtomicUsize>,
    }
    impl MixedSession {
        fn current_pass(&self, request: &Value) -> bool {
            let record_type = request
                .pointer("/query/recordType")
                .and_then(Value::as_str)
                .unwrap_or("");
            if record_type.contains("WithoutHiddenOrDeleted") {
                !self.current_on_hidden
            } else if record_type.contains("Hidden") {
                self.current_on_hidden
            } else {
                false
            }
        }
    }
    #[async_trait::async_trait]
    impl PhotosSession for MixedSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            if url.contains("/zones/list") {
                return Ok(
                    json!({"zones":[{"zoneID":self.zone_id,"syncToken":self.boundary,"deleted":false}]}),
                );
            }
            if request.pointer("/query/recordType").and_then(Value::as_str)
                == Some("CheckIndexingState")
            {
                return Ok(
                    json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                let batch: Vec<Value> = request["batch"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|query| {
                        let record_type = query
                            .pointer("/query/recordType")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let count = if record_type.contains("WithoutHiddenOrDeleted") {
                            u64::from(!self.current_on_hidden)
                        } else if record_type.contains("Hidden") {
                            u64::from(self.current_on_hidden)
                        } else {
                            0
                        };
                        json!({"records":[{"fields":{"itemCount":{"value":count}}}]})
                    })
                    .collect();
                return Ok(json!({"batch":batch}));
            }
            if url.contains("/records/query?") {
                let record_type = request
                    .pointer("/query/recordType")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Album folder enumeration for the PrimarySync library.
                if record_type == "CPLAlbumByPositionLive" {
                    return Ok(json!({"records":[]}));
                }
                let records = if self.current_pass(&request) {
                    self.current.as_ref().clone()
                } else {
                    Vec::new()
                };
                return Ok(json!({"records":records,"syncToken":"inventory-last-page"}));
            }
            if url.contains("/changes/zone?") {
                let bridge = self.changes.fetch_add(1, Ordering::SeqCst);
                let expected = if bridge == 0 {
                    self.boundary.clone()
                } else {
                    format!("after-{}-{bridge}", self.zone)
                };
                assert_eq!(
                    request["zones"][0]["syncToken"], expected,
                    "baseline bridge must use its saved {} boundary",
                    self.zone
                );
                return Ok(json!({"zones":[{"zoneID":self.zone_id,
                    "records":[],
                    "syncToken":format!("after-{}-{}",self.zone,bridge+1),
                    "moreComing":false}]}));
            }
            if url.contains("/records/lookup?") {
                self.lookups.fetch_add(1, Ordering::SeqCst);
                let requested: Vec<String> = request["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|query| query["recordName"].as_str().map(str::to_owned))
                    .collect();
                self.requested.lock().unwrap().push(requested.clone());
                let records: Vec<Value> = self
                    .deleted
                    .iter()
                    .filter(|record| {
                        requested
                            .iter()
                            .any(|name| record["recordName"].as_str() == Some(name))
                    })
                    .cloned()
                    .collect();
                return Ok(json!({"records":records}));
            }
            anyhow::bail!("unexpected mixed-source endpoint {url}")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let server = crate::start_wiremock_or_skip!();
    for id in ["shared-current", "primary-current"] {
        Mock::given(method("GET"))
            .and(path(format!("/{id}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    super::seed_existing_upgrade_db(&db_path);
    let db = Arc::new(
        SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap(),
    );

    let deleted_paths = [root.join("live.jpg"), root.join("live.mov")];
    let deleted_bytes = [
        b"synthetic still".as_slice(),
        b"synthetic motion".as_slice(),
    ];
    for ((version, name), (path, bytes)) in [
        (crate::state::VersionSizeKey::Original, "live.jpg"),
        (crate::state::VersionSizeKey::LiveOriginal, "live.mov"),
    ]
    .into_iter()
    .zip(deleted_paths.iter().zip(deleted_bytes))
    {
        std::fs::write(path, bytes).unwrap();
        let record = crate::test_helpers::TestAssetRecord::new(DELETED_MASTER)
            .library(ZONE)
            .version_size(version)
            .added_at(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap())
            .filename(name)
            .checksum(&super::generation(90))
            .size(MEDIA.len() as u64)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            ZONE,
            DELETED_MASTER,
            version.as_str(),
            path,
            "synthetic-local",
            None,
        )
        .await
        .unwrap();
    }
    db.upsert_asset_master_mapping(ZONE, DELETED_CHILD, DELETED_MASTER)
        .await
        .unwrap();
    assert!(
        db.claim_legacy_master_state_owner(ZONE, DELETED_MASTER, DELETED_CHILD)
            .await
            .unwrap()
    );
    let deleted_snapshots = deleted_paths.map(|path| {
        (
            path.clone(),
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().modified().unwrap(),
        )
    });

    let shared_zone_id = json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"});
    let primary_zone_id = json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"});
    let shared_current = mixed_pair(
        &shared_zone_id,
        "shared-current",
        "shared-current.JPG",
        &format!("{}/shared-current", server.uri()),
        91,
        false,
    );
    let primary_current = mixed_pair(
        &primary_zone_id,
        "primary-current",
        "primary-current.JPG",
        &format!("{}/primary-current", server.uri()),
        92,
        true,
    );
    let mut shared_lookup_records = deleted_lookup_records(&shared_zone_id);
    shared_lookup_records.extend(shared_current.iter().cloned());
    let shared_deleted = Arc::new(shared_lookup_records);
    let primary_deleted = Arc::new(primary_current.clone());
    let shared_lookups = Arc::new(AtomicUsize::new(0));
    let primary_lookups = Arc::new(AtomicUsize::new(0));
    let shared_requests = Arc::new(Mutex::new(Vec::new()));
    let primary_requests = Arc::new(Mutex::new(Vec::new()));

    let shared_session = MixedSession {
        zone_id: shared_zone_id.clone(),
        zone: ZONE.into(),
        boundary: format!("before-{ZONE}"),
        current: Arc::new(shared_current),
        current_on_hidden: false,
        deleted: Arc::clone(&shared_deleted),
        lookups: Arc::clone(&shared_lookups),
        requested: Arc::clone(&shared_requests),
        changes: Arc::new(AtomicUsize::new(0)),
    };
    let primary_session = MixedSession {
        zone_id: primary_zone_id.clone(),
        zone: PRIMARY.into(),
        boundary: format!("before-{PRIMARY}"),
        current: Arc::new(primary_current),
        current_on_hidden: true,
        deleted: Arc::clone(&primary_deleted),
        lookups: Arc::clone(&primary_lookups),
        requested: Arc::clone(&primary_requests),
        changes: Arc::new(AtomicUsize::new(0)),
    };

    let mut shared_library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::default(),
        Box::new(shared_session),
        Arc::new(shared_zone_id),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    let mut primary_library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::default(),
        Box::new(primary_session),
        Arc::new(primary_zone_id),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    let capture =
        || crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
    shared_library.shadow_capture = Some(capture());
    primary_library.shadow_capture = Some(capture());

    let selection = Selection {
        albums: AlbumSelector::None,
        albums_explicit: false,
        smart_folders: SmartFolderSelector::Named {
            included: BTreeSet::from(["Hidden".to_string()]),
            excluded: BTreeSet::new(),
        },
        smart_folders_explicit: true,
        libraries: LibrarySelector {
            primary: false,
            shared_all: true,
            named: BTreeSet::new(),
            excluded: BTreeSet::new(),
        },
        unfiled: true,
    };
    let selected_libraries = vec![shared_library.clone()];
    let all_libraries = vec![primary_library.clone(), shared_library.clone()];
    let collection = collection_libraries(&selection, &selected_libraries, &all_libraries);
    let collection_context = build_collection_context(&selection, collection)
        .await
        .unwrap();
    let selected_zones = zone_name_set(&selected_libraries);
    let collection_zones = zone_name_set(collection);
    let shared_scope = pass_scope_for_zone(&selection, ZONE, &selected_zones, &collection_zones);
    let primary_scope =
        pass_scope_for_zone(&selection, PRIMARY, &selected_zones, &collection_zones);
    let shared_plan = resolve_passes_for_scope(
        &shared_library,
        &selection,
        shared_scope,
        &collection_context,
        &[],
    )
    .await
    .unwrap();
    let primary_plan = resolve_passes_for_scope(
        &primary_library,
        &selection,
        primary_scope,
        &collection_context,
        &[],
    )
    .await
    .unwrap();
    let kinds = |plan: &crate::commands::AlbumPlan| {
        plan.passes.iter().map(|pass| pass.kind).collect::<Vec<_>>()
    };
    assert_eq!(
        kinds(&shared_plan),
        vec![PassKind::SmartFolder, PassKind::Unfiled],
        "M1 shared source must resolve Hidden+Unfiled from the actual resolver"
    );
    assert_eq!(
        kinds(&primary_plan),
        vec![PassKind::SmartFolder],
        "M1 primary source must resolve Hidden only from the actual resolver"
    );

    let mut shared_state =
        make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), shared_plan.passes);
    shared_state.library = shared_library;
    let mut primary_state = make_run_cycle_library_state_with_passes(
        PRIMARY,
        &sync_token_key(PRIMARY),
        primary_plan.passes,
    );
    primary_state.library = primary_library;

    let builder = make_run_cycle_download_config_builder(&root, db.clone());
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let result = run_cycle(
        &[&shared_state, &primary_state],
        &make_run_cycle_config(),
        Some(db.as_ref()),
        false,
        &builder,
        download::DownloadControls::download_hidden(),
        &shared,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    {
        let conn = db.acquire_lock("inspect mixed-source baseline").unwrap();
        let epoch: String = conn
            .query_row(
                "SELECT json_extract(value,'$.reason') FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        let token: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [sync_token_key(ZONE)],
                |row| row.get(0),
            )
            .ok();
        let rounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        let debt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        let descriptor: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("observed_selection:v27:{ZONE}")],
                |row| row.get(0),
            )
            .ok();
        let primary_descriptor: Option<String> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [format!("observed_selection:v27:{PRIMARY}")],
                |row| row.get(0),
            )
            .ok();
        eprintln!(
            "B-PREREQ epoch={epoch} rounds={rounds} debt={debt} lookups={} token_present={} downloaded={} failed={} descriptor={descriptor:?} primary_descriptor={primary_descriptor:?}",
            shared_lookups.load(Ordering::SeqCst),
            token.is_some(),
            result.stats.downloaded,
            result.failed_count
        );
        assert_eq!(epoch, "existing_upgrade");
        assert!(
            token.is_some(),
            "the mixed baseline must complete its ordinary checkpoint: {result:?}"
        );

        let canonical = |descriptor: &Value, passes: &[AlbumPass]| {
            let plans = descriptor["plans"].as_array().unwrap();
            assert_eq!(
                plans.len(),
                1,
                "a source ACTIVE descriptor must own one full plan"
            );
            let plan = &plans[0];
            let descriptor_scope = plan["scope"].as_str().unwrap().to_owned();
            let mut descriptor_entries: Vec<Value> = plan["profile"]["passes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|pass| {
                    json!({
                        "kind":pass["kind"],
                        "scope":pass["scope"],
                        "excludes":pass["excludes"]
                    })
                })
                .collect();
            descriptor_entries.sort_by_key(|entry| serde_json::to_string(entry).unwrap());
            let source_scope = passes
                .iter()
                .find_map(|pass| pass.album.download_scope().ok().flatten())
                .map(|(_, scope, _)| scope)
                .unwrap();
            let mut pass_entries: Vec<Value> = passes
                .iter()
                .map(|pass| {
                    let mut excludes: Vec<String> = pass.exclude_ids.iter().cloned().collect();
                    excludes.sort_unstable();
                    json!({
                        "kind":format!("{:?}", pass.kind),
                        "scope":pass.album.selection_scope(),
                        "excludes":excludes
                    })
                })
                .collect();
            pass_entries.sort_by_key(|entry| serde_json::to_string(entry).unwrap());
            assert_eq!(descriptor_scope, source_scope);
            assert_eq!(descriptor_entries, pass_entries);
            (
                plans.len(),
                pass_entries.len(),
                descriptor_scope,
                descriptor_entries,
            )
        };
        let shared_probe = canonical(
            &serde_json::from_str(descriptor.as_ref().unwrap()).unwrap(),
            &shared_state.plan.passes,
        );
        let primary_probe = canonical(
            &serde_json::from_str(primary_descriptor.as_ref().unwrap()).unwrap(),
            &primary_state.plan.passes,
        );
        eprintln!("B-CANON shared{shared_probe:?} primary{primary_probe:?}");

        assert_eq!(
            debt, 1,
            "mixed-source Shared+Hidden baseline must record one strict source census obligation for the downloaded still+motion family"
        );
    }

    crate::sync_loop::runner::drain_source_revalidation(
        &[&shared_state, &primary_state],
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    {
        let conn = db
            .acquire_lock("verify mixed-source deletion census")
            .unwrap();
        let rows: Vec<(String, i64)> = conn
            .prepare(
                "SELECT version_size,is_deleted FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size",
            )
            .unwrap()
            .query_map(rusqlite::params![ZONE, DELETED_MASTER], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let shared_requests = shared_requests.lock().unwrap();
        assert!(
            shared_requests.iter().any(|names| {
                names.iter().any(|name| name == DELETED_MASTER)
                    && names.iter().any(|name| name == DELETED_CHILD)
            }),
            "the exact authenticated shared source must be asked for the omitted family, requested={shared_requests:?}"
        );
        assert_eq!(
            rows,
            vec![
                ("live_original".to_string(), 1),
                ("original".to_string(), 1)
            ],
            "a strict deleted CPLMaster+CPLAsset pair must mark both downloaded renditions without touching local media"
        );
    }

    for (path, bytes, mtime) in &deleted_snapshots {
        assert_eq!(&std::fs::read(path).unwrap(), bytes);
        assert_eq!(&std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
    }
    assert_eq!(
        std::fs::read(root.join("shared-current.JPG")).unwrap(),
        MEDIA
    );
    assert_eq!(
        std::fs::read(root.join("primary-current.JPG")).unwrap(),
        MEDIA
    );
    server.verify().await;
}

/// B token-expiry counterexample on unchanged product.
///
/// Cycle 0 is an ordinary fresh baseline and creates the active token. Cycle 1
/// reopens with that token, the provider rejects it, and dispatch silently
/// falls back to an inventory. The cycle was Incremental, so no fresh
/// `fresh_baseline_cursor` and no mixed-plan catalog seed can run. The request
/// order and the missing catalog round are recorded without hand-installing a
/// token, completing an epoch, or forcing a boundary.
#[tokio::test]
async fn v027_expired_active_token_fallback_omits_catalog_origin() {
    use std::sync::Mutex;

    #[derive(Clone)]
    struct ExpirySession {
        zone_id: Value,
        cycle: usize,
        boundary: String,
        expire: bool,
        current: Arc<Vec<Value>>,
        order: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl PhotosSession for ExpirySession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            if url.contains("/zones/list") {
                self.order.lock().unwrap().push("zones/list".into());
                return Ok(
                    json!({"zones":[{"zoneID":self.zone_id,"syncToken":self.boundary,"deleted":false}]}),
                );
            }
            if request.pointer("/query/recordType").and_then(Value::as_str)
                == Some("CheckIndexingState")
            {
                return Ok(
                    json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                let batch: Vec<Value> = request["batch"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|query| {
                        let record_type = query
                            .pointer("/query/recordType")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let count = u64::from(
                            record_type.contains("WithoutHiddenOrDeleted") && self.cycle == 0,
                        );
                        json!({"records":[{"fields":{"itemCount":{"value":count}}}]})
                    })
                    .collect();
                return Ok(json!({"batch":batch}));
            }
            if url.contains("/records/query?") {
                let record_type = request
                    .pointer("/query/recordType")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.order
                    .lock()
                    .unwrap()
                    .push(format!("query:{record_type}"));
                if record_type.contains("WithoutHiddenOrDeleted") && self.cycle == 0 {
                    return Ok(
                        json!({"records":self.current.as_ref().clone(),"syncToken":"inventory"}),
                    );
                }
                return Ok(json!({"records":[],"syncToken":"inventory"}));
            }
            if url.contains("/changes/zone?") {
                let token = request["zones"][0]["syncToken"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                self.order.lock().unwrap().push(format!("changes:{token}"));
                if self.expire {
                    return Ok(
                        json!({"zones":[{"zoneID":self.zone_id,"serverErrorCode":"BAD_REQUEST"}]}),
                    );
                }
                return Ok(json!({"zones":[{"zoneID":self.zone_id,"records":[],
                    "syncToken":"current-after-baseline","moreComing":false}]}));
            }
            if url.contains("/records/lookup?") {
                self.order.lock().unwrap().push("lookup".into());
                let requested = request["records"].as_array().unwrap();
                let records: Vec<Value> = self
                    .current
                    .iter()
                    .filter(|record| {
                        requested
                            .iter()
                            .any(|query| query["recordName"] == record["recordName"])
                    })
                    .cloned()
                    .collect();
                return Ok(json!({"records":records}));
            }
            anyhow::bail!("unexpected token-expiry endpoint {url}")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    let server = crate::start_wiremock_or_skip!();
    Mock::given(method("GET"))
        .and(path("/expiry-current"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    let zone_id = json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"});
    let mut current = crate::sync_loop::test_support::full_album_page_with_download(
        ZONE,
        "expiry-current",
        "inventory",
        &format!("{}/expiry-current", server.uri()),
        MEDIA.len() as u64,
        &super::generation(93),
    )["records"]
        .as_array()
        .unwrap()
        .clone();
    current[0]["zoneID"] = zone_id.clone();
    current[1]["zoneID"] = zone_id.clone();
    current[1]["fields"]["masterRef"]["value"]["zoneID"] = zone_id.clone();
    let current = Arc::new(current);

    let mut token = None;
    for cycle in 0..2 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let order = Arc::new(Mutex::new(Vec::new()));
        let session = ExpirySession {
            zone_id: zone_id.clone(),
            cycle,
            boundary: "baseline-boundary".into(),
            expire: cycle == 1,
            current: Arc::clone(&current),
            order: Arc::clone(&order),
        };
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session),
            Arc::new(zone_id.clone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(crate::icloud::photos::inbox::ShadowCapture::new(
            db.clone(),
            baseline_owner(),
            "com",
        ));
        let mut albums = library.albums().await.unwrap();
        let hidden = albums.remove("Hidden").unwrap();
        let passes = vec![
            AlbumPass {
                kind: PassKind::SmartFolder,
                album: hidden,
                exclude_ids: Arc::default(),
            },
            AlbumPass {
                kind: PassKind::Unfiled,
                album: library.all(),
                exclude_ids: Arc::default(),
            },
        ];
        let mut selected =
            make_run_cycle_library_state_with_passes(ZONE, &sync_token_key(ZONE), passes);
        selected.library = library;
        let builder = make_run_cycle_download_config_builder(&root, db.clone());
        let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
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
        .await
        .unwrap();
        let active = db.get_metadata(&sync_token_key(ZONE)).await.unwrap();
        if cycle == 0 {
            assert!(
                active.is_some(),
                "baseline must create the active token: {result:?}"
            );
            assert_eq!(result.stats.downloaded, 1, "{result:?}");
            token = active;
            continue;
        }
        let order = order.lock().unwrap().clone();
        let changes = order
            .iter()
            .position(|entry| entry.starts_with("changes:"))
            .expect("next cycle must attempt the existing token");
        assert!(
            !order[..changes].iter().any(|entry| entry == "zones/list"),
            "cycle 1 must not refresh a boundary before the stale-token attempt"
        );
        assert!(
            order[changes].contains(token.as_deref().unwrap()),
            "the next cycle must present the baseline-created token: {order:?}"
        );
        assert!(
            order[changes + 1..]
                .iter()
                .any(|entry| entry.starts_with("query:")),
            "the rejected token must fall back to an inventory: {order:?}"
        );
        assert!(
            !order[changes..].iter().any(|entry| entry == "zones/list"),
            "the fallback inventory must not establish a fresh cycle-owned boundary: {order:?}"
        );
        let conn = db
            .acquire_lock("inspect expired-token catalog omission")
            .unwrap();
        let rounds: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_rounds WHERE library=?1",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        let debt: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM source_revalidation_debt d JOIN source_revalidation_rounds r ON r.id=d.round WHERE r.library=?1",
                [ZONE],
                |row| row.get(0),
            )
            .unwrap();
        eprintln!(
            "B-EXPIRY full_enumeration_reason={:?} sync_token_blocked={} rounds={rounds} debt={debt} order={order:?} token_present={}",
            result.stats.full_enumeration_reason,
            result.stats.sync_token_blocked,
            token.is_some()
        );
        assert_eq!(
            (rounds, debt),
            (0, 0),
            "unchanged product omits the mixed-source catalog origin on token expiry"
        );
    }
    server.verify().await;
}
