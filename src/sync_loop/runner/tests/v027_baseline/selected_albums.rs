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

/// These fixtures start with an ordinary inventory, without a catalog origin.
/// Complete the production bootstrap and its inventory/bridge before measuring
/// incremental membership changes or quiet reopens against that baseline.
async fn complete_album_catalog(
    selected: &crate::sync_cycle::LibraryState,
    db: &SqliteStateDb,
    builder: &crate::sync_cycle::BuildDownloadConfigFn<'_>,
    shared: &crate::auth::SharedSession,
    unconfirmed: usize,
) {
    for phase in 0..2 {
        let result = run_cycle(
            &[selected],
            &make_run_cycle_config(),
            Some(db),
            false,
            builder,
            download::DownloadControls::download_hidden(),
            shared,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result.stats.downloaded, 0,
            "catalog setup must reuse the completed media: {result:?}"
        );
        let uncertain = if phase == 0 { 0 } else { unconfirmed };
        assert_eq!(result.stats.skipped.uncertain, uncertain, "{result:?}");
        assert_eq!(result.failed_count, uncertain, "{result:?}");
        if phase == 0 {
            assert!(!result.can_advance_database_checkpoint(), "{result:?}");
            assert!(
                result
                    .holds
                    .iter()
                    .all(|hold| hold.reason == "catalog_bootstrap_pending"),
                "{result:?}"
            );
        } else {
            assert!(!result.has_selected_holds(), "{result:?}");
            assert!(
                result
                    .sources
                    .iter()
                    .all(|source| source.ordinary_committed),
                "{result:?}"
            );
            assert_eq!(
                result.can_advance_database_checkpoint(),
                unconfirmed == 0,
                "{result:?}"
            );
        }
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
        if cycle == 0 {
            complete_album_catalog(&selected, &db, &builder, &shared, 0).await;
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
        if iteration == 0 {
            complete_album_catalog(&selected, &db, &builder, &shared_session, 0).await;
        }
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
            // The initial observation and the subsequent catalog inventory
            // each retain their unresolved owner until exact confirmation.
            assert_eq!(outstanding, if iteration == 0 { 1 } else { 2 });
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
        } else {
            complete_album_catalog(&selected, &db, &builder, &shared, 1).await;
            baseline_ranks = ranks.load(Ordering::SeqCst);
            failed_lookups = lookups.load(Ordering::SeqCst);
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
    mixed_shared_hidden_baseline_census(json!({"zoneName":ZONE,"ownerRecordName":"_defaultOwner"}))
        .await;
}

#[tokio::test]
async fn v027_mixed_shared_hidden_baseline_censuses_distinct_full_zone() {
    mixed_shared_hidden_baseline_census(json!({
        "zoneName":ZONE,
        "ownerRecordName":"synthetic-shared-owner",
        "zoneType":"synthetic-shared-zone"
    }))
    .await;
}

#[tokio::test]
async fn v027_both_sources_hidden_and_unfiled_use_actual_resolver() {
    mixed_shared_hidden_baseline_variant(
        json!({"zoneName":ZONE,"ownerRecordName":"synthetic-shared-owner","zoneType":"synthetic-shared-zone"}),
        MixedSelection::BothWithUnfiled,
    ).await;
}

#[tokio::test]
async fn v027_both_sources_hidden_only_use_actual_resolver() {
    mixed_shared_hidden_baseline_variant(
        json!({"zoneName":ZONE,"ownerRecordName":"synthetic-shared-owner","zoneType":"synthetic-shared-zone"}),
        MixedSelection::BothHiddenOnly,
    ).await;
}

#[tokio::test]
async fn v027_bound_other_rendition_custody_or_natural_retry() {
    mixed_shared_hidden_baseline_variant(
        json!({"zoneName":ZONE,"ownerRecordName":"synthetic-shared-owner","zoneType":"synthetic-shared-zone"}),
        MixedSelection::BoundPublication,
    ).await;
}

#[tokio::test]
async fn v027_bridge_only_item_is_accounted_when_repeat_inventory_omits_it() {
    mixed_shared_hidden_baseline_variant(
        json!({"zoneName":ZONE,"ownerRecordName":"synthetic-shared-owner","zoneType":"synthetic-shared-zone"}),
        MixedSelection::BridgeOnly,
    ).await;
}

#[tokio::test]
async fn v027_second_rank_change_retains_original_source_and_peer_progress() {
    mixed_shared_hidden_baseline_variant(
        json!({"zoneName":ZONE,"ownerRecordName":"synthetic-shared-owner","zoneType":"synthetic-shared-zone"}),
        MixedSelection::BridgeTwice,
    ).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MixedSelection {
    SharedWithUnfiled,
    BothWithUnfiled,
    BothHiddenOnly,
    BoundPublication,
    BridgeOnly,
    BridgeTwice,
}

impl MixedSelection {
    fn bridge_test(self) -> bool {
        matches!(self, Self::BridgeOnly | Self::BridgeTwice)
    }
}

async fn mixed_shared_hidden_baseline_census(shared_zone_id: Value) {
    mixed_shared_hidden_baseline_variant(shared_zone_id, MixedSelection::SharedWithUnfiled).await;
}

async fn mixed_shared_hidden_baseline_variant(shared_zone_id: Value, variant: MixedSelection) {
    mixed_shared_hidden_baseline_input(shared_zone_id, variant, false).await;
}

#[tokio::test]
async fn released36_mixed_selection_preserves_pre_migration_live_families() {
    for variant in [
        MixedSelection::SharedWithUnfiled,
        MixedSelection::BothWithUnfiled,
        MixedSelection::BothHiddenOnly,
    ] {
        mixed_shared_hidden_baseline_input(
            json!({"zoneName":ZONE,"ownerRecordName":"synthetic-shared-owner","zoneType":"synthetic-shared-zone"}),
            variant,
            true,
        ).await;
    }
}

async fn mixed_shared_hidden_baseline_input(
    shared_zone_id: Value,
    variant: MixedSelection,
    released_input: bool,
) {
    use crate::commands::{
        build_collection_context, collection_libraries, pass_scope_for_zone,
        resolve_passes_for_scope, zone_name_set,
    };
    use crate::icloud::photos::inbox::ShadowCapture;
    use crate::selection::{AlbumSelector, LibrarySelector, Selection, SmartFolderSelector};
    use std::collections::BTreeSet;
    use std::sync::Mutex;

    const PRIMARY: &str = "PrimarySync";
    const DELETED_MASTER: &str = "shared-live-master";
    const DELETED_CHILD: &str = "asset-shared-live-master";

    fn mixed_selection(variant: MixedSelection) -> Selection {
        Selection {
            albums: AlbumSelector::None,
            albums_explicit: false,
            smart_folders: if variant.bridge_test() {
                SmartFolderSelector::None
            } else {
                SmartFolderSelector::Named {
                    included: BTreeSet::from(["Hidden".to_string()]),
                    excluded: BTreeSet::new(),
                }
            },
            smart_folders_explicit: true,
            libraries: LibrarySelector {
                primary: variant != MixedSelection::SharedWithUnfiled,
                shared_all: true,
                named: BTreeSet::new(),
                excluded: BTreeSet::new(),
            },
            unfiled: variant != MixedSelection::BothHiddenOnly,
        }
    }

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

    async fn current_bound_motion(
        db: Arc<SqliteStateDb>,
        zone: &Value,
        url: &str,
    ) -> crate::icloud::photos::PhotoAsset {
        use crate::icloud::photos::{ProviderRecordId, RecordLookupRequest, RecordResolution};
        #[derive(Clone)]
        struct CurrentPair {
            zone: Value,
            records: Vec<Value>,
        }
        #[async_trait::async_trait]
        impl PhotosSession for CurrentPair {
            async fn post(
                &self,
                url: &str,
                body: String,
                _: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                assert!(url.contains("/shared/records/lookup?"));
                let request: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(request["zoneID"], self.zone);
                let names = request["records"].as_array().unwrap();
                assert_eq!(names.len(), 2);
                for expected in [DELETED_MASTER, DELETED_CHILD] {
                    assert!(names.iter().any(|record| record["recordName"] == expected));
                }
                Ok(json!({"records":self.records}))
            }
            fn clone_box(&self) -> Box<dyn PhotosSession> {
                Box::new(self.clone())
            }
        }
        let mut records = mixed_pair(
            zone,
            DELETED_MASTER,
            "prepared-alternate.JPG",
            url,
            90,
            false,
        );
        records[0]["fields"]["resOriginalVidComplRes"] =
            records[0]["fields"]["resOriginalRes"].clone();
        records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] =
            json!(format!("{url}-still"));
        records[0]["fields"]["resOriginalVidComplFileType"] =
            json!({"value":"com.apple.quicktime-movie"});
        let mut album = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: "https://synthetic.example/database/1/photos/production/shared"
                    .into(),
                name: Arc::from(""),
                list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter: None,
                page_size: 200,
                zone_id: Arc::new(zone.clone()),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(CurrentPair {
                zone: zone.clone(),
                records,
            }),
        );
        album.set_shadow_capture(
            ShadowCapture::new(db, baseline_owner(), "com"),
            "shared".into(),
        );
        let mut batch = album
            .resolve_records(&[RecordLookupRequest::paired(
                ProviderRecordId::new(DELETED_MASTER),
                ProviderRecordId::new(DELETED_MASTER),
                ProviderRecordId::new(DELETED_CHILD),
            )])
            .await;
        assert!(batch.complete);
        assert_eq!(batch.results.len(), 1);
        let (_, RecordResolution::Present(asset)) = batch.results.pop().unwrap() else {
            panic!("bound recovery requires the current exact source pair");
        };
        asset
    }

    #[derive(Clone)]
    struct MixedSession {
        zone_id: Value,
        zone: String,
        boundary: String,
        current: Arc<Vec<Value>>,
        bridge_only: Arc<Vec<Value>>,
        repeat_bridge: bool,
        current_on_hidden: bool,
        deleted: Arc<Vec<Value>>,
        lookups: Arc<AtomicUsize>,
        requested: Arc<Mutex<Vec<Vec<String>>>>,
        changes: Arc<AtomicUsize>,
        inventory: Arc<Mutex<Vec<String>>>,
        expected_database: Option<&'static str>,
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
            if let Some(database) = self.expected_database {
                assert!(
                    url.contains(&format!("/photos/production/{database}")),
                    "request must use its source database: {url}"
                );
            }
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
                let start = request["query"]["filterBy"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|filter| filter["fieldName"] == "startRank")
                    .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                    .unwrap_or(0);
                self.inventory.lock().unwrap().push(format!(
                    "{record_type}|{}|{start}",
                    self.changes.load(Ordering::SeqCst)
                ));
                let records = if self.current_pass(&request) {
                    self.current.as_ref().clone()
                } else {
                    Vec::new()
                };
                return Ok(json!({"records":records,"syncToken":"inventory-last-page"}));
            }
            if url.contains("/changes/zone?") {
                assert_eq!(request["zones"][0]["zoneID"], self.zone_id);
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
                    "records":if bridge == 0 || (bridge == 1 && self.repeat_bridge) { self.bridge_only.as_ref().clone() } else { Vec::new() },
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

    if variant == MixedSelection::BoundPublication
        && let Some(path) = std::env::var_os("KEI_R2_RESUME_ROOT")
    {
        use crate::state::db::{PreparedPublication, TempFileOwnershipStore};
        use sha2::{Digest, Sha256};
        let dir = std::path::PathBuf::from(path);
        let saved: Value =
            serde_json::from_slice(&std::fs::read(dir.join("r2-state.json")).unwrap()).unwrap();
        let proof: PreparedPublication = serde_json::from_value(saved["proof"].clone()).unwrap();
        let round = saved["round"].as_str().unwrap();
        let deadline = saved["deadline"].as_i64().unwrap();
        assert_eq!(saved["zone"], shared_zone_id);
        let before = saved["family_before"].as_array().unwrap();
        let finalized = saved["finalized_family"].as_array().unwrap();
        assert_eq!((before.len(), finalized.len()), (2, 2));
        for (before, after) in before.iter().zip(finalized) {
            let before: Value = serde_json::from_str(before.as_str().unwrap()).unwrap();
            let after: Value = serde_json::from_str(after.as_str().unwrap()).unwrap();
            for column in [0, 2, 3, 5, 6, 7] {
                assert_eq!(
                    before[column], after[column],
                    "publication must preserve the family's identity and metadata"
                );
            }
            if before[0] == "live_original" {
                assert_eq!(before[1], "failed");
                assert_eq!(after[1], "downloaded");
                assert_ne!(before[4], after[4]);
                assert_eq!(after[4], proof.destination.to_path().to_str().unwrap());
            } else {
                assert_eq!(before[1], after[1]);
                assert_eq!(before[4], after[4]);
            }
        }
        let db = Arc::new(
            SqliteStateDb::open_owned(&dir.join("state.db"), &baseline_owner())
                .await
                .unwrap(),
        );
        let (actual_deadline, family): (i64, Vec<String>) = {
            let conn = db
                .acquire_lock("verify saved family and original retry deadline")
                .unwrap();
            let due = conn
                .query_row(
                    "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                    rusqlite::params![round, DELETED_MASTER],
                    |row| row.get(0),
                )
                .unwrap();
            let mut stmt = conn.prepare("SELECT json_array(version_size,status,checksum,size_bytes,local_path,is_deleted,deleted_at,metadata_hash) FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size").unwrap();
            let family = stmt
                .query_map(rusqlite::params![ZONE, DELETED_MASTER], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (due, family)
        };
        assert_eq!(actual_deadline, deadline);
        assert_eq!(
            serde_json::to_value(&family).unwrap(),
            saved["finalized_family"]
        );
        assert!(
            db.get_prepared_publications(
                ZONE,
                DELETED_MASTER,
                crate::state::VersionSizeKey::LiveOriginal
            )
            .await
            .unwrap()
            .is_empty()
        );
        assert!(!proof.temporary.to_path().exists());
        assert_eq!(
            data_encoding::HEXLOWER.encode(&Sha256::digest(
                std::fs::read(proof.destination.to_path()).unwrap()
            )),
            proof.local_checksum
        );
        let published_mtime = std::fs::metadata(proof.destination.to_path())
            .unwrap()
            .modified()
            .unwrap();
        let check_old_files = || {
            for file in saved["old_files"].as_array().unwrap() {
                let path = std::path::Path::new(file["path"].as_str().unwrap());
                assert_eq!(
                    data_encoding::HEXLOWER.encode(&Sha256::digest(std::fs::read(path).unwrap())),
                    file["sha256"].as_str().unwrap()
                );
                assert_eq!(
                    std::fs::metadata(path)
                        .unwrap()
                        .modified()
                        .unwrap()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    u128::from(file["mtime_ns"].as_u64().unwrap())
                );
            }
        };
        check_old_files();
        let metadata_snapshot = || {
            let conn = db.acquire_lock("snapshot family metadata paths").unwrap();
            let mut stmt = conn.prepare("SELECT json_array(version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size) FROM asset_metadata_paths WHERE library=?1 AND id=?2 ORDER BY version_size,local_path").unwrap();
            stmt.query_map(rusqlite::params![ZONE, DELETED_MASTER], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        let metadata_before = metadata_snapshot();
        assert_eq!(
            metadata_before.len(),
            3,
            "both old metadata paths and the new path must remain recorded"
        );
        let primary_zone = json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"});
        let shared_requests = Arc::new(Mutex::new(Vec::new()));
        let source = |zone_id: Value, name: &str, database: &'static str, deleted: Vec<Value>| {
            MixedSession {
                zone_id,
                zone: name.to_owned(),
                boundary: format!("before-{name}"),
                current: Arc::new(Vec::new()),
                bridge_only: Arc::new(Vec::new()),
                repeat_bridge: false,
                current_on_hidden: false,
                deleted: Arc::new(deleted),
                lookups: Arc::new(AtomicUsize::new(0)),
                requested: if name == ZONE {
                    shared_requests.clone()
                } else {
                    Arc::new(Mutex::new(Vec::new()))
                },
                changes: Arc::new(AtomicUsize::new(1)),
                inventory: Arc::new(Mutex::new(Vec::new())),
                expected_database: Some(database),
            }
        };
        let mut shared_library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::default(),
            Box::new(source(
                shared_zone_id.clone(),
                ZONE,
                "shared",
                deleted_lookup_records(&shared_zone_id),
            )),
            Arc::new(shared_zone_id.clone()),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        let mut primary_library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(source(primary_zone.clone(), PRIMARY, "private", Vec::new())),
            Arc::new(primary_zone),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        shared_library.shadow_capture =
            Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
        primary_library.shadow_capture =
            Some(ShadowCapture::new(db.clone(), baseline_owner(), "com"));
        let selection = mixed_selection(variant);
        let selected = vec![primary_library.clone(), shared_library.clone()];
        let all = vec![primary_library.clone(), shared_library.clone()];
        let collection = collection_libraries(&selection, &selected, &all);
        let context = build_collection_context(&selection, collection)
            .await
            .unwrap();
        let selected_zones = zone_name_set(&selected);
        let collection_zones = zone_name_set(collection);
        let mut states = Vec::new();
        for library in [shared_library, primary_library] {
            let name = library.zone_name().to_owned();
            let scope = pass_scope_for_zone(&selection, &name, &selected_zones, &collection_zones);
            let plan = resolve_passes_for_scope(&library, &selection, scope, &context, &[])
                .await
                .unwrap();
            let mut source = make_run_cycle_library_state_with_passes(
                &name,
                &sync_token_key(&name),
                plan.passes,
            );
            source.library = library;
            states.push(source);
        }
        let media = dir.join("media");
        let base = make_run_cycle_download_config_builder(&media, db.clone());
        let builder = |mode, exclude, groups, library| {
            let mut config = (*base(mode, exclude, groups, library)).clone();
            config.folder_structure = "none".into();
            config.folder_structure_albums = Arc::from("none");
            config.folder_structure_smart_folders = Arc::from("none");
            Arc::new(config)
        };
        let token = db.get_metadata(&sync_token_key(ZONE)).await.unwrap();
        let outcome = crate::sync_loop::runner::drain_catalog_source_revalidation(
            &[&states[0], &states[1]],
            &builder,
            download::DownloadControls::download_hidden(),
            &download::compute_config_hash(&make_run_cycle_config()),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        if chrono::Utc::now().timestamp() < deadline {
            assert_eq!(outcome.retained, 1);
            assert!(
                shared_requests.lock().unwrap().is_empty(),
                "future debt must not be looked up"
            );
            let unchanged: i64 = db
                .acquire_lock("verify future debt remains unchanged")
                .unwrap()
                .query_row(
                    "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                    rusqlite::params![round, DELETED_MASTER],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(unchanged, deadline);
            assert_eq!(db.get_metadata(&sync_token_key(ZONE)).await.unwrap(), token);
            assert_eq!(metadata_snapshot(), metadata_before);
            eprintln!("R2_NATURAL_RETRY_NOT_DUE deadline={deadline}");
            return;
        }
        assert_eq!(outcome.retained, 0);
        assert!(
            shared_requests
                .lock()
                .unwrap()
                .iter()
                .any(|names| names.iter().any(|x| x == DELETED_MASTER)
                    && names.iter().any(|x| x == DELETED_CHILD)),
            "strict retry must ask for the exact family pair"
        );
        {
            let conn = db
                .acquire_lock("verify eligible strict family settlement")
                .unwrap();
            let deleted: Vec<(String,i64)> = conn.prepare("SELECT version_size,is_deleted FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size").unwrap()
            .query_map(rusqlite::params![ZONE,DELETED_MASTER],|row|Ok((row.get(0)?,row.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            assert_eq!(
                deleted,
                vec![("live_original".to_owned(), 1), ("original".to_owned(), 1)]
            );
            let remaining: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                    rusqlite::params![round, DELETED_MASTER],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(remaining, 0);
            let receipt: i64 = conn.query_row("SELECT COUNT(*) FROM download_receipts WHERE library=?1 AND state_id=?2 AND version_size='live_original' AND local_checksum=?3",rusqlite::params![ZONE,DELETED_MASTER,proof.local_checksum],|row|row.get(0)).unwrap();
            assert_eq!(receipt, 1);
            let other_source: i64 = conn.query_row("SELECT COUNT(*) FROM download_receipts WHERE library=?1 AND child='asset-primary-current' AND master='primary-current'",[PRIMARY],|row|row.get(0)).unwrap();
            assert_eq!(other_source, 1);
        }
        assert_eq!(db.get_metadata(&sync_token_key(ZONE)).await.unwrap(), token);
        assert_eq!(
            data_encoding::HEXLOWER.encode(&Sha256::digest(
                std::fs::read(proof.destination.to_path()).unwrap()
            )),
            proof.local_checksum
        );
        assert_eq!(
            std::fs::metadata(proof.destination.to_path())
                .unwrap()
                .modified()
                .unwrap(),
            published_mtime
        );
        assert_eq!(metadata_snapshot(), metadata_before);
        check_old_files();
        eprintln!("R2_NATURAL_RETRY_SETTLED deadline={deadline}");
        return;
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
    if variant.bridge_test() {
        Mock::given(method("GET"))
            .and(path("/bridge-only"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(1)
            .mount(&server)
            .await;
    }

    let mut dir = if variant == MixedSelection::BoundPublication {
        if let Some(parent) = std::env::var_os("KEI_R2_RETAIN_ROOT") {
            tempfile::Builder::new()
                .prefix("r2-family-")
                .tempdir_in(parent)
                .unwrap()
        } else {
            tempfile::tempdir().unwrap()
        }
    } else {
        tempfile::tempdir().unwrap()
    };
    if variant == MixedSelection::BoundPublication
        && std::env::var_os("KEI_R2_RETAIN_ROOT").is_some()
    {
        dir.disable_cleanup(true);
        eprintln!("R2_FIXTURE_ROOT={}", dir.path().display());
    }
    let root = dir.path().join("media");
    std::fs::create_dir(&root).unwrap();
    let db_path = dir.path().join("state.db");
    super::seed_existing_upgrade_db(&db_path);
    if released_input {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        assert_eq!(crate::state::schema::get_schema_version(&conn).unwrap(), 36);
        for (version, name, bytes) in [
            ("original", "live.jpg", b"synthetic still".as_slice()),
            ("live_original", "live.mov", b"synthetic motion".as_slice()),
        ] {
            let path = root.join(name);
            std::fs::write(&path, bytes).unwrap();
            conn.execute(
                "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,added_at,size_bytes,media_type,status,downloaded_at,local_path,last_seen_at,local_checksum,download_checksum) VALUES (?1,?2,?3,?4,?5,1700000000,1700000000,?6,'photo','downloaded',1700000001,?7,1700000001,'synthetic-local','synthetic-local')",
                rusqlite::params![ZONE, DELETED_MASTER, version, super::generation(90), name, MEDIA.len() as i64, path.to_str().unwrap()],
            ).unwrap();
            conn.execute(
                "INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum) VALUES (?1,?2,?3,?4,?5,'synthetic-local','synthetic-local')",
                rusqlite::params![ZONE, DELETED_MASTER, version, path.to_str().unwrap(), super::generation(90)],
            ).unwrap();
        }
        conn.execute("INSERT INTO asset_master_mappings(library,asset_record_name,master_record_name,updated_at) VALUES (?1,?2,?3,1700000001)", rusqlite::params![ZONE, DELETED_CHILD, DELETED_MASTER]).unwrap();
        conn.execute("INSERT INTO legacy_master_state_owners(library,master_record_name,asset_record_name,claimed_at) VALUES (?1,?2,?3,1700000001)", rusqlite::params![ZONE, DELETED_MASTER, DELETED_CHILD]).unwrap();
        conn.execute(
            "VACUUM INTO ?1",
            [dir.path().join("released36-input.db").to_str().unwrap()],
        )
        .unwrap();
    }
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
    if !released_input {
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
    }
    let deleted_snapshots = deleted_paths.map(|path| {
        (
            path.clone(),
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().modified().unwrap(),
        )
    });

    let primary_zone_id = json!({"zoneName":PRIMARY,"ownerRecordName":"_defaultOwner"});
    let shared_current = mixed_pair(
        &shared_zone_id,
        "shared-current",
        "shared-current.JPG",
        &format!("{}/shared-current", server.uri()),
        91,
        variant == MixedSelection::BothHiddenOnly,
    );
    let primary_current = mixed_pair(
        &primary_zone_id,
        "primary-current",
        "primary-current.JPG",
        &format!("{}/primary-current", server.uri()),
        92,
        !variant.bridge_test(),
    );
    let bridge_only = if variant.bridge_test() {
        mixed_pair(
            &shared_zone_id,
            "bridge-only",
            "bridge-only.JPG",
            &format!("{}/bridge-only", server.uri()),
            93,
            false,
        )
    } else {
        Vec::new()
    };
    let mut shared_lookup_records = deleted_lookup_records(&shared_zone_id);
    shared_lookup_records.extend(shared_current.iter().cloned());
    shared_lookup_records.extend(bridge_only.iter().cloned());
    let shared_deleted = Arc::new(shared_lookup_records);
    let primary_deleted = Arc::new(primary_current.clone());
    let shared_lookups = Arc::new(AtomicUsize::new(0));
    let primary_lookups = Arc::new(AtomicUsize::new(0));
    let shared_requests = Arc::new(Mutex::new(Vec::new()));
    let primary_requests = Arc::new(Mutex::new(Vec::new()));
    let shared_inventory = Arc::new(Mutex::new(Vec::new()));
    let primary_inventory = Arc::new(Mutex::new(Vec::new()));

    let shared_session = MixedSession {
        zone_id: shared_zone_id.clone(),
        zone: ZONE.into(),
        boundary: format!("before-{ZONE}"),
        current: Arc::new(shared_current),
        bridge_only: Arc::new(bridge_only),
        repeat_bridge: variant == MixedSelection::BridgeTwice,
        current_on_hidden: variant == MixedSelection::BothHiddenOnly,
        deleted: Arc::clone(&shared_deleted),
        lookups: Arc::clone(&shared_lookups),
        requested: Arc::clone(&shared_requests),
        changes: Arc::new(AtomicUsize::new(0)),
        inventory: Arc::clone(&shared_inventory),
        expected_database: (variant != MixedSelection::SharedWithUnfiled).then_some("shared"),
    };
    let primary_session = MixedSession {
        zone_id: primary_zone_id.clone(),
        zone: PRIMARY.into(),
        boundary: format!("before-{PRIMARY}"),
        current: Arc::new(primary_current),
        bridge_only: Arc::new(Vec::new()),
        repeat_bridge: false,
        current_on_hidden: !variant.bridge_test(),
        deleted: Arc::clone(&primary_deleted),
        lookups: Arc::clone(&primary_lookups),
        requested: Arc::clone(&primary_requests),
        changes: Arc::new(AtomicUsize::new(0)),
        inventory: Arc::clone(&primary_inventory),
        expected_database: Some("private"),
    };

    let mut shared_library = PhotoLibrary::new(
        if variant == MixedSelection::SharedWithUnfiled {
            "https://synthetic.example/database/1/photos/production/private"
        } else {
            "https://synthetic.example/database/1/photos/production/shared"
        }
        .into(),
        Arc::default(),
        Box::new(shared_session.clone()),
        Arc::new(shared_zone_id.clone()),
        "shared".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    let mut primary_library = PhotoLibrary::new(
        "https://synthetic.example/database/1/photos/production/private".into(),
        Arc::default(),
        Box::new(primary_session.clone()),
        Arc::new(primary_zone_id.clone()),
        "private".into(),
        crate::retry::RetryConfig::default(),
    )
    .await
    .unwrap();
    let capture =
        || crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
    shared_library.shadow_capture = Some(capture());
    primary_library.shadow_capture = Some(capture());

    let selection = mixed_selection(variant);
    let selected_libraries = if variant == MixedSelection::SharedWithUnfiled {
        vec![shared_library.clone()]
    } else {
        vec![primary_library.clone(), shared_library.clone()]
    };
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
        if variant.bridge_test() {
            vec![PassKind::Unfiled]
        } else if variant == MixedSelection::BothHiddenOnly {
            vec![PassKind::SmartFolder]
        } else {
            vec![PassKind::SmartFolder, PassKind::Unfiled]
        },
        "shared source must resolve the selected passes from the actual resolver"
    );
    assert_eq!(
        kinds(&primary_plan),
        if variant.bridge_test() {
            vec![PassKind::Unfiled]
        } else if matches!(
            variant,
            MixedSelection::BothWithUnfiled | MixedSelection::BoundPublication
        ) {
            vec![PassKind::SmartFolder, PassKind::Unfiled]
        } else {
            vec![PassKind::SmartFolder]
        },
        "primary source must resolve the selected passes from the actual resolver"
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

    let base = make_run_cycle_download_config_builder(&root, db.clone());
    let builder = |mode, exclude, groups, library| {
        let mut config = (*base(mode, exclude, groups, library)).clone();
        config.folder_structure = "none".into();
        config.folder_structure_albums = Arc::from("none");
        config.folder_structure_smart_folders = Arc::from("none");
        Arc::new(config)
    };
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

    if variant == MixedSelection::BridgeTwice {
        assert!(
            !result.db_sync_token_advance_safe,
            "a second rank change must retain the source boundary: {result:?}"
        );
        assert_eq!(shared_session.changes.load(Ordering::SeqCst), 2);
        let inventory = shared_inventory.lock().unwrap().clone();
        eprintln!("SECOND_RANK_INVENTORY={inventory:?}");
        let starts: Vec<_> = inventory
            .iter()
            .filter(|query| query.ends_with("|0"))
            .collect();
        assert_eq!(
            starts.len(),
            2,
            "only one additional rank inventory is allowed: {inventory:?}"
        );
        assert_eq!(db.get_metadata(&sync_token_key(ZONE)).await.unwrap(), None);
        assert_eq!(
            db.get_metadata(&sync_token_key(PRIMARY))
                .await
                .unwrap()
                .as_deref(),
            Some(format!("after-{PRIMARY}-1").as_str())
        );
        assert_eq!(
            std::fs::read(root.join("primary-current.JPG")).unwrap(),
            MEDIA
        );
        assert_eq!(std::fs::read(root.join("bridge-only.JPG")).unwrap(), MEDIA);
        let before = {
            let conn = db
                .acquire_lock("verify repeated rank-change custody")
                .unwrap();
            let origin: (String, String) = conn.query_row(
                "SELECT r.phase,m.value FROM source_revalidation_rounds r JOIN metadata m ON m.key LIKE 'baseline_cursor:v27:%' WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='observing' AND m.value=?2",
                rusqlite::params![ZONE, format!("before-{ZONE}")], |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            let peer: i64 = conn.query_row("SELECT COUNT(*) FROM download_receipts WHERE library=?1 AND child='asset-primary-current'",[PRIMARY],|row|row.get(0)).unwrap();
            assert_eq!(peer, 1);
            let bridge: i64 = conn.query_row("SELECT COUNT(*) FROM download_receipts WHERE library=?1 AND child='asset-bridge-only' AND master='bridge-only' AND version_size='original'",[ZONE],|row|row.get(0)).unwrap();
            assert_eq!(bridge, 1, "first bridge work must remain durably recorded");
            origin
        };
        let bridge_path = root.join("bridge-only.JPG");
        let bridge_mtime = std::fs::metadata(&bridge_path).unwrap().modified().unwrap();
        drop(result);
        drop(shared_state);
        drop(primary_state);
        drop(collection_context);
        drop(selected_libraries);
        drop(all_libraries);
        drop(base);
        assert_eq!(Arc::strong_count(&db), 1);
        drop(db);
        let reopened = SqliteStateDb::open_owned(&db_path, &baseline_owner())
            .await
            .unwrap();
        assert_eq!(
            reopened.get_metadata(&sync_token_key(ZONE)).await.unwrap(),
            None
        );
        {
            let conn = reopened
                .acquire_lock("reopen repeated rank-change custody")
                .unwrap();
            let after: (String,String) = conn.query_row(
            "SELECT r.phase,m.value FROM source_revalidation_rounds r JOIN metadata m ON m.key LIKE 'baseline_cursor:v27:%' WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='observing' AND m.value=?2",
            rusqlite::params![ZONE,format!("before-{ZONE}")], |row|Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
            assert_eq!(after, before);
            let bridge: i64 = conn.query_row("SELECT COUNT(*) FROM download_receipts WHERE library=?1 AND child='asset-bridge-only' AND master='bridge-only' AND version_size='original'",[ZONE],|row|row.get(0)).unwrap();
            assert_eq!(bridge, 1);
        }
        assert_eq!(std::fs::read(&bridge_path).unwrap(), MEDIA);
        assert_eq!(
            std::fs::metadata(&bridge_path).unwrap().modified().unwrap(),
            bridge_mtime
        );
        server.verify().await;
        return;
    }

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
        let captured_zone = &serde_json::from_str::<Value>(&shared_probe.2).unwrap()["zone"];
        assert_eq!(captured_zone["zoneName"], shared_zone_id["zoneName"]);
        assert_eq!(
            captured_zone["ownerRecordName"], shared_zone_id["ownerRecordName"],
            "the captured source must retain the distinct Shared owner"
        );
        let enrolled_scope: String = conn
            .query_row(
                "SELECT json_extract(value,'$.baseline_scope') FROM metadata WHERE key=?1",
                [crate::state::db::sync_epoch::library_key(ZONE)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            enrolled_scope,
            PhotoLibrary::baseline_scope_hash("shared", &shared_zone_id),
            "the epoch must pin the entire Shared zone, not just its capture projection"
        );
        let primary_probe = canonical(
            &serde_json::from_str(primary_descriptor.as_ref().unwrap()).unwrap(),
            &primary_state.plan.passes,
        );
        for (library, child, master, database, owner, destination) in [
            (
                ZONE,
                "asset-shared-current",
                "shared-current",
                "shared",
                shared_zone_id["ownerRecordName"].as_str().unwrap(),
                root.join("shared-current.JPG"),
            ),
            (
                PRIMARY,
                "asset-primary-current",
                "primary-current",
                "private",
                "_defaultOwner",
                root.join("primary-current.JPG"),
            ),
        ] {
            let cursor: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [sync_token_key(library)],
                    |row| row.get(0),
                )
                .unwrap();
            let initial_bridge = if variant == MixedSelection::BridgeOnly && library == ZONE {
                2
            } else {
                1
            };
            assert_eq!(cursor, format!("after-{library}-{initial_bridge}"));
            let (scope, native, source, local): (String, String, String, String) = conn.query_row(
                "SELECT scope,native_path,source_checksum,local_checksum FROM download_receipts WHERE library=?1 AND child=?2 AND master=?3 AND version_size='original'",
                rusqlite::params![library, child, master], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
            ).unwrap();
            assert_eq!(
                PhotoLibrary::provider_scope_projection(&scope),
                Some((library.to_owned(), owner.to_owned(), database.to_owned()))
            );
            assert_eq!(
                serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                    &native
                )
                .unwrap()
                .to_path(),
                destination
            );
            use sha2::{Digest, Sha256};
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
            assert_eq!(source, hash);
            assert_eq!(local, hash);
        }
        let shared_pass = shared_inventory.lock().unwrap();
        let selected_shared = if variant == MixedSelection::BothHiddenOnly {
            "Hidden"
        } else {
            "WithoutHiddenOrDeleted"
        };
        assert!(
            shared_pass
                .iter()
                .any(|record_type| record_type.contains(selected_shared)),
            "the intended Shared pass must produce current media"
        );
        let selected_primary = if variant == MixedSelection::BridgeOnly {
            "WithoutHiddenOrDeleted"
        } else {
            "Hidden"
        };
        assert!(
            primary_inventory
                .lock()
                .unwrap()
                .iter()
                .any(|record_type| record_type.contains(selected_primary))
        );
        eprintln!("B-CANON shared{shared_probe:?} primary{primary_probe:?}");

        assert_eq!(
            debt, 1,
            "mixed-source Shared+Hidden baseline must record one strict source census obligation for the downloaded still+motion family"
        );
    }

    let protected = if variant == MixedSelection::BoundPublication {
        use crate::state::db::{
            PreparedPublication, PublicationBinding, TempFileOwnershipStore,
            provider_selection::SelectionPath,
        };
        use sha2::{Digest, Sha256};
        let (scope, pass, checksum, size, round, due): (String, String, String, i64, String, i64) = {
            let conn = db
                .acquire_lock("read actual selected source and alternate rendition")
                .unwrap();
            let (scope, pass) = conn.query_row(
                "SELECT scope,origin_pass FROM download_receipts WHERE library=?1 AND child='asset-shared-current' AND master='shared-current' AND version_size='original'", [ZONE],
                |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            let (checksum, size) = conn.query_row(
                "SELECT checksum,size_bytes FROM assets WHERE library=?1 AND id=?2 AND version_size='live_original' AND status='downloaded' AND is_deleted=0",
                rusqlite::params![ZONE, DELETED_MASTER], |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            let (round, due) = conn.query_row(
                "SELECT r.id,d.next_retry_at FROM source_revalidation_rounds r JOIN source_revalidation_debt d ON d.round=r.id WHERE r.library=?1 AND r.mode='catalog_v2' AND r.phase='active' AND d.id=?2",
                rusqlite::params![ZONE, DELETED_MASTER], |row| Ok((row.get(0)?,row.get(1)?)),
            ).unwrap();
            (scope, pass, checksum, size, round, due)
        };
        assert!(
            due <= chrono::Utc::now().timestamp(),
            "the real candidate must initially be due"
        );
        assert_eq!(
            PhotoLibrary::provider_scope_projection(&scope),
            Some((
                ZONE.to_owned(),
                shared_zone_id["ownerRecordName"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                "shared".to_owned()
            ))
        );
        let binding = db
            .bind_publication(
                baseline_owner(),
                PublicationBinding {
                    account_key: String::new(),
                    provider_key: String::new(),
                    scope,
                    library: ZONE.to_owned(),
                    child: DELETED_CHILD.to_owned(),
                    master: DELETED_MASTER.to_owned(),
                    state_id: DELETED_MASTER.to_owned(),
                    origin_pass: pass,
                },
            )
            .await
            .unwrap();
        let destination = root.join("prepared-alternate.MOV");
        let temporary =
            download::test_temp_download_path(&destination, &checksum, ".family-custody").unwrap();
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
        let proof = PreparedPublication {
            binding: Some(binding),
            library: ZONE.to_owned(),
            id: DELETED_MASTER.to_owned(),
            version: "live_original".to_owned(),
            provider_checksum: checksum,
            provider_size: size.try_into().unwrap(),
            temporary: SelectionPath::from_path(&temporary),
            destination: SelectionPath::from_path(&destination),
            local_checksum: hash.clone(),
            source_checksum: hash,
        };
        let collision = destination.clone();
        Mock::given(method("GET"))
            .and(path("/prepared-motion"))
            .respond_with(move |_: &wiremock::Request| {
                // The occupant arrives after task selection and before publish.
                std::fs::write(&collision, b"injected foreign occupant").unwrap();
                ResponseTemplate::new(200).set_body_bytes(MEDIA)
            })
            .expect(1)
            .mount(&server)
            .await;
        let current = current_bound_motion(
            db.clone(),
            &shared_zone_id,
            &format!("{}/prepared-motion", server.uri()),
        )
        .await;
        let outcome = download::run_bound_publication_for_test(
            db.clone(),
            &proof,
            &format!("{}/prepared-motion", server.uri()),
            &current,
        )
        .await;
        assert_eq!(outcome, (0, 1));
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"injected foreign occupant"
        );
        assert_eq!(std::fs::read(&temporary).unwrap(), MEDIA);
        assert_eq!(
            db.get_prepared_publications(
                ZONE,
                DELETED_MASTER,
                crate::state::VersionSizeKey::LiveOriginal
            )
            .await
            .unwrap(),
            vec![proof.clone()]
        );
        eprintln!("R2_REAL_PIPELINE collision_refused=true prepared_by_transfer=true");
        Some((proof, round))
    } else {
        None
    };

    let family_before = if protected.is_some() {
        let conn = db
            .acquire_lock("snapshot protected family before strict settlement")
            .unwrap();
        let mut stmt = conn.prepare("SELECT json_array(version_size,status,checksum,size_bytes,local_path,is_deleted,deleted_at,metadata_hash) FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size").unwrap();
        let rows = stmt
            .query_map(rusqlite::params![ZONE, DELETED_MASTER], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        Some(rows)
    } else {
        None
    };
    let requests_before_drain = shared_requests.lock().unwrap().len();
    let catalog = crate::sync_loop::runner::drain_catalog_source_revalidation(
        &[&shared_state, &primary_state],
        &builder,
        download::DownloadControls::download_hidden(),
        &download::compute_config_hash(&make_run_cycle_config()),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!catalog.session_expired);
    assert!(
        shared_requests.lock().unwrap()[requests_before_drain..]
            .iter()
            .any(|names| names.iter().any(|name| name == DELETED_MASTER)
                && names.iter().any(|name| name == DELETED_CHILD)),
        "the typed drain must request the exact source pair"
    );
    if let Some((proof, round)) = protected {
        use crate::state::db::TempFileOwnershipStore;
        assert_eq!(catalog.retained, 1);
        assert_eq!(catalog.retained_sources, vec![ZONE]);
        let deadline: i64 = {
            let conn = db
                .acquire_lock("verify protected family and retained debt")
                .unwrap();
            let mut stmt = conn.prepare("SELECT json_array(version_size,status,checksum,size_bytes,local_path,is_deleted,deleted_at,metadata_hash) FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size").unwrap();
            let rows = stmt
                .query_map(rusqlite::params![ZONE, DELETED_MASTER], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            assert_eq!(Some(rows), family_before);
            conn.query_row(
                "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                rusqlite::params![round, DELETED_MASTER],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert!(deadline > chrono::Utc::now().timestamp());
        assert_eq!(std::fs::read(proof.temporary.to_path()).unwrap(), MEDIA);
        assert_eq!(
            std::fs::read(proof.destination.to_path()).unwrap(),
            b"injected foreign occupant"
        );
        drop(shared_state);
        drop(primary_state);
        drop(collection_context);
        drop(selected_libraries);
        drop(all_libraries);
        drop(base);
        drop(result);
        assert_eq!(
            Arc::strong_count(&db),
            1,
            "all owner handles must close before publication recovery"
        );
        drop(db);
        let reopened = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        assert_eq!(
            reopened
                .get_prepared_publications(
                    ZONE,
                    DELETED_MASTER,
                    crate::state::VersionSizeKey::LiveOriginal
                )
                .await
                .unwrap(),
            vec![proof.clone()]
        );
        let saved_deadline: i64 = reopened
            .acquire_lock("reopened exact protected debt")
            .unwrap()
            .query_row(
                "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                rusqlite::params![round, DELETED_MASTER],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved_deadline, deadline);
        // Remove only the injected collision. The normal pending-publication
        // owner must recover the prepared part before a task may reuse it.
        std::fs::remove_file(proof.destination.to_path()).unwrap();
        Mock::given(method("GET"))
            .and(path("/prepared-recovery"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
            .expect(0)
            .mount(&server)
            .await;
        let current = current_bound_motion(
            reopened.clone(),
            &shared_zone_id,
            &format!("{}/prepared-recovery", server.uri()),
        )
        .await;
        download::recover_bound_publication_for_test(&reopened, &proof, &current).await;
        assert!(!proof.temporary.to_path().exists());
        assert_eq!(std::fs::read(proof.destination.to_path()).unwrap(), MEDIA);
        eprintln!("R2_REAL_PIPELINE recovery_published=true finalized_by_pending_owner=true");
        assert!(
            reopened
                .get_prepared_publications(
                    ZONE,
                    DELETED_MASTER,
                    crate::state::VersionSizeKey::LiveOriginal
                )
                .await
                .unwrap()
                .is_empty()
        );
        let later: i64 = reopened
            .acquire_lock("verify finalized debt remains deferred")
            .unwrap()
            .query_row(
                "SELECT next_retry_at FROM source_revalidation_debt WHERE round=?1 AND id=?2",
                rusqlite::params![round, DELETED_MASTER],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(later, deadline);
        for (path, bytes, mtime) in &deleted_snapshots {
            assert_eq!(&std::fs::read(path).unwrap(), bytes);
            assert_eq!(&std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
        }
        let finalized_family: Vec<String> = {
            let conn = reopened.acquire_lock("snapshot finalized family").unwrap();
            let mut stmt = conn.prepare("SELECT json_array(version_size,status,checksum,size_bytes,local_path,is_deleted,deleted_at,metadata_hash) FROM assets WHERE library=?1 AND id=?2 ORDER BY version_size").unwrap();
            stmt.query_map(rusqlite::params![ZONE, DELETED_MASTER], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let old_files:Vec<_> = deleted_snapshots.iter().map(|(path, bytes, mtime)| {
            use sha2::{Digest, Sha256};
            json!({"path":path,"sha256":data_encoding::HEXLOWER.encode(&Sha256::digest(bytes)),"mtime_ns":mtime.duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()})
        }).collect();
        drop(reopened);
        let state = json!({"round":round,"deadline":deadline,"zone":shared_zone_id,"proof":proof,"family_before":family_before,"finalized_family":finalized_family,"old_files":old_files});
        std::fs::write(
            dir.path().join("r2-state.json"),
            serde_json::to_vec_pretty(&state).unwrap(),
        )
        .unwrap();
        server.verify().await;
        if std::env::var_os("KEI_R2_RETAIN_ROOT").is_some() {
            let path = dir.keep();
            eprintln!("R2_RETAINED_ROOT={} R2_DEADLINE={deadline}", path.display());
        }
        return;
    }
    assert_eq!(catalog.retained, 0);

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
    if variant == MixedSelection::BridgeOnly {
        assert_eq!(std::fs::read(root.join("bridge-only.JPG")).unwrap(), MEDIA);
        let conn = db
            .acquire_lock("verify bridge-only source receipt")
            .unwrap();
        let (scope, native, source, local): (String, String, String, String) = conn.query_row(
            "SELECT scope,native_path,source_checksum,local_checksum FROM download_receipts WHERE library=?1 AND child='asset-bridge-only' AND master='bridge-only' AND version_size='original'",
            [ZONE], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).unwrap();
        assert_eq!(
            PhotoLibrary::provider_scope_projection(&scope),
            Some((
                ZONE.to_owned(),
                shared_zone_id["ownerRecordName"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
                "shared".to_owned()
            ))
        );
        assert_eq!(
            serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(&native)
                .unwrap()
                .to_path(),
            root.join("bridge-only.JPG")
        );
        use sha2::{Digest, Sha256};
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(MEDIA));
        assert_eq!((source, local), (hash.clone(), hash));
    }
    if variant != MixedSelection::SharedWithUnfiled {
        let state_snapshot = |db: &SqliteStateDb| {
            let conn = db
                .acquire_lock("snapshot mixed quiet history and receipts")
                .unwrap();
            let mut result = Vec::new();
            for sql in [
                "SELECT r.id,r.library,r.scope,r.phase,d.id,d.next_retry_at FROM source_revalidation_rounds r LEFT JOIN source_revalidation_debt d ON d.round=r.id ORDER BY r.id,d.id",
                "SELECT library,id,version_size,local_path,is_deleted,deleted_at,metadata_hash FROM assets WHERE library='SharedSync-v027-synthetic' AND id='shared-live-master' ORDER BY version_size",
                "SELECT library,child,master,version_size,native_path,source_checksum,local_checksum,proof_hash FROM download_receipts WHERE child IN ('asset-shared-current','asset-primary-current','asset-bridge-only') ORDER BY library,child",
            ] {
                let mut stmt = conn.prepare(sql).unwrap();
                let rows = stmt
                    .query_map([], |row| {
                        (0..row.as_ref().column_count())
                            .map(|i| row.get::<_, rusqlite::types::Value>(i))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                result.push(rows);
            }
            result
        };
        let retained_state = state_snapshot(&db);
        let inventory_counts = (
            shared_inventory.lock().unwrap().len(),
            primary_inventory.lock().unwrap().len(),
        );
        let snapshots = [
            root.join("shared-current.JPG"),
            root.join("primary-current.JPG"),
        ]
        .map(|path| {
            (
                path.clone(),
                std::fs::read(&path).unwrap(),
                std::fs::metadata(&path).unwrap().modified().unwrap(),
            )
        });
        let bridge_snapshot = (variant == MixedSelection::BridgeOnly).then(|| {
            let path = root.join("bridge-only.JPG");
            let bytes = std::fs::read(&path).unwrap();
            let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            (path, bytes, mtime)
        });
        drop(shared_state);
        drop(primary_state);
        drop(collection_context);
        drop(selected_libraries);
        drop(all_libraries);
        drop(base);
        assert_eq!(
            Arc::strong_count(&db),
            1,
            "all owner handles must close before quiet reopen"
        );
        drop(db);
        for pass in 2..=3 {
            let reopened = Arc::new(
                SqliteStateDb::open_owned(&db_path, &baseline_owner())
                    .await
                    .unwrap(),
            );
            let mut shared_library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/shared".into(),
                Arc::default(),
                Box::new(shared_session.clone()),
                Arc::new(shared_zone_id.clone()),
                "shared".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            let mut primary_library = PhotoLibrary::new(
                "https://synthetic.example/database/1/photos/production/private".into(),
                Arc::default(),
                Box::new(primary_session.clone()),
                Arc::new(primary_zone_id.clone()),
                "private".into(),
                crate::retry::RetryConfig::default(),
            )
            .await
            .unwrap();
            shared_library.shadow_capture = Some(ShadowCapture::new(
                reopened.clone(),
                baseline_owner(),
                "com",
            ));
            primary_library.shadow_capture = Some(ShadowCapture::new(
                reopened.clone(),
                baseline_owner(),
                "com",
            ));
            let selected = vec![primary_library.clone(), shared_library.clone()];
            let all = vec![primary_library.clone(), shared_library.clone()];
            let collection = collection_libraries(&selection, &selected, &all);
            let context = build_collection_context(&selection, collection)
                .await
                .unwrap();
            let selected_zones = zone_name_set(&selected);
            let collection_zones = zone_name_set(collection);
            let mut states = Vec::new();
            for library in [shared_library, primary_library] {
                let name = library.zone_name().to_owned();
                let scope =
                    pass_scope_for_zone(&selection, &name, &selected_zones, &collection_zones);
                let plan = resolve_passes_for_scope(&library, &selection, scope, &context, &[])
                    .await
                    .unwrap();
                let mut state = make_run_cycle_library_state_with_passes(
                    &name,
                    &sync_token_key(&name),
                    plan.passes,
                );
                state.library = library;
                states.push(state);
            }
            let base = make_run_cycle_download_config_builder(&root, reopened.clone());
            let builder = |mode, exclude, groups, library| {
                let mut config = (*base(mode, exclude, groups, library)).clone();
                config.folder_structure = "none".into();
                config.folder_structure_albums = Arc::from("none");
                config.folder_structure_smart_folders = Arc::from("none");
                Arc::new(config)
            };
            let result = run_cycle(
                &[&states[0], &states[1]],
                &make_run_cycle_config(),
                Some(reopened.as_ref()),
                false,
                &builder,
                download::DownloadControls::download_hidden(),
                &shared,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(result.failed_count, 0);
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(
                (
                    shared_inventory.lock().unwrap().len(),
                    primary_inventory.lock().unwrap().len()
                ),
                inventory_counts,
                "quiet reopens must not repeat either source inventory"
            );
            for name in [ZONE, PRIMARY] {
                assert_eq!(
                    reopened
                        .get_metadata(&sync_token_key(name))
                        .await
                        .unwrap()
                        .as_deref(),
                    Some(
                        format!(
                            "after-{name}-{}",
                            pass + usize::from(
                                variant == MixedSelection::BridgeOnly && name == ZONE
                            )
                        )
                        .as_str()
                    )
                );
                let descriptor = reopened
                    .get_metadata(&format!("observed_selection:v27:{name}"))
                    .await
                    .unwrap()
                    .unwrap();
                let descriptor: Value = serde_json::from_str(&descriptor).unwrap();
                assert_eq!(descriptor["library"], name);
            }
            assert_eq!(
                state_snapshot(&reopened),
                retained_state,
                "quiet cycles must preserve exact old family, catalog debt and current receipts"
            );
            for (path, bytes, mtime) in snapshots
                .iter()
                .chain(deleted_snapshots.iter())
                .chain(bridge_snapshot.iter())
            {
                assert_eq!(&std::fs::read(path).unwrap(), bytes);
                assert_eq!(&std::fs::metadata(path).unwrap().modified().unwrap(), mtime);
            }
            // A completed cycle may retain its prework snapshot and its DB owner.
            drop(result);
            drop(states);
            drop(context);
            drop(selected);
            drop(all);
            drop(base);
            assert_eq!(
                Arc::strong_count(&reopened),
                1,
                "each quiet cycle must close all owner handles"
            );
        }
    }
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
        let replacement_zones = order[changes + 1..]
            .iter()
            .position(|entry| entry == "zones/list");
        let replacement_query = order[changes + 1..]
            .iter()
            .position(|entry| entry.starts_with("query:"));
        assert!(
            replacement_zones.is_some(),
            "the rejected token must require a fresh cycle-owned boundary: {order:?}"
        );
        assert!(
            replacement_query.is_some(),
            "a qualified fresh boundary must be followed by real inventory: {order:?}"
        );
        assert!(
            replacement_zones.unwrap() < replacement_query.unwrap(),
            "fresh boundary must precede replacement inventory: {order:?}"
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
            (1, 1),
            "the fresh cycle-owned boundary must install one qualified catalog origin"
        );
    }
    server.verify().await;
}
