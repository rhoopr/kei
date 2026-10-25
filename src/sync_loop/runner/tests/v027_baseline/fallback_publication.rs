//! Named albums keep source-specific publication proof across fallback hydration.
use super::{MEDIA, baseline_owner, pair};
use crate::commands::{AlbumPass, PassKind};
use crate::download;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoLibrary, PhotosSession};
use crate::state::SqliteStateDb;
use crate::sync_cycle::{run_cycle, sync_token_key};
use crate::sync_loop::test_support::{
    album_count_response, make_run_cycle_config, make_run_cycle_download_config_builder,
    make_run_cycle_library_state_with_passes, make_shared_session_for_run_cycle,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const OWNER: &str = "PrimarySync";
const SOURCE: &str = "SharedSync-fallback-test";
const ALBUM: &str = "album-fallback-test";

fn zone(name: &str) -> Value {
    json!({"zoneName":name,"ownerRecordName":"_defaultOwner"})
}

#[derive(Clone)]
struct CrossZoneSession {
    cycle: usize,
    records: Arc<Vec<Value>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}
impl CrossZoneSession {
    fn records_for(&self, name: &str) -> Vec<Value> {
        self.records
            .iter()
            .filter(|r| r["zoneID"]["zoneName"] == name)
            .cloned()
            .collect()
    }
}
#[async_trait::async_trait]
impl PhotosSession for CrossZoneSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let body: Value = serde_json::from_str(&body)?;
        self.requests
            .lock()
            .unwrap()
            .push((url.into(), body.clone()));
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(OWNER),"syncToken":"boundary","deleted":false}]}),
            );
        }
        if body["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"recordName":"index","recordType":"CheckIndexingState","fields":{"state":{"value":"FINISHED"}}}]}),
            );
        }
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(2));
        }
        if url.contains("/records/lookup?") {
            let name = body["zoneID"]["zoneName"].as_str().unwrap();
            let requested = body["records"].as_array().unwrap();
            let records: Vec<_> = self
                .records_for(name)
                .into_iter()
                .filter(|r| requested.iter().any(|q| q["recordName"] == r["recordName"]))
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/records/query?") {
            let name = body["zoneID"]["zoneName"].as_str().unwrap();
            let offset = body["query"]["filterBy"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["fieldName"] == "startRank")
                .and_then(|f| f["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            let records = if offset == 0 {
                self.records_for(name)
            } else {
                Vec::new()
            };
            return Ok(json!({"records":records,"syncToken":format!("query-{}",self.cycle)}));
        }
        if url.contains("/changes/zone?") {
            let requested = &body["zones"][0]["zoneID"];
            let name = requested["zoneName"].as_str().unwrap();
            let cursor = body["zones"][0]["syncToken"].as_str();
            // A scan without a cursor returns current records. Baseline bridges
            // are quiet; cycle one has one genuine relation observation, while
            // its successor and later cycles do not replay the same change.
            let observed = cursor.is_none() || (self.cycle == 1 && cursor == Some("current-0"));
            let records = if !observed {
                Vec::new()
            } else if name == OWNER {
                ["owner","shared"].into_iter().map(|id|json!({
                    "recordName":format!("relation-{id}"),"recordType":"CPLContainerRelation",
                    "recordChangeTag":format!("revision-{}",self.cycle),
                    "fields":{"containerId":{"value":ALBUM,"type":"STRING"},
                        "itemId":{"value":format!("asset-{id}"),"type":"STRING"}}
                })).collect()
            } else {
                self.records_for(name)
            };
            return Ok(json!({"zones":[{"zoneID":requested,"records":records,
                "syncToken":format!("current-{}",self.cycle),"moreComing":false}]}));
        }
        anyhow::bail!("unexpected synthetic endpoint {url}");
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
fn album(
    session: &CrossZoneSession,
    name: &str,
    capture: &crate::icloud::photos::inbox::ShadowCapture,
) -> PhotoAlbum {
    let database = if name == OWNER { "private" } else { "shared" };
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: format!(
                "https://synthetic.example/database/1/photos/production/{database}"
            )
            .into(),
            name: "Selected".into(),
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(zone(name)),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: if name == OWNER {
                Some(ALBUM.into())
            } else {
                None
            },
            cross_zone_sources: Vec::new(),
        },
        Box::new(session.clone()),
    );
    album.set_shadow_capture(capture.clone(), database.into());
    album
}

#[tokio::test]
async fn v027_cross_zone_album_modified_publications_reopen_without_get_or_identity_collapse() {
    let (trace, _guard) = crate::test_helpers::TracingCapture::install();
    let server = crate::start_wiremock_or_skip!();
    for endpoint in ["/owner", "/shared"] {
        Mock::given(method("GET"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(MEDIA)
                    .insert_header("content-type", "image/jpeg"),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let sentinel = root.join("unrelated.bin");
    std::fs::write(&sentinel, b"unrelated existing file").unwrap();
    let sentinel_mtime = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    let db_path = dir.path().join("state.db");
    let mut records = Vec::new();
    for (id, name, tag) in [("owner", OWNER, 21), ("shared", SOURCE, 22)] {
        let mut current = pair(
            id,
            &format!("{id}.JPG"),
            &format!("{}/{id}", server.uri()),
            tag,
        );
        for record in &mut current {
            record["zoneID"] = zone(name);
            if record["recordType"] == "CPLAsset" {
                record["fields"]["masterRef"]["value"]["zoneID"] = zone(name);
            }
        }
        records.extend(current);
    }
    let (_session_dir, shared) = make_shared_session_for_run_cycle().await;
    let mut saved = None;
    for cycle in 0..3 {
        let db = Arc::new(
            SqliteStateDb::open_owned(&db_path, &baseline_owner())
                .await
                .unwrap(),
        );
        let session = CrossZoneSession {
            cycle,
            records: Arc::new(records.clone()),
            requests: Arc::default(),
        };
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
        let source = album(&session, SOURCE, &capture);
        let selected_album = album(&session, OWNER, &capture).with_cross_zone_sources(vec![source]);
        let passes = vec![AlbumPass {
            kind: PassKind::Album,
            album: selected_album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }];
        let mut selected =
            make_run_cycle_library_state_with_passes(OWNER, &sync_token_key(OWNER), passes);
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            Box::new(session.clone()),
            Arc::new(zone(OWNER)),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(capture);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, exclude, groups, zone| {
            let mut config = (*base(mode, exclude, groups, zone)).clone();
            config.folder_structure = "none".into();
            config.folder_structure_albums = "{album}".into();
            config.metadata.set_exif_datetime = true;
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
            result.failed_count,
            0,
            "cycle {cycle}: {result:?}; trace={:?}",
            trace.events()
        );
        assert_eq!(
            result.stats.downloaded,
            if cycle == 0 { 2 } else { 0 },
            "{result:?}"
        );
        assert_eq!(result.stats.skipped.uncertain, 0, "{result:?}");
        let mut snapshot = Vec::new();
        for (id, name) in [("owner", OWNER), ("shared", SOURCE)] {
            let output = root.join("Selected").join(format!("{id}.JPG"));
            let bytes = std::fs::read(&output).unwrap();
            assert_ne!(bytes, MEDIA);
            let mtime = std::fs::metadata(&output).unwrap().modified().unwrap();
            snapshot.push((bytes, mtime));
            let measured = download::file::compute_sha256(&output).await.unwrap();
            let conn = db.acquire_lock("cross-zone source proof").unwrap();
            let (scope,master,source_hash,local_hash):(String,String,String,String)=conn.query_row(
                "SELECT scope,master,source_checksum,local_checksum FROM download_receipts WHERE library=?1 AND child=?2",
                rusqlite::params![name,format!("asset-{id}")],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
            let scope: Value = serde_json::from_str(&scope).unwrap();
            assert_eq!(scope["zone"]["zoneName"], name);
            assert_eq!(
                scope["database"],
                if name == OWNER { "private" } else { "shared" }
            );
            assert_eq!(master, id);
            assert_ne!(source_hash, local_hash);
            assert_eq!(local_hash, measured);
            {
                use sha2::{Digest, Sha256};
                assert_eq!(source_hash, format!("{:x}", Sha256::digest(MEDIA)));
            }
            let wrong_scope: i64 = conn
                .query_row(
                    "SELECT count(*) FROM download_receipts WHERE library<>?1 AND child=?2",
                    rusqlite::params![name, format!("asset-{id}")],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                wrong_scope, 0,
                "source identity must not acquire the album-owner scope"
            );
        }
        if let Some(prior) = &saved {
            assert_eq!(&snapshot, prior);
        }
        saved = Some(snapshot);
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"unrelated existing file"
        );
        assert_eq!(
            std::fs::metadata(&sentinel).unwrap().modified().unwrap(),
            sentinel_mtime
        );
    }
    server.verify().await;
}
