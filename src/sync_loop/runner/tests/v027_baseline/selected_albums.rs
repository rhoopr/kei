//! Selected album additions and removals use current membership after reopen.
use super::{MEDIA, ZONE, baseline_owner, pair, zone};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoLibrary, PhotosSession};
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
    records: Arc<Vec<Vec<Value>>>,
    rank_queries: Arc<AtomicUsize>,
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
            let requested = request["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records
                .iter()
                .flatten()
                .filter(|record| {
                    requested
                        .iter()
                        .any(|query| query["recordName"] == record["recordName"])
                })
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
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = Session {
            cycle,
            records: Arc::clone(&records),
            rank_queries: Arc::new(AtomicUsize::new(0)),
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
        assert_eq!(
            result.stats.downloaded,
            match cycle {
                0 => 3,
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
