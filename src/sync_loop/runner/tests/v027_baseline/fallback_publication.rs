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

async fn run_cross_zone_case(
    incremental_addition: bool,
    folder_change: bool,
    refusal: Option<&'static str>,
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let server = crate::start_wiremock_or_skip!();
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
        current_cycle.store(cycle, Ordering::SeqCst);
        let db = Arc::new(
            SqliteStateDb::open_owned(&database, &baseline_owner())
                .await
                .unwrap(),
        );
        let base_session = CrossZoneSession {
            cycle,
            records: Arc::new(records.clone()),
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
        .unwrap_or_else(|error| {
            panic!("cycle {cycle}: {error}");
        });
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
