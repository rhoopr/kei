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
    session: &dyn PhotosSession,
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
        session.clone_box(),
    );
    album.set_shadow_capture(capture.clone(), database.into());
    album
}

// Preserve this fixture's changing provider history while supplying the current
// scoped lookup evidence also used by the two-destination lifecycle fixture.
#[derive(Clone)]
struct CurrentLookupSession(CrossZoneSession);
#[async_trait::async_trait]
impl PhotosSession for CurrentLookupSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/records/lookup?") {
            TwoAlbumSession(self.0.clone())
                .post(url, body, headers)
                .await
        } else {
            self.0.post(url, body, headers).await
        }
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
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
        let source = album(&CurrentLookupSession(session.clone()), SOURCE, &capture);
        let selected_album = album(&CurrentLookupSession(session.clone()), OWNER, &capture)
            .with_cross_zone_sources(vec![source]);
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

#[derive(Clone)]
struct TwoAlbumSession(CrossZoneSession);
#[async_trait::async_trait]
impl PhotosSession for TwoAlbumSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        let mut response = self.0.post(url, body, headers).await?;
        if url.contains("/records/lookup?") && request["zoneID"]["zoneName"] == OWNER {
            let rows = response["records"].as_array_mut().unwrap();
            for requested in request["records"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|record| {
                    matches!(
                        record["recordName"].as_str(),
                        Some("asset-shared" | "owner-proxy")
                    )
                })
            {
                rows.push(
                    json!({"recordName":requested["recordName"],"recordType":"CPLAsset","zoneID":zone(OWNER),
                    "fields":{"isSparsePrivateRecord":{"value":1,"type":"INT64"},
                    "linkedShareRecordName":{"value":"asset-shared","type":"STRING"},
                    "linkedShareZoneName":{"value":SOURCE,"type":"STRING"},
                    "linkedShareZoneOwner":{"value":{"recordName":"_defaultOwner"},"type":"REFERENCE"}}}),
                );
            }
            for requested in request["records"].as_array().unwrap() {
                let name = requested["recordName"].as_str().unwrap();
                let (container, suffix) =
                    if let Some(suffix) = name.strip_prefix("second-relation-") {
                        ("album-second", suffix)
                    } else if let Some(suffix) = name.strip_prefix("relation-") {
                        (ALBUM, suffix)
                    } else {
                        continue;
                    };
                rows.push(json!({"recordName":name,"recordType":"CPLContainerRelation","zoneID":zone(OWNER),
                    "fields":{"containerId":{"value":container},"itemId":{"value":format!("asset-{suffix}")}}}));
            }
        }
        if url.contains("/changes/zone?") {
            if request["zones"][0]["syncToken"].as_str().is_some() {
                response["zones"][0]["records"] = json!([]);
            } else if request["zones"][0]["zoneID"]["zoneName"] == OWNER {
                let rows = response["zones"][0]["records"].as_array_mut().unwrap();
                let additional: Vec<_> = rows
                    .iter()
                    .cloned()
                    .map(|mut row| {
                        row["recordName"] =
                            json!(format!("second-{}", row["recordName"].as_str().unwrap()));
                        row["fields"]["containerId"]["value"] = json!("album-second");
                        row
                    })
                    .collect();
                rows.extend(additional);
            }
        }
        Ok(response)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn cross_zone_two_album_destinations_keep_independent_failed_work() {
    run_cross_zone_case(false, false, None).await;
}
#[tokio::test]
async fn cross_zone_incremental_addition_retains_both_destinations_and_reopens() {
    run_cross_zone_case(true, false, None).await;
}
#[tokio::test]
async fn cross_zone_folder_change_preserves_pending_destination_on_reopen() {
    run_cross_zone_case(false, true, None).await;
}

#[derive(Clone)]
struct AddMemberSession(TwoAlbumSession);
#[async_trait::async_trait]
impl PhotosSession for AddMemberSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        let mut response = self.0.post(url, body, headers).await?;
        let cycle = self.0.0.cycle;
        if cycle == 0 && url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(1));
        }
        if url.contains("/changes/zone?") && request["zones"][0]["zoneID"]["zoneName"] == OWNER {
            if cycle == 0 {
                response["zones"][0]["records"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|record| record["fields"]["itemId"]["value"] != "asset-shared");
            } else if cycle == 1 && request["zones"][0]["syncToken"] == "current-0" {
                response["zones"][0]["records"] = json!([
                    {"recordName":"relation-shared","recordType":"CPLContainerRelation","fields":{"containerId":{"value":ALBUM},"itemId":{"value":"owner-proxy"}}},
                    {"recordName":"second-relation-shared","recordType":"CPLContainerRelation","fields":{"containerId":{"value":"album-second"},"itemId":{"value":"owner-proxy"}}}
                ]);
            }
        }
        if cycle > 0 && url.contains("/records/lookup?") && request["zoneID"]["zoneName"] == OWNER {
            for record in response["records"].as_array_mut().unwrap() {
                if record["recordType"] == "CPLContainerRelation"
                    && record["fields"]["itemId"]["value"] == "asset-shared"
                {
                    record["fields"]["itemId"]["value"] = json!("owner-proxy");
                }
            }
        }
        Ok(response)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

tokio::task_local! { static CURRENT_SOURCE_METADATA: bool; }
#[cfg(feature = "xmp")]
#[tokio::test]
async fn cross_zone_current_metadata_and_missing_copy_retry_independently() {
    CURRENT_SOURCE_METADATA
        .scope(true, run_cross_zone_case(false, false, None))
        .await;
}

async fn run_cross_zone_case(
    incremental_addition: bool,
    folder_change: bool,
    refusal: Option<&'static str>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = crate::start_wiremock_or_skip!();
    let source_metadata = CURRENT_SOURCE_METADATA
        .try_with(|value| *value)
        .unwrap_or(false);
    let mut completed_sidecars = std::collections::BTreeMap::new();
    let failure_cycle = usize::from(incremental_addition);
    let shared_gets = Arc::new(AtomicUsize::new(0));
    let count = shared_gets.clone();
    let current_cycle = Arc::new(AtomicUsize::new(0));
    let response_cycle = current_cycle.clone();
    Mock::given(method("GET"))
        .and(path("/shared"))
        .respond_with(move |_: &wiremock::Request| {
            let request = count.fetch_add(1, Ordering::SeqCst);
            if response_cycle.load(Ordering::SeqCst) == failure_cycle && request != 1 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_bytes(MEDIA)
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/owner"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(MEDIA))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("photos");
    std::fs::create_dir(&root).unwrap();
    let sentinel = root.join("sentinel.bin");
    std::fs::write(&sentinel, b"unchanged original").unwrap();
    let stamp = std::fs::metadata(&sentinel).unwrap().modified().unwrap();
    let database = dir.path().join("state.db");
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
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let mut observed = Vec::new();
    let mut first_stamp = None;
    let mut baseline_gets = 0;
    for cycle in 0..(failure_cycle + 3) {
        if source_metadata {
            let blocked = root.join("Second/shared.JPG.xmp");
            if cycle == 0 {
                std::fs::create_dir_all(&blocked).unwrap();
            }
            if cycle == 1 {
                std::fs::remove_dir(&blocked).unwrap();
            }
        }
        current_cycle.store(cycle, Ordering::SeqCst);
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let mut current_records = records.clone();
        if CHANGED_MASTER.try_with(|enabled| *enabled).unwrap_or(false) && cycle > 0 {
            for record in &mut current_records {
                if record["zoneID"]["zoneName"] == SOURCE {
                    if record["recordType"] == "CPLMaster" {
                        record["recordName"] = json!("shared-updated");
                    }
                    if record["recordType"] == "CPLAsset" {
                        record["fields"]["masterRef"]["value"]["recordName"] =
                            json!("shared-updated");
                    }
                }
            }
        }
        let base_session = CrossZoneSession {
            cycle,
            records: Arc::new(current_records),
            requests: Arc::default(),
        };
        let requests = Arc::clone(&base_session.requests);
        let session: Box<dyn PhotosSession> = if let Some(reason) = refusal {
            Box::new(RefusedMembershipSession(
                TwoAlbumSession(base_session.clone()),
                reason,
            ))
        } else if incremental_addition {
            Box::new(AddMemberSession(TwoAlbumSession(base_session.clone())))
        } else {
            Box::new(TwoAlbumSession(base_session.clone()))
        };
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), baseline_owner(), "com");
        let mut passes = Vec::new();
        for (name, container) in [("First", ALBUM), ("Second", "album-second")] {
            let source = album(&base_session, SOURCE, &capture);
            let mut selected = PhotoAlbum::new(
                PhotoAlbumConfig {
                    params: Arc::default(),
                    service_endpoint:
                        "https://synthetic.example/database/1/photos/production/private".into(),
                    name: name.into(),
                    list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                    obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                    query_filter: None,
                    page_size: 100,
                    zone_id: Arc::new(zone(OWNER)),
                    retry_config: crate::retry::RetryConfig::default(),
                    container_id: Some(container.into()),
                    cross_zone_sources: vec![source],
                },
                session.clone_box(),
            );
            selected.set_shadow_capture(capture.clone(), "private".into());
            passes.push(AlbumPass {
                kind: PassKind::Album,
                album: selected,
                exclude_ids: Arc::default(),
            });
        }
        let mut selected =
            make_run_cycle_library_state_with_passes(OWNER, &sync_token_key(OWNER), passes);
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/private".into(),
            Arc::default(),
            session.clone_box(),
            Arc::new(zone(OWNER)),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(capture);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, excludes, groups, library| {
            let mut config = (*base(mode, excludes, groups, library)).clone();
            config.retry.max_retries = 0;
            config.folder_structure_albums = if folder_change && cycle > 0 {
                "changed/{album}"
            } else {
                "{album}"
            }
            .into();
            #[cfg(feature = "xmp")]
            {
                config.metadata.xmp_sidecar = source_metadata;
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
        .unwrap_or_else(|error| {
            panic!("cycle {cycle}: {error}");
        });
        if refusal.is_none() && !folder_change {
            assert!(
                db.get_metadata(&format!("observed_selection:v27:{OWNER}"))
                    .await
                    .unwrap()
                    .is_some(),
                "cross-source selection must carry proof at cycle {cycle}"
            );
        }
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged original");
        assert_eq!(
            std::fs::metadata(&sentinel).unwrap().modified().unwrap(),
            stamp
        );
        if cycle > 0 && !(folder_change && cycle == 1) {
            assert!(
                !requests.lock().unwrap().iter().any(|(url, body)| (url
                    .contains("/records/query?")
                    && body["query"]["recordType"] != "CheckIndexingState")
                    || (url.contains("/changes/zone?") && body["zones"][0]["syncToken"].is_null())),
                "quiet retry must not repeat inventory"
            );
        }
        if cycle == failure_cycle {
            baseline_gets = shared_gets.load(Ordering::SeqCst);
            assert!(
                shared_gets.load(Ordering::SeqCst) >= 2,
                "fixture must reach failed and successful transfers"
            );
        }
        if cycle >= failure_cycle {
            let first = root.join("Second/shared.JPG");
            assert_eq!(std::fs::read(&first).unwrap(), MEDIA);
            let current_stamp = std::fs::metadata(&first).unwrap().modified().unwrap();
            if let Some(previous) = first_stamp {
                assert_eq!(current_stamp, previous);
            } else {
                first_stamp = Some(current_stamp);
            }
        }
        let conn = rusqlite::Connection::open(&database).unwrap();
        let pending: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND child='asset-shared' AND verified_media=0", [SOURCE], |row| row.get(0)).unwrap();
        let second = root.join("First/shared.JPG");
        observed.push((cycle, second.exists(), pending, result.failed_count));
        if cycle == failure_cycle {
            assert_eq!(
                pending, 1,
                "baseline must leave exactly the failed destination pending"
            );
        }
        if source_metadata {
            let unresolved:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE library=?1 AND child='asset-shared' AND verified_media=1 AND verified_metadata=0",[SOURCE],|r|r.get(0)).unwrap();
            if cycle == 0 {
                assert_eq!(unresolved, 1);
                assert!(result.failed_count > 0);
            } else {
                assert_eq!(
                    unresolved, 0,
                    "source-root metadata must finish on quiet reopen"
                );
                for album in ["First", "Second"] {
                    let path = root.join(album).join("shared.JPG.xmp");
                    let bytes = std::fs::read(&path).unwrap();
                    let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
                    let observed = (bytes, stamp);
                    if let Some(prior) = completed_sidecars.get(album) {
                        assert_eq!(prior, &observed);
                    } else {
                        completed_sidecars.insert(album, observed);
                    }
                }
            }
        }
        if cycle >= failure_cycle {
            db.make_selection_retry_due(baseline_owner(), "asset-shared".into())
                .await
                .unwrap();
        }
    }
    assert_eq!(
        observed[failure_cycle].2, 1,
        "failed destination must remain in the current ledger: {observed:?}"
    );
    if refusal.is_some() {
        assert!(
            observed[failure_cycle + 1..]
                .iter()
                .all(|(_, exists, pending, _)| !exists && *pending == 1),
            "unverified membership must retain the missing destination: {observed:?}"
        );
        assert_eq!(
            shared_gets.load(Ordering::SeqCst),
            baseline_gets,
            "refused membership cannot authorize another transfer"
        );
    } else {
        assert!(
            observed[failure_cycle + 1].1 && observed[failure_cycle + 2].1,
            "quiet reopen must complete only the missing destination: {observed:?}"
        );
        assert_eq!(shared_gets.load(Ordering::SeqCst), baseline_gets + 1);
        assert_eq!(std::fs::read(root.join("First/shared.JPG")).unwrap(), MEDIA);
    }
    if folder_change {
        assert!(
            !root.join("changed/First/shared.JPG").exists(),
            "pending copy keeps its admitted destination"
        );
    }
}

#[tokio::test]
async fn selector_link_scopes_colliding_child_ids_and_never_guesses() {
    const DECOY: &str = "SharedSync-decoy";
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &baseline_owner())
            .await
            .unwrap(),
    );
    let capture = crate::icloud::photos::inbox::ShadowCapture::new(db, baseline_owner(), "com");
    let mut records = Vec::new();
    for name in [SOURCE, DECOY] {
        let mut values = pair(
            "shared",
            if name == SOURCE {
                "right.JPG"
            } else {
                "wrong.JPG"
            },
            "https://synthetic.example/content",
            22,
        );
        for value in &mut values {
            value["zoneID"] = zone(name);
            if value["recordType"] == "CPLAsset" {
                value["fields"]["masterRef"]["value"]["zoneID"] = zone(name);
            }
        }
        records.extend(values);
    }
    let session = CrossZoneSession {
        cycle: 0,
        records: Arc::new(records),
        requests: Arc::default(),
    };
    let make_owner = |linked: bool| {
        let mut selected = PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: "https://synthetic.example/database/1/photos/production/private"
                    .into(),
                name: "First".into(),
                list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(zone(OWNER)),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: Some(ALBUM.into()),
                cross_zone_sources: vec![
                    album(&session, DECOY, &capture),
                    album(&session, SOURCE, &capture),
                ],
            },
            if linked {
                Box::new(TwoAlbumSession(session.clone()))
            } else {
                Box::new(session.clone())
            },
        );
        selected.set_shadow_capture(capture.clone(), "private".into());
        selected
    };
    for child in ["asset-shared", "owner-proxy"] {
        session.requests.lock().unwrap().clear();
        let asset = make_owner(true)
            .confirm_selector_target(child, None)
            .await
            .unwrap();
        assert_eq!(asset.source_zone(), Some(SOURCE));
        assert_eq!(asset.asset_record_name(), "asset-shared");
        assert_eq!(asset.selector_child_in_zone(OWNER), Some(child));
        assert!(
            !session
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, body)| body["zoneID"]["zoneName"] == DECOY),
            "an identically named decoy must never be queried"
        );
        let mut wrong = asset.selector_membership().unwrap().clone();
        wrong["source_zone"] = zone(DECOY);
        session.requests.lock().unwrap().clear();
        assert!(
            make_owner(true)
                .confirm_selector_target(child, Some(&wrong))
                .await
                .is_err()
        );
        assert_eq!(
            session.requests.lock().unwrap().len(),
            1,
            "changed binding must fail before target lookup"
        );
    }
    session.requests.lock().unwrap().clear();
    assert!(
        make_owner(false)
            .confirm_selector_target("asset-shared", None)
            .await
            .is_err()
    );
    assert_eq!(
        session.requests.lock().unwrap().len(),
        1,
        "missing owner evidence cannot trigger a search of matching source IDs"
    );
}

#[derive(Clone)]
struct RefusedMembershipSession(TwoAlbumSession, &'static str);
#[async_trait::async_trait]
impl PhotosSession for RefusedMembershipSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        let mut response = self.0.post(url, body, headers).await?;
        if self.0.0.cycle > 0
            && url.contains("/records/lookup?")
            && request["zoneID"]["zoneName"] == OWNER
        {
            let rows = response["records"].as_array_mut().unwrap();
            if self.1 == "missing-link" {
                rows.retain(|record| record["recordName"] != "asset-shared");
            }
            for record in rows {
                if record["recordType"] == "CPLContainerRelation" {
                    if self.1 == "deleted" {
                        record["deleted"] = json!(true);
                    }
                    if self.1 == "wrong-album" {
                        record["fields"]["containerId"]["value"] = json!("not-selected");
                    }
                }
                if record["recordName"] == "asset-shared" && self.1 == "changed-source" {
                    record["fields"]["linkedShareZoneName"]["value"] =
                        json!("SharedSync-not-selected");
                }
            }
        }
        Ok(response)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
#[tokio::test]
async fn cross_zone_unverified_membership_survives_quiet_reopen_without_transfer() {
    for refusal in ["deleted", "wrong-album", "missing-link", "changed-source"] {
        run_cross_zone_case(false, false, Some(refusal)).await;
    }
}

#[derive(Clone)]
struct OwnerRankOmission(CrossZoneSession);
#[async_trait::async_trait]
impl PhotosSession for OwnerRankOmission {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let mut response = self.0.post(url, body, headers).await?;
        if url.contains("/records/query?") {
            response["records"]
                .as_array_mut()
                .unwrap()
                .retain(|record| {
                    !matches!(
                        record["recordName"].as_str(),
                        Some("asset-shared" | "shared")
                    )
                });
        }
        Ok(response)
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
#[tokio::test]
async fn owner_rank_omission_recovers_without_cross_source_witness() {
    use futures_util::TryStreamExt;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&dir.path().join("state.db"), &baseline_owner())
            .await
            .unwrap(),
    );
    let capture = crate::icloud::photos::inbox::ShadowCapture::new(db, baseline_owner(), "com");
    let mut records = Vec::new();
    for id in ["owner", "shared"] {
        let mut values = pair(
            id,
            &format!("{id}.JPG"),
            "https://synthetic.example/content",
            22,
        );
        for value in &mut values {
            value["zoneID"] = zone(OWNER);
            if value["recordType"] == "CPLAsset" {
                value["fields"]["masterRef"]["value"]["zoneID"] = zone(OWNER);
            }
        }
        records.extend(values);
    }
    let base = CrossZoneSession {
        cycle: 0,
        records: Arc::new(records),
        requests: Arc::default(),
    };
    let mut selected = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: "https://synthetic.example/database/1/photos/production/private"
                .into(),
            name: "First".into(),
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(zone(OWNER)),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: Some(ALBUM.into()),
            cross_zone_sources: vec![album(&base, SOURCE, &capture)],
        },
        Box::new(OwnerRankOmission(base.clone())),
    );
    selected.set_shadow_capture(capture, "private".into());
    let (stream, token) = selected.photo_stream_with_token(None, Some(2), 1);
    let assets: Vec<_> = stream.try_collect().await.unwrap();
    assert_eq!(assets.len(), 2);
    assert!(
        assets
            .iter()
            .any(|asset| asset.asset_record_name() == "asset-shared"
                && asset.source_zone() == Some(OWNER)
                && asset.selector_membership().is_none())
    );
    assert!(token.await.unwrap().is_some());
    assert!(
        !base
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| body["zoneID"]["zoneName"] == SOURCE)
    );
}

tokio::task_local! { static CHANGED_MASTER: bool; }
#[tokio::test]
async fn changed_master_two_albums_keep_completed_destination() {
    CHANGED_MASTER
        .scope(true, run_cross_zone_case(false, false, None))
        .await;
}

#[derive(Clone)]
struct Issue845Session(CrossZoneSession, bool);

#[async_trait::async_trait]
impl PhotosSession for Issue845Session {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/zones/list") {
            return Ok(
                json!({"zones":[{"zoneID":zone(SOURCE),"syncToken":"boundary","deleted":false}]}),
            );
        }
        if !self.1 && self.0.cycle == 1 && url.contains("/changes/zone?") {
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(request["zones"][0]["zoneID"], zone(SOURCE));
            assert_eq!(request["zones"][0]["syncToken"], "current-0");
            self.0.requests.lock().unwrap().push((url.into(), request));
            return Ok(json!({"zones":[{"zoneID":zone(SOURCE),"records":[],
                "syncToken":"current-1","moreComing":false}]}));
        }
        self.0.post(url, body, headers).await
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn issue845_collision_still_with_expired_movie_survives_reopen() {
    issue845_partial_movie_case(false).await;
}

#[tokio::test]
async fn issue845_repeated_unchanged_observation_retry_diagnostic() {
    issue845_partial_movie_case(true).await;
}

async fn issue845_partial_movie_case(repeated_observation: bool) {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let server = crate::test_helpers::start_wiremock_or_skip("issue845 partial live")
        .await
        .expect("synthetic CDN must bind");
    let occupant = include_bytes!("../../../../../tests/data/media/icloud-still.heic").as_slice();
    let still = include_bytes!("../../../../../tests/data/media/apple-live.heic").as_slice();
    let movie = include_bytes!("../../../../../tests/data/media/apple-live.mov").as_slice();
    let still_gets = Arc::new(AtomicUsize::new(0));
    let movie_gets = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/occupant"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(occupant))
        .expect(1)
        .mount(&server)
        .await;
    let count = still_gets.clone();
    Mock::given(method("GET"))
        .and(path("/still"))
        .respond_with(move |_: &wiremock::Request| {
            count.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_bytes(still)
        })
        .mount(&server)
        .await;
    let count = movie_gets.clone();
    Mock::given(method("GET"))
        .and(path("/expired-movie"))
        .respond_with(move |_: &wiremock::Request| {
            count.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(410)
        })
        .mount(&server)
        .await;

    // An explicit diagnostic directory retains only this synthetic fixture,
    // including on panic; without it the test keeps ordinary TempDir cleanup.
    let diagnostic_parent = std::env::var_os("KEI_ISSUE845_DIAGNOSTIC_DIR");
    let dir = if let Some(parent) = &diagnostic_parent {
        std::fs::create_dir_all(parent).unwrap();
        tempfile::Builder::new()
            .prefix("issue845-")
            .tempdir_in(parent)
            .unwrap()
    } else {
        tempfile::tempdir().unwrap()
    };
    let fixture = dir.path().to_path_buf();
    let _fixture_guard = if diagnostic_parent.is_some() {
        let retained = dir.keep();
        eprintln!("ISSUE845_DIAGNOSTIC_FIXTURE path={}", retained.display());
        None
    } else {
        Some(dir)
    };
    let root = fixture.join("photos");
    std::fs::create_dir(&root).unwrap();
    let database = fixture.join("state.db");
    let date = crate::sync_loop::test_support::run_cycle_expected_date_dir();
    let first_path = root.join(&date).join("collision.HEIC");
    let still_path = root
        .join(&date)
        .join(format!("collision-{}.HEIC", still.len()));
    let movie_path = root
        .join(&date)
        .join(format!("collision-{}_HEVC.MOV", still.len()));
    let _timing = crate::download::planner::distinct_timing_test_hook::Control::install_positive(
        first_path.clone(),
        still_path.clone(),
    );
    let owner = baseline_owner();
    let (account, provider) = owner.authenticated_keys().unwrap();
    let mut records = Vec::new();
    for (id, endpoint, bytes) in [("owner", "occupant", occupant), ("shared", "still", still)] {
        let mut current = pair(
            id,
            "collision.HEIC",
            &format!("{}/{endpoint}", server.uri()),
            22,
        );
        current[0]["fields"]["itemType"] = json!({"value":"public.heic"});
        current[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
        current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(bytes.len());
        current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes)));
        if id == "shared" {
            current[0]["fields"]["resOriginalVidComplRes"] = json!({"value":{
                "downloadURL":format!("{}/expired-movie", server.uri()), "size":movie.len(),
                "fileChecksum":base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
            }});
            current[0]["fields"]["resOriginalVidComplFileType"] =
                json!({"value":"com.apple.quicktime-movie"});
        }
        for record in &mut current {
            record["zoneID"] = zone(SOURCE);
            if record["recordType"] == "CPLAsset" {
                record["fields"]["masterRef"]["value"]["zoneID"] = zone(SOURCE);
            }
        }
        records.extend(current);
    }
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let expected_scope = json!({
        "zone":zone(SOURCE), "name":"Selected",
        "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
        "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
        "query_filter":null, "container_id":null, "cross_zone_sources":[]
    });
    let expected_pass = format!(
        "{:x}",
        Sha256::digest(
            json!({"kind":"Unfiled","scope":expected_scope,"excludes":[]})
                .to_string()
                .as_bytes()
        )
    );
    let mut saved_receipt = None;
    let mut saved_stamp = None;
    let mut occupant_stamp = None;
    let mut saved_retry: Option<Value> = None;
    let mut original_equivalence: Option<Value> = None;
    let cycles = if repeated_observation { 2 } else { 3 };
    for cycle in 0..cycles {
        let db = Arc::new(SqliteStateDb::open_owned(&database, &owner).await.unwrap());
        let session = Issue845Session(
            CrossZoneSession {
                cycle,
                records: Arc::new(records.clone()),
                requests: Arc::default(),
            },
            repeated_observation,
        );
        let provider_requests = session.0.requests.clone();
        let capture =
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com");
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: album(&session, SOURCE, &capture),
            exclude_ids: Arc::default(),
        };
        assert_eq!(pass.album.selection_scope(), expected_scope);
        if cycle == 2 {
            // Advance only this authenticated retained obligation, after a quiet
            // reopen has proved that the original deadline was respected.
            db.make_selection_retry_due(owner.clone(), "asset-shared".into())
                .await
                .unwrap();
        }
        let before_requests = movie_gets.load(Ordering::SeqCst);
        let before_still_requests = still_gets.load(Ordering::SeqCst);
        let before_time = chrono::Utc::now().timestamp();
        let mut selected =
            make_run_cycle_library_state_with_passes(SOURCE, &sync_token_key(SOURCE), vec![pass]);
        let mut library = PhotoLibrary::new(
            "https://synthetic.example/database/1/photos/production/shared".into(),
            Arc::default(),
            Box::new(session),
            Arc::new(zone(SOURCE)),
            "shared".into(),
            crate::retry::RetryConfig::default(),
        )
        .await
        .unwrap();
        library.shadow_capture = Some(capture);
        selected.library = library;
        let base = make_run_cycle_download_config_builder(&root, db.clone());
        let builder = |mode, excludes, groups, library| {
            let mut config = base(mode, excludes, groups, library);
            Arc::make_mut(&mut config).retry.max_retries = 0;
            config
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
        assert_eq!(
            std::fs::read(&first_path).unwrap(),
            occupant,
            "cycle {cycle}: {result:?}"
        );
        assert_eq!(
            std::fs::read(&still_path).unwrap(),
            still,
            "cycle {cycle}: {result:?}"
        );
        fn collect_tree(
            path: &std::path::Path,
            found: &mut std::collections::BTreeSet<std::path::PathBuf>,
        ) {
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let kind = entry.file_type().unwrap();
                assert!(!kind.is_symlink(), "unexpected symlink");
                assert!(
                    kind.is_dir() || kind.is_file(),
                    "unexpected filesystem entry"
                );
                let path = entry.path();
                assert!(found.insert(path.clone()));
                if kind.is_dir() {
                    collect_tree(&path, found);
                }
            }
        }
        let mut actual_tree = std::collections::BTreeSet::new();
        collect_tree(&root, &mut actual_tree);
        let mut expected_tree =
            std::collections::BTreeSet::from([first_path.clone(), still_path.clone()]);
        let mut parent = first_path.parent().unwrap();
        while parent != root.as_path() {
            expected_tree.insert(parent.to_path_buf());
            parent = parent.parent().unwrap();
        }
        assert_eq!(
            actual_tree, expected_tree,
            "complete media tree on cycle {cycle}"
        );
        let first_stamp = std::fs::metadata(&first_path).unwrap().modified().unwrap();
        if let Some(previous) = occupant_stamp {
            assert_eq!(first_stamp, previous);
        }
        occupant_stamp = Some(first_stamp);
        let stamp = std::fs::metadata(&still_path).unwrap().modified().unwrap();
        if let Some(previous) = saved_stamp {
            assert_eq!(stamp, previous);
        }
        saved_stamp = Some(stamp);
        let still_requests = still_gets.load(Ordering::SeqCst) - before_still_requests;
        assert_eq!(
            still_requests,
            usize::from(cycle == 0),
            "per-cycle still GETs"
        );
        assert_eq!(still_gets.load(Ordering::SeqCst), 1);
        assert!(!movie_path.exists());
        let conn = db.acquire_lock("issue845 scoped receipts").unwrap();
        let still_receipts: i64 = conn.query_row(
            "SELECT count(*) FROM download_receipts WHERE child='asset-shared' AND version_size='original'",
            [], |row| row.get(0)).unwrap();
        assert_eq!(still_receipts, 1, "exact scoped still receipt count");
        let receipt: String = conn.query_row(
            "SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE library=?1 AND child='asset-shared' AND version_size='original'",
            [SOURCE], |row| row.get(0)).unwrap();
        let value: Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(value[0], account);
        assert_eq!(value[1], provider);
        let scope: Value = serde_json::from_str(value[2].as_str().unwrap()).unwrap();
        assert_eq!(
            scope,
            json!({
                "container":"com.apple.photos.cloud","database":"shared",
                "environment":"production","format":1,"realm":"com","zone":zone(SOURCE)
            })
        );
        assert_eq!(value[3], SOURCE);
        assert_eq!(value[4], "asset-shared");
        assert_eq!(value[5], "shared");
        assert_eq!(value[6], "asset-shared");
        assert_eq!(value[7], expected_pass);
        assert_eq!(value[8], "original");
        assert_eq!(
            value[9],
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still))
        );
        assert_eq!(value[10], still.len());
        let native: crate::state::db::provider_selection::SelectionPath =
            serde_json::from_str(value[11].as_str().unwrap()).unwrap();
        assert_eq!(native.to_path(), still_path);
        let hash = format!("{:x}", Sha256::digest(still));
        assert_eq!(value[12], hash);
        assert_eq!(value[13], hash);
        assert_eq!(value[14].as_str().unwrap().len(), 64);
        if let Some(previous) = &saved_receipt {
            assert_eq!(&receipt, previous);
        }
        saved_receipt = Some(receipt);
        let movie_receipts: i64 = conn.query_row(
            "SELECT count(*) FROM download_receipts WHERE child='asset-shared' AND version_size='live_original'",
            [], |row| row.get(0)).unwrap();
        assert_eq!(movie_receipts, 0);
        let pending: Vec<(String, String, String, String, String, i64, i64)> = conn.prepare(
            "SELECT library,master,version_size,path,checksum,size_bytes,admitted FROM provider_active_destinations WHERE child='asset-shared' AND verified_media=0 ORDER BY library,master,version_size,path,checksum,size_bytes,admitted"
        ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)))
            .unwrap().collect::<Result<_,_>>().unwrap();
        let actual_destinations: std::collections::BTreeSet<_> = pending
            .iter()
            .map(
                |(library, master, version, path, checksum, size, admitted)| {
                    (
                    library.clone(),
                    master.clone(),
                    version.clone(),
                    serde_json::from_str::<crate::state::db::provider_selection::SelectionPath>(
                        path,
                    )
                    .unwrap()
                    .to_path(),
                    checksum.clone(),
                    *size,
                    *admitted,
                )
                },
            )
            .collect();
        let expected_destinations = std::collections::BTreeSet::from([(
            SOURCE.to_owned(),
            "shared".to_owned(),
            "live_original".to_owned(),
            movie_path.clone(),
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie)),
            movie.len() as i64,
            1,
        )]);
        assert_eq!(
            actual_destinations, expected_destinations,
            "exact pending destinations on cycle {cycle}"
        );
        let retry_rows: Vec<String> = conn.prepare(
            "SELECT json_array(g.id,g.account_key,g.provider_key,g.scope,d.pass_key,d.child,d.admission,d.reason,d.attempts,d.next_retry_at) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-shared' AND EXISTS (SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND w.verified_media=0) ORDER BY g.id,d.pass_key"
        ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        // Diagnostic only: preserve the count assertion below. These rows and
        // provider requests contain synthetic identities and localhost URLs.
        let diagnostic_rows: Vec<String> = conn.prepare(
            "SELECT json_array(g.id,g.account_key,g.provider_key,g.scope,g.config_hash,g.basis,g.metadata_enabled,CAST(g.specification AS TEXT),g.specification_hash,g.sealed,g.checkpoint_ready,g.checkpoint_veto,g.seal_hash,g.seal_header_hash,g.created_at,g.replay_after,g.last_replayed_at,g.charged_bytes,d.pass_key,d.child,CAST(d.manifest AS TEXT),d.manifest_hash,d.outcome,d.admission,d.reason,d.admission_hash,d.attempts,d.next_retry_at,d.charged_bytes,d.compacted_hash,w.library,w.asset_id,w.master,w.version_size,w.path,w.compat_path,w.checksum,w.size_bytes,w.created_at,w.added_at,w.metadata_hash,w.admitted,w.verified_media,w.verified_metadata,w.local_checksum,w.source_checksum,w.grouping_hash,w.intent_hash,w.progress_hash,w.prepared_checksum,w.prepared_size,w.prepared_hash) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation LEFT JOIN provider_active_destinations w ON w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child WHERE d.child='asset-shared' ORDER BY g.id,d.pass_key,w.version_size,w.path"
        ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        let request_trace: Vec<Value> = {
            let requests = provider_requests.lock().unwrap();
            requests.iter().map(|(url, body)| json!({
                "endpoint": if url.contains("/changes/zone?") { "changes" }
                    else if url.contains("/records/lookup?") { "lookup" }
                    else if url.contains("/records/query?") { "query" }
                    else if url.contains("/zones/list") { "zones" }
                    else { "other" },
                "zone": body["zoneID"]["zoneName"].as_str()
                    .or_else(|| body["zones"][0]["zoneID"]["zoneName"].as_str()),
                "cursor": body["zones"][0]["syncToken"],
                "record_names": body["records"].as_array().map(|items|
                    items.iter().map(|item| item["recordName"].clone()).collect::<Vec<_>>()),
                "query_type": body["query"]["recordType"]
            })).collect()
        };
        if cycle == 1 {
            let changes = request_trace
                .iter()
                .filter(|request| {
                    request["endpoint"] == "changes"
                        && request["zone"] == SOURCE
                        && request["cursor"] == "current-0"
                })
                .count();
            assert_eq!(
                changes, 1,
                "the intended Shared changes page must be requested"
            );
        }
        eprintln!(
            "ISSUE845_DIAG_COLUMNS owner_destinations=[generation,account,provider,scope,config_hash,basis,metadata_enabled,specification,specification_hash,sealed,checkpoint_ready,checkpoint_veto,seal_hash,seal_header_hash,created_at,replay_after,last_replayed_at,generation_charged_bytes,pass,child,manifest,manifest_hash,outcome,admission,reason,admission_hash,attempts,next_retry_at,decision_charged_bytes,compacted_hash,library,asset_id,master,version,path,compat_path,checksum,size,created_at,added_at,metadata_hash,admitted,verified_media,verified_metadata,local_checksum,source_checksum,grouping_hash,intent_hash,progress_hash,prepared_checksum,prepared_size,prepared_hash]"
        );
        eprintln!(
            "ISSUE845_DIAG cycle={cycle} previous_retry={saved_retry:?} still_gets={} movie_gets={} retry_rows={retry_rows:?} owner_destinations={diagnostic_rows:?} provider_requests={request_trace:?}",
            still_gets.load(Ordering::SeqCst),
            movie_gets.load(Ordering::SeqCst)
        );
        // Compare immutable source identity, resource manifest (including the
        // synthetic URLs), destination and retry policy without equating byte
        // matches to cross-account ownership or requiring a particular count.
        let equivalent_rows: Vec<String> = conn.prepare(
            "SELECT json_array(g.id,g.account_key,g.provider_key,g.scope,g.config_hash,json_extract(CAST(g.specification AS TEXT),'$.profile.retry_policy'),json_extract(CAST(g.specification AS TEXT),'$.profile.admitted_retry_policy'),d.pass_key,d.child,d.manifest_hash,CAST(d.manifest AS TEXT),d.attempts,d.next_retry_at) FROM provider_active_decisions d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-shared' AND EXISTS (SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.admitted=1 AND w.verified_media=0) ORDER BY g.id,d.pass_key"
        ).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        if cycle == 0 {
            assert_eq!(equivalent_rows.len(), 1, "initial owned obligation");
            original_equivalence = Some(serde_json::from_str(&equivalent_rows[0]).unwrap());
        }
        if repeated_observation && cycle == 1 {
            let prior = saved_retry.as_ref().expect("cycle-zero retry");
            assert!(
                prior[9].as_i64().unwrap() > before_time,
                "old retry is not due"
            );
            let original = original_equivalence.as_ref().unwrap();
            let mut original_matches = 0;
            let mut observed = Vec::new();
            for encoded in &equivalent_rows {
                let row: Value = serde_json::from_str(encoded).unwrap();
                for index in 1..=10 {
                    assert_eq!(
                        row[index], original[index],
                        "source, policy and manifest field {index}"
                    );
                }
                assert_eq!(row[1], account);
                assert_eq!(row[2], provider);
                assert_eq!(row[7], expected_pass);
                assert_eq!(row[8], "asset-shared");
                assert_eq!(row[5].as_str().unwrap().len(), 64);
                assert_eq!(row[6].as_str().unwrap().len(), 64);
                let manifest: Value = serde_json::from_str(row[10].as_str().unwrap()).unwrap();
                let confirmation: Vec<u8> =
                    serde_json::from_value(manifest["decision"]["confirmation"].clone()).unwrap();
                let source: Value = serde_json::from_slice(&confirmation).unwrap();
                let master = source["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| {
                        record["recordName"] == "shared" && record["recordType"] == "CPLMaster"
                    })
                    .unwrap();
                let child = source["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|record| {
                        record["recordName"] == "asset-shared" && record["recordType"] == "CPLAsset"
                    })
                    .unwrap();
                assert_eq!(master["zoneID"], zone(SOURCE));
                assert_eq!(child["zoneID"], zone(SOURCE));
                assert_eq!(master["recordChangeTag"], "ct-master");
                assert_eq!(child["recordChangeTag"], "ct-asset");
                assert_eq!(
                    child["fields"]["masterRef"]["value"]["recordName"],
                    "shared"
                );
                assert_eq!(
                    child["fields"]["masterRef"]["value"]["zoneID"],
                    zone(SOURCE)
                );
                assert_eq!(
                    master["fields"]["resOriginalRes"]["value"]["downloadURL"],
                    format!("{}/still", server.uri())
                );
                assert_eq!(
                    master["fields"]["resOriginalRes"]["value"]["fileChecksum"],
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still))
                );
                assert_eq!(
                    master["fields"]["resOriginalRes"]["value"]["size"],
                    still.len()
                );
                assert_eq!(
                    master["fields"]["resOriginalVidComplRes"]["value"]["downloadURL"],
                    format!("{}/expired-movie", server.uri())
                );
                assert_eq!(
                    master["fields"]["resOriginalVidComplRes"]["value"]["fileChecksum"],
                    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(movie))
                );
                assert_eq!(
                    master["fields"]["resOriginalVidComplRes"]["value"]["size"],
                    movie.len()
                );
                if row[0] == original[0] {
                    original_matches += 1;
                    assert_eq!(row[11], original[11], "older attempt must remain unchanged");
                    assert_eq!(
                        row[12], original[12],
                        "older deadline must remain unchanged"
                    );
                }
                observed.push(json!({"generation":row[0], "attempts":row[11], "deadline":row[12]}));
            }
            assert_eq!(original_matches, 1, "preserve the original generation");
            assert!(
                retry_rows
                    .iter()
                    .any(|encoded| serde_json::from_str::<Value>(encoded).unwrap() == *prior),
                "the exact original retry owner remains retained"
            );
            let repeated_gets = movie_gets.load(Ordering::SeqCst) - before_requests;
            assert!(
                chrono::Utc::now().timestamp() < prior[9].as_i64().unwrap(),
                "the repeated-observation cycle must finish before the old deadline"
            );
            assert!(repeated_gets <= 2, "bounded GETs for repeated observation");
            assert!(
                request_trace
                    .iter()
                    .any(|request| request["endpoint"] == "changes"
                        && request["zone"] == SOURCE
                        && request["cursor"] == "current-0"),
                "the repeated observation must reach the intended Shared cursor"
            );
            eprintln!(
                "ISSUE845_REPEAT_DIAG cycle=1 source_equivalent=true old_retry_future=true cycle_movie_410={repeated_gets} new_transfer_before_old_deadline={} retry_owners={observed:?} get_generation_attribution=unavailable",
                repeated_gets > 0
            );
            drop(conn);
            continue;
        }
        assert_eq!(
            retry_rows.len(),
            1,
            "exact retained retry owner on cycle {cycle}"
        );
        let retry: Value = serde_json::from_str(&retry_rows[0]).unwrap();
        assert!(!retry[0].as_str().unwrap().is_empty());
        assert_eq!(retry[1], account);
        assert_eq!(retry[2], provider);
        let retry_scope: Value = serde_json::from_str(retry[3].as_str().unwrap()).unwrap();
        assert_eq!(
            retry_scope,
            json!({
                "container":"com.apple.photos.cloud","database":"shared",
                "environment":"production","format":1,"realm":"com","zone":zone(SOURCE)
            })
        );
        assert_eq!(retry[4], expected_pass);
        assert_eq!(retry[5], "asset-shared");
        assert_eq!(retry[6], "admitted");
        assert_eq!(retry[7], "download_failed");
        let expected_attempts = if cycle == 2 { 2 } else { 1 };
        assert_eq!(retry[8], expected_attempts);
        let deadline = retry[9].as_i64().unwrap();
        let after_time = chrono::Utc::now().timestamp();
        if cycle == 1 {
            assert!(
                deadline > after_time,
                "quiet cycle must remain under backoff"
            );
            assert_eq!(
                Some(&retry),
                saved_retry.as_ref(),
                "quiet reopen preserves owner and backoff exactly"
            );
        } else {
            let delay = if cycle == 2 { 7200 } else { 3600 };
            assert!(
                deadline >= before_time + delay && deadline <= after_time + delay,
                "deadline must be set by this attempt: {retry}"
            );
            if let Some(previous) = &saved_retry {
                for index in 0..8 {
                    assert_eq!(
                        retry[index], previous[index],
                        "retry identity field {index}"
                    );
                }
            }
        }
        saved_retry = Some(retry);
        drop(conn);
        let requests = movie_gets.load(Ordering::SeqCst) - before_requests;
        let expected_requests = match cycle {
            0 => 4,
            1 => 0,
            _ => 2,
        };
        assert_eq!(
            requests, expected_requests,
            "per-cycle 410 requests on cycle {cycle}"
        );
        eprintln!(
            "ISSUE845_REACHED cycle={cycle} cycle_still_gets={still_requests} cycle_movie_410={requests} pending_paths=1 still_receipt=1 movie_receipts=0 retry_attempts={expected_attempts} retry_generation={} retry_deadline={deadline} pass={expected_pass} exact_tree=true",
            saved_retry.as_ref().unwrap()[0]
        );
    }
    server.verify().await;
}
