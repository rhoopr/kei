//! Independent byte/path/database/checkpoint oracles through the normal cycle.
use crate::commands::{AlbumPass, PassKind};
use crate::icloud::photos::PhotosSession;
use crate::sync_loop::test_support::{
    full_album_page_with_download, make_full_album_with_boxed_session, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle,
};
use crate::types::EditedNaming;
use crate::{download, state};
use base64::Engine as _;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORIGINAL: &[u8] = b"\xff\xd8\xff\xe0original immutable bytes";
const EDIT_ONE: &[u8] = b"\xff\xd8\xff\xe0first adjusted independent bytes";
const EDIT_TWO: &[u8] = b"\xff\xd8\xff\xe0second adjusted independent bytes";
mod indexing;

#[derive(Clone)]
struct IndexingOptions {
    readiness: Arc<std::sync::Mutex<&'static str>>,
    regress: bool,
    cancel: CancellationToken,
    interrupt: bool,
    stale_plan: bool,
    recent: Option<u32>,
    retry_only: bool,
    token: Option<&'static str>,
    queries: Arc<AtomicUsize>,
    cursors: Arc<std::sync::Mutex<Vec<String>>>,
}
fn provider_hash(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}
fn records(server: &MockServer, edit: Option<(&str, &[u8])>) -> Vec<Value> {
    records_at(&server.uri(), edit)
}
fn records_at(uri: &str, edit: Option<(&str, &[u8])>) -> Vec<Value> {
    let mut page = full_album_page_with_download(
        "PrimarySync",
        "501",
        "selected-token",
        &format!("{uri}/original"),
        ORIGINAL.len() as u64,
        &provider_hash(ORIGINAL),
    );
    page["records"][0]["fields"]["filenameEnc"]["value"] = json!("IMG_0501.JPG");
    if let Some((name, bytes)) = edit {
        page["records"][1]["fields"]["resJPEGFullRes"] = json!({"value":{"size":bytes.len(),"downloadURL":format!("{uri}/{name}"),"fileChecksum":provider_hash(bytes)}});
        page["records"][1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
        page["records"][1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    }
    page["records"].as_array().unwrap().clone()
}
type RequestTrace = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

#[derive(Clone)]
struct Session {
    reported_count: Option<usize>,
    records: Vec<Value>,
    quiet: bool,
    lookups: Arc<AtomicUsize>,
    current: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
    lookup_change: Option<(usize, Vec<Value>)>,
    indexing: Option<IndexingOptions>,
    baseline_successor: Option<&'static str>,
    cycle_token: Option<&'static str>,
    trace: Option<RequestTrace>,
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
        if url.ends_with("/zones/list") {
            assert_eq!(request, json!({}));
            return Ok(crate::sync_loop::test_support::baseline_zone_response(
                "PrimarySync",
            ));
        }
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"fields":{"state":{"value":*self.indexing.as_ref().unwrap().readiness.lock().unwrap()}}}]}),
            );
        }
        if url.contains("/changes/zone?") {
            if let Some(trace) = &self.trace {
                trace
                    .lock()
                    .unwrap()
                    .push(("changes".into(), request.clone()));
            }
            if let Some(indexing) = &self.indexing {
                indexing.cursors.lock().unwrap().push(
                    request["zones"][0]["syncToken"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":if request["zones"][0]["syncToken"] == "fixture-zone-history-boundary" {self.baseline_successor.unwrap_or(self.cycle_token.unwrap_or("cycle-token"))} else {self.cycle_token.unwrap_or("cycle-token")},"moreComing":false,"records":if self.quiet || request["zones"][0]["syncToken"] == "fixture-zone-history-boundary" {vec![]} else {self.records.clone()}}]}),
            );
        }
        if url.contains("/records/lookup?") {
            if let Some(trace) = &self.trace {
                trace
                    .lock()
                    .unwrap()
                    .push(("lookup".into(), request.clone()));
            }
            let lookup = self.lookups.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_str(&body)?;
            let current = self
                .lookup_change
                .as_ref()
                .filter(|(after, _)| lookup >= *after)
                .map(|(_, records)| records.clone())
                .unwrap_or_else(|| {
                    self.current
                        .as_ref()
                        .map(|current| current.lock().unwrap().clone())
                        .unwrap_or_else(|| self.records.clone())
                });
            let records: Vec<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|wanted| {
                    current
                        .iter()
                        .find(|record| record["recordName"] == wanted["recordName"])
                        .cloned()
                })
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/records/query/batch?") || url.contains("/internal/records/query/batch") {
            let body: Value = serde_json::from_str(&body)?;
            return Ok(
                json!({"batch":body["batch"].as_array().unwrap().iter().map(|_|json!({"records":[{"fields":{"itemCount":{"value":self.reported_count.unwrap_or_else(|| if self.indexing.is_some() {self.records.len()/2} else {1})}}}]})).collect::<Vec<_>>()}),
            );
        }
        if url.contains("/records/query?") {
            if let Some(indexing) = &self.indexing {
                indexing.queries.fetch_add(1, Ordering::SeqCst);
                if indexing.regress {
                    *indexing.readiness.lock().unwrap() = "RUNNING";
                }
                if indexing.interrupt {
                    indexing.cancel.cancel();
                }
            }
            let body: Value = serde_json::from_str(&body)?;
            let offset = body["query"]["filterBy"]
                .as_array()
                .and_then(|items| items.iter().find(|i| i["fieldName"] == "startRank"))
                .and_then(|i| i["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset==0 {self.records.clone()} else {vec![]},"syncToken":self.indexing.as_ref().map_or(Some(if self.baseline_successor.is_some() {"fixture-rank-token"} else {"query-token"}), |indexing| indexing.token)}),
            );
        }
        anyhow::bail!("unexpected synthetic provider endpoint: {url}")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
async fn cycle(
    root: &std::path::Path,
    records: Vec<Value>,
    quiet: bool,
    naming: EditedNaming,
) -> (crate::sync_cycle::CycleResult, usize) {
    cycle_resolution(
        root,
        records,
        quiet,
        naming,
        crate::types::PhotoResolution::Original,
    )
    .await
}
async fn cycle_resolution(
    root: &std::path::Path,
    records: Vec<Value>,
    quiet: bool,
    naming: EditedNaming,
    resolution: crate::types::PhotoResolution,
) -> (crate::sync_cycle::CycleResult, usize) {
    cycle_options(
        root,
        records,
        quiet,
        naming,
        CycleOptions {
            resolution,
            ..CycleOptions::default()
        },
    )
    .await
}
struct CycleOptions {
    retry: Option<crate::retry::RetryConfig>,
    reported_count: Option<usize>,
    indexing: Option<IndexingOptions>,
    baseline_successor: Option<&'static str>,
    cycle_token: Option<&'static str>,
    trace: Option<RequestTrace>,
    refresh_metadata: bool,
    embed_rating: bool,
    media_directory: &'static str,
    state_file: &'static str,
    folder_structure: &'static str,
    pass_kind: PassKind,
    resolution: crate::types::PhotoResolution,
    private: bool,
    disable_capture: bool,
    other_capture_db: bool,
    #[cfg(feature = "xmp")]
    sidecar: bool,
    controls: download::DownloadControls,
    mov_policy: crate::types::LivePhotoMovFilenamePolicy,
    live_mode: crate::types::LivePhotoMode,
    current: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
    lookup_change: Option<(usize, Vec<Value>)>,
}
impl Default for CycleOptions {
    fn default() -> Self {
        Self {
            retry: None,
            reported_count: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
            refresh_metadata: false,
            embed_rating: false,
            media_directory: "media",
            state_file: "state.db",
            folder_structure: "",
            pass_kind: PassKind::Unfiled,
            resolution: crate::types::PhotoResolution::Original,
            private: false,
            disable_capture: false,
            other_capture_db: false,
            #[cfg(feature = "xmp")]
            sidecar: false,
            controls: download::DownloadControls::download_hidden(),
            mov_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            live_mode: crate::types::LivePhotoMode::Both,
            current: None,
            lookup_change: None,
        }
    }
}
async fn cycle_options(
    root: &std::path::Path,
    records: Vec<Value>,
    quiet: bool,
    naming: EditedNaming,
    options: CycleOptions,
) -> (crate::sync_cycle::CycleResult, usize) {
    try_cycle_options(root, records, quiet, naming, options)
        .await
        .unwrap()
}

async fn try_cycle_options(
    root: &std::path::Path,
    records: Vec<Value>,
    quiet: bool,
    naming: EditedNaming,
    options: CycleOptions,
) -> anyhow::Result<(crate::sync_cycle::CycleResult, usize)> {
    let resolution = options.resolution;
    let lookups = Arc::new(AtomicUsize::new(0));
    let provider = Session {
        reported_count: options.reported_count,
        records,
        quiet,
        lookups: lookups.clone(),
        current: options.current.clone(),
        lookup_change: options.lookup_change.clone(),
        indexing: options.indexing.clone(),
        baseline_successor: options.baseline_successor,
        cycle_token: options.cycle_token,
        trace: options.trace.clone(),
    };
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let inner = Arc::new(if options.private {
        state::SqliteStateDb::open_owned(&root.join(options.state_file), &owner)
            .await
            .unwrap()
    } else {
        state::SqliteStateDb::open(&root.join(options.state_file))
            .await
            .unwrap()
    });
    let mut album = make_full_album_with_boxed_session("PrimarySync", Box::new(provider.clone()));
    if options.private || options.pass_kind == PassKind::Album {
        album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: (options.pass_kind == PassKind::Album)
                    .then(|| Arc::from("album-501")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider.clone()),
        );
        if options.private && !options.disable_capture {
            let capture_db = if options.other_capture_db {
                Arc::new(
                    state::SqliteStateDb::open_owned(&root.join("other-account.db"), &owner)
                        .await
                        .unwrap(),
                )
            } else {
                inner.clone()
            };
            album.set_shadow_capture(
                crate::icloud::photos::inbox::ShadowCapture::new(capture_db, owner, "com"),
                Arc::from("private"),
            );
        }
    }
    let pass = AlbumPass {
        kind: options.pass_kind,
        album,
        exclude_ids: Arc::default(),
    };
    let mut library = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        "sync_token:PrimarySync",
        vec![pass],
    );
    library.library = if let Some(indexing) = &options.indexing {
        library.plan_is_stale = indexing.stale_plan;
        crate::icloud::photos::PhotoLibrary::new(
            "https://example.com".into(),
            Arc::default(),
            Box::new(provider),
            Arc::new(json!({"zoneName":"PrimarySync"})),
            "private".into(),
            crate::retry::RetryConfig::default(),
        )
        .await?
    } else {
        crate::icloud::photos::PhotoLibrary::new_stub_with_zone(Box::new(provider), "PrimarySync")
    };
    let db: Arc<dyn download::DownloadStore> = inner;
    let media = root.join(options.media_directory);
    // run_loop creates the configured root before entering run_cycle. Reproduce
    // that caller contract when a fixture changes roots and reuses local bytes.
    std::fs::create_dir_all(&media).unwrap();
    let base = make_run_cycle_download_config_builder(&media, db.clone());
    let build = |mode, exclude, groups, zone| {
        let mut config = base(mode, exclude, groups, zone);
        let config = Arc::make_mut(&mut config);
        if let Some(retry) = options.retry {
            config.retry = retry;
        }
        config.refresh_metadata = options.refresh_metadata;
        config.metadata.set_exif_rating = options.embed_rating;
        config.edited = true;
        config.edited_naming = naming;
        config.recent = options
            .indexing
            .as_ref()
            .and_then(|indexing| indexing.recent);
        config.retry_only = options
            .indexing
            .as_ref()
            .is_some_and(|indexing| indexing.retry_only);
        config.resolution = resolution;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = options.sidecar;
        }
        config.live_photo_mov_filename_policy = options.mov_policy;
        config.live_photo_mode = options.live_mode;
        config.folder_structure = options.folder_structure.to_owned();
        config.folder_structure_albums = Arc::from(options.folder_structure);
        config.folder_structure_smart_folders = Arc::from(options.folder_structure);
        Arc::new(config.clone())
    };
    let mut config = make_run_cycle_config();
    config.download.directory = media.clone();
    config.download.folder_structure = options.folder_structure.to_owned();
    config.download.folder_structure_albums = options.folder_structure.to_owned();
    config.download.folder_structure_smart_folders = options.folder_structure.to_owned();
    config.runtime.refresh_metadata = options.refresh_metadata;
    config.metadata.set_exif_rating = options.embed_rating;
    config.photos.edited = true;
    config.photos.edited_naming = naming;
    config.photos.resolution = resolution;
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = options.sidecar;
    }
    config.photos.live_photo_mov_filename_policy = options.mov_policy;
    config.photos.live_photo_mode = options.live_mode;
    let (_session_root, session) = make_shared_session_for_run_cycle().await;
    let shutdown = options
        .indexing
        .as_ref()
        .map_or_else(CancellationToken::new, |indexing| indexing.cancel.clone());
    let result = Box::pin(crate::sync_cycle::run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        options.controls,
        &session,
        &shutdown,
    ))
    .await?;
    drop(base);
    drop(db);
    Ok((result, lookups.load(Ordering::SeqCst)))
}
fn assert_bytes(path: &std::path::Path, bytes: &[u8]) {
    assert_eq!(std::fs::read(path).unwrap(), bytes, "{}", path.display());
}
fn db_count(root: &std::path::Path, sql: &str) -> i64 {
    rusqlite::Connection::open(root.join("state.db"))
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}
async fn server() -> MockServer {
    let server = MockServer::start().await;
    for (name, bytes) in [
        ("original", ORIGINAL),
        ("edit-one", EDIT_ONE),
        ("edit-two", EDIT_TWO),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
    }
    server
}
/// Recreate the saved released writer's prepared edit transition using the
/// current planner, then restore the earlier synthetic catalogue observation.
async fn portable_prepolicy_fixture(
    root: &std::path::Path,
    server: &MockServer,
    publishing: bool,
) -> String {
    let unedited = records(server, None);
    let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (initial, _) = cycle_options(
        root,
        unedited,
        false,
        EditedNaming::Primary,
        CycleOptions {
            baseline_successor: Some("query-token"),
            trace: Some(trace.clone()),
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{initial:?}");
    assert_eq!(initial.stats.downloaded, 1);
    assert_eq!(initial.stats.state_write_failures, 0);
    assert!(initial.db_sync_token_advance_safe);
    {
        let requests = trace.lock().unwrap();
        let changes: Vec<_> = requests
            .iter()
            .filter(|(kind, _)| kind == "changes")
            .collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].1["zones"][0]["syncToken"],
            "fixture-zone-history-boundary"
        );
        assert_eq!(
            changes[0].1["zones"][0]["zoneID"]["zoneName"],
            "PrimarySync"
        );
    }
    let media = root.join("media");
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    state::db::account::adopt_legacy(
        &root.join("state.db"),
        &root.join("adopted.db"),
        &owner,
        "synthetic@example.invalid",
        "com",
    )
    .await
    .unwrap();
    {
        let conn = rusqlite::Connection::open(root.join("adopted.db")).unwrap();
        // The released source schema had no preserved_path_key column. Model
        // its migration as NULL in this test-only copy; do not alter live rows.
        conn.execute_batch("CREATE TABLE synthetic_prepolicy_assets AS SELECT * FROM assets WHERE library='PrimarySync' AND id='asset-501'; CREATE TABLE synthetic_prepolicy_paths AS SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501'; UPDATE synthetic_prepolicy_paths SET preserved_path_key=NULL;").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM synthetic_prepolicy_assets",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("adopted.db"), &owner)
            .await
            .unwrap(),
    );
    let source = records(server, Some(("edit-one", EDIT_ONE)));
    let provider = Session {
        reported_count: None,
        records: source,
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
        Arc::from("private"),
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    };
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let build = make_run_cycle_download_config_builder(&media, store);
    let mut config = build(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    let mut op = download::seed_prepared_edited_compat(&config, &pass).await;
    if publishing {
        download::seed_partly_published_compat(&config, &mut op).await;
    }
    assert_eq!(op.phase, if publishing { "publishing" } else { "prepared" });
    assert!(
        op.members
            .iter()
            .filter(|member| member.prepared.is_some())
            .count()
            >= 2
    );
    drop(config);
    drop(build);
    drop(pass);
    drop(db);
    {
        let conn = rusqlite::Connection::open(root.join("adopted.db")).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE; DELETE FROM assets WHERE library='PrimarySync' AND id='asset-501'; INSERT INTO assets SELECT * FROM synthetic_prepolicy_assets; DELETE FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501'; INSERT INTO asset_metadata_paths SELECT * FROM synthetic_prepolicy_paths; DROP TABLE synthetic_prepolicy_assets; DROP TABLE synthetic_prepolicy_paths; COMMIT;").unwrap();
    }
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = state::SqliteStateDb::open_owned(&root.join("adopted.db"), &owner)
        .await
        .unwrap();
    // The synthetic bridge committed the released fixture's cursor through
    // the current checkpoint owner; do not replace durable cursor state here.
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("query-token")
    );
    let pending = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    let op = &pending[0];
    assert_eq!(op.phase, if publishing { "publishing" } else { "prepared" });
    if publishing {
        assert!(op.members.iter().any(|member| {
            member.installed.is_some()
                && member
                    .prepared
                    .as_ref()
                    .is_some_and(|file| !file.path.to_path().exists())
        }));
    }
    let original = op
        .members
        .iter()
        .find(|member| {
            member
                .record
                .as_ref()
                .is_some_and(|record| record.version_size == state::VersionSizeKey::Original)
        })
        .unwrap();
    let adjusted = op
        .members
        .iter()
        .find(|member| {
            member
                .record
                .as_ref()
                .is_some_and(|record| record.version_size == state::VersionSizeKey::Adjusted)
        })
        .unwrap();
    assert_eq!(
        original.record.as_ref().unwrap().filename.as_ref(),
        "IMG_0501_original.JPG"
    );
    assert_eq!(
        adjusted.record.as_ref().unwrap().filename.as_ref(),
        "IMG_0501.JPG"
    );
    assert!(adjusted.old.is_some() && original.prepared.is_some() && adjusted.prepared.is_some());
    if publishing {
        let installed = original
            .installed
            .as_ref()
            .expect("original archive installed");
        assert_eq!(original.destination.as_ref(), Some(&installed.path));
        assert!(!original.prepared.as_ref().unwrap().path.to_path().exists());
        assert_bytes(&installed.path.to_path(), ORIGINAL);
        assert!(adjusted.installed.is_none());
        assert_bytes(
            &adjusted.prepared.as_ref().unwrap().path.to_path(),
            EDIT_ONE,
        );
    }
    {
        let conn = rusqlite::Connection::open(root.join("adopted.db")).unwrap();
        let row: (String, String, String, String) = conn.query_row("SELECT filename,checksum,local_path,local_checksum FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='original'", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap();
        let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL));
        assert_eq!(
            row,
            (
                "IMG_0501.JPG".into(),
                provider_hash(ORIGINAL),
                root.join("media/IMG_0501.JPG").to_str().unwrap().into(),
                checksum.clone()
            )
        );
        for table in [
            "provider_selection_generations",
            "provider_active_generations",
            "provider_active_decisions",
            "provider_active_destinations",
            "download_receipts",
        ] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(
                count, 0,
                "unexpected selected work in release-shaped snapshot: {table}"
            );
        }
        let receipt: (String, String) = conn.query_row("SELECT provider_checksum,local_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='original' AND local_path=?1", [root.join("media/IMG_0501.JPG").to_str().unwrap()], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
        assert_eq!(receipt, (provider_hash(ORIGINAL), checksum));
    }
    let error = db
        .save_primary_layout(op.clone())
        .await
        .expect_err("released-shaped observation must hit the strict fence");
    assert!(
        error
            .to_string()
            .contains("recorded layout differs from current provider observation"),
        "{error}"
    );
    op.operation.clone()
}

async fn assert_portable_prepolicy_completed(root: &std::path::Path, operation: &str) {
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = state::SqliteStateDb::open_owned(&root.join("adopted.db"), &owner)
        .await
        .unwrap();
    assert!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("cycle-token")
    );
    drop(db);
    let conn = rusqlite::Connection::open(root.join("adopted.db")).unwrap();
    let phase: String = conn
        .query_row(
            "SELECT phase FROM primary_layout_operations WHERE operation=?1",
            [operation],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase, "committed");
    let receipts: Vec<(Vec<u8>, String)> = conn
        .prepare(
            "SELECT native_path,media_checksum FROM primary_layout_preserved WHERE operation=?1",
        )
        .unwrap()
        .query_map([operation], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let original_checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL));
    assert!(receipts.iter().any(|(receipt, checksum)| {
        let path = serde_json::from_slice::<state::db::provider_selection::SelectionPath>(receipt)
            .unwrap()
            .to_path();
        checksum == &original_checksum && std::fs::read(path).is_ok_and(|bytes| bytes == ORIGINAL)
    }));
    assert_bytes(&root.join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.join("media/IMG_0501_original.JPG"), ORIGINAL);
}

#[tokio::test]
async fn portable_prepolicy_reconfirmation_preserves_edit_and_quiet_reopen() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let operation = portable_prepolicy_fixture(root.path(), &server, false).await;
    {
        let conn = rusqlite::Connection::open(root.path().join("adopted.db")).unwrap();
        let markers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM asset_metadata_paths WHERE preserved_path_key IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let legacy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM unattributed_legacy_paths",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (markers, legacy),
            (0, 0),
            "synthetic fixture must not invent read-only ownership"
        );
    }
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
    let options = || CycleOptions {
        private: true,
        state_file: "adopted.db",
        trace: Some(trace.clone()),
        ..CycleOptions::default()
    };
    let (result, lookups) = try_cycle_options(
        root.path(),
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert!(lookups >= 2);
    {
        let requests = trace.lock().unwrap();
        let changes: Vec<_> = requests
            .iter()
            .filter(|(kind, _)| kind == "changes")
            .collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].1["zones"][0]["syncToken"],
            "fixture-zone-history-boundary"
        );
        assert_eq!(
            changes[0].1["zones"][0]["zoneID"]["zoneName"],
            "PrimarySync"
        );
        let lookups: Vec<_> = requests
            .iter()
            .filter(|(kind, _)| kind == "lookup")
            .collect();
        assert!(lookups.iter().all(|(_, request)| request["zoneID"]
            == json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})));
        assert!(lookups.iter().any(|(_, request)| request["records"]
            == json!([{"recordName":"asset-501"},{"recordName":"501"}])));
    }
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
    let (quiet, lookups) =
        try_cycle_options(root.path(), edited, true, EditedNaming::Primary, options())
            .await
            .unwrap();
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(lookups, 0);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
    assert_portable_prepolicy_completed(root.path(), &operation).await;
}

#[tokio::test]
async fn portable_prepolicy_reconfirmation_reopens_consumed_publishing_stage() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let operation = portable_prepolicy_fixture(root.path(), &server, true).await;
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = || CycleOptions {
        private: true,
        state_file: "adopted.db",
        ..CycleOptions::default()
    };
    let (result, lookups) = try_cycle_options(
        root.path(),
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert!(lookups >= 2);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
    let (quiet, lookups) =
        try_cycle_options(root.path(), edited, true, EditedNaming::Primary, options())
            .await
            .unwrap();
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(lookups, 0);
    assert_portable_prepolicy_completed(root.path(), &operation).await;
}

#[tokio::test]
async fn portable_prepolicy_reconfirmation_accepts_equivalent_rendered_name() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let operation = portable_prepolicy_fixture(root.path(), &server, false).await;
    let mut edited = records(&server, Some(("edit-one", EDIT_ONE)));
    // The fresh provider spelling differs, but slash cleaning renders the
    // same native filenames as the recorded underscore spelling.
    edited[0]["fields"]["filenameEnc"]["value"] = json!("IMG/0501.JPG");
    let options = || CycleOptions {
        private: true,
        state_file: "adopted.db",
        ..CycleOptions::default()
    };
    let (result, lookups) = try_cycle_options(
        root.path(),
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert!(lookups >= 2);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
    let (quiet, lookups) =
        try_cycle_options(root.path(), edited, true, EditedNaming::Primary, options())
            .await
            .unwrap();
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(lookups, 0);
    assert_portable_prepolicy_completed(root.path(), &operation).await;
}

#[tokio::test]
async fn portable_prepolicy_recovery_gate_rejects_root_and_store_drift() {
    for variant in ["root", "wrong-store"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        portable_prepolicy_fixture(root.path(), &server, false).await;
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&root.path().join("adopted.db"), &owner)
                .await
                .unwrap(),
        );
        let capture_db = if variant == "wrong-store" {
            Arc::new(
                state::SqliteStateDb::open_owned(&root.path().join("other.db"), &owner)
                    .await
                    .unwrap(),
            )
        } else {
            db.clone()
        };
        let lookups = Arc::new(AtomicUsize::new(0));
        let provider = Session {
            reported_count: None,
            records: records(&server, Some(("edit-one", EDIT_ONE))),
            quiet: true,
            lookups: lookups.clone(),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(capture_db, owner, "com"),
            Arc::from("private"),
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        let media = root.path().join(if variant == "root" {
            "other-media"
        } else {
            "media"
        });
        std::fs::create_dir_all(&media).unwrap();
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store);
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        let error = download::probe_recorded_recovery_gate(&config, &pass)
            .await
            .expect_err("changed authority must be held");
        let message = format!("{error:#}");
        let reason = if variant == "root" {
            "Recorded recovery options or source changed"
        } else {
            "Recorded recovery account context changed"
        };
        assert!(message.contains(reason), "{variant}: {message}");
        assert_eq!(lookups.load(Ordering::SeqCst), 0);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("query-token")
        );
        assert_eq!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .len(),
            1
        );
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    }
}

#[tokio::test]
async fn portable_prepolicy_reconfirmation_retains_work_on_drift() {
    for variant in [
        "added",
        "resource",
        "metadata",
        "path",
        "missing",
        "scope",
        "unowned",
        "wrong-store",
        "master",
        "root",
        "options",
        "sidecar",
        "local",
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        portable_prepolicy_fixture(root.path(), &server, false).await;
        let mut source = records(&server, Some(("edit-one", EDIT_ONE)));
        let mut options = CycleOptions {
            private: true,
            state_file: "adopted.db",
            ..CycleOptions::default()
        };
        let original = root.path().join("media/IMG_0501.JPG");
        let expected = if variant == "local" {
            std::fs::write(&original, b"local edit retained").unwrap();
            b"local edit retained".as_slice()
        } else {
            ORIGINAL
        };
        match variant {
            "added" => {
                source[1]["fields"]["addedDate"] =
                    json!({"value":1_700_100_000_000_i64,"type":"TIMESTAMP"})
            }
            "resource" => {
                source[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                    json!(provider_hash(EDIT_TWO))
            }
            "metadata" => source[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"}),
            "path" => source[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG"),
            "missing" => {
                source[1]["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("masterRef");
            }
            "scope" => options.pass_kind = PassKind::Album,
            "unowned" => options.disable_capture = true,
            "wrong-store" => options.other_capture_db = true,
            "master" => {
                source[0]["recordName"] = json!("other-master");
                source[1]["fields"]["masterRef"]["value"]["recordName"] = json!("other-master");
            }
            "root" => options.media_directory = "other-media",
            "options" => options.embed_rating = true,
            "sidecar" => std::fs::write(
                root.path().join("media/IMG_0501.JPG.xmp"),
                b"foreign sidecar retained",
            )
            .unwrap(),
            "local" => {}
            other => panic!("unexpected synthetic variant: {other}"),
        }
        let result =
            try_cycle_options(root.path(), source, true, EditedNaming::Primary, options).await;
        if matches!(variant, "wrong-store" | "root") {
            match result {
                Ok((cycle, lookups)) => {
                    assert!(
                        cycle.failed_count > 0 && !cycle.db_sync_token_advance_safe,
                        "{variant}: {cycle:?}"
                    );
                    assert_eq!(lookups, 0, "{variant}: must block before provider lookup");
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let expected = if variant == "root" {
                        "Recorded recovery options or source changed"
                    } else {
                        "Recorded recovery account context changed"
                    };
                    assert!(message.contains(expected), "{variant}: {message}");
                }
            }
        } else {
            let error = result
                .err()
                .unwrap_or_else(|| panic!("{variant}: expected an error"));
            let message = format!("{error:#}");
            let reason = match variant {
                "added" => "Recorded recovery provider metadata changed",
                "resource" | "metadata" | "path" | "master" => {
                    "Recorded recovery provider facts changed"
                }
                "missing" => "Unresolved current master identity",
                "scope" => "Recorded recovery has no matching current pass",
                "unowned" => "Recorded recovery lacks authenticated scope",
                "root" | "options" => "Recorded recovery options or source changed",
                "sidecar" => "Owned layout sidecar changed locally",
                "local" => "Owned layout file changed locally",
                other => panic!("unexpected synthetic variant: {other}"),
            };
            assert!(message.contains(reason), "{variant}: {message}");
        }
        assert_bytes(&original, expected);
        if variant == "sidecar" {
            assert_bytes(
                &root.path().join("media/IMG_0501.JPG.xmp"),
                b"foreign sidecar retained",
            );
        }
        assert!(!root.path().join("media/IMG_0501_original.JPG").exists());
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = state::SqliteStateDb::open_owned(&root.path().join("adopted.db"), &owner)
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            None,
            "{variant}: adopted upgrade must invalidate the copied legacy cursor"
        );
        assert!(
            db.sync_epoch_id().await.unwrap().is_some(),
            "{variant}: adopted owner must carry one sync epoch"
        );
        assert!(
            db.sync_epoch_has_pending("PrimarySync".into())
                .await
                .unwrap(),
            "{variant}: enrollment must retain pending epoch work"
        );
        let record = {
            let conn = db.acquire_lock("inspect adopted epoch state").unwrap();
            state::db::sync_epoch::read_library(&conn, "PrimarySync")
                .unwrap()
                .expect("enrolled library")
        };
        assert_eq!(record.state, "pending", "{variant}: {record:?}");
        assert_eq!(record.reason, "existing_upgrade", "{variant}: {record:?}");
        assert!(record.binding.is_none(), "{variant}: {record:?}");
        let pending = db
            .primary_layout_operations("PrimarySync".into())
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "{variant}");
        assert_eq!(pending[0].phase, "prepared", "{variant}");
        for member in &pending[0].members {
            if let Some(prepared) = &member.prepared {
                let expected = match prepared.version.as_str() {
                    "original" => ORIGINAL,
                    "adjusted" => EDIT_ONE,
                    other => panic!("unexpected prepared version {other}"),
                };
                assert_bytes(&prepared.path.to_path(), expected);
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "synthetic released-writer fixture is generated separately"]
async fn released_prepolicy_reconfirmed_recovery_and_quiet_reopen() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_SYNTHETIC_COMPAT_ROOT").unwrap());
    assert_eq!(
        std::fs::read(root.join("fixture-owner")).unwrap(),
        b"kei-synthetic-process-death"
    );
    let publishing = std::env::var("KEI_SYNTHETIC_EXPECT_PUBLISHING").as_deref() == Ok("yes");
    assert_eq!(
        std::fs::read(root.join("ready")).unwrap(),
        if publishing {
            b"layout-member-published".as_slice()
        } else {
            b"layout-prepared".as_slice()
        }
    );
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    state::db::account::adopt_legacy(
        &root.join("state.db"),
        &root.join("adopted.db"),
        &owner,
        "synthetic@example.invalid",
        "com",
    )
    .await
    .unwrap();
    assert_bytes(&root.join("media/IMG_0501.JPG"), ORIGINAL);
    if publishing {
        let db = state::SqliteStateDb::open_owned(&root.join("adopted.db"), &owner)
            .await
            .unwrap();
        let pending = db
            .primary_layout_operations("PrimarySync".into())
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].phase, "publishing");
        assert!(
            pending[0]
                .members
                .iter()
                .any(|member| member.installed.is_some()
                    && member
                        .prepared
                        .as_ref()
                        .is_some_and(|file| !file.path.to_path().exists())),
            "an installed stage must already be consumed"
        );
    }
    let server = server().await;
    let records = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = || CycleOptions {
        private: true,
        state_file: "adopted.db",
        ..CycleOptions::default()
    };
    let (result, lookups) = try_cycle_options(
        &root,
        records.clone(),
        true,
        EditedNaming::Primary,
        options(),
    )
    .await
    .unwrap();
    assert_eq!(result.failed_count, 0, "{result:?}");
    assert!(lookups >= 2, "fresh child/master confirmation required");
    assert_bytes(&root.join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.join("media/IMG_0501_original.JPG"), ORIGINAL);
    let db = state::SqliteStateDb::open_owned(&root.join("adopted.db"), &owner)
        .await
        .unwrap();
    assert!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    drop(db);
    let (quiet, lookups) =
        try_cycle_options(&root, records, true, EditedNaming::Primary, options())
            .await
            .unwrap();
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(lookups, 0);
    assert_bytes(&root.join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.join("media/IMG_0501_original.JPG"), ORIGINAL);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "synthetic released-writer fixture is generated separately"]
async fn released_prepolicy_reconfirmation_rejects_drift() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_SYNTHETIC_COMPAT_ROOT").unwrap());
    let variant = std::env::var("KEI_SYNTHETIC_RECONFIRM_VARIANT").unwrap();
    assert_eq!(
        std::fs::read(root.join("fixture-owner")).unwrap(),
        b"kei-synthetic-process-death"
    );
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    state::db::account::adopt_legacy(
        &root.join("state.db"),
        &root.join("adopted.db"),
        &owner,
        "synthetic@example.invalid",
        "com",
    )
    .await
    .unwrap();
    let server = server().await;
    let mut source = records(&server, Some(("edit-one", EDIT_ONE)));
    let mut options = CycleOptions {
        private: true,
        state_file: "adopted.db",
        ..CycleOptions::default()
    };
    let expected = if variant == "local" {
        std::fs::write(root.join("media/IMG_0501.JPG"), b"local edit retained").unwrap();
        b"local edit retained".as_slice()
    } else {
        ORIGINAL
    };
    match variant.as_str() {
        "added" => {
            source[1]["fields"]["addedDate"] =
                json!({"value":1_700_100_000_000_i64,"type":"TIMESTAMP"})
        }
        "resource" => {
            source[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                json!(provider_hash(EDIT_TWO))
        }
        "metadata" => source[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"}),
        "path" => source[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG"),
        "missing" => source[1]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("masterRef")
            .map(|_| ())
            .unwrap(),
        "scope" => options.pass_kind = PassKind::Album,
        "local" => {}
        _ => panic!("unknown synthetic variant"),
    }
    let result = try_cycle_options(&root, source, true, EditedNaming::Primary, options).await;
    let error = result.expect_err("changed released operation must remain held");
    let message = format!("{error:#}");
    let expected_error = match variant.as_str() {
        "added" => "Recorded recovery provider metadata changed",
        "resource" | "metadata" | "path" => "Recorded recovery provider facts changed",
        "missing" => "Unresolved current master identity",
        "scope" => "Recorded recovery has no matching current pass",
        "local" => "Owned layout file changed locally",
        other => panic!("unexpected synthetic variant: {other}"),
    };
    assert!(message.contains(expected_error), "{variant}: {message}");
    assert_bytes(&root.join("media/IMG_0501.JPG"), expected);
    assert!(!root.join("media/IMG_0501_original.JPG").exists());
    let db = state::SqliteStateDb::open_owned(&root.join("adopted.db"), &owner)
        .await
        .unwrap();
    let pending = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "{variant}");
    assert_eq!(pending[0].phase, "prepared", "{variant}");
    for member in &pending[0].members {
        if let Some(prepared) = &member.prepared {
            let expected = match prepared.version.as_str() {
                "original" => ORIGINAL,
                "adjusted" => EDIT_ONE,
                other => panic!("unexpected prepared version {other}"),
            };
            assert_bytes(&prepared.path.to_path(), expected);
        }
    }
}

#[tokio::test]
async fn contract_file_publish_no_overwrite_primary_layout_edits_revert_and_quiet_reopen() {
    // Exercise recorded ownership and preservation independently of current naming.
    recorded_edited_family_recovery_scenario().await;
    use crate::state::db::DownloadStateStore;
    use crate::state::db::match_diagnostics::MatchDiagnostics;
    use crate::state::db::provider_selection::SelectionPath;
    use std::sync::atomic::AtomicBool;

    fn rows(path: &std::path::Path, sql: &str) -> Vec<Value> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect()
    }
    let file_snapshot = |path: &std::path::Path| {
        let handle = std::fs::File::open(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&handle).unwrap(),
            handle.metadata().unwrap().modified().unwrap(),
        )
    };
    // A genuine saved transfer obligation is held at its exact S, independently
    // of current selection and independently of healthy media progress.
    {
        fn signature(bytes: &[u8]) -> String {
            use sha1::{Digest as _, Sha1};
            let mut hash = Sha1::new();
            hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
            hash.update(bytes);
            base64::engine::general_purpose::STANDARD
                .encode([&[1][..], &hash.finalize()[..]].concat())
        }
        let server = server().await;
        let root = tempfile::tempdir().unwrap().keep();
        eprintln!("CONTRACT_DEBT_ROOT={}", root.display());
        let db_path = root.join("state.db");
        let media = root.join("media");
        let p = media.join("IMG_0501.JPG");
        let trace: RequestTrace = Arc::default();
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let options = |resolution, token| CycleOptions {
            private: true,
            resolution,
            cycle_token: Some(token),
            trace: Some(trace.clone()),
            retry: Some(crate::retry::RetryConfig {
                max_retries: 1,
                base_delay_secs: 0,
                max_delay_secs: 0,
            }),
            ..CycleOptions::default()
        };
        let page = |edit: Option<(&str, &[u8])>| {
            let mut page = records(&server, edit);
            page[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(signature(ORIGINAL));
            if let Some((_, bytes)) = edit {
                page[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                    json!(signature(bytes));
            }
            page
        };
        let receipt_sql = "SELECT json_object('id',id,'account',account_key,'provider',provider_key,'scope',scope,'library',library,'child',child,'master',master,'state',state_id,'pass',origin_pass,'version',version_size,'checksum',provider_checksum,'size',provider_size,'path',native_path,'source',source_checksum,'local',local_checksum,'proof',proof_hash,'trust',trust_receipt,'metadata',selected_metadata_hash,'generation',generation_id) FROM download_receipts WHERE library='PrimarySync' AND child='asset-501' AND version_size='original' ORDER BY id";
        let adjusted_sql =
            receipt_sql.replace("version_size='original'", "version_size='adjusted'");
        let intent_sql = "SELECT json_object('library',library,'child',id,'version',version_size,'requested',requested_path_key,'destination_key',destination_path_key,'destination',destination_path,'checksum',provider_checksum,'size',provider_size) FROM reconciliation_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' ORDER BY provider_checksum,provider_size";
        let debt_sql = "SELECT json_object('generation',g.id,'format',json_extract(CAST(g.specification AS TEXT),'$.format'),'account',g.account_key,'provider',g.provider_key,'scope',g.scope,'pass',d.pass_key,'library',d.library,'child',d.child,'state',d.asset_id,'master',d.master,'version',d.version_size,'path',d.path,'compat_path',d.compat_path,'checksum',d.checksum,'size',d.size_bytes,'admitted',d.admitted,'verified',d.verified_media,'sealed',g.sealed,'ready',g.checkpoint_ready) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.version_size='adjusted' ORDER BY g.id,d.pass_key,d.path";
        let decision_sql = "SELECT json_object('generation',generation,'pass',pass_key,'child',child,'manifest',hex(manifest),'manifest_hash',manifest_hash,'outcome',outcome,'admission',admission,'charged_bytes',charged_bytes,'compacted_hash',compacted_hash) FROM provider_active_decisions d WHERE child='asset-501' AND EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.version_size='adjusted') ORDER BY generation,pass_key";
        let schedule_sql = "SELECT json_object('generation',generation,'pass',pass_key,'attempts',attempts,'next_retry_at',next_retry_at,'reason',reason,'admission',admission,'compacted_hash',compacted_hash) FROM provider_active_decisions d WHERE child='asset-501' AND EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.version_size='adjusted') ORDER BY generation,pass_key";
        let error_sql = "SELECT json_quote(last_error) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'";
        let checkpoint_sql =
            "SELECT json_quote(value) FROM metadata WHERE key='sync_token:PrimarySync'";
        let original_path_sql = "SELECT json_array(library,id,version_size,local_path,provider_checksum,source_checksum,local_checksum,download_checksum) FROM asset_metadata_paths WHERE id='asset-501' AND version_size='original' ORDER BY local_path";
        let original_asset_sql = "SELECT json_array(library,id,version_size,status,checksum,size_bytes,local_path,local_checksum,download_checksum,is_deleted) FROM assets WHERE id='asset-501' AND version_size='original'";
        let temp_sql = "SELECT json_array(hex(path),claimed_at,publication) FROM owned_temp_files ORDER BY path";
        let cohort = || {
            let conn = rusqlite::Connection::open_with_flags(
                &db_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut stmt = conn.prepare("SELECT * FROM provider_active_destinations WHERE library='PrimarySync' AND child='asset-501' AND version_size='adjusted' ORDER BY generation,pass_key,path").unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |r| {
                (0..columns)
                    .map(|i| {
                        r.get::<_, rusqlite::types::Value>(i)
                            .map(|v| format!("{v:?}"))
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        };
        let (initial, _) = cycle_options(
            &root,
            page(None),
            false,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::Original, "contract-original"),
        )
        .await;
        assert_eq!(
            (initial.failed_count, initial.stats.downloaded),
            (0, 1),
            "{initial:?}"
        );
        assert!(initial.db_sync_token_advance_safe);
        assert_bytes(&p, ORIGINAL);
        let original_file = file_snapshot(&p);
        let original_receipt = rows(&db_path, receipt_sql);
        assert_eq!(original_receipt.len(), 1);
        let original_paths = rows(&db_path, original_path_sql);
        let original_sha = format!("{:x}", Sha256::digest(ORIGINAL));
        assert_eq!(
            original_paths,
            [json!([
                "PrimarySync",
                "asset-501",
                "original",
                p.to_str().unwrap(),
                signature(ORIGINAL),
                original_sha,
                original_sha,
                original_sha
            ])]
        );
        let original_asset = rows(&db_path, original_asset_sql);
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let (account, provider) = owner.authenticated_keys().unwrap();
        let scope = json!({"container":"com.apple.photos.cloud","database":"private","environment":"production","format":1,"realm":"com","zone":{"ownerRecordName":"_defaultOwner","zoneName":"PrimarySync"}}).to_string();
        let selection = json!({"zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"name":"TestAlbum","list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted","obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted","query_filter":null,"container_id":null,"cross_zone_sources":[]});
        let pass = format!(
            "{:x}",
            Sha256::digest(
                json!({"kind":"Unfiled","scope":selection,"excludes":[]})
                    .to_string()
                    .as_bytes()
            )
        );
        let binding = state::db::PublicationBinding {
            account_key: account.into(),
            provider_key: provider.into(),
            scope,
            origin_pass: pass,
            library: "PrimarySync".into(),
            child: "asset-501".into(),
            master: "501".into(),
            state_id: "asset-501".into(),
        };
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        let original_proofs = db
            .bound_download_receipts(
                binding.clone(),
                "original".into(),
                signature(ORIGINAL),
                ORIGINAL.len() as u64,
            )
            .await
            .unwrap();
        assert_eq!(original_proofs.len(), 1);
        assert_eq!(original_proofs[0].native_path.to_path(), p);
        assert_eq!(
            original_proofs[0].source_checksum,
            format!("{:x}", Sha256::digest(ORIGINAL))
        );
        assert_eq!(
            original_proofs[0].local_checksum,
            original_proofs[0].source_checksum
        );
        drop(db);

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let fault = Arc::new(AtomicBool::new(true));
        let fail_requests = fault.clone();
        Mock::given(method("GET"))
            .and(path("/edit-one"))
            .respond_with(move |_: &wiremock::Request| {
                if fail_requests.load(Ordering::SeqCst) {
                    seen.fetch_add(1, Ordering::SeqCst);
                    ResponseTemplate::new(500)
                } else {
                    ResponseTemplate::new(200).set_body_bytes(EDIT_ONE)
                }
            })
            .with_priority(2)
            .mount(&server)
            .await;
        let mut changed = page(Some(("edit-one", EDIT_ONE)));
        let (failed, _) = cycle_options(
            &root,
            changed.clone(),
            false,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None, "contract-debt"),
        )
        .await;
        let initial_debt = rows(&db_path, debt_sql);
        let initial_cohort = cohort();
        let temp_custody = rows(&db_path, temp_sql);
        std::fs::write(root.join("failed-transfer-evidence.json"), serde_json::to_vec_pretty(&json!({"cycle":format!("{failed:?}"),"failed_gets":calls.load(Ordering::SeqCst),"debt":initial_debt,"full_cohort":initial_cohort,"intent":rows(&db_path,intent_sql),"original_receipt":original_receipt,"error":rows(&db_path,error_sql)})).unwrap()).unwrap();
        assert!(failed.failed_count > 0, "{failed:?}");
        assert!(failed.db_sync_token_advance_safe);
        assert_eq!(calls.load(Ordering::SeqCst), 8);
        assert!(!initial_debt.is_empty());
        let intent = rows(&db_path, intent_sql);
        assert_eq!(intent.len(), 1);
        let s = std::path::PathBuf::from(intent[0]["destination"].as_str().unwrap());
        assert_ne!(s, p);
        assert!(s.starts_with(&media) && !s.exists());
        assert_eq!(
            intent[0]["requested"],
            crate::fs_util::confined_path_key(&p).unwrap()
        );
        assert_eq!(
            intent[0]["destination_key"],
            crate::fs_util::confined_path_key(&s).unwrap()
        );
        assert_eq!(intent[0]["checksum"], signature(EDIT_ONE));
        assert_eq!(intent[0]["size"], EDIT_ONE.len());
        assert!(rows(&db_path, &adjusted_sql).is_empty());
        assert_eq!(file_snapshot(&p), original_file);
        let assert_debt = |debt: &[Value]| {
            for d in debt {
                for (key, expected) in [
                    ("account", &binding.account_key),
                    ("provider", &binding.provider_key),
                    ("scope", &binding.scope),
                    ("pass", &binding.origin_pass),
                    ("library", &binding.library),
                    ("child", &binding.child),
                    ("master", &binding.master),
                    ("state", &binding.state_id),
                ] {
                    assert_eq!(d[key], expected.as_str(), "{key}: {d:?}");
                }
                assert_eq!(d["format"], 2);
                assert_eq!(d["version"], "adjusted");
                assert_eq!(d["checksum"], signature(EDIT_ONE));
                assert_eq!(d["size"], EDIT_ONE.len());
                assert_eq!(
                    serde_json::from_str::<SelectionPath>(d["path"].as_str().unwrap())
                        .unwrap()
                        .to_path(),
                    s
                );
                assert_eq!(d["compat_path"], s.to_str().unwrap());
                for key in ["admitted", "sealed", "ready"] {
                    assert_eq!(d[key], 1);
                }
                assert_eq!(d["verified"], 0);
            }
        };
        assert_debt(&initial_debt);
        let failure_error = rows(&db_path, error_sql);
        assert!(failure_error[0].as_str().unwrap().contains("HTTP 500"));
        assert!(MatchDiagnostics::decode(failure_error[0].as_str().unwrap()).is_none());
        fault.store(false, Ordering::SeqCst);
        std::fs::write(&s, b"foreign obstruction at frozen S").unwrap();
        let obstruction = file_snapshot(&s);
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
        drop(db);
        let mut healthy = page(Some(("edit-two", EDIT_TWO)));
        for record in &mut healthy {
            record["recordName"] =
                json!(record["recordName"].as_str().unwrap().replace("501", "502"));
            if let Some(reference) = record["fields"].get_mut("masterRef") {
                reference["value"]["recordName"] = json!("502");
            }
        }
        healthy[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
        healthy[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL": format!("{}/edit-two", server.uri()), "size": EDIT_TWO.len(), "fileChecksum": signature(EDIT_TWO)});
        changed.extend(healthy.clone());
        let (held, _) = cycle_options(
            &root,
            changed,
            false,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None, "contract-obstructed"),
        )
        .await;
        let held_debt = rows(&db_path, debt_sql);
        let held_cohort = cohort();
        let evidence = rows(&db_path, error_sql);
        std::fs::write(root.join("obstruction-evidence.json"), serde_json::to_vec_pretty(&json!({"cycle":format!("{held:?}"),"debt":held_debt,"full_cohort":held_cohort,"intent":rows(&db_path,intent_sql),"diagnostic":evidence,"requests":server.received_requests().await.unwrap().iter().map(|r|r.url.path()).collect::<Vec<_>>(),"S":s,"foreign_physical":format!("{obstruction:?}")})).unwrap()).unwrap();
        assert!(held.failed_count > 0, "{held:?}");
        assert!(held.db_sync_token_advance_safe);
        assert_eq!(
            held.stats.downloaded, 1,
            "only healthy502 publishes: {held:?}"
        );
        assert_debt(&held_debt);
        for row in &initial_debt {
            assert!(held_debt.contains(row));
        }
        for row in &initial_cohort {
            assert!(
                held_cohort.contains(row),
                "lost full initial destination row"
            );
        }
        let diagnostic = MatchDiagnostics::decode(evidence[0].as_str().unwrap()).unwrap();
        assert!(
            diagnostic.attempts.iter().any(|a| {
                a.child == "asset-501"
                    && a.destination.to_path() == s
                    && a.checksum == signature(EDIT_ONE)
                    && a.pass.as_deref() == Some(binding.origin_pass.as_str())
                    && initial_debt
                        .iter()
                        .any(|d| a.generation.as_deref() == d["generation"].as_str())
                    && a.explanation.candidates.iter().any(|c| {
                        c.path.as_ref().is_some_and(|p| p.to_path() == s)
                            && c.reason == "candidate_bytes_not_verified_for_current_resource"
                    })
            }),
            "fresh S-specific initial-generation refusal required: {}",
            evidence[0]
        );
        let b = media.join("IMG_0502.JPG");
        assert_bytes(&b, EDIT_TWO);
        let healthy_file = file_snapshot(&b);
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        let healthy_binding = state::db::PublicationBinding {
            child: "asset-502".into(),
            master: "502".into(),
            state_id: "asset-502".into(),
            ..binding.clone()
        };
        let healthy_proofs = db
            .bound_download_receipts(
                healthy_binding.clone(),
                "adjusted".into(),
                signature(EDIT_TWO),
                EDIT_TWO.len() as u64,
            )
            .await
            .unwrap();
        assert_eq!(healthy_proofs.len(), 1);
        assert_eq!(healthy_proofs[0].native_path.to_path(), b);
        assert_eq!(
            healthy_proofs[0].local_checksum,
            healthy_proofs[0].source_checksum
        );
        assert_eq!(
            healthy_proofs[0].source_checksum,
            format!("{:x}", Sha256::digest(EDIT_TWO))
        );
        assert!(
            db.get_pending_retry_candidates()
                .await
                .unwrap()
                .iter()
                .all(|a| a.id.as_ref() != "asset-501"),
            "format2 debt is not an independent pending candidate"
        );
        drop(db);
        let held_decisions = rows(&db_path, decision_sql);
        let tree = || {
            let mut entries = std::fs::read_dir(&media)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    assert!(e.metadata().unwrap().is_file());
                    (e.path(), file_snapshot(&e.path()))
                })
                .collect::<Vec<_>>();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            entries
        };
        let held_tree = tree();
        let requests = server.received_requests().await.unwrap().len();
        let gets = server.received_requests().await.unwrap();
        assert_eq!(
            gets.iter().filter(|r| r.url.path() == "/edit-one").count(),
            8
        );
        assert_eq!(
            gets.iter().filter(|r| r.url.path() == "/edit-two").count(),
            1
        );
        let adjusted_state_sql = "SELECT json_object('library',library,'id',id,'version',version_size,'status',status,'checksum',checksum,'size',size_bytes,'filename',filename,'local_path',local_path,'local_checksum',local_checksum,'download_checksum',download_checksum,'is_deleted',is_deleted) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'";
        let adjusted_state = rows(&db_path, adjusted_state_sql);
        // Pending canonical work has no completed local path. Its filename and
        // the separately asserted intent/destination retain the exact frozen S.
        assert_eq!(
            adjusted_state,
            [json!({
                "library":"PrimarySync","id":"asset-501","version":"adjusted",
                "status":"pending","checksum":signature(EDIT_ONE),"size":EDIT_ONE.len(),
                "filename":s.file_name().unwrap().to_str().unwrap(),"local_path":null,
                "local_checksum":null,"download_checksum":null,"is_deleted":0
            })],
            "obstructed canonical Adjusted must be the exact pending resource"
        );
        std::fs::write(
            root.join("canonical-adjusted-obstructed.json"),
            serde_json::to_vec_pretty(&adjusted_state).unwrap(),
        )
        .unwrap();
        let prove_custody = || {
            let current_adjusted = rows(&db_path, adjusted_state_sql);
            assert_eq!(current_adjusted.len(), 1);
            assert_eq!(
                current_adjusted[0]["status"], "pending",
                "retained canonical Adjusted must remain pending"
            );
            assert_eq!(
                current_adjusted, adjusted_state,
                "canonical Adjusted identity/resource/path and undeleted state must survive"
            );
            assert_eq!(file_snapshot(&p), original_file);
            assert_eq!(file_snapshot(&s), obstruction);
            assert_eq!(file_snapshot(&b), healthy_file);
            assert_eq!(tree(), held_tree);
            assert_eq!(rows(&db_path, receipt_sql), original_receipt);
            assert_eq!(rows(&db_path, original_path_sql), original_paths);
            assert_eq!(rows(&db_path, original_asset_sql), original_asset);
            assert!(rows(&db_path, &adjusted_sql).is_empty());
            assert_eq!(rows(&db_path, intent_sql), intent);
            assert_eq!(
                cohort(),
                held_cohort,
                "no completion, retarget or cohort retirement"
            );
            assert_eq!(
                rows(&db_path, decision_sql),
                held_decisions,
                "immutable admission/manifest must survive"
            );
            assert_eq!(rows(&db_path, error_sql), evidence);
            assert_eq!(rows(&db_path, temp_sql), temp_custody);
            for table in [
                "primary_layout_operations",
                "primary_layout_bindings",
                "primary_layout_preserved",
                "primary_layout_claims",
            ] {
                assert_eq!(db_count(&root, &format!("SELECT count(*) FROM {table}")), 0);
            }
        };
        prove_custody();
        let mut reverted = page(None);
        assert!(reverted[1]["fields"].get("resJPEGFullRes").is_none());
        reverted.extend(healthy);
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
        drop(db);
        for (phase, quiet) in [("reverted", false), ("quiet-1", true), ("quiet-2", true)] {
            trace.lock().unwrap().clear();
            let event_start = capture.events().len();
            let before = rows(&db_path, schedule_sql);
            let (result, lookups) = cycle_options(
                &root,
                reverted.clone(),
                quiet,
                EditedNaming::Primary,
                options(crate::types::PhotoResolution::None, "contract-reverted"),
            )
            .await;
            let after = rows(&db_path, schedule_sql);
            let lookup_trace = trace.lock().unwrap().clone();
            let events = capture.events()[event_start..].to_vec();
            std::fs::write(root.join(format!("{phase}-evidence.json")),serde_json::to_vec_pretty(&json!({"cycle":format!("{result:?}"),"lookups":lookups,"provider_facts":reverted,"trace":lookup_trace,"schedule_before":before,"schedule_after":after,"debt":rows(&db_path,debt_sql),"full_cohort":cohort(),"decisions":rows(&db_path,decision_sql),"diagnostic":rows(&db_path,error_sql),"canonical_adjusted":rows(&db_path,adjusted_state_sql),"intent":rows(&db_path,intent_sql),"checkpoint":rows(&db_path,checkpoint_sql),"events":format!("{events:?}")})).unwrap()).unwrap();
            if !quiet {
                assert!(
                    lookups > 0,
                    "revert must actually reconfirm old selected work"
                );
                for target in ["asset-501", "501"] {
                    assert!(
                        lookup_trace.iter().any(|(kind, body)| kind == "lookup"
                            && body["records"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|r| r["recordName"] == target)),
                        "no fresh targeted {target} lookup: {lookup_trace:?}"
                    );
                }
                assert!(
                    before
                        .iter()
                        .filter(|b| initial_debt
                            .iter()
                            .any(|d| d["generation"] == b["generation"]))
                        .all(|b| b["next_retry_at"] == 1)
                );
                assert!(
                    after.iter().any(|a| initial_debt
                        .iter()
                        .any(|d| d["generation"] == a["generation"])
                        && before.iter().any(|b| b["generation"] == a["generation"]
                            && b["pass"] == a["pass"]
                            && a["attempts"].as_i64().unwrap() > b["attempts"].as_i64().unwrap())
                        && a["reason"] == "current_confirmation_unavailable"
                        && a["admission"] == "admitted"
                        && a["compacted_hash"].is_null()),
                    "initial cohort must hit confirmation deferral, not just backoff: {before:?} -> {after:?}"
                );
                assert!(events.iter().any(|e| e.message()
                    == Some(
                        "Previously selected work remains pending after confirmation refusal"
                    )));
            }
            for a in &after {
                let b = before
                    .iter()
                    .find(|b| b["generation"] == a["generation"] && b["pass"] == a["pass"])
                    .unwrap();
                assert!(a["attempts"].as_i64().unwrap() >= b["attempts"].as_i64().unwrap());
                if a != b {
                    assert!(lookups > 0);
                    assert_eq!(a["reason"], "current_confirmation_unavailable");
                    assert_eq!(a["admission"], "admitted");
                    assert!(a["next_retry_at"].as_i64().unwrap() > 1);
                }
            }
            assert!(
                result.db_sync_token_advance_safe,
                "durably accounted debt: {result:?}"
            );
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(rows(&db_path, checkpoint_sql), [json!("contract-reverted")]);
            prove_custody();
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let db = state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap();
            assert_eq!(
                db.bound_download_receipts(
                    binding.clone(),
                    "original".into(),
                    signature(ORIGINAL),
                    ORIGINAL.len() as u64
                )
                .await
                .unwrap(),
                original_proofs
            );
            assert_eq!(
                db.bound_download_receipts(
                    healthy_binding.clone(),
                    "adjusted".into(),
                    signature(EDIT_TWO),
                    EDIT_TWO.len() as u64
                )
                .await
                .unwrap(),
                healthy_proofs
            );
            assert!(
                db.get_pending_retry_candidates()
                    .await
                    .unwrap()
                    .iter()
                    .all(|a| a.id.as_ref() != "asset-501")
            );
            eprintln!(
                "CONTRACT_DEBT_PHASE {phase} lookups={lookups} GETs={requests} cohort={} checkpoint_safe={}",
                held_cohort.len(),
                result.db_sync_token_advance_safe
            );
        }
    }
    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    root.disable_cleanup(true);
    eprintln!("CONTRACT_NAMING_ROOT={}", root.path().display());
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    std::fs::write(media.join("foreign.jpg"), b"unrelated user bytes").unwrap();
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let published_facts = current.clone();
    let trace: RequestTrace = Arc::default();
    let options = || CycleOptions {
        private: true,
        trace: Some(trace.clone()),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
    let naming_files = [
        media.join("IMG_0501.JPG"),
        media.join("IMG_0501_edited.JPG"),
        media.join("foreign.jpg"),
    ]
    .map(|p| (p.clone(), file_snapshot(&p)));
    let db_path = root.path().join("state.db");
    let receipt_sql = "SELECT json_object('id',id,'account',account_key,'provider',provider_key,'scope',scope,'library',library,'child',child,'master',master,'state',state_id,'pass',origin_pass,'version',version_size,'checksum',provider_checksum,'size',provider_size,'path',native_path,'source',source_checksum,'local',local_checksum,'proof',proof_hash,'trust',trust_receipt,'metadata',selected_metadata_hash,'generation',generation_id) FROM download_receipts ORDER BY id";
    let receipts = rows(&db_path, receipt_sql);
    assert_eq!(receipts.len(), 2);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let (account, provider) = owner.authenticated_keys().unwrap();
    let scope = json!({"container":"com.apple.photos.cloud","database":"private","environment":"production","format":1,"realm":"com","zone":{"ownerRecordName":"_defaultOwner","zoneName":"PrimarySync"}}).to_string();
    let selection = json!({"zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"name":"TestAlbum","list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted","obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted","query_filter":null,"container_id":null,"cross_zone_sources":[]});
    let pass = format!(
        "{:x}",
        Sha256::digest(
            json!({"kind":"Unfiled","scope":selection,"excludes":[]})
                .to_string()
                .as_bytes()
        )
    );
    for (version, path, bytes) in [
        ("original", media.join("IMG_0501.JPG"), ORIGINAL),
        ("adjusted", media.join("IMG_0501_edited.JPG"), EDIT_ONE),
    ] {
        let receipt = receipts.iter().find(|r| r["version"] == version).unwrap();
        assert_eq!(receipt.as_object().unwrap().len(), 19);
        for (key, expected) in [
            ("account", account),
            ("provider", provider),
            ("scope", scope.as_str()),
            ("library", "PrimarySync"),
            ("child", "asset-501"),
            ("master", "501"),
            ("state", "asset-501"),
            ("pass", pass.as_str()),
        ] {
            assert_eq!(receipt[key], expected, "{key}: {receipt:?}");
        }
        assert_eq!(
            serde_json::from_str::<SelectionPath>(receipt["path"].as_str().unwrap())
                .unwrap()
                .to_path(),
            path
        );
        assert_eq!(receipt["checksum"], provider_hash(bytes));
        assert_eq!(receipt["size"], bytes.len());
        for key in ["source", "local"] {
            assert_eq!(receipt[key], format!("{:x}", Sha256::digest(bytes)));
        }
        for key in ["id", "proof"] {
            assert_eq!(receipt[key].as_str().unwrap().len(), 64);
        }
        for key in ["trust", "metadata", "generation"] {
            assert!(receipt[key].is_null());
        }
    }
    let paths_sql = "SELECT json_array(library,id,version_size,local_path,provider_checksum,source_checksum,local_checksum,download_checksum) FROM asset_metadata_paths ORDER BY library,id,version_size,local_path";
    let paths = rows(&db_path, paths_sql);
    let assets_sql = "SELECT json_array(library,id,version_size,status,checksum,size_bytes,local_path,local_checksum,download_checksum,is_deleted) FROM assets ORDER BY library,id,version_size";
    let assets = rows(&db_path, assets_sql);
    let policy_sql = "SELECT json_object('download',(SELECT value FROM metadata WHERE key='config_hash'),'pending_download',(SELECT value FROM metadata WHERE key='pending_download_config_hash'),'enumeration',(SELECT value FROM metadata WHERE key='enum_config_hash'),'pending_enumeration',(SELECT value FROM metadata WHERE key='pending_enum_config_hash'))";
    let revalidation_sql = "SELECT json_object('round',r.id,'scope',r.scope,'enumeration',r.enum_hash,'boundary',r.boundary,'phase',r.phase,'child',d.id,'next_retry_at',d.next_retry_at) FROM source_revalidation_rounds r LEFT JOIN source_revalidation_debt d ON d.round=r.id ORDER BY r.id,d.id";
    let suffix_policy = rows(&db_path, policy_sql);
    let requests = server.received_requests().await.unwrap().len();
    for (phase, naming, config_drift) in [
        ("to-primary", EditedNaming::Primary, true),
        ("stable-primary", EditedNaming::Primary, false),
        ("to-suffix", EditedNaming::Suffix, true),
        ("stable-suffix-1", EditedNaming::Suffix, false),
        ("stable-suffix-2", EditedNaming::Suffix, false),
    ] {
        trace.lock().unwrap().clear();
        let policy_before = rows(&db_path, policy_sql);
        let revalidation_before = rows(&db_path, revalidation_sql);
        let outcome =
            try_cycle_options(root.path(), current.clone(), true, naming, options()).await;
        let actual_trace = trace.lock().unwrap().clone();
        let policy_after = rows(&db_path, policy_sql);
        let gets = server.received_requests().await.unwrap();
        std::fs::write(root.path().join(format!("naming-{phase}-evidence.json")), serde_json::to_vec_pretty(&json!({
            "phase":phase,"naming":format!("{naming:?}"),"declared_config_drift":config_drift,
            "outcome":format!("{outcome:?}"),"full_enumeration_reason":outcome.as_ref().ok().map(|(r,_)| r.stats.full_enumeration_reason),
            "lookups":outcome.as_ref().ok().map(|(_,n)|n),"trace":actual_trace,"current_facts":current,
            "policy_before":policy_before,"policy_after":policy_after,"source_revalidation_before":revalidation_before,
            "source_revalidation_after":rows(&db_path,revalidation_sql),"receipts":rows(&db_path,receipt_sql),
            "paths":rows(&db_path,paths_sql),"assets":rows(&db_path,assets_sql),"physical":naming_files.iter().map(|(p,_)|format!("{:?}", (p,file_snapshot(p)))).collect::<Vec<_>>(),
            "GET_paths":gets.iter().map(|r|r.url.path()).collect::<Vec<_>>()
        })).unwrap()).unwrap();
        let (result, lookups) = outcome.expect("naming-only revalidation must not fail its owner");
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.skipped.uncertain, 0);
        assert_eq!(result.stats.state_write_failures, 0);
        assert!(result.db_sync_token_advance_safe);
        assert_eq!(result.stats.downloaded, 0);
        let lookup_requests: Vec<_> = actual_trace
            .iter()
            .filter(|(kind, _)| kind == "lookup")
            .collect();
        assert_eq!(
            lookups,
            lookup_requests.len(),
            "count must match actual traced requests"
        );
        assert_eq!(
            current, published_facts,
            "provider facts did not change with naming"
        );
        for (_, request) in lookup_requests {
            assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
            let wanted = request["records"].as_array().unwrap();
            assert!(!wanted.is_empty());
            for record in wanted {
                let name = record["recordName"].as_str().unwrap();
                assert!(
                    matches!(name, "asset-501" | "501"),
                    "unexpected lookup: {request:?}"
                );
                assert!(
                    published_facts
                        .iter()
                        .any(|fact| fact["recordName"] == name)
                );
            }
        }
        assert_eq!(
            policy_before[0]["enumeration"],
            policy_after[0]["enumeration"]
        );
        assert!(
            policy_after[0]["pending_download"].is_null()
                && policy_after[0]["pending_enumeration"].is_null()
        );
        if config_drift {
            // Naming affects the path hash, not provider facts. A new owned
            // selection confirms the current child/master before reusing files.
            assert_eq!(
                result.stats.full_enumeration_reason,
                Some(download::FullEnumerationReason::DownloadConfigHashDrift)
            );
            assert_ne!(policy_before[0]["download"], policy_after[0]["download"]);
        } else {
            assert_eq!(result.stats.full_enumeration_reason, None);
            assert_eq!(policy_before, policy_after);
            assert_eq!(
                lookups,
                0,
                "unexpected new work on stable {phase}: {actual_trace:?}; source before={revalidation_before:?} after={:?}",
                rows(&db_path, revalidation_sql)
            );
        }
        if naming == EditedNaming::Suffix {
            assert_eq!(policy_after, suffix_policy);
        }
        assert_eq!(
            rows(&db_path, receipt_sql),
            receipts,
            "all 19 proof fields stay immutable"
        );
        assert_eq!(rows(&db_path, paths_sql), paths);
        assert_eq!(rows(&db_path, assets_sql), assets);
        for (path, snapshot) in &naming_files {
            assert_eq!(&file_snapshot(path), snapshot);
        }
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
        assert_bytes(&media.join("foreign.jpg"), b"unrelated user bytes");
        assert!(!media.join("IMG_0501_original.JPG").exists());
        assert!(!media.join(".kei-history").exists());
        assert!(!root.path().join(".kei-history").exists());
        assert_eq!(
            std::fs::read_dir(&media).unwrap().count(),
            naming_files.len()
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            0
        );
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
            0
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        eprintln!(
            "CONTRACT_NAMING_PHASE {phase} lookups={lookups} reason={:?} GETs={requests} full_proofs=unchanged physical=unchanged",
            result.stats.full_enumeration_reason
        );
    }
}

#[tokio::test]
async fn primary_layout_heic_jpeg_extension_changes_retire_only_preserved_owned_aliases() {
    async fn current_publication(
        db_path: &std::path::Path,
        version: &str,
        provider: &str,
        bytes: &[u8],
    ) -> std::path::PathBuf {
        let db = state::SqliteStateDb::open_read_only(db_path).await.unwrap();
        let local = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        let row = db
            .get_manifest_assets()
            .await
            .unwrap()
            .into_iter()
            .find(|row| {
                row.asset_id == "asset-501" && row.version == version && row.checksum == provider
            })
            .expect("selected rendition row");
        let path = row.local_path.clone().expect("selected path");
        assert_eq!(
            (row.status.as_str(), row.local_checksum.as_deref()),
            ("downloaded", Some(local.as_str()))
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let receipt = crate::state::db::DownloadContextStateStore::get_baseline_receipts(&db)
            .await
            .unwrap()
            .into_iter()
            .find(|receipt| {
                receipt.id == "asset-501"
                    && receipt.version_size.as_str() == version
                    && receipt.provider_checksum == provider
                    && receipt.local_path == path
            })
            .expect("selected path receipt");
        assert_eq!(
            (
                receipt.local_checksum.as_deref(),
                receipt.source_checksum.as_deref(),
                receipt.is_current_path
            ),
            (Some(local.as_str()), Some(local.as_str()), true)
        );
        path
    }

    let original = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/pattern.heic"
    ));
    let server = MockServer::start().await;
    for (endpoint, bytes) in [
        ("original", original.as_slice()),
        ("edit-one", EDIT_ONE),
        ("edit-two", EDIT_TWO),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{endpoint}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
    }
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let db_path = root.path().join("state.db");
    let snap = |path: &std::path::Path| {
        let metadata = std::fs::metadata(path).unwrap();
        let modified = metadata.modified().unwrap();
        #[cfg(unix)]
        let inode = {
            use std::os::unix::fs::MetadataExt;
            metadata.ino()
        };
        #[cfg(not(unix))]
        let inode = 0_u64;
        (std::fs::read(path).unwrap(), modified, inode)
    };
    type Published = (std::path::PathBuf, (Vec<u8>, std::time::SystemTime, u64));
    let mut published: Vec<Published> = Vec::new();
    let mut original_path: Option<std::path::PathBuf> = None;
    let mut edit_one_path: Option<std::path::PathBuf> = None;
    for (index, edit, current_name, current_bytes) in [
        (0, None, "IMG_0501.HEIC", original.as_slice()),
        (1, Some(("edit-one", EDIT_ONE)), "IMG_0501.JPG", EDIT_ONE),
        (2, None, "IMG_0501.HEIC", original.as_slice()),
        (3, Some(("edit-two", EDIT_TWO)), "IMG_0501.JPG", EDIT_TWO),
    ] {
        let mut current = records(&server, edit);
        current[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0501.HEIC");
        current[0]["fields"]["itemType"] = json!({"value":"public.heic"});
        current[0]["fields"]["resOriginalRes"] = resource(&server.uri(), "original", original);
        current[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
        let version = if current_name.ends_with(".HEIC") {
            "original"
        } else {
            "adjusted"
        };
        let provider = provider_hash(current_bytes);
        let (changed, _) = cycle(root.path(), current.clone(), false, EditedNaming::Primary).await;
        assert_eq!(
            changed.failed_count, 0,
            "transition {index}: {:?}",
            changed.stats
        );
        assert!(changed.db_sync_token_advance_safe);
        let path = current_publication(&db_path, version, &provider, current_bytes).await;
        if index == 0 {
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("IMG_0501.HEIC")
            );
            original_path = Some(path.clone());
        } else if index == 1 {
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("IMG_0501.JPG")
            );
            edit_one_path = Some(path.clone());
        } else if index == 2 {
            assert_eq!(
                path.file_name().and_then(|name| name.to_str()),
                Some("IMG_0501.HEIC")
            );
            assert_eq!(original_path.as_ref(), Some(&path));
            assert_eq!(changed.stats.downloaded, 0);
        } else {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap();
            assert!(
                name.starts_with("IMG_0501") && name.ends_with(".JPG"),
                "adjusted sibling must stay in the file family: {name}"
            );
            assert_ne!(Some(&path), edit_one_path.as_ref());
        }
        for (known, expected) in &published {
            assert_eq!(&snap(known), expected, "transition {index}: {known:?}");
        }
        if !published.iter().any(|(known, _)| known == &path) {
            published.push((path.clone(), snap(&path)));
        }
        for name in [
            "IMG_0501_original.HEIC",
            "IMG_0501_original.JPG",
            ".kei-history",
        ] {
            assert!(
                !media.join(name).exists(),
                "transition {index}: unexpected archive {name}"
            );
        }
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        assert_eq!(operations, 0, "transition {index}");
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
            0,
            "transition {index}"
        );
        let requests = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            let (quiet, lookups) =
                cycle(root.path(), current.clone(), true, EditedNaming::Primary).await;
            assert_eq!(quiet.failed_count, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(lookups, 0, "transition {index}: {:?}", quiet.stats);
            assert_eq!(
                current_publication(&db_path, version, &provider, current_bytes).await,
                path
            );
            for (known, expected) in &published {
                assert_eq!(&snap(known), expected, "quiet {index}: {known:?}");
            }
            for name in [
                "IMG_0501_original.HEIC",
                "IMG_0501_original.JPG",
                ".kei-history",
            ] {
                assert!(
                    !media.join(name).exists(),
                    "quiet {index}: unexpected archive {name}"
                );
            }
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                operations
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }
    }
    let original_path = original_path.expect("original path");
    let edit_one_path = edit_one_path.expect("edit-one path");
    assert_eq!(std::fs::read(&original_path).unwrap(), original);
    assert_eq!(std::fs::read(&edit_one_path).unwrap(), EDIT_ONE);
    let db = state::SqliteStateDb::open_read_only(&db_path)
        .await
        .unwrap();
    let receipts = crate::state::db::DownloadContextStateStore::get_baseline_receipts(&db)
        .await
        .unwrap();
    let edit_one_local = data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_ONE));
    let edit_one_proof: (Option<String>, Option<String>) = {
        let conn = db.acquire_lock("inspect retained EDIT_ONE proof").unwrap();
        conn.query_row(
            "SELECT local_checksum, source_checksum FROM asset_metadata_paths
             WHERE library=?1 AND id=?2 AND version_size=?3
               AND provider_checksum=?4 AND local_path=?5",
            rusqlite::params![
                "PrimarySync",
                "asset-501",
                "adjusted",
                provider_hash(EDIT_ONE),
                edit_one_path.to_str().unwrap()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("retained EDIT_ONE path proof")
    };
    assert_eq!(
        (edit_one_proof.0.as_deref(), edit_one_proof.1.as_deref()),
        (Some(edit_one_local.as_str()), Some(edit_one_local.as_str()))
    );
    let original_local = data_encoding::HEXLOWER.encode(&Sha256::digest(original));
    let original_receipt = receipts
        .iter()
        .find(|receipt| {
            receipt.id == "asset-501"
                && receipt.version_size.as_str() == "original"
                && receipt.provider_checksum == provider_hash(original)
                && receipt.local_path == original_path
        })
        .expect("retained original receipt");
    assert_eq!(
        (
            original_receipt.local_checksum.as_deref(),
            original_receipt.source_checksum.as_deref()
        ),
        (Some(original_local.as_str()), Some(original_local.as_str()))
    );
    drop(db);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        3,
        "local revert redownloaded a proved original"
    );
}

#[tokio::test]
async fn naming_switch_preserves_existing_paths_without_redownload() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (legacy, _) = cycle(root.path(), edited.clone(), false, EditedNaming::Suffix).await;
    assert_eq!(legacy.failed_count, 0);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
    let downloads = server.received_requests().await.unwrap().len();
    let (migrated, _) = cycle(root.path(), edited.clone(), true, EditedNaming::Primary).await;
    assert_eq!(migrated.failed_count, 0, "{:?}", migrated.stats);
    assert!(migrated.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
    assert!(!media.join("IMG_0501_original.JPG").exists());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        downloads,
        "naming change fetched existing media"
    );
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.get_all_asset_albums("PrimarySync")
            .await
            .unwrap()
            .is_empty(),
        "unfiled path label became real membership and would suppress reversal"
    );
    drop(db);
    let (disabled, _) = cycle(root.path(), edited.clone(), true, EditedNaming::Suffix).await;
    assert_eq!(disabled.failed_count, 0, "{:?}", disabled.stats);
    assert!(disabled.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
    assert!(!media.join("IMG_0501_original.JPG").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), downloads);
}

#[tokio::test]
async fn ordinary_naming_change_does_not_initiate_layout_migration() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let (initial, _) = cycle(root.path(), current.clone(), true, EditedNaming::Suffix).await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        let (result, _) = cycle(root.path(), current.clone(), true, EditedNaming::Primary).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
        assert!(!media.join("IMG_0501_original.JPG").exists());
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            0
        );
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
            0
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
}

#[tokio::test]
async fn current_unverified_primary_candidate_is_held_across_reopen() {
    use sha1::{Digest as _, Sha1};
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let foreign = media.join("IMG_0501.JPG");
    std::fs::write(&foreign, b"foreign primary bytes").unwrap();
    let mtime = std::fs::metadata(&foreign).unwrap().modified().unwrap();
    let mut current = records(&server, Some(("edit-one", EDIT_ONE)));
    let signature = |bytes: &[u8]| {
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        base64::engine::general_purpose::STANDARD.encode([&[1][..], &hash.finalize()[..]].concat())
    };
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(signature(ORIGINAL));
    current[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] = json!(signature(EDIT_ONE));
    for _ in 0..2 {
        let (result, _) = cycle(root.path(), current.clone(), true, EditedNaming::Primary).await;
        assert!(
            result.failed_count > 0 || result.stats.skipped.uncertain > 0,
            "unverified candidate must remain visible: {result:?}"
        );
        assert_eq!(result.stats.downloaded, 0);
        assert_bytes(&foreign, b"foreign primary bytes");
        assert_eq!(
            std::fs::metadata(&foreign).unwrap().modified().unwrap(),
            mtime
        );
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 1);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(!db.get_pending().await.unwrap().is_empty());
        assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
        drop(db);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            0
        );
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
            0
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 0);
}

#[tokio::test]
async fn recorded_edited_family_local_modification_holds_runner_checkpoint_then_recovers() {
    recorded_edited_family_recovery_scenario().await;
}

async fn recorded_edited_family_recovery_scenario() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let (initial, _) = cycle(root.path(), current.clone(), true, EditedNaming::Suffix).await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    let original = media.join("IMG_0501.JPG");
    let edited = media.join("IMG_0501_edited.JPG");
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&edited, EDIT_ONE);
    let requests = server.received_requests().await.unwrap().len();
    let provider = Session {
        reported_count: None,
        records: current.clone(),
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
        exclude_ids: Arc::default(),
    };
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
    assert!(checkpoint.is_some());
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let base = make_run_cycle_download_config_builder(&media, store);
    let mut config = base(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Suffix;
    config.refresh_metadata = true;
    // This explicit refresh is planned and journaled while both recorded files
    // are intact. Subsequent ordinary cycles must replay the saved intent.
    let operation = download::seed_unprepared_refresh(&config, &pass).await;
    assert!(operation.refresh_metadata);
    let old: Vec<_> = operation
        .members
        .iter()
        .filter_map(|member| member.old.as_ref())
        .collect();
    assert_eq!(old.len(), 2);
    assert!(old.iter().any(|file| file.path.to_path() == original));
    assert!(old.iter().any(|file| file.path.to_path() == edited));
    let receipts: Vec<_> = old
        .iter()
        .map(|file| serde_json::to_value(file).unwrap())
        .collect();
    drop(config);
    drop(base);
    drop(db);
    std::fs::write(&edited, b"user changed owned bytes").unwrap();
    for _ in 0..2 {
        let result = try_cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Suffix,
            CycleOptions::default(),
        )
        .await;
        let error = result.expect_err("local modification must stop recorded recovery");
        assert!(
            format!("{error:#}").contains("Owned layout file changed locally"),
            "unexpected recovery failure: {error:#}"
        );
        assert_bytes(&edited, b"user changed owned bytes");
        assert_bytes(&original, ORIGINAL);
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            checkpoint
        );
        let pending = db
            .primary_layout_operations("PrimarySync".into())
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation, operation.operation);
        assert!(pending[0].refresh_metadata);
        assert_eq!(pending[0].phase, operation.phase);
        assert!(
            pending[0]
                .members
                .iter()
                .all(|member| member.prepared.is_none())
        );
        assert!(
            pending[0]
                .members
                .iter()
                .filter_map(|member| member.stage.as_ref())
                .all(|stage| !stage.to_path().exists())
        );
        let held: Vec<_> = pending[0]
            .members
            .iter()
            .filter_map(|member| member.old.as_ref())
            .map(|file| serde_json::to_value(file).unwrap())
            .collect();
        assert_eq!(held, receipts);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
    std::fs::write(&edited, EDIT_ONE).unwrap();
    let mut completed_receipts = None;
    for recovery_cycle in 0..2 {
        let (result, lookups) =
            cycle(root.path(), current.clone(), true, EditedNaming::Suffix).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_eq!(result.stats.downloaded, 0);
        if recovery_cycle > 0 {
            assert_eq!(
                lookups, 0,
                "quiet reopen must not repeat source confirmation"
            );
        }
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&edited, EDIT_ONE);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        let manifest = db.get_manifest_assets().await.unwrap();
        let preserved: Vec<_> = manifest
            .iter()
            .flat_map(|row| &row.preserved_files)
            .collect();
        assert_eq!(preserved.len(), 2);
        for (version, bytes) in [("original", ORIGINAL), ("adjusted", EDIT_ONE)] {
            let old_file = old.iter().find(|file| file.version == version).unwrap();
            let expected = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let receipt = preserved
                .iter()
                .find(|receipt| receipt.local_checksum == expected)
                .unwrap();
            assert_eq!(receipt.operation, operation.operation);
            assert_eq!(receipt.family, operation.family);
            assert_eq!(receipt.phase, "committed");
            assert_eq!(receipt.previous_generation, operation.generation);
            assert_eq!(receipt.provider_checksum, old_file.provider_checksum);
            assert_eq!(receipt.source_checksum, old_file.source_checksum);
            assert_eq!(receipt.size_bytes, bytes.len() as u64);
            assert_eq!(
                receipt.local_checksum,
                data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
            );
            assert_eq!(std::fs::read(receipt.native_path.to_path()).unwrap(), bytes);
        }
        let snapshot: Vec<_> = preserved.into_iter().cloned().collect();
        if let Some(previous) = &completed_receipts {
            assert_eq!(
                &snapshot, previous,
                "quiet reopen must retain the same receipts"
            );
        }
        completed_receipts = Some(snapshot);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
}

#[tokio::test]
async fn contract_source_checkpoint_requires_durable_recovery_primary_layout_after_database_failure()
 {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let current = records(&server, None);
    let (initial, _) = cycle(root.path(), current.clone(), true, EditedNaming::Primary).await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    let original = media.join("IMG_0501.JPG");
    assert_bytes(&original, ORIGINAL);
    let requests = server.received_requests().await.unwrap().len();
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
    assert!(
        checkpoint.is_some(),
        "precondition: initial checkpoint is durable"
    );
    let operation = download::prepared_layout_with_refresh(
        &db,
        &media,
        "recorded-recovery.JPG",
        "PrimarySync",
        true,
    )
    .await;
    let destination = operation.members[0].destination.as_ref().unwrap().to_path();
    // The recorded journal alone must wake the selected zone on a quiet watch.
    assert!(
        db.get_metadata(&format!(
            "{}PrimarySync",
            crate::sync_cycle::RETAINED_CHECKPOINT_HOLD_PREFIX
        ))
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        !db.has_metadata_capture_work(&["PrimarySync"], state::METADATA_CAPTURE_REVISION)
            .await
            .unwrap()
    );
    assert!(
        db.get_metadata(&state::unresolved_identity_key("PrimarySync"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(db.get_pending().await.unwrap().is_empty());
    use crate::state::ReportStateStore as _;
    assert!(!db.has_download_retry_work("PrimarySync").await.unwrap());
    let selected = crate::sync_loop::test_support::make_run_cycle_library_state(
        "PrimarySync",
        "sync_token",
        "zone_token",
    );
    let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
    crate::sync_loop::precheck::include_pending_local_work(&mut precheck, db.as_ref(), &[selected])
        .await;
    assert!(precheck.should_sync_zone("PrimarySync"));
    assert!(!precheck.should_sync_zone("SharedSync-unselected"));
    assert!(precheck.db_sync_token_after_success().is_none());
    db.acquire_lock("inject recorded layout commit failure").unwrap().execute_batch(
        "CREATE TRIGGER fail_layout_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected layout commit failure'); END"
    ).unwrap();
    drop(db);
    let held = try_cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        CycleOptions::default(),
    )
    .await;
    assert!(held.is_err(), "recorded commit failure must surface");
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&destination, b"prepared current rendition");
    let db = state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync").await.unwrap(),
        checkpoint,
        "recorded work must hold the source checkpoint"
    );
    let pending = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0].refresh_metadata,
        "recorded intent must remain durable"
    );
    db.acquire_lock("clear recorded layout commit failure")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_layout_commit")
        .unwrap();
    drop(db);
    for _ in 0..2 {
        let (recovered, _) = cycle(root.path(), current.clone(), true, EditedNaming::Primary).await;
        assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
        assert!(recovered.db_sync_token_advance_safe);
        assert_eq!(recovered.stats.downloaded, 0);
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&destination, b"prepared current rendition");
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
#[cfg(unix)]
async fn pending_choice_bookmark_only_transaction_fault_reopens_at_sibling() {
    pending_choice_transaction_fault_reopens_at_sibling(PublicationChoiceFault::AfterBookmark)
        .await;
}

#[tokio::test]
#[cfg(unix)]
async fn pending_choice_before_bookmark_transaction_fault_reopens_at_sibling() {
    pending_choice_transaction_fault_reopens_at_sibling(PublicationChoiceFault::BeforeBookmark)
        .await;
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
enum PublicationChoiceFault {
    BeforeBookmark,
    AfterBookmark,
}

#[cfg(unix)]
async fn pending_choice_transaction_fault_reopens_at_sibling(fault: PublicationChoiceFault) {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::MetadataExt;
    let mut trace = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(trace.try_clone().unwrap())
        .finish();
    let _trace_guard = tracing::subscriber::set_default(subscriber);
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let options = |resolution, token| CycleOptions {
        private: true,
        resolution,
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (baseline, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(
            crate::types::PhotoResolution::Original,
            "intent-baseline-token",
        ),
    )
    .await;
    assert_eq!(baseline.failed_count, 0);
    assert!(baseline.db_sync_token_advance_safe);
    let p = root.path().join("media/IMG_0501.JPG");
    let s = root.path().join("media/IMG_0501-asset-501.JPG");
    let before = std::fs::metadata(&p).unwrap();
    let db_path = root.path().join("state.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let proof_sql = "SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original'";
    let proof: String = conn.query_row(proof_sql, [], |r| r.get(0)).unwrap();
    let (scope, pass): (String, String) = conn.query_row("SELECT scope,origin_pass FROM download_receipts WHERE child='asset-501' AND version_size='original'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
    let p_key = crate::fs_util::confined_path_key(&p).unwrap();
    let s_key = crate::fs_util::confined_path_key(&s).unwrap();
    let (trigger, diagnostic) = match fault {
        PublicationChoiceFault::BeforeBookmark => {
            conn.execute_batch(&format!(
                "CREATE TRIGGER before_bookmark_fault BEFORE INSERT ON reconciliation_paths
                 WHEN NEW.library='PrimarySync' AND NEW.id='asset-501' AND NEW.version_size='adjusted'
                 AND NEW.requested_path_key='{}' AND NEW.destination_path_key='{}'
                 AND NEW.destination_path='{}' AND NEW.provider_checksum='{}' AND NEW.provider_size={}
                 BEGIN SELECT RAISE(ABORT,'before-bookmark reconciliation intent write fault'); END;",
                p_key.replace('\'', "''"), s_key.replace('\'', "''"),
                s.to_str().unwrap().replace('\'', "''"), provider_hash(EDIT_ONE), EDIT_ONE.len()
            )).unwrap();
            (
                "before_bookmark_fault",
                "before-bookmark reconciliation intent write fault",
            )
        }
        PublicationChoiceFault::AfterBookmark => {
            conn.execute_batch(&format!(
        "CREATE TRIGGER intent_only_admission_fault BEFORE INSERT ON provider_active_destinations
         WHEN NEW.child='asset-501' AND NEW.version_size='adjusted' BEGIN
         SELECT CASE WHEN NOT EXISTS(SELECT 1 FROM reconciliation_paths r
           WHERE r.library=NEW.library AND r.id=NEW.asset_id AND r.version_size=NEW.version_size
           AND r.provider_checksum=NEW.checksum AND r.provider_size=NEW.size_bytes
           AND r.requested_path_key='{}' AND r.destination_path_key='{}'
           AND r.destination_path=NEW.compat_path AND NEW.admitted=1)
         THEN RAISE(ABORT,'intent-only fault reached without exact bookmark')
         ELSE RAISE(ABORT,'intent-only admission fault after exact bookmark') END; END;",
        p_key.replace('\'', "''"),
        s_key.replace('\'', "''")
    ))
            .unwrap();
            (
                "intent_only_admission_fault",
                "intent-only admission fault after exact bookmark",
            )
        }
    };
    drop(conn);
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let requests = server.received_requests().await.unwrap().len();
    let (faulted, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "intent-fault-token"),
    )
    .await;
    assert!(faulted.failed_count > 0, "{faulted:?}");
    assert!(faulted.stats.state_write_failures > 0, "{faulted:?}");
    assert!(!faulted.db_sync_token_advance_safe);
    assert!(faulted.stats.sync_token_blocked);
    assert_eq!(
        faulted.stats.sync_token_blocked_reason,
        Some("selection_generation_deferred")
    );
    trace.seek(SeekFrom::Start(0)).unwrap();
    let mut diagnostics = String::new();
    trace.read_to_string(&mut diagnostics).unwrap();
    assert!(diagnostics.contains(diagnostic), "{diagnostics}");
    assert!(
        !diagnostics.contains("intent-only fault reached without exact bookmark"),
        "{diagnostics}"
    );
    eprintln!(
        "PUBLICATION_CHOICE_BOUNDARY {fault:?} state_write_failures={} reason={:?}\n{diagnostics}",
        faulted.stats.state_write_failures, faulted.stats.sync_token_blocked_reason
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert!(!s.exists());
    assert_bytes(&p, ORIGINAL);
    let failed_metadata = std::fs::metadata(&p).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (
            failed_metadata.ino(),
            failed_metadata.mtime(),
            failed_metadata.mtime_nsec()
        )
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let map_sql = "SELECT json_array(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths WHERE version_size='adjusted'";
    let mapping = match fault {
        PublicationChoiceFault::BeforeBookmark => {
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM reconciliation_paths WHERE version_size='adjusted'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            json!([
                "PrimarySync",
                "asset-501",
                "adjusted",
                p_key,
                s_key,
                s.to_str().unwrap(),
                provider_hash(EDIT_ONE),
                EDIT_ONE.len()
            ])
            .to_string()
        }
        PublicationChoiceFault::AfterBookmark => {
            let mapping: String = conn.query_row(map_sql, [], |r| r.get(0)).unwrap();
            let (requested, destination, native, checksum, size): (String,String,String,String,i64) = conn.query_row("SELECT requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size FROM reconciliation_paths WHERE version_size='adjusted'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
            assert_eq!(
                (requested, destination, native, checksum, size),
                (
                    p_key,
                    s_key,
                    s.to_str().unwrap().into(),
                    provider_hash(EDIT_ONE),
                    i64::try_from(EDIT_ONE.len()).unwrap()
                )
            );
            mapping
        }
    };
    assert_eq!(conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE version_size='adjusted' AND admitted=1", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM download_receipts WHERE version_size='adjusted'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row(
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "intent-baseline-token"
    );
    assert_eq!(
        conn.query_row(proof_sql, [], |r| r.get::<_, String>(0))
            .unwrap(),
        proof
    );
    drop(conn);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    {
        let reopened = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        reopened
            .acquire_lock("remove only synthetic publication fault")
            .unwrap()
            .execute_batch(&format!("DROP TRIGGER {trigger}"))
            .unwrap();
    }
    let callback_db = db_path.clone();
    let callback_s = s.clone();
    let callback_map = mapping.clone();
    Mock::given(method("GET")).and(path("/edit-one")).respond_with(move |_: &wiremock::Request| {
        let conn = rusqlite::Connection::open_with_flags(&callback_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(conn.query_row(map_sql, [], |r| r.get::<_,String>(0)).unwrap(), callback_map);
        let paths: Vec<String> = conn.prepare("SELECT path FROM provider_active_destinations WHERE version_size='adjusted' AND admitted=1").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(!paths.is_empty());
        for path in paths {
            let path: crate::state::db::provider_selection::SelectionPath = serde_json::from_str(&path).unwrap();
            assert_eq!(path.to_path(), callback_s);
        }
        ResponseTemplate::new(200).set_body_bytes(EDIT_ONE)
    }).with_priority(1).mount(&server).await;
    let (recovered, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "intent-success-token"),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{recovered:?}");
    assert_eq!(recovered.stats.downloaded, 1);
    assert!(recovered.db_sync_token_advance_safe);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests + 1
    );
    assert_bytes(&s, EDIT_ONE);
    {
        let reopened = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        let binding = reopened
            .bind_publication(
                owner.clone(),
                state::db::PublicationBinding {
                    account_key: String::new(),
                    provider_key: String::new(),
                    scope,
                    library: "PrimarySync".into(),
                    child: "asset-501".into(),
                    master: "501".into(),
                    state_id: "asset-501".into(),
                    origin_pass: pass,
                },
            )
            .await
            .unwrap();
        let receipts = reopened
            .bound_download_receipts(
                binding,
                "adjusted".into(),
                provider_hash(EDIT_ONE),
                EDIT_ONE.len() as u64,
            )
            .await
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].native_path.to_path(), s);
        assert_eq!(
            receipts[0].source_checksum,
            format!("{:x}", Sha256::digest(EDIT_ONE))
        );
        let conn = reopened.acquire_lock("verify resumed publication").unwrap();
        assert_eq!(
            conn.query_row(map_sql, [], |r| r.get::<_, String>(0))
                .unwrap(),
            mapping
        );
        assert_eq!(
            conn.query_row(proof_sql, [], |r| r.get::<_, String>(0))
                .unwrap(),
            proof
        );
        assert_eq!(
            conn.query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "intent-success-token"
        );
    }
    let (quiet, lookups) = cycle_options(
        root.path(),
        edited,
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "intent-success-token"),
    )
    .await;
    assert_eq!(quiet.failed_count, 0, "{quiet:?}");
    assert_eq!((quiet.stats.downloaded, lookups), (0, 0));
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests + 1
    );
    assert_bytes(&p, ORIGINAL);
    let after = std::fs::metadata(&p).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec())
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE version_size='adjusted' AND (verified_media=0 OR path<>?1)", [state::db::provider_selection::SelectionPath::from_path(&s).key().unwrap()], |r| r.get::<_,i64>(0)).unwrap(), 0);
    assert_eq!(
        conn.query_row(map_sql, [], |r| r.get::<_, String>(0))
            .unwrap(),
        mapping
    );
    assert_eq!(
        conn.query_row(proof_sql, [], |r| r.get::<_, String>(0))
            .unwrap(),
        proof
    );
}

#[tokio::test]
#[cfg(unix)]
async fn pending_choice_failed_gets_reopen_only_saved_sibling() {
    use std::os::unix::fs::MetadataExt;
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let options = |resolution| CycleOptions {
        // Keep the synthetic transport retry schedule bounded and delay-free.
        retry: Some(crate::retry::RetryConfig {
            max_retries: 1,
            base_delay_secs: 0,
            max_delay_secs: 0,
        }),
        private: true,
        resolution,
        ..CycleOptions::default()
    };
    let (first, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::Original),
    )
    .await;
    assert_eq!(first.failed_count, 0);
    let original = root.path().join("media/IMG_0501.JPG");
    let before = std::fs::metadata(&original).unwrap();
    let db_path = root.path().join("state.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let proof: String = conn.query_row("SELECT json_array(id,source_checksum,local_checksum,proof_hash,native_path) FROM download_receipts WHERE version_size='original'", [], |r| r.get(0)).unwrap();
    drop(conn);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let callback_path = db_path.clone();
    let p = original.clone();
    let failure = Mock::given(method("GET")).and(path("/edit-one"))
        .respond_with(move |_: &wiremock::Request| {
            let conn = rusqlite::Connection::open_with_flags(&callback_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            let (requested, destination): (String, String) = conn.query_row("SELECT requested_path_key,destination_path FROM reconciliation_paths WHERE version_size='adjusted'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(requested, crate::fs_util::confined_path_key(&p).unwrap());
            assert_ne!(destination, p.to_str().unwrap());
            let paths: Vec<String> = conn.prepare("SELECT path FROM provider_active_destinations WHERE version_size='adjusted' AND admitted=1").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
            assert!(!paths.is_empty());
            for path in paths {
                let path: crate::state::db::provider_selection::SelectionPath = serde_json::from_str(&path).unwrap();
                assert_eq!(path.to_path(), std::path::PathBuf::from(&destination));
            }
            seen.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(500)
        }).with_priority(2).mount_as_scoped(&server).await;
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (failed, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None),
    )
    .await;
    assert!(failed.failed_count > 0, "{:?}", failed.stats);
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    drop(failure);
    Mock::given(method("GET"))
        .and(path("/edit-one"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(EDIT_ONE))
        .with_priority(1)
        .mount(&server)
        .await;
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner, "asset-501".into())
            .await
            .unwrap();
    }
    let before_retry = server.received_requests().await.unwrap().len();
    let (recovered, _) = cycle_options(
        root.path(),
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None),
    )
    .await;
    let errors: Vec<(String, Option<String>)> = rusqlite::Connection::open(&db_path)
        .unwrap()
        .prepare("SELECT version_size,last_error FROM assets WHERE id='asset-501'")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        recovered.failed_count, 0,
        "{:?}; {errors:?}",
        recovered.stats
    );
    assert_eq!(recovered.stats.downloaded, 1);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        before_retry + 1
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let sibling: String = conn
        .query_row(
            "SELECT destination_path FROM reconciliation_paths WHERE version_size='adjusted'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_bytes(std::path::Path::new(&sibling), EDIT_ONE);
    assert_eq!(conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE version_size='adjusted' AND verified_media=0", [], |r| r.get::<_,i64>(0)).unwrap(), 0);
    assert_eq!(conn.query_row("SELECT json_array(id,source_checksum,local_checksum,proof_hash,native_path) FROM download_receipts WHERE version_size='original'", [], |r| r.get::<_,String>(0)).unwrap(), proof);
    drop(conn);
    for _ in 0..2 {
        let requests = server.received_requests().await.unwrap().len();
        let (quiet, lookups) = cycle_options(
            root.path(),
            edited.clone(),
            true,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert_eq!((quiet.stats.downloaded, lookups), (0, 0));
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
    assert_bytes(&original, ORIGINAL);
    let after = std::fs::metadata(&original).unwrap();
    assert_eq!(
        (before.ino(), before.mtime(), before.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec())
    );
}

#[tokio::test]
async fn scoped_original_adjusted_only_publishes_distinct_sibling() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = |resolution| CycleOptions {
        private: true,
        resolution,
        ..CycleOptions::default()
    };
    let (first, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::Original),
    )
    .await;
    assert_eq!(first.failed_count, 0, "{:?}", first.stats);
    let original = media.join("IMG_0501.JPG");
    assert_bytes(&original, ORIGINAL);
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (result, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None),
    )
    .await;
    assert_eq!(result.failed_count, 0, "{:?}", result.stats);
    assert_bytes(&original, ORIGINAL);
    let entries: Vec<_> = std::fs::read_dir(&media)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    let sibling = entries
        .iter()
        .find(|path| *path != &original && std::fs::read(path).unwrap() == EDIT_ONE)
        .expect("adjusted sibling must contain edit")
        .clone();
    assert_ne!(sibling, original);
    {
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let conn = db
            .acquire_lock("scoped original and adjusted receipts")
            .unwrap();
        let receipts: Vec<(String, String)> = conn.prepare("SELECT version_size,native_path FROM download_receipts WHERE child='asset-501' AND source_checksum IS NOT NULL ORDER BY version_size")
            .unwrap().query_map([], |r| Ok((r.get(0)?,r.get(1)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(receipts.len(), 2, "{receipts:?}");
        assert_eq!(receipts[0].0, "adjusted");
        assert_eq!(receipts[1].0, "original");
        assert_ne!(receipts[0].1, receipts[1].1);
        let reserved: String = conn.query_row(
            "SELECT destination_path FROM reconciliation_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'",
            [], |r| r.get(0),
        ).unwrap();
        assert_eq!(std::path::Path::new(&reserved), sibling);
    }
    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        let (quiet, _) = cycle_options(
            root.path(),
            edited.clone(),
            true,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&sibling, EDIT_ONE);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
async fn scoped_completed_edit_keeps_old_path_after_naming_change() {
    for scenario in ["completed", "changed", "reservation"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let options = |token| CycleOptions {
            private: true,
            cycle_token: Some(token),
            ..CycleOptions::default()
        };
        let page = records(&server, Some(("edit-one", EDIT_ONE)));
        let (initial, _) = cycle_options(
            root.path(),
            page.clone(),
            false,
            EditedNaming::Suffix,
            options("initial-token"),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{scenario}: {:?}", initial.stats);
        let original = media.join("IMG_0501.JPG");
        let edit = media.join("IMG_0501_edited.JPG");
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&edit, EDIT_ONE);
        let original_mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
        let edit_mtime = std::fs::metadata(&edit).unwrap().modified().unwrap();
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let receipts: Vec<String> = conn.prepare("SELECT json_array(version_size,native_path,provider_checksum,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' ORDER BY version_size").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(receipts.len(), 2);
        let reserved = media.join("saved-adjusted.JPG");
        if scenario == "reservation" {
            let requested = crate::fs_util::confined_path_key(&original).unwrap();
            let destination = crate::fs_util::confined_path_key(&reserved).unwrap();
            conn.execute("INSERT INTO reconciliation_paths(library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) VALUES('PrimarySync','asset-501','adjusted',?1,?2,?3,?4,?5)", rusqlite::params![requested,destination,reserved.to_str().unwrap(),provider_hash(EDIT_ONE),EDIT_ONE.len() as i64]).unwrap();
        }
        drop(conn);
        if scenario == "changed" {
            std::fs::write(&edit, b"changed local edit").unwrap();
        }
        let requests = server.received_requests().await.unwrap().len();
        for round in 0..2 {
            let (result, _) = cycle_options(
                root.path(),
                page.clone(),
                round != 0,
                EditedNaming::Primary,
                options("changed-token"),
            )
            .await;
            assert_bytes(&original, ORIGINAL);
            assert_eq!(
                std::fs::metadata(&original).unwrap().modified().unwrap(),
                original_mtime
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            if scenario == "completed" {
                assert_eq!(result.failed_count, 0, "round {round}: {:?}", result.stats);
                assert!(result.db_sync_token_advance_safe);
                assert_eq!(result.stats.downloaded, 0);
                assert_bytes(&edit, EDIT_ONE);
                assert_eq!(
                    std::fs::metadata(&edit).unwrap().modified().unwrap(),
                    edit_mtime
                );
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                let token: String = conn
                    .query_row(
                        "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(token, "changed-token");
                let after: Vec<String> = conn.prepare("SELECT json_array(version_size,native_path,provider_checksum,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' ORDER BY version_size").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
                assert_eq!(after, receipts);
                let debt: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND admitted=1 AND (verified_media=0 OR verified_metadata=0)", [], |r| r.get(0)).unwrap();
                assert_eq!(debt, 0);
            } else if scenario == "changed" {
                assert!(
                    result.failed_count > 0 || !result.db_sync_token_advance_safe,
                    "{result:?}"
                );
                assert_bytes(&edit, b"changed local edit");
            } else {
                assert_bytes(&edit, EDIT_ONE);
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                let destination: String = conn.query_row("SELECT destination_path FROM reconciliation_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND provider_checksum=?1", [provider_hash(EDIT_ONE)], |r| r.get(0)).unwrap();
                assert_eq!(std::path::Path::new(&destination), reserved);
                let debt: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1 AND verified_media=0", [provider_hash(EDIT_ONE)], |r| r.get(0)).unwrap();
                assert!(debt > 0, "{result:?}");
            }
            assert!(!reserved.exists());
        }
    }
}

#[tokio::test]
async fn scoped_bad_admitted_adjusted_recovers_after_reopen() {
    use crate::state::db::match_diagnostics::MatchDiagnostics;
    use crate::state::db::provider_selection::SelectionPath;
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicBool;

    fn provider_hash(bytes: &[u8]) -> String {
        use sha1::{Digest as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        base64::engine::general_purpose::STANDARD.encode([&[1][..], &hash.finalize()[..]].concat())
    }
    fn rows(db_path: &std::path::Path, sql: &str) -> Vec<Value> {
        let conn = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect()
    }
    let server = server().await;
    // Retain and label the real root before any assertion, including failures.
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("BAD_ADMITTED_ROOT={}", root.display());
    let db_path = root.join("state.db");
    let media = root.join("media");
    let original = media.join("IMG_0501.JPG");
    let page = |edit: Option<(&str, &[u8])>| {
        let mut page = records(&server, edit);
        page[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(provider_hash(ORIGINAL));
        if let Some((_, bytes)) = edit {
            page[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                json!(provider_hash(bytes));
        }
        page
    };
    let file_snapshot = |path: &std::path::Path| {
        let file = std::fs::File::open(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&file).unwrap(),
            file.metadata().unwrap().modified().unwrap(),
        )
    };
    let receipt_sql = "SELECT json_object('id',id,'account',account_key,'provider',provider_key,'scope',scope,'library',library,'child',child,'master',master,'state',state_id,'pass',origin_pass,'version',version_size,'checksum',provider_checksum,'size',provider_size,'path',native_path,'source',source_checksum,'local',local_checksum,'proof',proof_hash,'trust',trust_receipt,'metadata',selected_metadata_hash,'generation',generation_id) FROM download_receipts WHERE library='PrimarySync' AND child='asset-501' AND version_size='original' ORDER BY id";
    let intent_sql = "SELECT json_object('library',library,'child',id,'version',version_size,'requested',requested_path_key,'destination_key',destination_path_key,'destination',destination_path,'checksum',provider_checksum,'size',provider_size) FROM reconciliation_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' ORDER BY provider_checksum,provider_size";
    let debt_sql = "SELECT json_object('generation',g.id,'account',g.account_key,'provider',g.provider_key,'scope',g.scope,'pass',d.pass_key,'library',d.library,'child',d.child,'master',d.master,'version',d.version_size,'path',d.path,'compat_path',d.compat_path,'checksum',d.checksum,'size',d.size_bytes,'admitted',d.admitted,'verified',d.verified_media,'sealed',g.sealed,'ready',g.checkpoint_ready) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.version_size='adjusted' ORDER BY g.id,d.pass_key,d.path";
    let checkpoint_sql =
        "SELECT json_quote(value) FROM metadata WHERE key='sync_token:PrimarySync'";
    let options = |resolution, cycle_token| CycleOptions {
        retry: Some(crate::retry::RetryConfig {
            max_retries: 1,
            base_delay_secs: 0,
            max_delay_secs: 0,
        }),
        private: true,
        resolution,
        cycle_token: Some(cycle_token),
        ..CycleOptions::default()
    };
    let (first, _) = cycle_options(
        &root,
        page(None),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::Original, "original-token"),
    )
    .await;
    assert_eq!(first.failed_count, 0);
    assert_eq!(rows(&db_path, checkpoint_sql), [json!("original-token")]);
    assert_bytes(&original, ORIGINAL);
    let original_file = file_snapshot(&original);
    let original_receipts = rows(&db_path, receipt_sql);
    assert_eq!(original_receipts.len(), 1);
    let mapping_sql = "SELECT json_array(library,asset_record_name,master_record_name) FROM asset_master_mappings ORDER BY library,asset_record_name";
    let original_mapping = rows(&db_path, mapping_sql);
    let edited_only = page(Some(("edit-one", EDIT_ONE)));
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let fault_enabled = Arc::new(AtomicBool::new(true));
    let fail_requests = fault_enabled.clone();
    Mock::given(method("GET"))
        .and(path("/edit-one"))
        .respond_with(move |_: &wiremock::Request| {
            if fail_requests.load(Ordering::SeqCst) {
                seen.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_bytes(EDIT_ONE)
            }
        })
        .with_priority(2)
        .mount(&server)
        .await;
    let (failed, _) = cycle_options(
        &root,
        edited_only.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "debt-token"),
    )
    .await;
    assert!(failed.failed_count > 0, "{failed:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    assert!(failed.db_sync_token_advance_safe);
    let intent = rows(&db_path, intent_sql);
    assert_eq!(intent.len(), 1);
    let sibling = std::path::PathBuf::from(intent[0]["destination"].as_str().unwrap());
    assert_ne!(sibling, original);
    assert!(sibling.starts_with(&media));
    assert!(!sibling.exists());
    assert_eq!(
        intent[0]["requested"],
        crate::fs_util::confined_path_key(&original).unwrap()
    );
    assert_eq!(
        intent[0]["destination_key"],
        crate::fs_util::confined_path_key(&sibling).unwrap()
    );
    assert_eq!(intent[0]["checksum"], provider_hash(EDIT_ONE));
    assert_eq!(intent[0]["size"], EDIT_ONE.len());
    let debt = rows(&db_path, debt_sql);
    let assert_debt = |debt: &[Value]| {
        for d in debt {
            for key in [
                "account", "provider", "scope", "pass", "library", "child", "master",
            ] {
                assert_eq!(d[key], original_receipts[0][key], "{key}: {d:?}");
            }
            let native: SelectionPath = serde_json::from_str(d["path"].as_str().unwrap()).unwrap();
            assert_eq!(native.to_path(), sibling);
            assert_eq!(d["compat_path"], sibling.to_str().unwrap());
            assert_eq!(d["checksum"], provider_hash(EDIT_ONE));
            assert_eq!(d["size"], EDIT_ONE.len());
            assert_eq!(d["version"], "adjusted");
            for key in ["admitted", "sealed", "ready"] {
                assert_eq!(d[key], 1, "{key}: {d:?}");
            }
            assert_eq!(d["verified"], 0);
        }
    };
    assert_debt(&debt);
    let generations: BTreeSet<_> = debt
        .iter()
        .map(|d| d["generation"].as_str().unwrap())
        .collect();
    assert!(generations.len() >= 2, "{debt:?}");
    assert_eq!(generations.len(), debt.len(), "one saved S per generation");
    assert_eq!(rows(&db_path, checkpoint_sql), [json!("debt-token")]);
    assert_eq!(rows(&db_path, receipt_sql), original_receipts);
    assert_eq!(rows(&db_path, mapping_sql), original_mapping);
    assert_eq!(file_snapshot(&original), original_file);
    // Keep peer mocks installed; scoped deactivation misindexes priority-sorted mocks.
    fault_enabled.store(false, Ordering::SeqCst);
    let error_sql = "SELECT json_quote(last_error) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'";
    let failed_error = rows(&db_path, error_sql);
    assert_eq!(failed_error.len(), 1);
    assert!(MatchDiagnostics::decode(failed_error[0].as_str().unwrap()).is_none());
    std::fs::write(&sibling, b"foreign obstruction at frozen S").unwrap();
    let obstruction = file_snapshot(&sibling);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    // Capture expected publication identity before recovery, never from its receipt.
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    assert_eq!(debt[0]["account"], account_key);
    assert_eq!(debt[0]["provider"], provider_key);
    let expected_binding = state::db::PublicationBinding {
        account_key: account_key.into(),
        provider_key: provider_key.into(),
        scope: debt[0]["scope"].as_str().unwrap().into(),
        library: "PrimarySync".into(),
        child: "asset-501".into(),
        master: "501".into(),
        state_id: "asset-501".into(),
        origin_pass: debt[0]["pass"].as_str().unwrap().into(),
    };
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let mut edited = edited_only;
    let mut healthy = page(None);
    for record in &mut healthy {
        record["recordName"] = json!(record["recordName"].as_str().unwrap().replace("501", "502"));
        if let Some(reference) = record["fields"].get_mut("masterRef") {
            reference["value"]["recordName"] = json!("502");
        }
    }
    healthy[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    healthy[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)});
    healthy[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)}});
    healthy[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
    healthy[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    edited.extend(healthy);
    let (held, _) = cycle_options(
        &root,
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "debt-token-2"),
    )
    .await;
    assert!(held.failed_count > 0, "{:?}", held.stats);
    assert!(held.db_sync_token_advance_safe);
    assert_eq!(held.stats.downloaded, 1, "{held:?}");
    assert_bytes(&media.join("IMG_0502.JPG"), EDIT_TWO);
    let blocked_debt = rows(&db_path, debt_sql);
    assert_debt(&blocked_debt);
    for initial in &debt {
        assert!(
            blocked_debt.contains(initial),
            "lost initial debt: {initial:?}; {blocked_debt:?}"
        );
    }
    let blocked_generations: BTreeSet<_> = blocked_debt
        .iter()
        .map(|d| d["generation"].as_str().unwrap())
        .collect();
    assert!(generations.is_subset(&blocked_generations));
    assert_eq!(
        blocked_generations.len(),
        blocked_debt.len(),
        "one saved S per generation"
    );
    let error = rows(&db_path, error_sql);
    assert_ne!(error, failed_error);
    let diagnostics = MatchDiagnostics::decode(error[0].as_str().unwrap()).unwrap();
    assert!(
        diagnostics.attempts.iter().any(|a| {
            a.child == "asset-501"
                && a.checksum == provider_hash(EDIT_ONE)
                && a.destination.to_path() == sibling
                && blocked_debt.iter().any(|d| {
                    a.generation.as_deref() == d["generation"].as_str()
                        && a.pass.as_deref() == d["pass"].as_str()
                })
                && a.explanation.candidates.iter().any(|c| {
                    c.path.as_ref().is_some_and(|p| p.to_path() == sibling)
                        && c.reason == "candidate_bytes_not_verified_for_current_resource"
                })
        }),
        "{}",
        error[0]
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "GET" && r.url.path() == "/edit-one")
            .count(),
        8
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "GET" && r.url.path() == "/edit-two")
            .count(),
        1
    );
    assert_eq!(rows(&db_path, checkpoint_sql), [json!("debt-token-2")]);
    assert_eq!(rows(&db_path, intent_sql), intent);
    assert_eq!(rows(&db_path, receipt_sql), original_receipts);
    assert_eq!(file_snapshot(&original), original_file);
    assert_eq!(file_snapshot(&sibling), obstruction);
    let healthy_file = file_snapshot(&media.join("IMG_0502.JPG"));
    // Only the fixture obstruction is removed; P is never restored or recertified.
    std::fs::remove_file(&sibling).unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let (recovered, _) = cycle_options(
        &root,
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "recovered-token"),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert_eq!(recovered.stats.downloaded, 1);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests.len() + 1
    );
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&sibling, EDIT_ONE);
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        let conn = db.acquire_lock("recovered adjusted ownership").unwrap();
        let incomplete: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0", [], |r|r.get(0)).unwrap();
        assert_eq!(incomplete, 0);
        let receipts: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE child='asset-501' AND version_size IN ('original','adjusted') AND source_checksum IS NOT NULL", [], |r|r.get(0)).unwrap();
        assert_eq!(receipts, 2);
    }
    assert_eq!(rows(&db_path, intent_sql), intent);
    assert_eq!(rows(&db_path, receipt_sql), original_receipts);
    assert_eq!(rows(&db_path, checkpoint_sql), [json!("recovered-token")]);
    let recovered_file = file_snapshot(&sibling);
    // Reuse the complete 19-column projection, including ID/proof and trust fields.
    let adjusted_receipt_sql =
        receipt_sql.replace("version_size='original'", "version_size='adjusted'");
    let adjusted_receipt = rows(&db_path, &adjusted_receipt_sql);
    assert_eq!(adjusted_receipt.len(), 1);
    for (key, expected) in [
        ("account", &expected_binding.account_key),
        ("provider", &expected_binding.provider_key),
        ("scope", &expected_binding.scope),
        ("pass", &expected_binding.origin_pass),
        ("library", &expected_binding.library),
        ("child", &expected_binding.child),
        ("master", &expected_binding.master),
        ("state", &expected_binding.state_id),
    ] {
        assert_eq!(adjusted_receipt[0][key], expected.as_str(), "{key}");
    }
    assert_eq!(adjusted_receipt[0]["version"], "adjusted");
    for key in ["trust", "metadata", "generation"] {
        assert!(adjusted_receipt[0][key].is_null(), "source-backed {key}");
    }
    let native: SelectionPath =
        serde_json::from_str(adjusted_receipt[0]["path"].as_str().unwrap()).unwrap();
    assert_eq!(native.to_path(), sibling);
    assert_eq!(adjusted_receipt[0]["checksum"], provider_hash(EDIT_ONE));
    assert_eq!(adjusted_receipt[0]["size"], EDIT_ONE.len());
    assert_eq!(
        adjusted_receipt[0]["source"],
        format!("{:x}", Sha256::digest(EDIT_ONE))
    );
    assert_eq!(adjusted_receipt[0]["local"], adjusted_receipt[0]["source"]);
    // The owner reader validates the saved ID and recomputed proof, not just hashes' shape.
    let verified_receipts = {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.bound_download_receipts(
            expected_binding.clone(),
            "adjusted".into(),
            provider_hash(EDIT_ONE),
            EDIT_ONE.len() as u64,
        )
        .await
        .unwrap()
    };
    assert_eq!(verified_receipts.len(), 1);
    assert_eq!(
        adjusted_receipt[0]["id"],
        verified_receipts[0].id().unwrap()
    );
    for _ in 0..2 {
        let before_requests = server.received_requests().await.unwrap().len();
        let (quiet, lookups) = cycle_options(
            &root,
            edited.clone(),
            true,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None, "recovered-token"),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert_eq!((quiet.stats.downloaded, lookups), (0, 0));
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            before_requests
        );
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&sibling, EDIT_ONE);
        assert_eq!(file_snapshot(&original), original_file);
        assert_eq!(file_snapshot(&sibling), recovered_file);
        assert_eq!(file_snapshot(&media.join("IMG_0502.JPG")), healthy_file);
        assert_eq!(rows(&db_path, intent_sql), intent);
        assert_eq!(rows(&db_path, receipt_sql), original_receipts);
        assert_eq!(rows(&db_path, &adjusted_receipt_sql), adjusted_receipt);
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        assert_eq!(
            db.bound_download_receipts(
                expected_binding.clone(),
                "adjusted".into(),
                provider_hash(EDIT_ONE),
                EDIT_ONE.len() as u64,
            )
            .await
            .unwrap(),
            verified_receipts
        );
        assert_eq!(rows(&db_path, checkpoint_sql), [json!("recovered-token")]);
        assert_eq!(
            db_count(
                &root,
                "SELECT count(*) FROM provider_active_destinations WHERE library='PrimarySync' AND child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0"
            ),
            0
        );
        assert_eq!(std::fs::read_dir(&media).unwrap().count(), 3);
    }
}

#[tokio::test]
async fn scoped_distinct_admitted_edits_publish_current_and_preserve_old_debt() {
    use rusqlite::OptionalExtension;
    use std::collections::BTreeSet;
    use std::os::unix::fs::MetadataExt;
    fn rows(
        conn: &rusqlite::Connection,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Vec<Vec<rusqlite::types::Value>> {
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map(params, |row| {
                (0..columns).map(|index| row.get(index)).collect()
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
    fn receipt(conn: &rusqlite::Connection) -> Vec<Vec<rusqlite::types::Value>> {
        rows(
            conn,
            "SELECT * FROM download_receipts WHERE child='asset-501' AND version_size='original' ORDER BY id",
            &[],
        )
    }
    fn immutable_debt(conn: &rusqlite::Connection) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let checksum = provider_hash(EDIT_ONE);
        let mut snapshot = [
            "SELECT g.id,g.account_key,g.provider_key,g.scope,g.config_hash,g.basis,g.metadata_enabled,g.specification,g.specification_hash,g.sealed,g.checkpoint_ready,g.seal_hash,g.seal_header_hash FROM provider_active_generations g WHERE g.id IN (SELECT generation FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1) ORDER BY g.id",
            "SELECT p.generation,p.pass_key,p.child,p.manifest,p.manifest_hash,p.outcome,p.admission,p.compacted_hash FROM provider_active_decisions p WHERE p.generation IN (SELECT generation FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1) ORDER BY p.generation,p.pass_key,p.child",
            "SELECT d.* FROM provider_active_destinations d WHERE d.generation IN (SELECT generation FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1) ORDER BY d.generation,d.pass_key,d.child,d.version_size,d.path",
            "SELECT * FROM reconciliation_paths WHERE id='asset-501' AND version_size='adjusted' AND provider_checksum=?1 ORDER BY library,id,requested_path_key,destination_path_key",
        ].map(|sql|rows(conn,sql,&[&checksum])).to_vec();
        snapshot.push(rows(
            conn,
            "SELECT * FROM owned_temp_files ORDER BY path",
            &[],
        ));
        snapshot
    }
    fn schedule(conn: &rusqlite::Connection) -> Vec<(String, String, u32, i64, String, String)> {
        conn.prepare("SELECT p.generation,p.pass_key,p.attempts,p.next_retry_at,p.reason,p.admission_hash FROM provider_active_decisions p WHERE p.generation IN (SELECT generation FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1) ORDER BY p.generation,p.pass_key")
            .unwrap().query_map([provider_hash(EDIT_ONE)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
            .unwrap().collect::<Result<_,_>>().unwrap()
    }
    fn policy(conn: &rusqlite::Connection) -> (Option<String>, Option<String>) {
        let get = |key| {
            conn.query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()
            .unwrap()
        };
        (get("config_hash"), get("pending_download_config_hash"))
    }
    fn policy_config(
        root: &std::path::Path,
        db: Arc<state::SqliteStateDb>,
        resolution: crate::types::PhotoResolution,
    ) -> Arc<download::DownloadConfig> {
        let media = root.join("media");
        let build = make_run_cycle_download_config_builder(&media, db);
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            "PrimarySync".into(),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        config.resolution = resolution;
        Arc::new(config)
    }
    async fn check_local_recovery(
        db: Arc<state::SqliteStateDb>,
        owner: &state::db::account::AccountOwner,
        config: Arc<download::DownloadConfig>,
        source: Vec<Value>,
        server: &MockServer,
    ) {
        let provider = Session {
            reported_count: None,
            records: source,
            quiet: true,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
            "private".into(),
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        let requests = server.received_requests().await.unwrap().len();
        let cancel = CancellationToken::new();
        let result = download::recover_recorded_sources(
            &reqwest::Client::new(),
            &[pass],
            config,
            cancel.clone(),
        )
        .await
        .unwrap();
        eprintln!(
            "OLDER_DEBT_LOCAL_GATE complete={} smart_query={} failed={} state_errors={} enumeration_errors={} cancelled={}",
            result.complete,
            result.complete_after_smart_query,
            result.stats.failed,
            result.stats.state_write_failures,
            result.stats.enumeration_errors,
            cancel.is_cancelled()
        );
        assert!(result.complete && !result.complete_after_smart_query);
        assert_eq!(
            (
                result.stats.failed,
                result.stats.state_write_failures,
                result.stats.enumeration_errors
            ),
            (0, 0, 0)
        );
        assert!(!cancel.is_cancelled());
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let conn = db
            .acquire_lock("recorded recovery indexing and source holds")
            .unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT value FROM metadata WHERE key=?1",
                [crate::sync_cycle::indexing_reconciliation_key(
                    "PrimarySync"
                )],
                |r| r.get::<_, String>(0)
            )
            .optional()
            .unwrap(),
            None
        );
    }
    let server = server().await;
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("OLDER_DEBT_RETAINED_FIXTURE {}", root.display());
    let media = root.join("media");
    let original = media.join("IMG_0501.JPG");
    let db_path = root.join("state.db");
    let options = |resolution, token| CycleOptions {
        retry: Some(crate::retry::RetryConfig {
            max_retries: 1,
            base_delay_secs: 0,
            max_delay_secs: 0,
        }),
        private: true,
        resolution,
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        &root,
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::Original, "initial-token"),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    assert_bytes(&original, ORIGINAL);
    let metadata = std::fs::metadata(&original).unwrap();
    let original_identity = (metadata.ino(), metadata.mtime(), metadata.mtime_nsec());
    let conn =
        rusqlite::Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let original_receipt = receipt(&conn);
    assert_eq!(original_receipt.len(), 1);
    let (account,provider,scope,pass,source_sha,local_sha):(String,String,String,String,String,String) = conn.query_row("SELECT account_key,provider_key,scope,origin_pass,source_checksum,local_checksum FROM download_receipts WHERE version_size='original'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
    assert_eq!(
        source_sha,
        data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL))
    );
    assert_eq!(source_sha, local_sha);
    let initial_policy = policy(&conn);
    drop(conn);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let (target, initial_target, legacy_initial_target) = {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap(),
        );
        let initial_config =
            policy_config(&root, db.clone(), crate::types::PhotoResolution::Original);
        let current_config = policy_config(&root, db.clone(), crate::types::PhotoResolution::None);
        let initial_target = download::hash_download_config(&initial_config);
        let legacy_initial_target = download::hash_legacy_download_config(&initial_config);
        let target = download::hash_download_config(&current_config);
        eprintln!(
            "OLDER_DEBT_EXPECTED_POLICY initial={initial_target} legacy_initial={legacy_initial_target} target={target} observed_initial={initial_policy:?}"
        );
        assert_eq!(initial_policy, (Some(initial_target.clone()), None));
        assert_ne!(initial_target, target);
        check_local_recovery(
            db.clone(),
            &owner,
            current_config,
            records(&server, Some(("edit-one", EDIT_ONE))),
            &server,
        )
        .await;
        let conn = db
            .acquire_lock("local recovery preserves original proof and applied policy")
            .unwrap();
        assert_eq!(receipt(&conn), original_receipt);
        assert_eq!(policy(&conn), initial_policy);
        (target, initial_target, legacy_initial_target)
    };
    eprintln!(
        "OLDER_DEBT_POLICY_HASH_OWNER target={target} initial={initial_target} legacy_initial={legacy_initial_target}"
    );
    let assert_original = |conn: &rusqlite::Connection| {
        assert_bytes(&original, ORIGINAL);
        let metadata = std::fs::metadata(&original).unwrap();
        assert_eq!(
            (metadata.ino(), metadata.mtime(), metadata.mtime_nsec()),
            original_identity
        );
        assert_eq!(receipt(conn), original_receipt);
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let callback_path = db_path.clone();
    let p = original.clone();
    let proof = original_receipt.clone();
    let expected_account = account.clone();
    let expected_provider = provider.clone();
    let expected_scope = scope.clone();
    let expected_pass = pass.clone();
    let expected_initial_policy = initial_policy.clone();
    let expected_pending = target.clone();
    // A finite fault avoids wiremock 0.6.5 scoped IDs becoming stale when
    // priority sorting moves the scoped entry away from its registered index.
    Mock::given(method("GET")).and(path("/edit-one")).respond_with(move |_:&wiremock::Request| {
        let conn=rusqlite::Connection::open_with_flags(&callback_path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let staged=policy(&conn);
        eprintln!("OLDER_DEBT_POLICY_BEFORE_PROMOTION applied={:?} pending={:?} expected_target={expected_pending}",staged.0,staged.1);
        assert_eq!(staged,(expected_initial_policy.0.clone(),Some(expected_pending.clone())));
        let (requested,destination,key,checksum,size):(String,String,String,String,i64)=conn.query_row("SELECT requested_path_key,destination_path,destination_path_key,provider_checksum,provider_size FROM reconciliation_paths WHERE id='asset-501' AND version_size='adjusted' AND provider_checksum=?1",[provider_hash(EDIT_ONE)],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
        assert_eq!(requested,crate::fs_util::confined_path_key(&p).unwrap());
        let s=std::path::PathBuf::from(destination);
        assert_ne!(s,p);
        assert_eq!(key,crate::fs_util::confined_path_key(&s).unwrap());
        assert_eq!((checksum,size),(provider_hash(EDIT_ONE),EDIT_ONE.len() as i64));
        assert!(!s.exists());
        let obligations:Vec<String>=conn.prepare("SELECT json_array(d.generation,d.pass_key,d.child,d.library,d.asset_id,d.master,d.version_size,d.path,d.checksum,d.size_bytes,d.admitted,d.verified_media,g.account_key,g.provider_key,g.scope,json_extract(CAST(g.specification AS TEXT),'$.format')) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-501' AND d.version_size='adjusted' AND d.admitted=1 ORDER BY d.generation").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(!obligations.is_empty());
        for obligation in obligations {
            let row:Value=serde_json::from_str(&obligation).unwrap();
            let root_id=row[0].as_str().unwrap();
            let native:state::db::provider_selection::SelectionPath=serde_json::from_str(row[7].as_str().unwrap()).unwrap();
            assert_eq!(native.to_path(),s);
            assert_eq!(row,json!([root_id,expected_pass,"asset-501","PrimarySync","asset-501","501","adjusted",row[7],provider_hash(EDIT_ONE),EDIT_ONE.len(),1,0,expected_account,expected_provider,expected_scope,2]));
            eprintln!("OLDER_DEBT_FAILED_GET root={root_id} pass={} S={} checksum={} admitted=1 media=0",expected_pass,s.display(),provider_hash(EDIT_ONE));
        }
        assert_bytes(&p,ORIGINAL);
        let metadata=std::fs::metadata(&p).unwrap();
        assert_eq!((metadata.ino(),metadata.mtime(),metadata.mtime_nsec()),original_identity);
        assert_eq!(receipt(&conn),proof);
        assert_eq!(conn.query_row("SELECT count(*) FROM download_receipts WHERE version_size='adjusted'",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        seen.fetch_add(1,Ordering::SeqCst);
        ResponseTemplate::new(500)
    }).with_priority(2).up_to_n_times(8).expect(8).mount(&server).await;
    let first = records(&server, Some(("edit-one", EDIT_ONE)));
    let (old, _) = cycle_options(
        &root,
        first,
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "old-edit-token"),
    )
    .await;
    eprintln!(
        "OLDER_DEBT_TRANSPORT old={old:?} failed_GETs={}",
        calls.load(Ordering::SeqCst)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    assert!(old.stats.failed > 0 && old.failed_count > 0);
    assert!(!old.session_expired && !old.stats.interrupted && !old.stats.identity_incomplete);
    assert_eq!(
        (
            old.stats.downloaded,
            old.stats.state_write_failures,
            old.stats.enumeration_errors
        ),
        (0, 0, 0)
    );
    // Every cycle helper has dropped its DB/source handles before this owned reopen.
    let (s, old_snapshot, fence, temp_custody, expected_old_obligations) = {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap(),
        );
        let (before_gate, before_policy) = {
            let conn = db
                .acquire_lock("capture actual promotion before recovery verification")
                .unwrap();
            let current_policy = policy(&conn);
            eprintln!(
                "OLDER_DEBT_POLICY_OWNED_REOPEN applied={:?} pending={:?} computed_target={target}",
                current_policy.0, current_policy.1
            );
            (immutable_debt(&conn), current_policy)
        };
        check_local_recovery(
            db.clone(),
            &owner,
            policy_config(&root, db.clone(), crate::types::PhotoResolution::None),
            records(&server, Some(("edit-one", EDIT_ONE))),
            &server,
        )
        .await;
        let custody = db
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|file| {
                let bytes = match std::fs::read(&file.path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("temp custody read: {error}"),
                };
                (file, bytes)
            })
            .collect::<Vec<_>>();
        let conn = db.acquire_lock("reopen actual failed media debt").unwrap();
        assert_original(&conn);
        let s:String=conn.query_row("SELECT destination_path FROM reconciliation_paths WHERE version_size='adjusted' AND provider_checksum=?1",[provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap();
        let snapshot = immutable_debt(&conn);
        assert_eq!(
            snapshot, before_gate,
            "local recovery must not settle format2 media"
        );
        assert!(!snapshot[0].is_empty());
        let roots:Vec<String>=conn.prepare("SELECT DISTINCT generation FROM provider_active_destinations WHERE checksum=?1 AND admitted=1 AND verified_media=0 ORDER BY generation").unwrap().query_map([provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(roots.len(), snapshot[0].len());
        let expected_old_obligations: BTreeSet<(String, String)> = schedule(&conn)
            .into_iter()
            .map(|row| (row.0, row.1))
            .collect();
        assert!(!expected_old_obligations.is_empty());
        eprintln!("OLDER_DEBT_EXPECTED_OLD_OBLIGATIONS {expected_old_obligations:?}");
        assert_eq!(conn.query_row("SELECT count(*) FROM provider_active_generations WHERE id IN (SELECT generation FROM provider_active_destinations WHERE checksum=?1) AND (sealed=0 OR seal_hash IS NULL)",[provider_hash(EDIT_ONE)],|r|r.get::<_,i64>(0)).unwrap(),0);
        let fence = policy(&conn);
        eprintln!(
            "OLDER_DEBT_POLICY_AFTER_LOCAL_GATE applied={:?} pending={:?} computed_target={target}",
            fence.0, fence.1
        );
        assert_eq!(fence, before_policy);
        assert_eq!(fence, (Some(target.clone()), None));
        eprintln!(
            "OLDER_DEBT_REOPEN roots={roots:?} S={s} immutable={snapshot:?} scheduling={:?} config_hash={:?} pending_hash={:?} temp_custody={custody:?}",
            schedule(&conn),
            fence.0,
            fence.1
        );
        (
            std::path::PathBuf::from(s),
            snapshot,
            fence,
            custody,
            expected_old_obligations,
        )
    };
    let current = records(&server, Some(("edit-two", EDIT_TWO)));
    let result = try_cycle_options(
        &root,
        current.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "current-edit-token"),
    )
    .await;
    eprintln!("OLDER_DEBT_CURRENT result={result:?}");
    if !result.as_ref().is_ok_and(|(cycle, _)| {
        cycle.stats.downloaded == 1
            && cycle.stats.failed == 0
            && cycle.stats.state_write_failures == 0
            && cycle.stats.enumeration_errors == 0
    }) {
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        eprintln!(
            "OLDER_DEBT_OBSTRUCTION immutable={:?} schedule={:?} mapping={:?} policy={:?}",
            immutable_debt(&conn),
            schedule(&conn),
            rows(
                &conn,
                "SELECT * FROM reconciliation_paths ORDER BY provider_checksum",
                &[]
            ),
            policy(&conn)
        );
        drop(conn);
        panic!(
            "current EDIT_TWO did not independently publish; retained={}",
            root.display()
        );
    }
    let (new, _) = result.unwrap();
    let (r, current_receipt) = {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        let conn = db
            .acquire_lock("verify independent R and retained S")
            .unwrap();
        assert_original(&conn);
        assert_eq!(immutable_debt(&conn), old_snapshot);
        assert_eq!(policy(&conn), fence);
        let native:String=conn.query_row("SELECT native_path FROM download_receipts WHERE version_size='adjusted' AND provider_checksum=?1",[provider_hash(EDIT_TWO)],|r|r.get(0)).unwrap();
        let r = serde_json::from_str::<state::db::provider_selection::SelectionPath>(&native)
            .unwrap()
            .to_path();
        assert_ne!(r, original);
        assert_ne!(r, s);
        assert!(!s.exists());
        assert_bytes(&r, EDIT_TWO);
        let proof = rows(
            &conn,
            "SELECT * FROM download_receipts WHERE version_size='adjusted' AND provider_checksum=?1 ORDER BY id",
            &[&provider_hash(EDIT_TWO)],
        );
        assert_eq!(proof.len(), 1);
        let checksum:String=conn.query_row("SELECT source_checksum FROM download_receipts WHERE version_size='adjusted' AND provider_checksum=?1 AND account_key=?2 AND provider_key=?3 AND scope=?4 AND origin_pass=?5 AND child='asset-501' AND master='501' AND state_id='asset-501' AND source_checksum=local_checksum",rusqlite::params![provider_hash(EDIT_TWO),account,provider,scope,pass],|r|r.get(0)).unwrap();
        assert_eq!(
            checksum,
            data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_TWO))
        );
        let sealed:i64=conn.query_row("SELECT count(*) FROM provider_active_generations WHERE sealed=1 AND checkpoint_ready=1 AND seal_hash IS NOT NULL AND id IN (SELECT generation FROM provider_active_destinations WHERE checksum=?1 AND verified_media=1)",[provider_hash(EDIT_TWO)],|r|r.get(0)).unwrap();
        assert!(
            sealed > 0,
            "current token needs actual accounted selection/seal"
        );
        assert!(new.db_sync_token_advance_safe);
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "current-edit-token");
        assert_eq!(conn.query_row("SELECT count(*) FROM download_receipts WHERE version_size='adjusted' AND provider_checksum=?1",[provider_hash(EDIT_ONE)],|r|r.get::<_,i64>(0)).unwrap(),0);
        eprintln!(
            "OLDER_DEBT_CURRENT_PUBLISHED R={} old_S={} sealed_current={sealed} failed_count={} errors=0 downloaded=1 config={:?}",
            r.display(),
            s.display(),
            new.failed_count,
            policy(&conn)
        );
        (r, proof)
    };
    let metadata = std::fs::metadata(&r).unwrap();
    let r_identity = (metadata.ino(), metadata.mtime(), metadata.mtime_nsec());
    let mut reconsidered_old_obligations = BTreeSet::new();
    for round in 0..2 {
        let before_schedule = {
            let db = state::SqliteStateDb::open_owned(&db_path, &owner)
                .await
                .unwrap();
            db.make_selection_retry_due(owner.clone(), "asset-501".into())
                .await
                .unwrap();
            let conn = db.acquire_lock("prove due old rediscovery").unwrap();
            let before = schedule(&conn);
            assert!(
                before
                    .iter()
                    .any(|row| row.3 <= chrono::Utc::now().timestamp())
            );
            assert_eq!(immutable_debt(&conn), old_snapshot);
            before
        };
        let trace: RequestTrace = Arc::default();
        let mut quiet_options = options(crate::types::PhotoResolution::None, "current-edit-token");
        quiet_options.trace = Some(trace.clone());
        let (quiet, lookups) = cycle_options(
            &root,
            current.clone(),
            true,
            EditedNaming::Primary,
            quiet_options,
        )
        .await;
        assert_eq!(quiet.stats.downloaded, 0);
        assert!(lookups > 0, "backoff skip is not rediscovery");
        assert!(
            trace
                .lock()
                .unwrap()
                .iter()
                .any(|(kind, _)| kind == "lookup")
        );
        assert_eq!(
            (
                quiet.stats.failed,
                quiet.stats.state_write_failures,
                quiet.stats.enumeration_errors
            ),
            (0, 0, 0)
        );
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        let custody = db
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|file| {
                let bytes = match std::fs::read(&file.path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("temp custody read: {error}"),
                };
                (file, bytes)
            })
            .collect::<Vec<_>>();
        assert_eq!(custody, temp_custody);
        {
            let conn = db
                .acquire_lock("reopened immutable historical debt and fresh backoff")
                .unwrap();
            let old_receipts: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE version_size='adjusted' AND provider_checksum=?1",[provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap();
            eprintln!("OLDER_DEBT_QUIET_OLD_RECEIPTS round={round} count={old_receipts}");
            assert_eq!(
                old_receipts, 0,
                "quiet rediscovery must not complete old media"
            );
            assert_original(&conn);
            assert_bytes(&r, EDIT_TWO);
            assert!(!s.exists());
            let metadata = std::fs::metadata(&r).unwrap();
            assert_eq!(
                (metadata.ino(), metadata.mtime(), metadata.mtime_nsec()),
                r_identity
            );
            assert_eq!(immutable_debt(&conn), old_snapshot);
            assert_eq!(policy(&conn), fence);
            assert_eq!(
                rows(
                    &conn,
                    "SELECT * FROM download_receipts WHERE version_size='adjusted' AND provider_checksum=?1 ORDER BY id",
                    &[&provider_hash(EDIT_TWO)]
                ),
                current_receipt
            );
            let after = schedule(&conn);
            assert_eq!(after.len(), before_schedule.len());
            let mut reconsidered = 0;
            for (before, after) in before_schedule.iter().zip(&after) {
                assert_eq!((&before.0, &before.1), (&after.0, &after.1));
                assert!(after.2 >= before.2);
                if after.2 > before.2 {
                    reconsidered += 1;
                    reconsidered_old_obligations.insert((after.0.clone(), after.1.clone()));
                    assert_eq!(after.4, "current_confirmation_unavailable");
                    assert!(after.3 > chrono::Utc::now().timestamp());
                    assert_ne!(after.5, before.5);
                }
            }
            assert!(
                reconsidered > 0,
                "fresh lookup did not reconsider old decision"
            );
            eprintln!(
                "OLDER_DEBT_QUIET round={round} lookups={lookups} before={before_schedule:?} after={after:?} immutable=unchanged P_R=unchanged applied_config=computed pending_config=None"
            );
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path() == "/edit-one")
                .count(),
            8
        );
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path() == "/edit-two")
                .count(),
            1
        );
    }
    eprintln!(
        "OLDER_DEBT_RECONSIDERED_OLD_OBLIGATIONS expected={expected_old_obligations:?} observed={reconsidered_old_obligations:?}"
    );
    assert_eq!(
        reconsidered_old_obligations, expected_old_obligations,
        "both quiet rounds must reconsider the full captured old obligation set"
    );
}

#[tokio::test]
async fn scoped_missing_original_keeps_admitted_adjusted_pending() {
    use crate::state::db::match_diagnostics::MatchDiagnostics;
    use crate::state::db::provider_selection::SelectionPath;
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicBool;

    fn provider_hash(bytes: &[u8]) -> String {
        use sha1::{Digest as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        base64::engine::general_purpose::STANDARD.encode([&[1][..], &hash.finalize()[..]].concat())
    }
    fn rows(db_path: &std::path::Path, sql: &str) -> Vec<Value> {
        let conn = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect()
    }
    let server = server().await;
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("MISSING_ORIGINAL_ROOT={}", root.display());
    let db_path = root.join("state.db");
    let media = root.join("media");
    let original = media.join("IMG_0501.JPG");
    let retained_original = root.join("retained-original.bin");
    let file_snapshot = |path: &std::path::Path| {
        let file = std::fs::File::open(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&file).unwrap(),
            file.metadata().unwrap().modified().unwrap(),
        )
    };
    let media_paths = || {
        std::fs::read_dir(&media)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<BTreeSet<_>>()
    };
    let page = |edit: Option<(&str, &[u8])>| {
        let mut page = records(&server, edit);
        page[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(provider_hash(ORIGINAL));
        if let Some((_, bytes)) = edit {
            page[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                json!(provider_hash(bytes));
        }
        page
    };
    let receipt_sql = "SELECT json_object('id',id,'account',account_key,'provider',provider_key,'scope',scope,'library',library,'child',child,'master',master,'state',state_id,'pass',origin_pass,'version',version_size,'checksum',provider_checksum,'size',provider_size,'path',native_path,'source',source_checksum,'local',local_checksum,'proof',proof_hash,'trust',trust_receipt,'metadata',selected_metadata_hash,'generation',generation_id) FROM download_receipts WHERE library='PrimarySync' AND child='asset-501' AND version_size='original' ORDER BY id";
    let adjusted_sql = receipt_sql.replace("version_size='original'", "version_size='adjusted'");
    let intent_sql = "SELECT json_object('library',library,'child',id,'version',version_size,'requested',requested_path_key,'destination_key',destination_path_key,'destination',destination_path,'checksum',provider_checksum,'size',provider_size) FROM reconciliation_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' ORDER BY provider_checksum,provider_size";
    let debt_sql = "SELECT json_object('generation',g.id,'format',json_extract(CAST(g.specification AS TEXT),'$.format'),'account',g.account_key,'provider',g.provider_key,'scope',g.scope,'pass',d.pass_key,'library',d.library,'child',d.child,'state',d.asset_id,'master',d.master,'version',d.version_size,'path',d.path,'compat_path',d.compat_path,'checksum',d.checksum,'size',d.size_bytes,'metadata',d.metadata_hash,'admitted',d.admitted,'verified',d.verified_media,'sealed',g.sealed,'ready',g.checkpoint_ready) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.version_size='adjusted' ORDER BY g.id,d.pass_key,d.path";
    let checkpoint_sql =
        "SELECT json_quote(value) FROM metadata WHERE key='sync_token:PrimarySync'";
    let error_sql = "SELECT json_quote(last_error) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'";
    let temp_sql =
        "SELECT json_array(hex(path),claimed_at,publication) FROM owned_temp_files ORDER BY path";
    let options = |resolution, token| CycleOptions {
        retry: Some(crate::retry::RetryConfig {
            max_retries: 1,
            base_delay_secs: 0,
            max_delay_secs: 0,
        }),
        private: true,
        resolution,
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (first, _) = cycle_options(
        &root,
        page(None),
        false,
        EditedNaming::Primary,
        options(
            crate::types::PhotoResolution::Original,
            "missing-original-start",
        ),
    )
    .await;
    assert_eq!(first.failed_count, 0);
    assert_bytes(&original, ORIGINAL);
    let original_file = file_snapshot(&original);
    let original_receipts = rows(&db_path, receipt_sql);
    assert_eq!(original_receipts.len(), 1);
    assert_eq!(
        rows(&db_path, checkpoint_sql),
        [json!("missing-original-start")]
    );
    let mapping_sql = "SELECT json_array(library,asset_record_name,master_record_name) FROM asset_master_mappings WHERE asset_record_name='asset-501' ORDER BY library";
    let original_mapping = rows(&db_path, mapping_sql);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let fault = Arc::new(AtomicBool::new(true));
    let fail_requests = fault.clone();
    Mock::given(method("GET"))
        .and(path("/edit-one"))
        .respond_with(move |_: &wiremock::Request| {
            if fail_requests.load(Ordering::SeqCst) {
                seen.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_bytes(EDIT_ONE)
            }
        })
        .with_priority(2)
        .mount(&server)
        .await;
    let mut edited = page(Some(("edit-one", EDIT_ONE)));
    let (failed, _) = cycle_options(
        &root,
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "missing-original-debt"),
    )
    .await;
    assert!(failed.failed_count > 0, "{failed:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    assert!(failed.db_sync_token_advance_safe);
    let intent = rows(&db_path, intent_sql);
    assert_eq!(intent.len(), 1);
    let sibling = std::path::PathBuf::from(intent[0]["destination"].as_str().unwrap());
    assert_ne!(sibling, original);
    assert!(sibling.starts_with(&media));
    assert!(!sibling.exists());
    assert_eq!(
        intent[0]["requested"],
        crate::fs_util::confined_path_key(&original).unwrap()
    );
    assert_eq!(
        intent[0]["destination_key"],
        crate::fs_util::confined_path_key(&sibling).unwrap()
    );
    assert_eq!(intent[0]["checksum"], provider_hash(EDIT_ONE));
    assert_eq!(intent[0]["size"], EDIT_ONE.len());
    let debt = rows(&db_path, debt_sql);
    assert!(!debt.is_empty());
    let assert_debt = |debt: &[Value]| {
        for d in debt {
            for key in [
                "account", "provider", "scope", "pass", "library", "child", "state", "master",
            ] {
                assert_eq!(d[key], original_receipts[0][key], "{key}: {d:?}");
            }
            assert_eq!(d["format"], 2);
            assert_eq!(d["version"], "adjusted");
            let native: SelectionPath = serde_json::from_str(d["path"].as_str().unwrap()).unwrap();
            assert_eq!(native.to_path(), sibling);
            assert_eq!(d["compat_path"], sibling.to_str().unwrap());
            assert_eq!(d["checksum"], provider_hash(EDIT_ONE));
            assert_eq!(d["size"], EDIT_ONE.len());
            for key in ["admitted", "sealed", "ready"] {
                assert_eq!(d[key], 1);
            }
            assert_eq!(d["verified"], 0);
        }
    };
    assert_debt(&debt);
    let generations: BTreeSet<_> = debt
        .iter()
        .map(|d| d["generation"].as_str().unwrap())
        .collect();
    assert_eq!(
        generations.len(),
        debt.len(),
        "one selected S per generation"
    );
    assert!(rows(&db_path, &adjusted_sql).is_empty());
    let failed_error = rows(&db_path, error_sql);
    assert_eq!(failed_error.len(), 1);
    assert!(failed_error[0].as_str().unwrap().contains("HTTP 500"));
    assert!(MatchDiagnostics::decode(failed_error[0].as_str().unwrap()).is_none());
    assert_eq!(
        rows(&db_path, checkpoint_sql),
        [json!("missing-original-debt")]
    );
    assert_eq!(rows(&db_path, receipt_sql), original_receipts);
    assert_eq!(file_snapshot(&original), original_file);
    let temp_custody = rows(&db_path, temp_sql);
    let failed_paths = media_paths();
    let other_files: Vec<_> = failed_paths
        .iter()
        .filter(|p| **p != original)
        .map(|p| (p.clone(), file_snapshot(p)))
        .collect();
    std::fs::write(root.join("failed-transfer-evidence.json"), serde_json::to_vec_pretty(&json!({"intent":intent,"debt":debt,"original_receipt":original_receipts,"error":failed_error,"failed_gets":calls.load(Ordering::SeqCst),"temps":temp_custody})).unwrap()).unwrap();
    fault.store(false, Ordering::SeqCst);
    // Retain the exact inode outside the media root, not as a discoverable sibling.
    std::fs::rename(&original, &retained_original).unwrap();
    assert_eq!(file_snapshot(&retained_original), original_file);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    assert_eq!(debt[0]["account"], account_key);
    assert_eq!(debt[0]["provider"], provider_key);
    let binding = state::db::PublicationBinding {
        account_key: account_key.into(),
        provider_key: provider_key.into(),
        scope: debt[0]["scope"].as_str().unwrap().into(),
        origin_pass: debt[0]["pass"].as_str().unwrap().into(),
        library: "PrimarySync".into(),
        child: "asset-501".into(),
        master: "501".into(),
        state_id: "asset-501".into(),
    };
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let mut healthy = page(None);
    for record in &mut healthy {
        record["recordName"] = json!(record["recordName"].as_str().unwrap().replace("501", "502"));
        if let Some(reference) = record["fields"].get_mut("masterRef") {
            reference["value"]["recordName"] = json!("502");
        }
    }
    healthy[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    healthy[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)});
    healthy[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)}});
    healthy[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
    healthy[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    edited.extend(healthy);
    let (held, lookups) = cycle_options(
        &root,
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "missing-original-held"),
    )
    .await;
    let held_debt = rows(&db_path, debt_sql);
    let error = rows(&db_path, error_sql);
    let requests = server.received_requests().await.unwrap();
    let adjusted_gets = requests
        .iter()
        .filter(|r| r.method == "GET" && r.url.path() == "/edit-one")
        .count();
    std::fs::write(root.join("missing-original-evidence.json"), serde_json::to_vec_pretty(&json!({"cycle":format!("{held:?}"),"lookups":lookups,"debt":held_debt,"intent":rows(&db_path,intent_sql),"adjusted_receipts":rows(&db_path,&adjusted_sql),"diagnostic":error,"adjusted_gets":adjusted_gets,"requests":requests.iter().map(|r| r.url.path()).collect::<Vec<_>>(),"original_absent":!original.exists(),"s_absent":!sibling.exists(),"retained_original":format!("{:?}",file_snapshot(&retained_original)),"temps":rows(&db_path,temp_sql),"checkpoint":rows(&db_path,checkpoint_sql)})).unwrap()).unwrap();
    assert!(
        lookups > 0,
        "actual due retry must confirm the retained child"
    );
    assert!(held.failed_count > 0, "{held:?}");
    assert_eq!(held.stats.downloaded, 1, "{held:?}");
    assert!(held.db_sync_token_advance_safe);
    assert!(!original.exists());
    assert!(!sibling.exists());
    assert_eq!(adjusted_gets, 8);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "GET" && r.url.path() == "/edit-two")
            .count(),
        1
    );
    assert_ne!(error, failed_error);
    let diagnostic = MatchDiagnostics::decode(error[0].as_str().unwrap()).unwrap();
    assert!(
        diagnostic.attempts.iter().any(|a| {
            a.child == "asset-501"
                && a.destination.to_path() == sibling
                && a.checksum == provider_hash(EDIT_ONE)
                && a.generation
                    .as_deref()
                    .is_some_and(|g| generations.contains(g))
                && a.pass.as_deref() == Some(binding.origin_pass.as_str())
                && a.explanation.candidates.iter().any(|c| {
                    c.path.as_ref().is_some_and(|p| p.to_path() == original)
                        && c.reason == "historical_occupant_changed_or_unsafe"
                })
        }),
        "{}",
        error[0]
    );
    assert_debt(&held_debt);
    for initial in &debt {
        assert!(
            held_debt.contains(initial),
            "lost initial debt: {initial:?}; {held_debt:?}"
        );
    }
    let held_generations: BTreeSet<_> = held_debt
        .iter()
        .map(|d| d["generation"].as_str().unwrap())
        .collect();
    assert!(generations.is_subset(&held_generations));
    assert_eq!(held_generations.len(), held_debt.len());
    assert_eq!(rows(&db_path, intent_sql), intent);
    assert!(rows(&db_path, &adjusted_sql).is_empty());
    assert_eq!(rows(&db_path, receipt_sql), original_receipts);
    assert_eq!(rows(&db_path, mapping_sql), original_mapping);
    assert_eq!(rows(&db_path, temp_sql), temp_custody);
    assert_eq!(
        rows(&db_path, checkpoint_sql),
        [json!("missing-original-held")]
    );
    assert_eq!(file_snapshot(&retained_original), original_file);
    for (path, before) in &other_files {
        assert_eq!(&file_snapshot(path), before);
    }
    let healthy_path = media.join("IMG_0502.JPG");
    assert_bytes(&healthy_path, EDIT_TWO);
    let healthy_file = file_snapshot(&healthy_path);
    let mut expected_paths = failed_paths;
    expected_paths.remove(&original);
    expected_paths.insert(healthy_path.clone());
    assert_eq!(media_paths(), expected_paths);
    let healthy_binding = state::db::PublicationBinding {
        child: "asset-502".into(),
        master: "502".into(),
        state_id: "asset-502".into(),
        ..binding.clone()
    };
    let healthy_receipts = {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.bound_download_receipts(
            healthy_binding.clone(),
            "adjusted".into(),
            provider_hash(EDIT_TWO),
            EDIT_TWO.len() as u64,
        )
        .await
        .unwrap()
    };
    assert_eq!(healthy_receipts.len(), 1);
    assert_eq!(healthy_receipts[0].native_path.to_path(), healthy_path);
    assert_eq!(
        healthy_receipts[0].source_checksum,
        format!("{:x}", Sha256::digest(EDIT_TWO))
    );
    // Restore the exact retained file, never synthesize fresh Original bytes or proof.
    std::fs::rename(&retained_original, &original).unwrap();
    assert_eq!(file_snapshot(&original), original_file);
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let (recovered, _) = cycle_options(
        &root,
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(
            crate::types::PhotoResolution::None,
            "missing-original-recovered",
        ),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert_eq!(recovered.stats.downloaded, 1);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests.len() + 1
    );
    assert_bytes(&sibling, EDIT_ONE);
    let sibling_file = file_snapshot(&sibling);
    let adjusted_receipts = rows(&db_path, &adjusted_sql);
    assert_eq!(adjusted_receipts.len(), 1);
    for (key, expected) in [
        ("account", &binding.account_key),
        ("provider", &binding.provider_key),
        ("scope", &binding.scope),
        ("pass", &binding.origin_pass),
        ("library", &binding.library),
        ("child", &binding.child),
        ("master", &binding.master),
        ("state", &binding.state_id),
    ] {
        assert_eq!(adjusted_receipts[0][key], expected.as_str(), "{key}");
    }
    assert_eq!(adjusted_receipts[0]["version"], "adjusted");
    let native: SelectionPath =
        serde_json::from_str(adjusted_receipts[0]["path"].as_str().unwrap()).unwrap();
    assert_eq!(native.to_path(), sibling);
    assert_eq!(adjusted_receipts[0]["checksum"], provider_hash(EDIT_ONE));
    assert_eq!(adjusted_receipts[0]["size"], EDIT_ONE.len());
    assert_eq!(
        adjusted_receipts[0]["source"],
        format!("{:x}", Sha256::digest(EDIT_ONE))
    );
    assert_eq!(
        adjusted_receipts[0]["local"],
        adjusted_receipts[0]["source"]
    );
    for key in ["trust", "metadata", "generation"] {
        assert!(adjusted_receipts[0][key].is_null());
    }
    let verified_receipts = {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.bound_download_receipts(
            binding.clone(),
            "adjusted".into(),
            provider_hash(EDIT_ONE),
            EDIT_ONE.len() as u64,
        )
        .await
        .unwrap()
    };
    assert_eq!(verified_receipts.len(), 1);
    assert_eq!(
        adjusted_receipts[0]["id"],
        verified_receipts[0].id().unwrap()
    );
    for round in 0..=2 {
        if round > 0 {
            let before = server.received_requests().await.unwrap().len();
            let (quiet, lookups) = cycle_options(
                &root,
                edited.clone(),
                true,
                EditedNaming::Primary,
                options(
                    crate::types::PhotoResolution::None,
                    "missing-original-recovered",
                ),
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{quiet:?}");
            assert_eq!((quiet.stats.downloaded, lookups), (0, 0));
            assert_eq!(server.received_requests().await.unwrap().len(), before);
        }
        assert_eq!(file_snapshot(&original), original_file);
        assert_eq!(file_snapshot(&sibling), sibling_file);
        assert_eq!(file_snapshot(&healthy_path), healthy_file);
        assert!(!retained_original.exists());
        assert_eq!(rows(&db_path, receipt_sql), original_receipts);
        assert_eq!(rows(&db_path, &adjusted_sql), adjusted_receipts);
        assert_eq!(rows(&db_path, mapping_sql), original_mapping);
        assert_eq!(rows(&db_path, intent_sql), intent);
        assert_eq!(
            rows(&db_path, checkpoint_sql),
            [json!("missing-original-recovered")]
        );
        let finished = rows(&db_path, debt_sql);
        for initial in &held_debt {
            let mut completed = initial.clone();
            completed["verified"] = json!(1);
            assert!(
                finished.contains(&completed),
                "initial destination not verified at exact S: {initial:?}; {finished:?}"
            );
        }
        assert_eq!(
            db_count(
                &root,
                "SELECT count(*) FROM provider_active_destinations WHERE library='PrimarySync' AND child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0"
            ),
            0
        );
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        assert_eq!(
            db.bound_download_receipts(
                binding.clone(),
                "adjusted".into(),
                provider_hash(EDIT_ONE),
                EDIT_ONE.len() as u64
            )
            .await
            .unwrap(),
            verified_receipts
        );
        assert_eq!(
            db.bound_download_receipts(
                healthy_binding.clone(),
                "adjusted".into(),
                provider_hash(EDIT_TWO),
                EDIT_TWO.len() as u64
            )
            .await
            .unwrap(),
            healthy_receipts
        );
        assert_eq!(media_paths().len(), 3);
    }
}

#[tokio::test]
async fn scoped_unaccounted_source_holds_distinct_checkpoint() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let options = |token| CycleOptions {
        private: true,
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (first, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options("source-start"),
    )
    .await;
    assert_eq!(first.failed_count, 0);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let conn = db.acquire_lock("baseline checkpoint").unwrap();
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "source-start");
    }
    let mut sparse = crate::test_helpers::sparse_shared_asset_record();
    sparse["recordName"] = json!("unresolved-child");
    sparse["fields"]
        .as_object_mut()
        .unwrap()
        .remove("masterRef");
    let (held, _) = cycle_options(
        root.path(),
        vec![sparse],
        false,
        EditedNaming::Primary,
        options("unaccounted-successor"),
    )
    .await;
    assert!(!held.db_sync_token_advance_safe, "{held:?}");
    let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
        .await
        .unwrap();
    let conn = db.acquire_lock("unaccounted checkpoint held").unwrap();
    let token: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(token, "source-start");
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
}

#[tokio::test]
async fn scoped_adjusted_only_preserves_changed_original_and_publishes_sibling() {
    use rusqlite::types::Value as SqlValue;
    use state::db::provider_selection::SelectionPath;

    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    if std::env::var_os("KEI_CHANGED_ORIGINAL_RETAIN_FIXTURE").is_some() {
        root.disable_cleanup(true);
    }
    eprintln!("CHANGED_ORIGINAL_FIXTURE {}", root.path().display());
    let media = root.path().join("media");
    let p = media.join("IMG_0501.JPG");
    let options = |resolution| CycleOptions {
        private: true,
        resolution,
        ..CycleOptions::default()
    };
    let (first, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::Original),
    )
    .await;
    assert_eq!((first.failed_count, first.stats.downloaded), (0, 1));
    assert_bytes(&p, ORIGINAL);
    let rows = |sql: &str| {
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| row.get::<_, SqlValue>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let file = |path: &std::path::Path| {
        let handle = std::fs::File::open(path).unwrap();
        let metadata = handle.metadata().unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&handle).unwrap(),
            metadata.len(),
            metadata.modified().unwrap(),
        )
    };
    let original_sql = "SELECT * FROM download_receipts WHERE child='asset-501' AND version_size='original' ORDER BY id";
    let original_receipts = rows(original_sql);
    assert_eq!(original_receipts.len(), 1);
    let binding_columns =
        "account_key,provider_key,scope,library,child,master,state_id,origin_pass";
    let original_binding = rows(&format!(
        "SELECT {binding_columns} FROM download_receipts WHERE child='asset-501' AND version_size='original'"
    ));
    let old_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL));
    assert_eq!(
        rows(
            "SELECT provider_checksum,provider_size,native_path,source_checksum,local_checksum,length(proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original'"
        ),
        vec![vec![
            SqlValue::Text(provider_hash(ORIGINAL)),
            SqlValue::Integer(i64::try_from(ORIGINAL.len()).unwrap()),
            SqlValue::Text(SelectionPath::from_path(&p).key().unwrap()),
            SqlValue::Text(old_hash.clone()),
            SqlValue::Text(old_hash.clone()),
            SqlValue::Integer(64),
        ]]
    );
    let original_owner_sql = "SELECT library,id,version_size,status,checksum,size_bytes,local_path,local_checksum FROM assets WHERE id='asset-501' AND version_size='original'";
    let original_owner = rows(original_owner_sql);
    let modified = b"foreign or locally changed original";
    std::fs::write(&p, modified).unwrap();
    let modified_file = file(&p);
    assert_ne!(
        old_hash,
        data_encoding::HEXLOWER.encode(&Sha256::digest(modified))
    );
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 1);
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (published, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None),
    )
    .await;
    assert_eq!(
        (published.failed_count, published.stats.downloaded),
        (0, 1),
        "{:?}",
        published.stats
    );
    let gets = server.received_requests().await.unwrap();
    assert_eq!(gets.len(), requests + 1);
    assert_eq!(
        gets.iter()
            .filter(|request| request.url.path() == "/edit-one")
            .count(),
        1
    );
    let paths = rows(
        "SELECT local_path FROM assets WHERE id='asset-501' AND version_size='adjusted' AND status='downloaded'",
    );
    let [path] = paths.as_slice() else {
        panic!("expected one downloaded Adjusted path")
    };
    let [SqlValue::Text(path)] = path.as_slice() else {
        panic!("expected native Adjusted locator")
    };
    let s = std::path::PathBuf::from(path);
    assert_ne!(s, p);
    assert_bytes(&s, EDIT_ONE);
    let sibling_file = file(&s);
    let local = data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_ONE));
    assert_eq!(
        rows(&format!(
            "SELECT {binding_columns} FROM download_receipts WHERE child='asset-501' AND version_size='adjusted'"
        )),
        original_binding
    );
    assert_eq!(
        rows(
            "SELECT version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,length(proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='adjusted'"
        ),
        vec![vec![
            SqlValue::Text("adjusted".into()),
            SqlValue::Text(provider_hash(EDIT_ONE)),
            SqlValue::Integer(i64::try_from(EDIT_ONE.len()).unwrap()),
            SqlValue::Text(SelectionPath::from_path(&s).key().unwrap()),
            SqlValue::Text(local.clone()),
            SqlValue::Text(local),
            SqlValue::Integer(64),
        ]]
    );
    let intent_sql = "SELECT library,id,version_size,requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size FROM reconciliation_paths WHERE id='asset-501' ORDER BY version_size,requested_path_key";
    let intent = rows(intent_sql);
    assert_eq!(
        intent,
        vec![vec![
            SqlValue::Text("PrimarySync".into()),
            SqlValue::Text("asset-501".into()),
            SqlValue::Text("adjusted".into()),
            SqlValue::Text(crate::fs_util::confined_path_key(&p).unwrap()),
            SqlValue::Text(crate::fs_util::confined_path_key(&s).unwrap()),
            SqlValue::Text(s.to_str().unwrap().to_owned()),
            SqlValue::Text(provider_hash(EDIT_ONE)),
            SqlValue::Integer(i64::try_from(EDIT_ONE.len()).unwrap()),
        ]]
    );
    assert_eq!(
        rows(
            "SELECT DISTINCT compat_path FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=1"
        ),
        vec![vec![SqlValue::Text(s.to_str().unwrap().to_owned())]]
    );
    let receipts_sql = "SELECT * FROM download_receipts WHERE child='asset-501' ORDER BY id";
    let completed_receipts = rows(receipts_sql);
    assert_eq!(completed_receipts.len(), 2);
    let owners_sql = "SELECT library,id,version_size,status,checksum,size_bytes,local_path,local_checksum FROM assets WHERE id='asset-501' ORDER BY version_size";
    let owners = rows(owners_sql);
    let prove_custody = || {
        assert_eq!(file(&p), modified_file);
        assert_eq!(file(&s), sibling_file);
        assert_eq!(rows(original_sql), original_receipts);
        assert_eq!(rows(original_owner_sql), original_owner);
        assert_eq!(rows(receipts_sql), completed_receipts);
        assert_eq!(rows(owners_sql), owners);
        assert_eq!(rows(intent_sql), intent);
        assert!(rows(&format!("SELECT compat_path FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND compat_path!='{}'", s.to_str().unwrap().replace('\'', "''"))).is_empty());
        for table in [
            "primary_layout_operations",
            "primary_layout_members",
            "primary_layout_bindings",
            "primary_layout_claims",
            "primary_layout_preserved",
            "primary_layout_superseded_paths",
        ] {
            assert_eq!(
                db_count(root.path(), &format!("SELECT count(*) FROM {table}")),
                0,
                "unexpected managed history: {table}"
            );
        }
        let mut files = std::fs::read_dir(&media)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        files.sort();
        let mut expected = vec![p.clone(), s.clone()];
        expected.sort();
        assert_eq!(files, expected, "unexpected archive or reorganization");
    };
    prove_custody();
    for rediscovery in 0..2 {
        let (observed, lookups) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None),
        )
        .await;
        assert_eq!(
            (observed.failed_count, observed.stats.downloaded),
            (0, 0),
            "{:?}",
            observed.stats
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            requests + 1
        );
        prove_custody();
        eprintln!(
            "CHANGED_ORIGINAL_REDISCOVERY index={rediscovery} assets_seen={} lookups={lookups} new_GETs=0 P_S_proofs_intent_stable=true",
            observed.stats.assets_seen
        );
    }
    eprintln!(
        "CHANGED_ORIGINAL_PASS new_adjusted_GET=1 independent_S=true P_old_proof_unchanged=true rediscoveries=2"
    );
}

#[tokio::test]
async fn scoped_adjusted_only_holds_foreign_original() {
    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    if std::env::var_os("KEI_CHANGED_ORIGINAL_RETAIN_FIXTURE").is_some() {
        root.disable_cleanup(true);
    }
    eprintln!("FOREIGN_ORIGINAL_FIXTURE {}", root.path().display());
    let media = root.path().join("media");
    std::fs::create_dir_all(&media).unwrap();
    let original = media.join("IMG_0501.JPG");
    let foreign = b"foreign or locally changed original";
    std::fs::write(&original, foreign).unwrap();
    let handle = std::fs::File::open(&original).unwrap();
    let before = (
        crate::fs_util::file_identity(&handle).unwrap(),
        handle.metadata().unwrap().modified().unwrap(),
    );
    drop(handle);
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 0);
    let (held, _) = cycle_options(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Primary,
        CycleOptions {
            private: true,
            resolution: crate::types::PhotoResolution::None,
            ..CycleOptions::default()
        },
    )
    .await;
    assert!(held.failed_count > 0, "{:?}", held.stats);
    assert_eq!(held.stats.downloaded, 0);
    assert_bytes(&original, foreign);
    let handle = std::fs::File::open(&original).unwrap();
    assert_eq!(
        (
            crate::fs_util::file_identity(&handle).unwrap(),
            handle.metadata().unwrap().modified().unwrap()
        ),
        before
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert!(!std::fs::read_dir(&media).unwrap().any(|entry| {
        std::fs::read(entry.unwrap().path()).is_ok_and(|bytes| bytes == EDIT_ONE)
    }));
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM download_receipts WHERE child='asset-501'"
        ),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM reconciliation_paths WHERE id='asset-501'"
        ),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM assets WHERE id='asset-501' AND version_size='original'"
        ),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM assets WHERE id='asset-501' AND version_size='adjusted' AND status='pending' AND local_path IS NULL AND local_checksum IS NULL"
        ),
        1
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0 AND source_checksum IS NULL AND local_checksum IS NULL"
        ),
        1
    );
    eprintln!(
        "FOREIGN_ORIGINAL_PASS downloads=0 GETs=0 receipts=0 intent=0 pending_unverified=1 P_preserved=true"
    );
}

#[tokio::test]
async fn primary_layout_retains_original_when_only_adjusted_selected() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = |resolution, token| CycleOptions {
        private: true,
        resolution,
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::Original, "original-token"),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let original = media.join("IMG_0501.JPG");
    assert_bytes(&original, ORIGINAL);
    let mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let receipt: String = conn.query_row("SELECT json_array(id,scope,child,master,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original' AND source_checksum IS NOT NULL", [], |r| r.get(0)).unwrap();
    drop(conn);
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (result, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "edit-token"),
    )
    .await;
    assert_eq!(result.failed_count, 0, "{:?}", result.stats);
    assert!(result.db_sync_token_advance_safe);
    assert_eq!(result.stats.downloaded, 1);
    let sibling = std::fs::read_dir(&media)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p != &original && std::fs::read(p).is_ok_and(|b| b == EDIT_ONE))
        .unwrap();
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let retained: String = conn.query_row("SELECT json_array(id,scope,child,master,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original' AND source_checksum IS NOT NULL", [], |r| r.get(0)).unwrap();
    assert_eq!(retained, receipt);
    let (path, source, local): (String,String,String) = conn.query_row("SELECT native_path,source_checksum,local_checksum FROM download_receipts WHERE child='asset-501' AND version_size='adjusted' AND provider_checksum=?1", [provider_hash(EDIT_ONE)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        std::path::Path::new(
            serde_json::from_str::<Value>(&path).unwrap()["path"]
                .as_str()
                .unwrap()
        ),
        sibling
    );
    assert_eq!(source, format!("{:x}", Sha256::digest(EDIT_ONE)));
    assert_eq!(source, local);
    let token: String = conn
        .query_row(
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(token, "edit-token");
    drop(conn);
    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        let (quiet, lookups) = cycle_options(
            root.path(),
            edited.clone(),
            true,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None, "edit-token"),
        )
        .await;
        assert_eq!(quiet.failed_count, 0);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&sibling, EDIT_ONE);
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            mtime
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[cfg(target_os = "linux")]
async fn process_death_layout_context(
    root: &std::path::Path,
    uri: &str,
) -> (download::DownloadConfig, AlbumPass) {
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let provider = Session {
        reported_count: None,
        records: records_at(uri, Some(("edit-one", EDIT_ONE))),
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
        Arc::from("private"),
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    };
    let media = root.join("media");
    let build = make_run_cycle_download_config_builder(&media, db);
    let mut config = build(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    (config, pass)
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "dedicated process-death fixture, spawned by its parent test"]
async fn primary_layout_death_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    let uri = std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap();
    let (mut config, pass) = process_death_layout_context(&root, &uri).await;
    let recovery_only = std::env::var("KEI_TEST_PROCESS_DEATH_RECOVERY_ONLY").ok();
    if recovery_only.as_deref() == Some("changed") {
        config.live_photo_mov_filename_policy = crate::types::LivePhotoMovFilenamePolicy::Original;
    }
    if matches!(
        std::env::var("KEI_TEST_PROCESS_DEATH_POINT").as_deref(),
        Ok("layout-planned" | "layout-prepared")
    ) {
        download::seed_prepared_edited_compat(&config, &pass).await;
        panic!("seed passed its selected rendezvous");
    }
    let recovered = download::recover_recorded_sources(
        &reqwest::Client::new(),
        &[pass],
        Arc::new(config),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    if let Some(mode) = recovery_only {
        assert!(!recovered.complete && !recovered.complete_after_smart_query);
        assert_eq!(recovered.stats.failed, 0);
        assert_eq!(recovered.stats.exif_failures, 0);
        assert_eq!(recovered.stats.state_write_failures, 0);
        eprintln!("DEATH_RECOVERY_ONLY {mode}: {recovered:?}");
        return;
    }
    panic!("process-death fixture passed its selected rendezvous");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn contract_source_checkpoint_requires_durable_recovery_and_temp_file_delete_requires_durable_ownership_primary_layout_process_death()
 {
    use crate::types::LivePhotoMovFilenamePolicy;
    use rusqlite::types::Value as SqlValue;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{Command, Stdio};
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let server = server().await;
    for point in [
        "layout-planned",
        "layout-prepared",
        "layout-preserved",
        "layout-member-published",
        "layout-committed",
        "journal-displaced",
        "journal-installed",
        "journal-committed",
    ] {
        let mut root = tempfile::tempdir().unwrap();
        root.disable_cleanup(true);
        eprintln!("C2 {point} {:?}", root.path());
        let media = root.path().join("media");
        eprintln!("DEATH_POINT_BEGIN {point}");
        let (baseline, _) = cycle_options(
            root.path(),
            records(&server, None),
            false,
            EditedNaming::Primary,
            CycleOptions {
                private: true,
                cycle_token: Some("death-original"),
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(baseline.failed_count, 0, "{point}: {:?}", baseline.stats);
        assert!(baseline.db_sync_token_advance_safe);
        let rows = |sql: &str| {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get::<_, SqlValue>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let custody = || {
            [
                rows("SELECT * FROM primary_layout_operations ORDER BY operation"),
                rows("SELECT * FROM primary_layout_members ORDER BY operation,member"),
                rows("SELECT * FROM primary_layout_claims ORDER BY path_key"),
                rows("SELECT * FROM primary_layout_bindings ORDER BY family"),
                rows("SELECT * FROM primary_layout_preserved ORDER BY operation,member"),
                rows(
                    "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,provider_checksum,native_path",
                ),
            ]
        };
        let members = || {
            rows("SELECT evidence FROM primary_layout_members ORDER BY operation,member")
                .into_iter()
                .map(|row| {
                    let [SqlValue::Blob(bytes)] = row.as_slice() else {
                        panic!("non-BLOB member evidence")
                    };
                    serde_json::from_slice::<state::db::primary_layout::LayoutMember>(bytes)
                        .unwrap()
                })
                .collect::<Vec<_>>()
        };
        let file = |path: &Path| {
            let file = std::fs::File::open(path).unwrap();
            (
                std::fs::read(path).unwrap(),
                crate::fs_util::file_identity(&file).unwrap(),
                file.metadata().unwrap().modified().unwrap(),
            )
        };
        let original = media.join("IMG_0501.JPG");
        let original_file = file(&original);
        assert_eq!(original_file.0, ORIGINAL);
        let immutable = rows(
            "SELECT * FROM download_receipts WHERE child='asset-501' AND version_size='original' ORDER BY id",
        );
        assert_eq!(immutable.len(), 1);
        let initial_history =
            rows("SELECT * FROM primary_layout_preserved ORDER BY operation,member");
        let metadata = async || {
            let (config, _) = process_death_layout_context(root.path(), &server.uri()).await;
            let db = config.state_db.as_ref().unwrap();
            let mut values = Vec::new();
            for key in [
                download::DOWNLOAD_CONFIG_HASH_KEY,
                crate::sync_cycle::PENDING_DOWNLOAD_CONFIG_HASH_KEY,
                "sync_token:PrimarySync",
            ] {
                values.push(db.get_metadata(key).await.unwrap());
            }
            values
        };
        let (config, pass) = process_death_layout_context(root.path(), &server.uri()).await;
        let old_expected = download::hash_download_config(&config);
        let mut changed = config.clone();
        changed.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
        let new_expected = download::hash_download_config(&changed);
        let new_legacy = download::hash_legacy_download_config(&changed);
        let (_, scope, _) = pass
            .album
            .publication_scope("PrimarySync")
            .unwrap()
            .unwrap();
        let pass_key = config.with_pass(&pass).primary_layout_pass.unwrap();
        let prove = |path: &Path, child: &str, version: &str, bytes: &[u8]| {
            assert_bytes(path, bytes);
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let native = serde_json::to_string(
                &state::db::provider_selection::SelectionPath::from_path(path),
            )
            .unwrap();
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key WHERE r.scope=?1 AND r.library='PrimarySync' AND r.child=?2 AND r.master=?3 AND r.state_id=?2 AND r.origin_pass=?4 AND r.version_size=?5 AND r.provider_checksum=?6 AND r.provider_size=?7 AND r.native_path=?8 AND r.source_checksum=?9 AND r.local_checksum=?9 AND length(r.proof_hash)=64",
                rusqlite::params![scope,child,child.strip_prefix("asset-").unwrap(),pass_key,version,provider_hash(bytes),i64::try_from(bytes.len()).unwrap(),native,hash], |r| r.get(0)).unwrap();
            assert_eq!(
                count,
                1,
                "{point}: scoped receipt for {} expected_scope={scope} expected_pass={pass_key} actual={:?}",
                path.display(),
                rows("SELECT * FROM download_receipts ORDER BY id")
            );
        };
        prove(&original, "asset-501", "original", ORIGINAL);
        assert_eq!(
            metadata().await,
            vec![
                Some(old_expected.clone()),
                None,
                Some("death-original".into())
            ]
        );
        if !matches!(point, "layout-planned" | "layout-prepared") {
            download::seed_prepared_edited_compat(&config, &pass).await;
        }
        drop((config, changed, pass));
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        let launch = |mode: &str| {
            let log =
                std::fs::File::create(root.path().join(format!("worker-{mode}.log"))).unwrap();
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "sync_loop::runner::tests::primary_layout::primary_layout_death_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
                .env("KEI_TEST_PROCESS_DEATH_URL", server.uri())
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .env_remove("KEI_TEST_PROCESS_DEATH_POINT")
                .env_remove("KEI_TEST_PROCESS_DEATH_RECOVERY_ONLY");
            if matches!(mode, "held" | "changed") {
                command.env("KEI_TEST_PROCESS_DEATH_RECOVERY_ONLY", mode);
            } else {
                command.env("KEI_TEST_PROCESS_DEATH_POINT", mode);
            }
            Worker(command.spawn().unwrap())
        };
        let recover_held = async |mode: &str| {
            let before = custody();
            let before_metadata = metadata().await;
            let requests = server.received_requests().await.unwrap().len();
            let mut worker = launch(mode);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            let status = loop {
                if let Some(status) = worker.0.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "recovery-only timeout: {mode}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            };
            let log =
                std::fs::read_to_string(root.path().join(format!("worker-{mode}.log"))).unwrap();
            assert!(status.success(), "{mode}: {log}");
            assert!(
                log.contains(&format!("DEATH_RECOVERY_ONLY {mode}:")),
                "{log}"
            );
            assert_eq!(worker.0.wait().unwrap(), status);
            assert_eq!(custody(), before);
            assert_eq!(metadata().await, before_metadata);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(file(&original), original_file);
            eprintln!(
                "DEATH_HELD_REOPEN {mode} incomplete=true failures=0 GETs=0 checkpoint_unchanged=true"
            );
        };
        let mut worker = launch(point);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !root.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited at {point}: {}",
                std::fs::read_to_string(root.path().join(format!("worker-{point}.log"))).unwrap()
            );
            assert!(
                std::time::Instant::now() < deadline,
                "worker timeout at {point}: {}",
                std::fs::read_to_string(root.path().join(format!("worker-{point}.log"))).unwrap()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("ready")).unwrap(),
            point
        );
        worker.0.kill().unwrap();
        assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
        eprintln!("DEATH_SIGKILL {point} ready_exact=true signal=9 reaped=true");
        if point == "layout-planned" {
            let held = custody();
            assert_eq!((held[0].len(), held[1].len(), held[2].len()), (1, 2, 2));
            assert!(held[3].is_empty());
            assert_eq!(held[4], initial_history);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT count(*) FROM primary_layout_operations WHERE phase='planned' AND json_extract(CAST(header AS TEXT),'$.refresh_metadata') IS NOT 1"
                ),
                1
            );
            for member in members() {
                assert!(
                    member.prepared.is_none()
                        && member.preserved.is_none()
                        && member.installed.is_none()
                );
                if let Some(stage) = member.stage {
                    assert!(!stage.to_path().exists());
                }
            }
            assert!(rows("SELECT * FROM owned_temp_files").is_empty());
            recover_held("held").await;
            assert_eq!(
                rows(
                    "SELECT * FROM download_receipts WHERE child='asset-501' AND version_size='original' ORDER BY id"
                ),
                immutable
            );
            let mut mixed = records(&server, Some(("edit-one", EDIT_ONE)));
            let mut peer = records(&server, None);
            for record in &mut peer {
                record["recordName"] =
                    json!(record["recordName"].as_str().unwrap().replace("501", "502"));
                if let Some(reference) = record["fields"].get_mut("masterRef") {
                    reference["value"]["recordName"] = json!("502");
                }
            }
            peer[0]["fields"]["filenameEnc"]["value"] = json!("NEW_0502.JPG");
            peer[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)});
            mixed.extend(peer);
            // These triggers assert ordering only; they seed no work or checkpoint.
            let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
            for event in ["INSERT", "UPDATE"] {
                conn.execute_batch(&format!("CREATE TRIGGER death_verified_before_token_{event} BEFORE {event} ON metadata WHEN NEW.key='sync_token:PrimarySync' AND NEW.value='death-current' BEGIN SELECT CASE WHEN (SELECT count(DISTINCT w.child) FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation JOIN download_receipts r ON r.scope=g.scope AND r.child=w.child AND r.master=w.master AND r.state_id=w.asset_id AND r.version_size=w.version_size AND r.native_path=w.path AND r.provider_checksum=w.checksum AND r.provider_size=w.size_bytes AND r.local_checksum=w.local_checksum AND r.source_checksum=w.source_checksum WHERE w.admitted=1 AND w.verified_media=1 AND ((w.child='asset-501' AND w.version_size='adjusted') OR (w.child='asset-502' AND w.version_size='original'))) != 2 THEN RAISE(ABORT,'current media must verify before successor token') END; END;")).unwrap();
            }
            drop(conn);
            let requests = server.received_requests().await.unwrap().len();
            let options = |policy, token| CycleOptions {
                private: true,
                mov_policy: policy,
                cycle_token: Some(token),
                ..CycleOptions::default()
            };
            let (progress, _) = cycle_options(
                root.path(),
                mixed.clone(),
                false,
                EditedNaming::Primary,
                options(LivePhotoMovFilenamePolicy::Suffix, "death-current"),
            )
            .await;
            assert_eq!(progress.failed_count, 0, "{:?}", progress.stats);
            assert!(progress.db_sync_token_advance_safe);
            assert_eq!(progress.stats.downloaded, 2);
            let gets = server.received_requests().await.unwrap();
            let mut paths: Vec<_> = gets[requests..]
                .iter()
                .map(|request| request.url.path())
                .collect();
            paths.sort_unstable();
            assert_eq!(paths, ["/edit-one", "/edit-two"]);
            let sibling = media.join("IMG_0501-asset-501.JPG");
            let healthy = media.join("NEW_0502.JPG");
            for member in members() {
                assert_ne!(member.destination.unwrap().to_path(), sibling);
            }
            prove(&sibling, "asset-501", "adjusted", EDIT_ONE);
            prove(&healthy, "asset-502", "original", EDIT_TWO);
            assert_eq!(metadata().await[2].as_deref(), Some("death-current"));
            assert_eq!(custody(), held);
            assert_eq!(file(&original), original_file);
            assert_eq!(
                rows(
                    "SELECT * FROM download_receipts WHERE child='asset-501' AND version_size='original' ORDER BY id"
                ),
                immutable
            );
            let stable_files = [file(&original), file(&sibling), file(&healthy)];
            let stable_receipts = rows("SELECT * FROM download_receipts ORDER BY id");
            assert_ne!(old_expected, new_expected);
            assert_ne!(old_expected, new_legacy);
            let (drift, _) = cycle_options(
                root.path(),
                mixed.clone(),
                false,
                EditedNaming::Primary,
                options(LivePhotoMovFilenamePolicy::Original, "death-config-current"),
            )
            .await;
            assert_eq!(drift.failed_count, 0, "{:?}", drift.stats);
            assert_eq!(drift.stats.downloaded, 0);
            assert_eq!(
                drift.stats.full_enumeration_reason,
                Some(download::FullEnumerationReason::DownloadConfigHashDrift)
            );
            let fenced = metadata().await;
            assert_eq!(fenced[0], Some(old_expected.clone()));
            assert_eq!(fenced[1], Some(new_expected.clone()));
            eprintln!(
                "DEATH_CONFIG_FENCE old={old_expected} new={new_expected} new_legacy={new_legacy} reason=DownloadConfigHashDrift applied_old=true pending_new=true"
            );
            recover_held("changed").await;
            for tail in 0..2 {
                recover_held("changed").await;
                let (quiet, _) = cycle_options(
                    root.path(),
                    mixed.clone(),
                    true,
                    EditedNaming::Primary,
                    options(LivePhotoMovFilenamePolicy::Original, "death-config-current"),
                )
                .await;
                assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
                assert_eq!(quiet.stats.downloaded, 0);
                assert_eq!(custody(), held);
                assert_eq!(
                    [file(&original), file(&sibling), file(&healthy)],
                    stable_files
                );
                assert_eq!(
                    rows("SELECT * FROM download_receipts ORDER BY id"),
                    stable_receipts
                );
                assert_eq!(&metadata().await[..2], &fenced[..2]);
                assert_eq!(server.received_requests().await.unwrap().len(), gets.len());
                prove(&sibling, "asset-501", "adjusted", EDIT_ONE);
                prove(&healthy, "asset-502", "original", EDIT_TWO);
                eprintln!(
                    "DEATH_QUIET {point} tail={tail} held=true downloads=0 GETs=0 proofs_stable=true config_fenced=true"
                );
            }
            eprintln!(
                "DEATH_POINT_PASS {point} held=true independent_current=true verified_before_token=true"
            );
            root.disable_cleanup(false);
            continue;
        }
        let requests = server.received_requests().await.unwrap().len();
        let (recovered, _) = cycle_options(
            root.path(),
            records(&server, Some(("edit-one", EDIT_ONE))),
            true,
            EditedNaming::Primary,
            CycleOptions {
                private: true,
                cycle_token: Some("death-recovered"),
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(recovered.failed_count, 0, "{point}: {:?}", recovered.stats);
        assert!(recovered.db_sync_token_advance_safe, "{point}");
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            requests,
            "{point}: redownloaded prepared media"
        );
        // Compatibility completion owns layout receipts, not new compact receipts.
        let binding_rows = rows("SELECT binding FROM primary_layout_bindings");
        let [SqlValue::Blob(binding)] = binding_rows[0].as_slice() else {
            panic!("non-BLOB layout binding")
        };
        let binding: state::db::primary_layout::LayoutBinding =
            serde_json::from_slice(binding).unwrap();
        for row in &immutable {
            assert!(rows("SELECT * FROM download_receipts WHERE child='asset-501' AND version_size='original' ORDER BY id").contains(row));
        }
        let finished = members();
        assert_eq!(finished.len(), 2);
        let mut stable_files = std::collections::BTreeMap::new();
        for member in &finished {
            assert!(member.installed.is_some());
            assert!(
                binding
                    .files
                    .iter()
                    .any(|saved| serde_json::to_value(saved).unwrap()
                        == serde_json::to_value(member.installed.as_ref().unwrap()).unwrap())
            );
            if let Some(stage) = &member.stage
                && stage.to_path().exists()
            {
                let displaced = member
                    .old
                    .as_ref()
                    .expect("only a recorded displaced original may remain");
                let path = stage.to_path();
                let snapshot = file(&path);
                assert_eq!(snapshot.0, ORIGINAL);
                assert_eq!(snapshot.1, displaced.fingerprint.identity);
                stable_files.insert(path, snapshot);
            }
            for saved in [&member.preserved, &member.installed].into_iter().flatten() {
                let path = saved.path.to_path();
                let (bytes, identity, mtime) = file(&path);
                assert_eq!(saved.fingerprint.identity, identity);
                assert_eq!(saved.fingerprint.size, bytes.len() as u64);
                let hash = Sha256::digest(&bytes);
                assert_eq!(saved.fingerprint.sha256.as_slice(), hash.as_slice());
                assert_eq!(
                    saved.source_checksum,
                    Some(data_encoding::HEXLOWER.encode(&hash))
                );
                assert_eq!(saved.provider_checksum, provider_hash(&bytes));
                if saved.version == "original" {
                    assert_ne!(identity, original_file.1);
                }
                assert_ne!(
                    identity,
                    file(&media.join(if saved.version == "original" {
                        "IMG_0501.JPG"
                    } else {
                        "IMG_0501_original.JPG"
                    }))
                    .1
                );
                stable_files.insert(path, (bytes, identity, mtime));
            }
        }
        assert!(finished.iter().any(|member| member.preserved.is_some()));
        assert_eq!(
            db_count(root.path(), "SELECT count(*) FROM primary_layout_bindings"),
            1
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT count(*) FROM primary_layout_operations WHERE phase='committed'"
            ),
            1
        );
        let mut directories = vec![media.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name();
                let name = name.to_str().unwrap();
                assert!(
                    !name.starts_with(".kei-replace-") && !name.ends_with(".part"),
                    "{point}: custody not cleaned: {name}"
                );
                if entry.file_type().unwrap().is_dir() {
                    directories.push(entry.path());
                }
            }
        }
        let (cleanup_config, _) = process_death_layout_context(root.path(), &server.uri()).await;
        let cleanup_db = cleanup_config.state_db.as_ref().unwrap();
        for owned in cleanup_db
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
        {
            assert!(
                !owned.path.exists(),
                "{point}: owned temporary link still exists"
            );
        }
        drop(cleanup_config); // Retained receipt lineage is deliberately not deleted (C10).
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
            ),
            0,
            "{point}"
        );
        let requests = server.received_requests().await.unwrap().len();
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        let stable = custody();
        let stable_receipts = rows("SELECT * FROM download_receipts ORDER BY id");
        assert_eq!(metadata().await[2].as_deref(), Some("death-recovered"));
        for tail in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                records(&server, Some(("edit-one", EDIT_ONE))),
                true,
                EditedNaming::Primary,
                CycleOptions {
                    private: true,
                    cycle_token: Some("death-recovered"),
                    ..CycleOptions::default()
                },
            )
            .await;
            assert_eq!(quiet.failed_count, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(lookups, 0, "{point}: quiet hydration");
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(custody(), stable);
            for (path, snapshot) in &stable_files {
                assert_eq!(&file(path), snapshot, "{point}: retained file {path:?}");
            }
            assert_eq!(
                rows("SELECT * FROM download_receipts ORDER BY id"),
                stable_receipts
            );
            assert_eq!(metadata().await[2].as_deref(), Some("death-recovered"));
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                operations
            );
            eprintln!(
                "DEATH_QUIET {point} tail={tail} downloads=0 lookups=0 GETs=0 proofs_stable=true"
            );
        }
        eprintln!(
            "DEATH_POINT_PASS {point} recovered=true pending=0 committed=true cleanup=true checkpoint_safe=true"
        );
        root.disable_cleanup(false);
    }
}

#[tokio::test]
async fn primary_layout_private_edit_retains_occupied_original_and_processes_new_work() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let original = media.join("IMG_0501.JPG");
    let options = |token| CycleOptions {
        private: true,
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options("original-token"),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&original, ORIGINAL);
    let mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let receipt: String = conn.query_row("SELECT json_array(id,scope,child,master,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original' AND source_checksum IS NOT NULL", [], |r| r.get(0)).unwrap();
    drop(conn);
    let mut healthy = records(&server, None);
    for record in &mut healthy {
        record["recordName"] = json!(record["recordName"].as_str().unwrap().replace("501", "502"));
        if let Some(reference) = record["fields"].get_mut("masterRef") {
            reference["value"]["recordName"] = json!("502");
        }
    }
    healthy[0]["fields"]["filenameEnc"]["value"] = json!("NEW_0502.JPG");
    healthy[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)});
    let mut paths = Vec::new();
    for (round, edit, token) in [
        (0, Some(("edit-one", EDIT_ONE)), "first-token"),
        (1, Some(("edit-two", EDIT_TWO)), "second-token"),
        (2, None, "revert-token"),
    ] {
        let mut page = records(&server, edit);
        page.extend(healthy.clone());
        let (result, _) = cycle_options(
            root.path(),
            page,
            false,
            EditedNaming::Primary,
            options(token),
        )
        .await;
        assert_eq!(result.failed_count, 0, "round {round}: {:?}", result.stats);
        assert_eq!(result.stats.downloaded, [2, 1, 0][round]);
        assert_eq!(result.stats.skipped.uncertain, 0);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let generations: (i64, i64) = conn.query_row("SELECT count(*), COALESCE(sum(sealed=1 AND checkpoint_ready=1),0) FROM provider_active_generations", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(generations, (1, 1));
        let destinations: (i64, i64) = conn.query_row("SELECT count(*), COALESCE(sum(verified_media=0 OR verified_metadata=0),0) FROM provider_active_destinations WHERE admitted=1", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(destinations, ([3, 3, 2][round], 0));
        for (index, bytes) in [EDIT_ONE, EDIT_TWO].into_iter().enumerate() {
            let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE child='asset-501' AND version_size='adjusted' AND provider_checksum=?1 AND source_checksum IS NOT NULL", [provider_hash(bytes)], |r| r.get(0)).unwrap();
            assert_eq!(count, i64::from(index <= round));
        }
        let token_saved: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token_saved, token);
        let retained: String = conn.query_row("SELECT json_array(id,scope,child,master,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original' AND source_checksum IS NOT NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(retained, receipt);
        drop(conn);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&original, ORIGINAL);
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            mtime
        );
        assert_bytes(&media.join("NEW_0502.JPG"), EDIT_TWO);
        if let Some((_, bytes)) = edit {
            let path = std::fs::read_dir(&media)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p != &original
                        && p != &media.join("NEW_0502.JPG")
                        && std::fs::read(p).is_ok_and(|b| b == bytes)
                });
            paths.push(path.expect("each edit has its own sibling"));
        }
    }
    assert_eq!(paths.len(), 2);
    assert_ne!(paths[0], paths[1]);
    assert_bytes(&paths[0], EDIT_ONE);
    assert_bytes(&paths[1], EDIT_TWO);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let receipts: Vec<String> = conn.prepare("SELECT json_array(provider_checksum,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='adjusted' ORDER BY provider_checksum").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert_eq!(receipts.len(), 2);
    for (path, bytes) in paths.iter().zip([EDIT_ONE, EDIT_TWO]) {
        let (native, source, local): (String, String, String) = conn.query_row("SELECT native_path,source_checksum,local_checksum FROM download_receipts WHERE child='asset-501' AND version_size='adjusted' AND provider_checksum=?1", [provider_hash(bytes)], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(
            std::path::Path::new(
                serde_json::from_str::<Value>(&native).unwrap()["path"]
                    .as_str()
                    .unwrap()
            ),
            path
        );
        assert_eq!(source, format!("{:x}", Sha256::digest(bytes)));
        assert_eq!(local, source);
    }
    drop(conn);
    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        let mut page = records(&server, None);
        page.extend(healthy.clone());
        let (quiet, lookups) = cycle_options(
            root.path(),
            page,
            true,
            EditedNaming::Primary,
            options("revert-token"),
        )
        .await;
        assert_eq!(quiet.failed_count, 0);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_bytes(&original, ORIGINAL);
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            mtime
        );
        assert_bytes(&paths[0], EDIT_ONE);
        assert_bytes(&paths[1], EDIT_TWO);
        assert_bytes(&media.join("NEW_0502.JPG"), EDIT_TWO);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let retained: Vec<String> = conn.prepare("SELECT json_array(provider_checksum,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='adjusted' ORDER BY provider_checksum").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(retained, receipts);
        let debt: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND (verified_media=0 OR verified_metadata=0)", [], |r| r.get(0)).unwrap();
        assert_eq!(debt, 0);
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "revert-token");
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn primary_layout_preserves_sidecar_bytes_and_fences_old_metadata_queue() {
    use crate::state::db::MetadataRewriteStore as _;

    fn signature(bytes: &[u8]) -> String {
        let mut hash = sha1::Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        base64::engine::general_purpose::STANDARD.encode([&[1][..], &hash.finalize()[..]].concat())
    }
    fn rows(root: &std::path::Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let conn = rusqlite::Connection::open_with_flags(
            root.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |row| (0..columns).map(|index| row.get(index)).collect())
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
    fn check_file(file: &state::db::primary_layout::LayoutFile, path: &std::path::Path) {
        let check = |path: &std::path::Path,
                     proof: &state::db::primary_layout::LayoutFingerprint| {
            let handle = std::fs::File::open(path).unwrap();
            let bytes = std::fs::read(path).unwrap();
            assert_eq!(bytes.len() as u64, proof.size, "{}", path.display());
            assert_eq!(Sha256::digest(&bytes).as_slice(), proof.sha256.as_slice());
            assert_eq!(
                crate::fs_util::file_identity(&handle).unwrap(),
                proof.identity
            );
        };
        check(path, &file.fingerprint);
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".xmp");
        check(
            std::path::Path::new(&sidecar),
            file.sidecar.as_ref().unwrap(),
        );
    }
    #[derive(Debug, PartialEq, Eq)]
    struct PacketOrderKey {
        before: String,
        namespaces: Vec<String>,
        properties: Vec<String>,
        after: String,
    }
    // Strict compact fixture grammar, not an XML canonicalizer. Every accepted
    // byte is retained except the two explicitly permitted permutations.
    fn packet_order_key(packet: &[u8]) -> Option<PacketOrderKey> {
        let text = std::str::from_utf8(packet).ok()?;
        let _: xmp_toolkit::XmpMeta = text.parse().ok()?;
        if text.contains("<!")
            || text.contains("<?")
            || text.matches("<rdf:Description").count() != 1
            || text.matches("</rdf:Description>").count() != 1
        {
            return None;
        }
        let (before, rest) = text.split_once("<rdf:Description ")?;
        let (attributes, rest) = rest.split_once('>')?;
        let (body, after) = rest.split_once("</rdf:Description>")?;
        let declarations = attributes.strip_prefix("rdf:about=\"\" ")?;
        let name_part = |name: &str| {
            !name.is_empty()
                && name.as_bytes()[0].is_ascii_alphabetic()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        };
        let mut prefixes = Vec::new();
        let mut namespaces = Vec::new();
        for token in declarations.split(' ') {
            let (prefix, uri) = token.strip_prefix("xmlns:")?.split_once("=\"")?;
            let uri = uri.strip_suffix('"')?;
            if !name_part(prefix)
                || uri.is_empty()
                || uri.chars().any(|c| c.is_whitespace() || "\"<>".contains(c))
                || prefixes.contains(&prefix)
            {
                return None;
            }
            prefixes.push(prefix);
            namespaces.push(token.to_owned());
        }
        let mut body = body.strip_prefix(' ')?;
        let mut properties = Vec::new();
        while !body.is_empty() {
            if !body.starts_with('<') {
                return None;
            }
            let open_end = body.find('>')?;
            if body.get(..=open_end)?.ends_with("/>") {
                return None;
            }
            let name = body.get(1..open_end)?.split_ascii_whitespace().next()?;
            let (prefix, local) = name.split_once(':')?;
            if !name_part(prefix)
                || !name_part(local)
                || !(prefixes.contains(&prefix) || matches!(prefix, "rdf" | "xml"))
            {
                return None;
            }
            let close = format!("</{name}>");
            let end = body.find(&close)? + close.len();
            let block = body.get(..end)?;
            // Nested same-name properties, comments and CDATA are unsupported,
            // never guessed at or flattened. Other child structure stays raw.
            if block.matches(format!("<{name}").as_str()).count() != 1 {
                return None;
            }
            properties.push(block.to_owned());
            body = body.get(end..)?.strip_prefix(' ')?;
        }
        if properties.is_empty() {
            return None;
        }
        namespaces.sort();
        properties.sort(); // Vec preserves multiplicity; never use a set.
        Some(PacketOrderKey {
            before: before.into(),
            namespaces,
            properties,
            after: after.into(),
        })
    }
    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    root.disable_cleanup(true);
    eprintln!("SIDECAR_RECORDED_FIXTURE {}", root.path().display());
    let media = root.path().join("media");
    let options = || CycleOptions {
        sidecar: true,
        private: true,
        ..CycleOptions::default()
    };
    let original_signature = signature(ORIGINAL);
    let adjusted_signature = signature(EDIT_ONE);
    let mut initial_records = records(&server, None);
    initial_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(original_signature);
    let mut current = records(&server, Some(("edit-one", EDIT_ONE)));
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(original_signature);
    current[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] = json!(adjusted_signature);
    for (expected, descriptor) in [
        (
            &original_signature,
            &initial_records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"],
        ),
        (
            &original_signature,
            &current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"],
        ),
        (
            &adjusted_signature,
            &current[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"],
        ),
    ] {
        assert_eq!(descriptor.as_str(), Some(expected.as_str()));
        assert_eq!(expected.len(), 28);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(expected)
            .unwrap();
        assert_eq!(decoded.len(), 21);
        assert_eq!(decoded[0], 0x01);
    }
    let (initial, _) = cycle_options(
        root.path(),
        initial_records,
        false,
        EditedNaming::Primary,
        options(),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert_eq!(initial.stats.downloaded, 1);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    let old_sidecar = std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap();
    assert!(!old_sidecar.is_empty());
    let old_packet_hash = Sha256::digest(&old_sidecar);
    let old_packet_identity = crate::fs_util::file_identity(
        &std::fs::File::open(media.join("IMG_0501.JPG.xmp")).unwrap(),
    )
    .unwrap();
    let old_key = packet_order_key(&old_sidecar).expect("unsupported source fixture packet");
    let old_text = std::str::from_utf8(&old_sidecar).unwrap();
    let permuted = format!(
        "{}<rdf:Description rdf:about=\"\" {}> {} </rdf:Description>{}",
        old_key.before,
        old_key
            .namespaces
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        old_key
            .properties
            .iter()
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        old_key.after
    );
    assert_ne!(permuted.as_bytes(), old_sidecar);
    assert_eq!(
        packet_order_key(permuted.as_bytes()).as_ref(),
        Some(&old_key)
    );
    let date = old_key
        .properties
        .iter()
        .find(|p| p.starts_with("<xmp:CreateDate>"))
        .unwrap();
    assert_eq!(old_text.matches(date.as_str()).count(), 1);
    let date_value = date
        .strip_prefix("<xmp:CreateDate>")
        .unwrap()
        .strip_suffix("</xmp:CreateDate>")
        .unwrap();
    let date_format = "%Y-%m-%dT%H:%M:%S";
    let changed_date_value = chrono::NaiveDateTime::parse_from_str(date_value, date_format)
        .unwrap()
        .checked_add_signed(chrono::Duration::seconds(1))
        .unwrap();
    let changed_date = format!(
        "<xmp:CreateDate>{}</xmp:CreateDate>",
        changed_date_value.format(date_format)
    );
    for changed in [
        old_text.replacen(date.as_str(), &changed_date, 1),
        old_text.replacen(
            "http://ns.adobe.com/exif/1.0/",
            "https://invalid.example/exif/1.0/",
            1,
        ),
        old_text.replacen(
            " </rdf:Description>",
            &format!(" {date} </rdf:Description>"),
            1,
        ),
        old_text.replacen("<xmp:CreateDate>", "<xmp:CreateDate xml:lang=\"en\">", 1),
        old_text.replacen("rdf:about=\"\"", "rdf:about=\"changed\"", 1),
    ] {
        assert_ne!(changed.as_bytes(), old_sidecar);
        assert_ne!(
            packet_order_key(changed.as_bytes()).as_ref(),
            Some(&old_key)
        );
    }
    let pair = "<rdf:li>TestAlbum</rdf:li> <rdf:li>SecondTag</rdf:li>";
    let two_items = old_text.replacen("<rdf:li>TestAlbum</rdf:li>", pair, 1);
    let reversed = two_items.replacen(
        pair,
        "<rdf:li>SecondTag</rdf:li> <rdf:li>TestAlbum</rdf:li>",
        1,
    );
    assert_ne!(
        packet_order_key(two_items.as_bytes()).expect("two-item fixture"),
        packet_order_key(reversed.as_bytes()).expect("reversed two-item fixture")
    );
    let original_receipts = rows(
        root.path(),
        "SELECT * FROM download_receipts WHERE version_size='original' ORDER BY id",
    );
    assert_eq!(original_receipts.len(), 1);
    let original = media.join("IMG_0501.JPG");
    assert_bytes(&original, ORIGINAL);
    assert!(!media.join("IMG_0501_edited.JPG").exists());
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 1);
    assert!(rows(root.path(), "SELECT * FROM primary_layout_operations").is_empty());
    assert!(rows(root.path(), "SELECT * FROM primary_layout_bindings").is_empty());

    // The ordinary Original publication supplies its real scoped receipt.
    // The compatibility owner alone prepares and downloads the released edit.
    let (
        op,
        scope,
        pass_key,
        marker,
        adjusted_packet,
        frozen_packets,
        old_source_index,
        old_source_proof,
    ) = {
        let (mut config, pass) = source_mutation_layout_context(
            root.path(),
            Arc::new(std::sync::Mutex::new(current.clone())),
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        config.metadata.xmp_sidecar = true;
        let (capture, scope, _) = pass
            .album
            .publication_scope("PrimarySync")
            .unwrap()
            .unwrap();
        let db = capture.db;
        let concrete = db.clone() as Arc<dyn download::DownloadStore>;
        assert!(Arc::ptr_eq(config.state_db.as_ref().unwrap(), &concrete));
        let pass_key = config.primary_layout_pass.as_ref().unwrap().to_string();
        let (account, provider) = capture.owner.authenticated_keys().unwrap();
        {
            let conn = db
                .acquire_lock("real prehandover source and local receipts")
                .unwrap();
            let native = serde_json::to_string(
                &state::db::provider_selection::SelectionPath::from_path(&original),
            )
            .unwrap();
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL));
            let checksum = current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"]
                .as_str()
                .unwrap();
            let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE account_key=?1 AND provider_key=?2 AND scope=?3 AND origin_pass=?4 AND library='PrimarySync' AND child='asset-501' AND master='501' AND state_id='asset-501' AND version_size='original' AND provider_checksum=?5 AND provider_size=?6 AND native_path=?7 AND source_checksum=?8 AND local_checksum=?8 AND length(proof_hash)=64", rusqlite::params![account,provider,scope,pass_key,checksum,ORIGINAL.len() as i64,native,hash], |r| r.get(0)).unwrap();
            assert_eq!(count, 1, "missing real source-backed Original download");
        }
        // Invoke the metadata failure state owner, not a fabricated timestamp.
        // At this point the sole original path is still the live P publication.
        db.record_metadata_write_failure("PrimarySync", "asset-501", "original")
            .await
            .unwrap();
        let marker: i64 = db.acquire_lock("capture actual original retry marker").unwrap()
            .query_row("SELECT metadata_write_failed_at FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='original' AND local_path=?1", [original.to_str().unwrap()], |r| r.get(0)).unwrap();
        assert!(marker > 0);
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                state::db::MetadataRewriteQueue::Ordinary,
                Some(&["PrimarySync"]),
                0,
                100,
            )
            .await
            .unwrap();
        assert!(
            pending
                .iter()
                .any(|p| p.asset.version_size == state::VersionSizeKey::Original
                    && p.asset.local_path.as_deref() == Some(original.as_path())),
            "retry marker was not eligible before handover"
        );

        eprintln!(
            "SIDECAR_OWNER_PREPARE_ENTER pass={pass_key} marker={marker} ordinary_original_GET={requests} ordinary_adjusted_GET=0 original_scoped_receipt=1"
        );
        let mut op = download::seed_prepared_edited_compat(&config, &pass).await;
        eprintln!(
            "SIDECAR_OWNER_PREPARED operation={} phase={} members={} total_GET={}",
            op.operation,
            op.phase,
            op.members.len(),
            server.received_requests().await.unwrap().len()
        );
        assert_eq!(op.phase, "prepared");
        assert_eq!(op.pass, pass_key);
        assert_eq!(op.library, "PrimarySync");
        assert_eq!(op.child, "asset-501");
        assert_eq!(op.source_library, "PrimarySync");
        assert_eq!(op.policy, "primary");
        assert!(config.metadata.xmp_sidecar);
        for member in &op.members {
            let prepared = member.prepared.as_ref().unwrap();
            check_file(prepared, &prepared.path.to_path());
            let (bytes, checksum) = match prepared.version.as_str() {
                "original" => (ORIGINAL, &original_signature),
                "adjusted" => (EDIT_ONE, &adjusted_signature),
                version => panic!("unexpected prepared rendition {version}"),
            };
            assert_bytes(&prepared.path.to_path(), bytes);
            assert_eq!(&prepared.provider_checksum, checksum);
            assert_eq!(
                prepared.source_checksum.as_deref(),
                Some(
                    data_encoding::HEXLOWER
                        .encode(&Sha256::digest(bytes))
                        .as_str()
                )
            );
        }
        let frozen_packets: Vec<_> = op
            .members
            .iter()
            .enumerate()
            .map(|(index, member)| {
                let proof = member.prepared.as_ref().unwrap().clone();
                check_file(&proof, &proof.path.to_path());
                let packet = std::fs::read(proof.path.to_path().with_extension("JPG.xmp")).unwrap();
                let sidecar = proof.sidecar.as_ref().unwrap();
                assert_eq!(packet.len() as u64, sidecar.size);
                assert_eq!(
                    Sha256::digest(&packet).as_slice(),
                    sidecar.sha256.as_slice()
                );
                (index, proof, packet)
            })
            .collect();
        let archive_packet = &frozen_packets
            .iter()
            .find(|(_, proof, _)| proof.version == "original")
            .unwrap()
            .2;
        assert_eq!(
            packet_order_key(archive_packet).as_ref(),
            Some(&old_key),
            "constant facts changed beyond the two allowed serialization permutations"
        );
        let adjusted_packet = frozen_packets
            .iter()
            .find(|(_, proof, _)| proof.version == "adjusted")
            .unwrap()
            .2
            .clone();
        let old_sources: Vec<_> = op
            .members
            .iter()
            .enumerate()
            .filter(|(_, member)| {
                member
                    .old
                    .as_ref()
                    .is_some_and(|file| file.path.to_path() == original)
            })
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            old_sources.len(),
            1,
            "old source must have one recorded preservation member"
        );
        let old_source_index = old_sources[0];
        let old_source_proof = op.members[old_source_index].old.as_ref().unwrap().clone();
        check_file(&old_source_proof, &original);
        let source_packet = old_source_proof.sidecar.as_ref().unwrap();
        assert_eq!(source_packet.sha256.as_slice(), old_packet_hash.as_slice());
        assert_eq!(source_packet.size, old_sidecar.len() as u64);
        assert_eq!(source_packet.identity, old_packet_identity);
        assert_bytes(&original, ORIGINAL);
        assert_eq!(
            std::fs::read(original.with_extension("JPG.xmp")).unwrap(),
            old_sidecar
        );
        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), requests + 1);
        assert_eq!(
            received
                .iter()
                .filter(|request| request.url.path() == "/original")
                .count(),
            1
        );
        assert_eq!(
            received
                .iter()
                .filter(|request| request.url.path() == "/edit-one")
                .count(),
            1
        );
        eprintln!(
            "SIDECAR_OWNER_FINISH_ENTER operation={} pass={} marker={marker} ordinary_original_GET=1 preparation_adjusted_GET=1",
            op.operation, op.pass
        );
        download::finish_prepared_edited_compat(&config, &mut op).await;
        eprintln!(
            "SIDECAR_OWNER_COMMITTED operation={} phase={} total_GET={}",
            op.operation,
            op.phase,
            server.received_requests().await.unwrap().len()
        );
        assert_eq!(op.phase, "committed");
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                state::db::MetadataRewriteQueue::Ordinary,
                Some(&["PrimarySync"]),
                0,
                100,
            )
            .await
            .unwrap();
        assert!(
            !pending
                .iter()
                .any(|p| p.asset.version_size == state::VersionSizeKey::Original
                    && p.asset.local_path.as_deref() == Some(original.as_path())),
            "superseded original alias entered ordinary rewrite queue"
        );
        assert!(matches!(
            db.guard_primary_writer(&original).await,
            Err(state::error::StateError::Invariant {
                operation: "primary_layout",
                detail,
            }) if detail == "managed primary slot requires its layout publication owner"
        ));
        (
            op,
            scope,
            pass_key,
            marker,
            adjusted_packet,
            frozen_packets,
            old_source_index,
            old_source_proof,
        )
    };
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 2);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    assert_bytes(&original, EDIT_ONE);
    let archive_packet = &frozen_packets
        .iter()
        .find(|(_, proof, _)| proof.version == "original")
        .unwrap()
        .2;
    assert_eq!(
        &std::fs::read(media.join("IMG_0501_original.JPG.xmp")).unwrap(),
        archive_packet
    );
    assert_eq!(
        std::fs::read(original.with_extension("JPG.xmp")).unwrap(),
        adjusted_packet
    );
    let verify_files = || {
        for member in &op.members {
            for file in [&member.installed, &member.preserved].into_iter().flatten() {
                check_file(file, &file.path.to_path());
            }
            if let Some(stage) = &member.stage
                && stage.to_path().exists()
            {
                let path = stage.to_path();
                let handle = std::fs::File::open(&path).unwrap();
                let identity = crate::fs_util::file_identity(&handle).unwrap();
                let saved = [&member.old, &member.prepared]
                    .into_iter()
                    .flatten()
                    .find(|file| file.fingerprint.identity == identity)
                    .expect("retained stage must have recorded old or prepared evidence");
                check_file(saved, &path);
            }
        }
    };
    let physical = || {
        let mut entries = std::collections::BTreeMap::new();
        let mut directories = vec![media.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    entries.insert(path.clone(), None);
                    directories.push(path);
                } else {
                    assert!(entry.file_type().unwrap().is_file());
                    let handle = std::fs::File::open(&path).unwrap();
                    entries.insert(
                        path.clone(),
                        Some((
                            std::fs::read(path).unwrap(),
                            crate::fs_util::file_identity(&handle).unwrap(),
                            handle.metadata().unwrap().modified().unwrap(),
                        )),
                    );
                }
            }
        }
        entries
    };
    let custody = || {
        [
        "SELECT * FROM primary_layout_operations ORDER BY operation",
        "SELECT * FROM primary_layout_members ORDER BY operation,member",
        "SELECT * FROM primary_layout_bindings ORDER BY family",
        "SELECT * FROM primary_layout_claims ORDER BY path_key",
        "SELECT * FROM primary_layout_preserved ORDER BY operation,member",
        "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,native_path",
        "SELECT * FROM download_receipts ORDER BY id",
        "SELECT library,id,version_size,local_path,provider_checksum,source_checksum,local_checksum,download_checksum,metadata_write_failed_at FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
        "SELECT * FROM owned_temp_files ORDER BY path",
    ].map(|sql| rows(root.path(), sql))
    };
    let guard = || {
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for (index, frozen, packet) in &frozen_packets {
            let member = &op.members[*index];
            assert_eq!(
                serde_json::to_value(member.prepared.as_ref().unwrap()).unwrap(),
                serde_json::to_value(frozen).unwrap(),
                "prepared proof changed after freeze"
            );
            let installed = member.installed.as_ref().unwrap();
            let prepared_sidecar = frozen.sidecar.as_ref().unwrap();
            let installed_sidecar = installed.sidecar.as_ref().unwrap();
            assert_eq!(
                (installed_sidecar.size, installed_sidecar.sha256),
                (prepared_sidecar.size, prepared_sidecar.sha256)
            );
            assert_eq!(
                &std::fs::read(installed.path.to_path().with_extension("JPG.xmp")).unwrap(),
                packet
            );
            check_file(installed, &installed.path.to_path());
        }
        let source_member = &op.members[old_source_index];
        assert_eq!(
            serde_json::to_value(source_member.old.as_ref().unwrap()).unwrap(),
            serde_json::to_value(&old_source_proof).unwrap()
        );
        let preserved = source_member.preserved.as_ref().unwrap();
        assert_eq!(source_member.preserved_path.as_ref(), Some(&preserved.path));
        assert_eq!(preserved.version, old_source_proof.version);
        assert_eq!(
            preserved.provider_checksum,
            old_source_proof.provider_checksum
        );
        assert_eq!(preserved.source_checksum, old_source_proof.source_checksum);
        assert_eq!(
            preserved.fingerprint.sha256,
            old_source_proof.fingerprint.sha256
        );
        assert_eq!(
            preserved.sidecar.as_ref().unwrap().sha256.as_slice(),
            old_packet_hash.as_slice()
        );
        let preserved_packet =
            std::fs::read(preserved.path.to_path().with_extension("JPG.xmp")).unwrap();
        assert_eq!(
            preserved_packet, old_sidecar,
            "raw old source packet was lost"
        );
        assert_eq!(
            Sha256::digest(&preserved_packet).as_slice(),
            old_packet_hash.as_slice()
        );
        check_file(preserved, &preserved.path.to_path());
        let (member, evidence, native, hash): (Vec<u8>, Vec<u8>, Vec<u8>, String) = conn.query_row(
            "SELECT m.evidence,p.evidence,p.native_path,p.sidecar_checksum FROM primary_layout_operations o JOIN primary_layout_members m ON m.operation=o.operation JOIN primary_layout_preserved p ON p.operation=m.operation AND p.member=m.member WHERE o.operation=?1 AND o.family=?2 AND o.phase='committed' AND m.member=?3",
            rusqlite::params![op.operation,op.family,old_source_index as i64],
            |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
        ).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&member).unwrap(),
            serde_json::to_value(source_member).unwrap()
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&evidence).unwrap(),
            serde_json::to_value(preserved).unwrap()
        );
        assert_eq!(native, serde_json::to_vec(&preserved.path).unwrap());
        assert_eq!(hash, data_encoding::HEXLOWER.encode(&old_packet_hash));
        let saved: i64 = conn.query_row("SELECT metadata_write_failed_at FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='original' AND local_path=?1", [original.to_str().unwrap()], |r| r.get(0)).unwrap();
        assert_eq!(saved, marker, "historical marker was discarded or changed");
        for (version, path, bytes, checksum) in [
            (
                "original",
                media.join("IMG_0501_original.JPG"),
                ORIGINAL,
                current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"]
                    .as_str()
                    .unwrap(),
            ),
            (
                "adjusted",
                original.clone(),
                EDIT_ONE,
                current[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"]
                    .as_str()
                    .unwrap(),
            ),
        ] {
            let native = serde_json::to_vec(
                &state::db::provider_selection::SelectionPath::from_path(&path),
            )
            .unwrap();
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let count: i64 = conn.query_row("SELECT count(*) FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size=?1 AND provider_checksum=?2 AND local_path=?3 AND source_checksum=?4 AND local_checksum=?4 AND download_checksum=?4",rusqlite::params![version,checksum,path.to_str().unwrap(),hash],|r|r.get(0)).unwrap();
            assert_eq!(count, 1, "missing committed {version} state-path proof");
            let claims: i64 = conn.query_row("SELECT count(*) FROM primary_layout_claims WHERE family=?1 AND library='PrimarySync' AND child='asset-501' AND version=?2 AND provider_checksum=?3 AND native_path=?4 AND local_checksum=?5 AND operation IS NULL",rusqlite::params![op.family,version,checksum,native,hash],|r|r.get(0)).unwrap();
            assert_eq!(claims, 1, "missing committed {version} path claim");
        }
        let binding: Vec<u8> = conn.query_row("SELECT binding FROM primary_layout_bindings WHERE family=?1 AND library='PrimarySync' AND source_library='PrimarySync' AND child='asset-501' AND generation=?2 AND decision=?3 AND policy=?4", rusqlite::params![op.family,op.generation+1,op.decision,op.policy], |r| r.get(0)).unwrap();
        let binding: state::db::primary_layout::LayoutBinding =
            serde_json::from_slice(&binding).unwrap();
        for member in &op.members {
            if let Some(installed) = &member.installed {
                let prepared = member.prepared.as_ref().unwrap();
                assert_eq!(installed.version, prepared.version);
                assert_eq!(installed.provider_checksum, prepared.provider_checksum);
                assert_eq!(installed.source_checksum, prepared.source_checksum);
                assert_eq!(installed.fingerprint.size, prepared.fingerprint.size);
                assert_eq!(installed.fingerprint.sha256, prepared.fingerprint.sha256);
                assert_eq!(
                    installed
                        .sidecar
                        .as_ref()
                        .map(|proof| (proof.size, proof.sha256)),
                    prepared
                        .sidecar
                        .as_ref()
                        .map(|proof| (proof.size, proof.sha256))
                );
                let saved = binding
                    .files
                    .iter()
                    .find(|file| file.path == installed.path)
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(saved).unwrap(),
                    serde_json::to_value(installed).unwrap()
                );
            }
        }
        let original_receipt = &original_receipts[0];
        assert!(rows(root.path(), "SELECT r.* FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key ORDER BY r.id").contains(original_receipt));
        let scoped: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE scope=?1 AND origin_pass=?2 AND library='PrimarySync' AND child='asset-501' AND master='501' AND state_id='asset-501' AND version_size='original' AND provider_checksum=?3 AND native_path=?4", rusqlite::params![scope,pass_key,original_signature,serde_json::to_string(&state::db::provider_selection::SelectionPath::from_path(&original)).unwrap()], |r|r.get(0)).unwrap();
        assert_eq!(
            scoped, 1,
            "original scoped receipt was lost during legacy publication"
        );
        for row in &original_receipts {
            assert!(
                rows(
                    root.path(),
                    "SELECT * FROM download_receipts WHERE version_size='original' ORDER BY id"
                )
                .contains(row)
            );
        }
        verify_files();
    };
    guard();
    let stable_files = physical();
    let stable_custody = custody();
    assert!(
        stable_files.len() > 4,
        "history and stage proof must not be just the visible pair"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    for round in 0..2 {
        let trace = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (quiet, lookups) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            CycleOptions {
                trace: Some(trace.clone()),
                ..options()
            },
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(
            quiet.stats.full_enumeration_reason.is_none(),
            "{:?}",
            quiet.stats
        );
        assert_eq!(quiet.stats.assets_seen, 0, "quiet feed was not empty");
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(quiet.stats.metadata_capture_refreshed, 0);
        assert_eq!(lookups, 0);
        let delta_requests = {
            let trace = trace.lock().unwrap();
            assert!(
                !trace.is_empty(),
                "quiet cycle did not request its incremental feed"
            );
            assert!(trace.iter().all(|(kind, _)| kind == "changes"));
            trace.len()
        };
        assert_bytes(&original, EDIT_ONE);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(
            &std::fs::read(media.join("IMG_0501_original.JPG.xmp")).unwrap(),
            archive_packet
        );
        assert_eq!(
            std::fs::read(original.with_extension("JPG.xmp")).unwrap(),
            adjusted_packet
        );
        guard();
        assert_eq!(
            physical(),
            stable_files,
            "media/history/stage bytes, identities, mtimes or namespace changed"
        );
        assert_eq!(
            custody(),
            stable_custody,
            "recorded custody, path proof or retry evidence changed"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        eprintln!(
            "SIDECAR_RECORDED_QUIET round={round} historical_marker={marker} files={} media_GET=0 rewrite=0 history_and_journal_growth=0 delta_requests={delta_requests} assets_seen={} full_enumeration_reason={:?}",
            stable_files.len(),
            quiet.stats.assets_seen,
            quiet.stats.full_enumeration_reason
        );
    }
}

#[tokio::test]
async fn primary_layout_read_only_plans_preserve_files_and_ownership() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (legacy, _) = cycle(root.path(), edited.clone(), false, EditedNaming::Suffix).await;
    assert_eq!(legacy.failed_count, 0);
    let requests = server.received_requests().await.unwrap().len();
    for run_mode in [
        download::DownloadRunMode::DryRun,
        download::DownloadRunMode::PrintFilenames,
    ] {
        let (result, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            CycleOptions {
                controls: download::DownloadControls::new(
                    run_mode,
                    download::DownloadReporting::hidden(),
                ),
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
        assert!(!media.join("IMG_0501_original.JPG").exists());
        assert!(!media.join(".kei-history").exists());
        for table in [
            "primary_layout_operations",
            "primary_layout_bindings",
            "primary_layout_claims",
            "primary_layout_preserved",
            "reconciliation_paths",
        ] {
            assert_eq!(
                db_count(root.path(), &format!("SELECT COUNT(*) FROM {table}")),
                0,
                "preview wrote {table}"
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
}

fn resource(uri: &str, endpoint: &str, bytes: &[u8]) -> Value {
    json!({"value":{"size":bytes.len(),"downloadURL":format!("{uri}/{endpoint}"),"fileChecksum":provider_hash(bytes)}})
}
fn live_records(
    uri: &str,
    still: &[u8],
    motion: &[u8],
    edit: Option<(&[u8], &[u8])>,
) -> Vec<Value> {
    let mut page = records_at(uri, None);
    page[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0501.HEIC");
    page[0]["fields"]["itemType"] = json!({"value":"public.heic"});
    page[0]["fields"]["resOriginalRes"] = resource(uri, "live-original-still", still);
    page[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
    page[0]["fields"]["resOriginalVidComplRes"] = resource(uri, "live-original-motion", motion);
    page[0]["fields"]["resOriginalVidComplFileType"] = json!({"value":"com.apple.quicktime-movie"});
    page[1]["fields"]["duration"] = json!({"value":0.0});
    page[1]["fields"]["vidComplDurValue"] = json!({"value":1160});
    page[1]["fields"]["vidComplDurScale"] = json!({"value":600});
    if let Some((still, motion)) = edit {
        page[1]["fields"]["resJPEGFullRes"] = resource(uri, "live-edited-still", still);
        page[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.heic"});
        page[1]["fields"]["resVidComplRes"] = resource(uri, "live-edited-motion", motion);
        page[1]["fields"]["resVidComplFileType"] = json!({"value":"com.apple.quicktime-movie"});
        page[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    }
    page
}

#[tokio::test]
async fn primary_layout_real_live_photo_pair_edits_reverts_and_reopens() {
    use crate::fs_util::file_identity;
    use crate::state::SqliteStateDb;
    use crate::state::db::{account::AccountOwner, primary_layout::LayoutOperation};
    use std::path::Path;
    // Compare sealed journal identity without its legitimately advancing phase/members.
    let journal_identity = |op: &LayoutOperation| {
        let mut value = serde_json::to_value(op).unwrap();
        value.as_object_mut().unwrap().remove("phase");
        value.as_object_mut().unwrap().remove("members");
        value
    };
    // Appending an ISO-BMFF free box changes each payload without rewriting
    // its Apple content identifier, which must survive the ordinary pipeline.
    let original_still = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/apple-live.heic"
    ));
    let original_motion = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/apple-live.mov"
    ));
    let mut edited_still = original_still.to_vec();
    edited_still.extend_from_slice(b"\0\0\0\x10freeeditone!");
    let mut edited_motion = original_motion.to_vec();
    edited_motion.extend_from_slice(b"\0\0\0\x10freeeditone!");
    let server = MockServer::start().await;
    for (endpoint, bytes) in [
        ("live-original-still", original_still.as_slice()),
        ("live-original-motion", original_motion.as_slice()),
        ("live-edited-still", edited_still.as_slice()),
        ("live-edited-motion", edited_motion.as_slice()),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{endpoint}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
    }
    for policy in [
        crate::types::LivePhotoMovFilenamePolicy::Suffix,
        crate::types::LivePhotoMovFilenamePolicy::Original,
    ] {
        let before = server.received_requests().await.unwrap().len();
        let mut root = tempfile::tempdir().unwrap();
        root.disable_cleanup(true);
        eprintln!("LIVE {policy:?} {:?}", root.path());
        let media = root.path().join("media");
        let current_motion = if policy == crate::types::LivePhotoMovFilenamePolicy::Suffix {
            "IMG_0501_HEVC.MOV"
        } else {
            "IMG_0501.MOV"
        };
        let options = || CycleOptions {
            mov_policy: policy,
            private: true,
            ..CycleOptions::default()
        };
        let original = live_records(&server.uri(), original_still, original_motion, None);
        let db_path = root.path().join("state.db");
        let (baseline, _) = cycle_options(
            root.path(),
            original.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(baseline.failed_count, 0, "{:?}", baseline.stats);
        assert!(baseline.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.HEIC"), original_still);
        assert_bytes(&media.join(current_motion), original_motion);
        let requests = server.received_requests().await.unwrap().len();
        assert_eq!(requests, before + 2);
        let owner = AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(SqliteStateDb::open_owned(&db_path, &owner).await.unwrap());
        let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
        assert!(checkpoint.is_some());
        let provider = Session {
            reported_count: None,
            records: original.clone(),
            quiet: true,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
            Arc::from("private"),
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        let build = make_run_cycle_download_config_builder(&media, db.clone());
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        config.live_photo_mov_filename_policy = policy;
        config.live_photo_mode = crate::types::LivePhotoMode::Both;
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
            0
        );
        let op = download::seed_prepared_edited_compat(&config, &pass).await;
        assert_eq!(op.phase, "prepared");
        assert_eq!(op.generation, 0);
        assert_eq!(op.child, "asset-501");
        assert_eq!(op.members.len(), 2);
        assert_eq!(
            db.get_master_record_name_for_asset("PrimarySync", "asset-501")
                .await
                .unwrap()
                .as_deref(),
            Some("501")
        );
        for (version, name, bytes) in [
            ("original", "IMG_0501.HEIC", original_still.as_slice()),
            ("live_original", current_motion, original_motion.as_slice()),
        ] {
            let member = op
                .members
                .iter()
                .find(|m| m.record.as_ref().unwrap().version_size.as_str() == version)
                .unwrap();
            let record = member.record.as_ref().unwrap();
            assert_eq!(record.checksum.as_ref(), provider_hash(bytes));
            assert_eq!(record.size_bytes, bytes.len() as u64);
            assert_eq!(
                member.destination.as_ref().unwrap().to_path(),
                media.join(name)
            );
            assert_bytes(&member.old.as_ref().unwrap().path.to_path(), bytes);
            assert_bytes(&member.prepared.as_ref().unwrap().path.to_path(), bytes);
        }
        drop(config);
        drop(build);
        drop(pass);
        drop(db);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let claim_sql = "SELECT group_concat(quote(path_key)||quote(family)||quote(operation)||quote(library)||quote(child)||quote(version)||quote(provider_checksum)||quote(native_path)||quote(local_checksum),'|') FROM (SELECT * FROM primary_layout_claims ORDER BY path_key)";
        let claims: String = conn.query_row(claim_sql, [], |r| r.get(0)).unwrap();
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
            ),
            2
        );
        conn.execute_batch("CREATE TRIGGER fail_float_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected float journal interruption'); END").unwrap();
        drop(conn);
        let error = try_cycle_options(
            root.path(),
            original.clone(),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await
        .expect_err("recorded pair commit must hit the injected trigger");
        assert!(
            format!("{error:#}").contains("injected float journal interruption"),
            "{error:#}"
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='publishing'"
            ),
            1
        );
        assert_bytes(&media.join("IMG_0501.HEIC"), original_still);
        assert_bytes(&media.join(current_motion), original_motion);
        let db = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            checkpoint
        );
        let pending = db
            .primary_layout_operations("PrimarySync".into())
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(journal_identity(&pending[0]), journal_identity(&op));
        assert_eq!(pending[0].phase, "publishing");
        assert_eq!(pending[0].members.len(), 2);
        for member in &pending[0].members {
            let installed = member.installed.as_ref().unwrap();
            let preserved = member.preserved.as_ref().unwrap();
            assert_eq!(member.destination.as_ref(), Some(&installed.path));
            assert_eq!(member.preserved_path.as_ref(), Some(&preserved.path));
            assert_eq!(
                preserved.fingerprint.sha256,
                member.old.as_ref().unwrap().fingerprint.sha256
            );
            for file in [installed, preserved] {
                let bytes = std::fs::read(file.path.to_path()).unwrap();
                assert_eq!(file.fingerprint.size, bytes.len() as u64);
                let sha256: [u8; 32] = Sha256::digest(&bytes).into();
                assert_eq!(file.fingerprint.sha256, sha256);
                let disk = std::fs::File::open(file.path.to_path()).unwrap();
                assert_eq!(file.fingerprint.identity, file_identity(&disk).unwrap());
            }
        }
        drop(db);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let preservation_sql = "SELECT group_concat(hex(evidence),'|') FROM (SELECT evidence FROM primary_layout_preserved ORDER BY member)";
        let preservation: String = conn.query_row(preservation_sql, [], |r| r.get(0)).unwrap();
        assert_eq!(
            conn.query_row(claim_sql, [], |r| r.get::<_, String>(0))
                .unwrap(),
            claims
        );
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
            0
        );
        conn.execute_batch("DROP TRIGGER fail_float_commit")
            .unwrap();
        drop(conn);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let (initial, _) = cycle_options(
            root.path(),
            original.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert!(initial.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.HEIC"), original_still);
        assert_bytes(&media.join(current_motion), original_motion);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        // Read the committed journal, binding and released debt in one SQLite snapshot.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (committed, generation, pending_claims): (i64, i64, i64) = conn.query_row("SELECT (SELECT COUNT(*) FROM primary_layout_operations WHERE phase='committed'),(SELECT generation FROM primary_layout_bindings WHERE family=?1),(SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL)", [&op.family], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(committed, 1);
        assert_eq!(generation, 1);
        assert_eq!(pending_claims, 0);
        drop(conn);
        let (quiet, lookups) = cycle_options(
            root.path(),
            original.clone(),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(lookups, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let duration: f64 = conn
            .query_row(
                "SELECT duration_secs FROM assets WHERE version_size='live_original'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(duration.to_bits(), (1160.0_f64 / 600.0).to_bits());
        drop(conn);
        let edited = live_records(
            &server.uri(),
            original_still,
            original_motion,
            Some((&edited_still, &edited_motion)),
        );
        let (changed, _) =
            cycle_options(root.path(), edited, false, EditedNaming::Primary, options()).await;
        assert_eq!(changed.failed_count, 0, "{:?}", changed.stats);
        assert!(changed.db_sync_token_advance_safe);
        assert_eq!(changed.stats.photos_downloaded, 1);
        assert_eq!(changed.stats.videos_downloaded, 1);
        type PairRow = (String, String, String, String, i64, Option<f64>, String);
        let pair_rows = || {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let mut statement = conn.prepare("SELECT a.version_size,a.checksum,a.local_path,p.local_checksum,a.size_bytes,a.duration_secs,p.source_checksum FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path AND p.provider_checksum=a.checksum WHERE a.library='PrimarySync' AND a.id='asset-501' AND a.status='downloaded' ORDER BY a.version_size").unwrap();
            statement
                .query_map([], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<PairRow>>>()
                .unwrap()
        };
        let pairs = pair_rows();
        assert_eq!(pairs.len(), 4);
        let assert_pairs = || {
            assert_eq!(pair_rows(), pairs);
            for (version, bytes) in [
                ("original", original_still.as_slice()),
                ("live_original", original_motion.as_slice()),
                ("adjusted", edited_still.as_slice()),
                ("live_adjusted", edited_motion.as_slice()),
            ] {
                let row = pairs.iter().find(|r| r.0 == version).unwrap();
                assert_eq!(row.1, provider_hash(bytes));
                assert_eq!(
                    row.3,
                    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
                );
                assert_eq!(row.4, i64::try_from(bytes.len()).unwrap());
                assert_eq!(row.6, row.3);
                assert_bytes(Path::new(&row.2), bytes);
                if version.starts_with("live_") {
                    assert_eq!(row.5.unwrap().to_bits(), (1160.0_f64 / 600.0).to_bits());
                }
            }
        };
        assert_pairs();
        assert_eq!(Path::new(&pairs[3].2), media.join("IMG_0501.HEIC"));
        assert_eq!(Path::new(&pairs[2].2), media.join(current_motion));
        assert_ne!(pairs[0].2, pairs[3].2);
        assert_ne!(pairs[1].2, pairs[2].2);
        let edit_requests = server.received_requests().await.unwrap().len();
        assert_eq!(edit_requests, requests + 2);
        let (reverted, _) = cycle_options(
            root.path(),
            original.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(reverted.failed_count, 0, "{:?}", reverted.stats);
        assert!(reverted.db_sync_token_advance_safe);
        assert_pairs();
        let requests = server.received_requests().await.unwrap().len();
        assert_eq!(requests, edit_requests);
        let ops = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        assert_eq!(ops, 1);
        for _ in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                original.clone(),
                true,
                EditedNaming::Primary,
                options(),
            )
            .await;
            assert_eq!(quiet.failed_count, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(lookups, 0);
            assert_bytes(&media.join("IMG_0501.HEIC"), original_still);
            assert_bytes(&media.join(current_motion), original_motion);
            assert_pairs();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            assert_eq!(
                conn.query_row(preservation_sql, [], |r| r.get::<_, String>(0))
                    .unwrap(),
                preservation
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                ops
            );
        }
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let receipts: Vec<Vec<u8>> = conn
            .prepare("SELECT evidence FROM primary_layout_preserved")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let preserved: Vec<Vec<u8>> = receipts
            .iter()
            .map(|bytes| {
                std::fs::read(
                    serde_json::from_slice::<state::db::primary_layout::LayoutFile>(bytes)
                        .unwrap()
                        .path
                        .to_path(),
                )
                .unwrap()
            })
            .collect();
        assert!(preserved.iter().any(|bytes| bytes == original_still));
        assert!(preserved.iter().any(|bytes| bytes == original_motion));
        root.disable_cleanup(false);
    }
}

async fn source_mutation_layout_context(
    root: &std::path::Path,
    current: Arc<std::sync::Mutex<Vec<Value>>>,
    lookups: Arc<AtomicUsize>,
) -> (download::DownloadConfig, AlbumPass) {
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let provider = Session {
        reported_count: None,
        records: current.lock().unwrap().clone(),
        quiet: true,
        lookups,
        current: Some(current.clone()),
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
        Arc::from("private"),
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    };
    let media = root.join("media");
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let build = make_run_cycle_download_config_builder(&media, store);
    let mut config = build(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    let (capture, _, _) = pass
        .album
        .publication_scope("PrimarySync")
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&db, &capture.db));
    (config.with_pass(&pass), pass)
}

#[tokio::test]
async fn primary_layout_prepared_source_drift_holds_then_recovers_same_operation() {
    use rusqlite::types::Value as SqlValue;
    use tracing::instrument::WithSubscriber;

    struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_poisoned| {
                    std::io::Error::other("source drift log capture lock poisoned")
                })?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    if std::env::var_os("KEI_SOURCE_DRIFT_RETAIN_FIXTURE").is_some() {
        root.disable_cleanup(true);
        eprintln!("source drift fixture: {}", root.path().display());
    }
    let media = root.path().join("media");
    let (initial, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        CycleOptions {
            private: true,
            baseline_successor: Some("source-original"),
            cycle_token: Some("source-original"),
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    assert_eq!(initial.stats.downloaded, 1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    let rows = |sql: &str| {
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns)
                    .map(|i| row.get::<_, SqlValue>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    };
    let custody = || {
        [
            rows("SELECT * FROM primary_layout_operations ORDER BY operation"),
            rows("SELECT * FROM primary_layout_members ORDER BY operation,member"),
            rows("SELECT * FROM primary_layout_claims ORDER BY path_key"),
            rows("SELECT * FROM primary_layout_bindings ORDER BY family"),
            rows("SELECT * FROM primary_layout_preserved ORDER BY operation,member"),
            rows(
                "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,provider_checksum,native_path",
            ),
        ]
    };
    let file = |path: &std::path::Path| {
        let handle = std::fs::File::open(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&handle).unwrap(),
            handle.metadata().unwrap().modified().unwrap(),
        )
    };
    let tree = || {
        let mut files = std::collections::BTreeMap::new();
        let mut directories = vec![media.clone()];
        while let Some(directory) = directories.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    directories.push(entry.path());
                } else {
                    assert!(entry.file_type().unwrap().is_file());
                    files.insert(entry.path(), file(&entry.path()));
                }
            }
        }
        files
    };
    let members = || {
        rows("SELECT evidence FROM primary_layout_members ORDER BY operation,member")
            .into_iter()
            .map(|row| {
                let [SqlValue::Blob(bytes)] = row.as_slice() else {
                    panic!("non-BLOB layout member")
                };
                serde_json::from_slice::<state::db::primary_layout::LayoutMember>(bytes).unwrap()
            })
            .collect::<Vec<_>>()
    };
    let visible = media.join("IMG_0501.JPG");
    let archive = media.join("IMG_0501_original.JPG");
    let original_file = file(&visible);
    assert_eq!(original_file.0, ORIGINAL);
    assert!(!archive.exists());
    let original_receipts = rows("SELECT * FROM download_receipts ORDER BY id");
    assert_eq!(original_receipts.len(), 1);

    let first = records(&server, Some(("edit-one", EDIT_ONE)));
    let second = records(&server, Some(("edit-two", EDIT_TWO)));
    let current = Arc::new(std::sync::Mutex::new(first.clone()));
    let lookups = Arc::new(AtomicUsize::new(0));
    let (op, scope) = {
        let (config, pass) =
            source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
        let db = config.state_db.as_ref().unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("source-original")
        );
        let (_, scope, _) = pass
            .album
            .publication_scope("PrimarySync")
            .unwrap()
            .unwrap();
        let op = download::seed_prepared_edited_compat(&config, &pass).await;
        assert_eq!(op.phase, "prepared");
        assert_eq!(op.pass, config.primary_layout_pass.as_deref().unwrap());
        assert_eq!(op.child, "asset-501");
        assert_eq!(op.members.len(), 2);
        assert!(!op.refresh_metadata);
        assert!(
            op.members
                .iter()
                .all(|member| member.installed.is_none() && member.preserved.is_none())
        );
        assert_eq!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .len(),
            1
        );
        (op, scope)
    };
    assert_eq!(file(&visible), original_file);
    assert!(!archive.exists());
    for member in &op.members {
        let prepared = member.prepared.as_ref().unwrap();
        let (bytes, identity, _) = file(&prepared.path.to_path());
        assert_eq!(prepared.fingerprint.identity, identity);
        assert_eq!(prepared.fingerprint.size, bytes.len() as u64);
        assert_eq!(
            prepared.fingerprint.sha256.as_slice(),
            Sha256::digest(&bytes).as_slice()
        );
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes));
        assert_eq!(prepared.source_checksum.as_deref(), Some(hash.as_str()));
        assert_eq!(prepared.provider_checksum, provider_hash(&bytes));
        assert_eq!(
            bytes,
            if prepared.version == "adjusted" {
                EDIT_ONE
            } else {
                ORIGINAL
            }
        );
    }
    assert_eq!(
        rows("SELECT * FROM download_receipts ORDER BY id"),
        original_receipts
    );
    let original_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL));
    let original_path = serde_json::to_string(
        &state::db::provider_selection::SelectionPath::from_path(&visible),
    )
    .unwrap();
    assert_eq!(
        rows(
            "SELECT r.scope,r.origin_pass,r.provider_checksum,r.source_checksum,r.local_checksum,r.native_path FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key WHERE r.library='PrimarySync' AND r.child='asset-501' AND r.master='501' AND r.state_id='asset-501' AND r.version_size='original' AND length(r.proof_hash)=64"
        ),
        vec![vec![
            SqlValue::Text(scope.clone()),
            SqlValue::Text(op.pass.clone()),
            SqlValue::Text(provider_hash(ORIGINAL)),
            SqlValue::Text(original_hash.clone()),
            SqlValue::Text(original_hash),
            SqlValue::Text(original_path)
        ]]
    );
    assert!(rows("SELECT * FROM primary_layout_bindings").is_empty());
    assert!(rows("SELECT * FROM primary_layout_preserved").is_empty());
    let held_custody = custody();
    let held_temps = rows("SELECT * FROM owned_temp_files ORDER BY path");
    let held_tree = tree();
    let checkpoints = rows("SELECT key,value FROM metadata ORDER BY key");
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(
        requests, 2,
        "only ORIGINAL and the prepared EDIT_ONE were fetched"
    );
    let recorded_edit = op
        .members
        .iter()
        .find_map(|member| {
            member
                .record
                .as_ref()
                .filter(|record| record.version_size == state::VersionSizeKey::Adjusted)
        })
        .unwrap();

    *current.lock().unwrap() = second;
    for _ in 0..2 {
        // Each iteration owns a fresh authenticated SQLite connection and source.
        let (config, pass) =
            source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
        let db = config.state_db.as_ref().unwrap();
        let observed = pass
            .album
            .confirm_layout_asset(&op.asset_record_name, &op.library)
            .await
            .unwrap()
            .asset;
        assert_eq!(observed.state_id(), op.child);
        assert_eq!(observed.id(), "501");
        let expected = download::filter::expected_paths_for(&observed, &config);
        let selected = expected
            .iter()
            .find(|path| path.version_size == state::VersionSizeKey::Adjusted)
            .unwrap();
        assert_eq!(selected.checksum.as_ref(), provider_hash(EDIT_TWO));
        let observation = state::AssetRecord::new_pending(
            Arc::from(observed.source_zone().unwrap()),
            observed.state_id().to_owned(),
            selected.version_size,
            selected.checksum.to_string(),
            selected
                .path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
            observed.created(),
            Some(observed.added_date()),
            selected.size,
            download::determine_media_type(selected.version_size, &observed),
        )
        .with_metadata_arc(download::filter::metadata_for_selected_version(
            &observed,
            &config,
            selected.version_size,
        ));
        assert_ne!(observation.checksum, recorded_edit.checksum);
        db.upsert_seen(&observation).await.expect("provider-derived EDIT_TWO observation must remain admissible while EDIT_ONE is prepared");
        let pending = db.get_pending().await.unwrap();
        let persisted = pending
            .iter()
            .find(|record| record.version_size == state::VersionSizeKey::Adjusted)
            .unwrap();
        assert_eq!(persisted.checksum, observation.checksum);
        assert_eq!(persisted.filename, observation.filename);
        assert_eq!(
            serde_json::to_value(&persisted.metadata).unwrap(),
            serde_json::to_value(&observation.metadata).unwrap()
        );
        assert!(
            db.capture_layout_recovery(op.clone())
                .await
                .unwrap()
                .is_some(),
            "catalogue/journal disagreement must require authenticated reconfirmation"
        );
        assert_eq!(
            serde_json::to_value(
                &db.primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap()[0]
            )
            .unwrap(),
            serde_json::to_value(&op).unwrap()
        );

        let logs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_target(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || LogWriter(sink.clone()))
            .finish();
        let before = lookups.load(Ordering::SeqCst);
        let held = download::recover_recorded_sources(
            &reqwest::Client::new(),
            std::slice::from_ref(&pass),
            Arc::new(config.clone()),
            CancellationToken::new(),
        )
        .with_subscriber(subscriber)
        .await
        .unwrap();
        assert!(!held.complete && !held.complete_after_smart_query);
        assert_eq!(held.stats.failed, 1);
        assert_eq!(held.stats.downloaded, 0);
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            before + 2,
            "recovery must perform fresh child and paired lookups"
        );
        let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
        let failures: Vec<_> = text.lines().filter(|line| line.contains("Could not recover managed layouts before path reconciliation; keeping checkpoints unchanged")).collect();
        assert_eq!(failures.len(), 1, "{text}");
        assert_eq!(
            failures[0]
                .rsplit_once("error=")
                .map(|(_, reason)| reason.trim()),
            Some("Recorded recovery provider facts changed"),
            "{text}"
        );
        drop(pass);
        drop(config);
        assert_eq!(custody(), held_custody);
        assert_eq!(
            rows("SELECT * FROM owned_temp_files ORDER BY path"),
            held_temps
        );
        assert_eq!(tree(), held_tree);
        assert_eq!(file(&visible), original_file);
        assert!(!archive.exists());
        assert_eq!(
            rows("SELECT * FROM download_receipts ORDER BY id"),
            original_receipts
        );
        assert_eq!(
            rows("SELECT key,value FROM metadata ORDER BY key"),
            checkpoints
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }

    // Restore every provider fact, but leave the genuine EDIT_TWO catalogue
    // observation in place so production must reconfirm before saving/committing.
    *current.lock().unwrap() = first.clone();
    {
        let (config, pass) =
            source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
        let db = config.state_db.as_ref().unwrap();
        let pending = db.get_pending().await.unwrap();
        assert_eq!(
            pending
                .iter()
                .find(|record| record.version_size == state::VersionSizeKey::Adjusted)
                .unwrap()
                .checksum
                .as_ref(),
            provider_hash(EDIT_TWO)
        );
        assert!(
            db.capture_layout_recovery(op.clone())
                .await
                .unwrap()
                .is_some()
        );
        let before = lookups.load(Ordering::SeqCst);
        let recovered = download::recover_recorded_sources(
            &reqwest::Client::new(),
            std::slice::from_ref(&pass),
            Arc::new(config.clone()),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(recovered.complete);
        assert_eq!(recovered.stats.failed, 0);
        assert_eq!(recovered.stats.downloaded, 0);
        assert_eq!(lookups.load(Ordering::SeqCst), before + 2);
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("source-original"),
            "recovery-only does not own source checkpoint advancement"
        );
        for owned in db.get_owned_temp_files_before(i64::MAX).await.unwrap() {
            assert!(
                !owned.path.exists(),
                "owned temporary link remains: {}",
                owned.path.display()
            );
        }
    }
    assert_bytes(&visible, EDIT_ONE);
    assert_bytes(&archive, ORIGINAL);
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert_eq!(
        rows("SELECT operation,phase FROM primary_layout_operations ORDER BY operation"),
        vec![vec![
            SqlValue::Text(op.operation.clone()),
            SqlValue::Text("committed".into())
        ]]
    );
    let header = rows("SELECT header FROM primary_layout_operations");
    let [SqlValue::Blob(header)] = header[0].as_slice() else {
        panic!("non-BLOB operation header")
    };
    let header: state::db::primary_layout::LayoutOperation =
        serde_json::from_slice(header).unwrap();
    let mut identity = op.clone();
    identity.members.clear();
    identity.phase = header.phase.clone();
    assert_eq!(
        serde_json::to_value(&header).unwrap(),
        serde_json::to_value(&identity).unwrap()
    );
    let bindings = rows("SELECT binding FROM primary_layout_bindings");
    assert_eq!(bindings.len(), 1);
    let [SqlValue::Blob(binding)] = bindings[0].as_slice() else {
        panic!("non-BLOB layout binding")
    };
    let binding: state::db::primary_layout::LayoutBinding =
        serde_json::from_slice(binding).unwrap();
    assert_eq!(binding.family, op.family);
    assert_eq!(binding.decision, op.decision);
    assert_eq!(binding.generation, op.generation + 1);
    let finished = members();
    assert_eq!(finished.len(), op.members.len());
    assert_eq!(binding.files.len(), finished.len());
    for (before, after) in op.members.iter().zip(&finished) {
        let mut expected = before.clone();
        expected.preserved = after.preserved.clone();
        expected.installed = after.installed.clone();
        assert_eq!(
            serde_json::to_value(&expected).unwrap(),
            serde_json::to_value(after).unwrap()
        );
        assert!(after.installed.is_some());
        assert_eq!(after.old.is_some(), after.preserved.is_some());
        for saved in [&after.preserved, &after.installed].into_iter().flatten() {
            let (bytes, identity, _) = file(&saved.path.to_path());
            assert_eq!(saved.fingerprint.identity, identity);
            assert_eq!(saved.fingerprint.size, bytes.len() as u64);
            assert_eq!(
                saved.fingerprint.sha256.as_slice(),
                Sha256::digest(&bytes).as_slice()
            );
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes));
            assert_eq!(saved.source_checksum.as_deref(), Some(hash.as_str()));
            assert_eq!(saved.provider_checksum, provider_hash(&bytes));
        }
        let installed = after.installed.as_ref().unwrap();
        assert!(
            binding
                .files
                .iter()
                .any(|saved| serde_json::to_value(saved).unwrap()
                    == serde_json::to_value(installed).unwrap())
        );
        if let Some(stage) = &after.stage
            && stage.to_path().exists()
        {
            // Conditional exchange may retain the displaced original at the
            // consumed stage, never unaccounted-for EDIT_ONE scratch bytes.
            let old = after
                .old
                .as_ref()
                .expect("only a recorded displaced original may remain at a consumed stage");
            let retained = file(&stage.to_path());
            assert_eq!(retained, original_file);
            assert_eq!(retained.1, old.fingerprint.identity);
        }
    }
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
        ),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_claims WHERE local_checksum IS NOT NULL"
        ),
        2
    );
    let mut expected_claims: Vec<_> = finished
        .iter()
        .map(|member| {
            let installed = member.installed.as_ref().unwrap();
            vec![
                SqlValue::Text(
                    state::db::primary_layout::path_key(&installed.path.to_path()).unwrap(),
                ),
                SqlValue::Text(installed.version.clone()),
                SqlValue::Text(installed.provider_checksum.clone()),
                SqlValue::Text(data_encoding::HEXLOWER.encode(&installed.fingerprint.sha256)),
                SqlValue::Blob(serde_json::to_vec(&installed.path).unwrap()),
                SqlValue::Null,
            ]
        })
        .collect();
    expected_claims.sort_by(|left, right| {
        let (SqlValue::Text(left), SqlValue::Text(right)) = (&left[0], &right[0]) else {
            panic!("non-text expected path key")
        };
        left.cmp(right)
    });
    assert_eq!(
        rows(
            "SELECT path_key,version,provider_checksum,local_checksum,native_path,operation FROM primary_layout_claims ORDER BY path_key"
        ),
        expected_claims
    );
    let prove_receipts = || {
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for (version, path, bytes) in [
            ("original", &archive, ORIGINAL),
            ("adjusted", &visible, EDIT_ONE),
        ] {
            let native = serde_json::to_string(
                &state::db::provider_selection::SelectionPath::from_path(path),
            )
            .unwrap();
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key WHERE r.scope=?1 AND r.library='PrimarySync' AND r.child='asset-501' AND r.master='501' AND r.state_id='asset-501' AND r.origin_pass=?2 AND r.version_size=?3 AND r.provider_checksum=?4 AND r.provider_size=?5 AND r.native_path=?6 AND r.source_checksum=?7 AND r.local_checksum=?7 AND length(r.proof_hash)=64", rusqlite::params![scope,op.pass,version,provider_hash(bytes),i64::try_from(bytes.len()).unwrap(),native,hash], |row| row.get(0)).unwrap();
            assert_eq!(count, 1, "missing scoped completion proof for {version}");
        }
        let receipts = rows("SELECT * FROM download_receipts ORDER BY id");
        for original in &original_receipts {
            assert!(receipts.contains(original));
        }
    };
    prove_receipts();
    let stable_custody = custody();
    let stable_tree = tree();
    for path in stable_tree.keys() {
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(!name.ends_with(".part") && !name.starts_with(".kei-replace-"));
    }
    let stable_receipts = rows("SELECT * FROM download_receipts ORDER BY id");
    for _ in 0..2 {
        let (quiet, quiet_lookups) = cycle_options(
            root.path(),
            first.clone(),
            true,
            EditedNaming::Primary,
            CycleOptions {
                private: true,
                cycle_token: Some("source-recovered"),
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(quiet_lookups, 0);
        assert_eq!(custody(), stable_custody);
        assert_eq!(tree(), stable_tree);
        assert_eq!(
            rows("SELECT * FROM download_receipts ORDER BY id"),
            stable_receipts
        );
        prove_receipts();
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let (config, _) =
            source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
        assert_eq!(
            config
                .state_db
                .as_ref()
                .unwrap()
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("source-recovered")
        );
        for owned in config
            .state_db
            .as_ref()
            .unwrap()
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
        {
            assert!(
                !owned.path.exists(),
                "quiet cleanup left an owned temporary link"
            );
        }
    }
}

#[tokio::test]
async fn primary_layout_read_only_conflict_discloses_and_keeps_local_changes() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let (initial, _) = cycle(root.path(), current.clone(), true, EditedNaming::Suffix).await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    let original = media.join("IMG_0501.JPG");
    let edited = media.join("IMG_0501_edited.JPG");
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&edited, EDIT_ONE);
    let requests = server.received_requests().await.unwrap().len();
    let provider = Session {
        reported_count: None,
        records: current.clone(),
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
        exclude_ids: Arc::default(),
    };
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
    assert!(checkpoint.is_some());
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let base = make_run_cycle_download_config_builder(&media, store);
    let mut config = base(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Suffix;
    config.refresh_metadata = true;
    let operation = download::seed_unprepared_refresh(&config, &pass).await;
    assert!(operation.refresh_metadata);
    let before = serde_json::to_value(&operation).unwrap();
    let claims = db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_claims");
    drop(config);
    drop(base);
    drop(db);
    let displaced = root.path().join("temporarily-missing-edited.JPG");
    let sidecar = media.join("IMG_0501_edited.JPG.xmp");
    for scenario in ["clean", "local-change", "missing", "sidecar-change"] {
        match scenario {
            "local-change" => std::fs::write(&edited, b"local user changes").unwrap(),
            "missing" => std::fs::rename(&edited, &displaced).unwrap(),
            "sidecar-change" => std::fs::write(&sidecar, b"new foreign sidecar").unwrap(),
            _ => {}
        }
        for run_mode in [
            download::DownloadRunMode::DryRun,
            download::DownloadRunMode::PrintFilenames,
        ] {
            let (result, _) = try_cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Suffix,
                CycleOptions {
                    controls: download::DownloadControls::new(
                        run_mode,
                        download::DownloadReporting::hidden(),
                    ),
                    ..CycleOptions::default()
                },
            )
            .await
            .unwrap();
            assert!(!result.db_sync_token_advance_safe);
            if scenario == "clean" {
                assert_eq!(result.failed_count, 0, "clean recorded owner: {result:?}");
            } else {
                assert_eq!(result.failed_count, 1, "{scenario}: {result:?}");
                assert_eq!(result.stats.failed, 1, "{scenario}");
                assert_eq!(
                    result.stats.sync_token_blocked_reason,
                    Some("recorded_layout_conflict")
                );
            }
            assert_bytes(&original, ORIGINAL);
            match scenario {
                "local-change" => assert_bytes(&edited, b"local user changes"),
                "missing" => {
                    assert!(!edited.exists());
                    assert_bytes(&displaced, EDIT_ONE);
                }
                "sidecar-change" => {
                    assert_bytes(&edited, EDIT_ONE);
                    assert_bytes(&sidecar, b"new foreign sidecar");
                }
                _ => assert_bytes(&edited, EDIT_ONE),
            }
            assert_eq!(
                db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_claims"),
                claims
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
                .await
                .unwrap();
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync").await.unwrap(),
                checkpoint
            );
            let pending = db
                .primary_layout_operations("PrimarySync".into())
                .await
                .unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(serde_json::to_value(&pending[0]).unwrap(), before);
            assert!(
                pending[0]
                    .members
                    .iter()
                    .filter_map(|member| member.stage.as_ref())
                    .all(|path| !path.to_path().exists())
            );
        }
        match scenario {
            "local-change" => std::fs::write(&edited, EDIT_ONE).unwrap(),
            "missing" => std::fs::rename(&displaced, &edited).unwrap(),
            "sidecar-change" => std::fs::remove_file(&sidecar).unwrap(),
            _ => {}
        }
    }
    for _ in 0..2 {
        let (result, _) = cycle(root.path(), current.clone(), true, EditedNaming::Suffix).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&edited, EDIT_ONE);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[tokio::test]
async fn primary_layout_root_template_transitions_cover_all_pass_kinds() {
    for pass_kind in [PassKind::Unfiled, PassKind::Album, PassKind::SmartFolder] {
        root_change_preserves_verified_paths_for_pass(pass_kind, "second").await;
    }
}

// Establish released managed ownership through the real preparation and commit
// owners, after the caller has proved ordinary authenticated downloads.
async fn seed_committed_root_journal_family(
    root: &std::path::Path,
    server: &MockServer,
) -> (String, String) {
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let provider = Session {
        reported_count: None,
        records: records(server, Some(("edit-one", EDIT_ONE))),
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
        Arc::from("private"),
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    };
    let (capture, scope, _) = pass
        .album
        .publication_scope("PrimarySync")
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&db, &capture.db));
    let media = root.join("media");
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let build = make_run_cycle_download_config_builder(&media, store);
    let mut config = build(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = true;
    }
    let expected_pass = config.with_pass(&pass).primary_layout_pass.unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let mut op = download::seed_prepared_edited_compat(&config, &pass).await;
    assert_eq!(op.phase, "prepared");
    assert_eq!(op.source_library, "PrimarySync");
    assert_eq!(op.library, "PrimarySync");
    assert_eq!(op.child, "asset-501");
    assert_eq!(op.pass, expected_pass);
    for (version, checksum) in [
        ("original", provider_hash(ORIGINAL)),
        ("adjusted", provider_hash(EDIT_ONE)),
    ] {
        assert!(op.members.iter().any(|member| {
            member.record.as_ref().is_some_and(|record| {
                record.version_size.as_str() == version && record.checksum.as_ref() == checksum
            }) && member.prepared.is_some()
        }));
    }
    download::finish_prepared_edited_compat(&config, &mut op).await;
    assert_eq!(op.phase, "committed");
    let binding = db
        .primary_layout_binding(op.family.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.generation, op.generation + 1);
    assert_eq!(binding.decision, op.decision);
    assert_eq!(binding.files.len(), 2);
    assert!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    (scope, expected_pass)
}

#[derive(Debug, PartialEq, Eq)]
struct RootJournalHistoryFile {
    recorded_path: std::path::PathBuf,
    bytes: Vec<u8>,
    identity: crate::fs_util::FileIdentity,
    mtime: std::time::SystemTime,
}

fn root_journal_history_snapshot(
    root: &std::path::Path,
    parent_mapping: Option<(&std::path::Path, &std::path::Path)>,
) -> Vec<RootJournalHistoryFile> {
    use crate::state::db::primary_layout::{LayoutFile, LayoutFingerprint, LayoutMember};
    use std::io::Read as _;
    use std::path::PathBuf;

    let conn = rusqlite::Connection::open_with_flags(
        root.join("state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let members: Vec<LayoutMember> = conn
        .prepare("SELECT evidence FROM primary_layout_members ORDER BY operation,member")
        .unwrap()
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|row| serde_json::from_slice(&row.unwrap()).unwrap())
        .collect();
    assert!(
        !members.is_empty(),
        "fixture has no recorded layout members"
    );
    let resolve = |path: &std::path::Path| {
        parent_mapping
            .and_then(|(old, moved)| path.strip_prefix(old).ok().map(|tail| moved.join(tail)))
            .unwrap_or_else(|| path.to_path_buf())
    };
    let mut snapshots = Vec::new();
    let mut capture = |recorded_path: PathBuf, expected: &LayoutFingerprint| {
        let physical_path = resolve(&recorded_path);
        let mut file = std::fs::File::open(&physical_path).unwrap_or_else(|error| {
            panic!("retained history {recorded_path:?} at {physical_path:?}: {error}")
        });
        let identity = crate::fs_util::file_identity(&file).unwrap();
        let mtime = file.metadata().unwrap().modified().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(
            u64::try_from(bytes.len()).unwrap(),
            expected.size,
            "retained history size at {recorded_path:?}"
        );
        assert_eq!(
            Sha256::digest(&bytes).as_slice(),
            expected.sha256.as_slice(),
            "retained history bytes at {recorded_path:?}"
        );
        assert_eq!(
            identity, expected.identity,
            "retained history identity at {recorded_path:?}"
        );
        eprintln!(
            "ROOT_HISTORY recorded={recorded_path:?} physical={physical_path:?} bytes={}",
            bytes.len()
        );
        snapshots.push(RootJournalHistoryFile {
            recorded_path,
            bytes,
            identity,
            mtime,
        });
    };
    let mut capture_file = |saved: &LayoutFile, path: PathBuf| {
        capture(path.clone(), &saved.fingerprint);
        #[cfg(feature = "xmp")]
        assert!(
            saved.sidecar.is_some(),
            "fixture has no recorded history sidecar at {path:?}"
        );
        if let Some(sidecar) = &saved.sidecar {
            let mut native = path.into_os_string();
            native.push(".xmp");
            capture(native.into(), sidecar);
        }
    };
    for member in members {
        let preserved = member
            .preserved
            .as_ref()
            .expect("fixture member was not preserved");
        capture_file(preserved, preserved.path.to_path());
        assert!(
            member.retirement.is_none(),
            "unexpected retired fixture alias"
        );
        if let Some(stage) = &member.stage {
            let path = stage.to_path();
            if resolve(&path).try_exists().unwrap() {
                let displaced = member
                    .old
                    .as_ref()
                    .expect("retained stage has no old-file proof");
                capture_file(displaced, path);
            }
        }
    }
    snapshots.sort_by(|a, b| a.recorded_path.cmp(&b.recorded_path));
    assert!(
        !snapshots.is_empty(),
        "fixture has no retained history files"
    );
    snapshots
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn primary_layout_root_drift_holds_recorded_media_and_exact_sidecar_journals() {
    use std::os::unix::ffi::OsStrExt;

    let server = server().await;
    for sidecar_journal in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let edited = records(&server, Some(("edit-one", EDIT_ONE)));
        let options = |media_directory| CycleOptions {
            media_directory,
            private: true,
            #[cfg(feature = "xmp")]
            sidecar: true,
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            options("media"),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert!(initial.db_sync_token_advance_safe);
        assert_eq!(initial.stats.downloaded, 2);
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
        let _ = seed_committed_root_journal_family(root.path(), &server).await;
        let history = root_journal_history_snapshot(root.path(), None);
        let current = root.path().join("media/IMG_0501.JPG");
        let archive = root.path().join("media/IMG_0501_original.JPG");
        let sidecar = current.with_file_name("IMG_0501.JPG.xmp");
        let packet = std::fs::read(&sidecar).ok();
        let archive_sidecar = archive.with_file_name("IMG_0501_original.JPG.xmp");
        let archive_packet = std::fs::read(&archive_sidecar).ok();
        #[cfg(feature = "xmp")]
        assert!(
            packet.is_some() && archive_packet.is_some(),
            "committed family sidecars must exist"
        );
        let target = if sidecar_journal { &sidecar } else { &current };
        let journal = target.with_file_name(format!(
            ".kei-replace-{}",
            data_encoding::HEXLOWER.encode(&Sha256::digest(target.file_name().unwrap().as_bytes()))
        ));
        std::fs::create_dir(&journal).unwrap();
        std::fs::write(
            journal.join("manifest.json"),
            b"retained replacement evidence",
        )
        .unwrap();
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        let claims = || {
            rusqlite::Connection::open(root.path().join("state.db")).unwrap()
                .query_row("SELECT group_concat(path_key || ':' || COALESCE(local_checksum,''), '|') FROM (SELECT path_key,local_checksum FROM primary_layout_claims ORDER BY path_key)", [], |row| row.get::<_, String>(0)).unwrap()
        };
        let original_claims = claims();
        let bindings = db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings");
        assert_eq!(bindings, 1);
        let requests = server.received_requests().await.unwrap().len();
        // Each cycle reopens SQLite and must preserve the same held generation.
        for _ in 0..2 {
            let (held, lookups) = cycle_options(
                root.path(),
                edited.clone(),
                false,
                EditedNaming::Primary,
                options("new-root"),
            )
            .await;
            assert!(held.failed_count > 0, "{:?}", held.stats);
            assert!(!held.db_sync_token_advance_safe);
            assert_eq!(
                lookups, 0,
                "source inspection must precede provider planning"
            );
            assert_bytes(&current, EDIT_ONE);
            assert_bytes(&archive, ORIGINAL);
            assert_eq!(std::fs::read(&sidecar).ok(), packet);
            assert_eq!(std::fs::read(&archive_sidecar).ok(), archive_packet);
            assert_eq!(root_journal_history_snapshot(root.path(), None), history);
            assert_eq!(
                std::fs::read(journal.join("manifest.json")).unwrap(),
                b"retained replacement evidence"
            );
            assert!(
                std::fs::read_dir(root.path().join("new-root"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                operations
            );
            assert_eq!(claims(), original_claims);
            assert_eq!(
                db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
                bindings
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }
        // Removing the obstruction permits ordinary reuse, not a root migration.
        std::fs::remove_dir_all(&journal).unwrap();
        for quiet in [false, true] {
            let (resumed, lookups) = cycle_options(
                root.path(),
                edited.clone(),
                quiet,
                EditedNaming::Primary,
                options("new-root"),
            )
            .await;
            assert_eq!(resumed.failed_count, 0, "{:?}", resumed.stats);
            assert!(resumed.db_sync_token_advance_safe);
            assert_eq!(resumed.stats.downloaded, 0);
            if quiet {
                assert_eq!(lookups, 0);
            }
            assert_bytes(&current, EDIT_ONE);
            assert_bytes(&archive, ORIGINAL);
            assert_eq!(std::fs::read(&sidecar).ok(), packet);
            assert_eq!(std::fs::read(&archive_sidecar).ok(), archive_packet);
            assert_eq!(root_journal_history_snapshot(root.path(), None), history);
            assert!(
                std::fs::read_dir(root.path().join("new-root"))
                    .unwrap()
                    .next()
                    .is_none()
            );
            assert_eq!(claims(), original_claims);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                operations
            );
            assert_eq!(
                db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
                bindings
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }
    }
}

#[tokio::test]
#[tracing_test::traced_test]
async fn primary_layout_root_drift_after_old_parent_moves_holds_until_exact_parent_restoration() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = |media_directory, token| CycleOptions {
        media_directory,
        cycle_token: Some(token),
        private: true,
        #[cfg(feature = "xmp")]
        sidecar: true,
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options("media", "root-parent-before"),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    assert_eq!(initial.stats.downloaded, 2);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
    let (scope, pass_key) = seed_committed_root_journal_family(root.path(), &server).await;
    let history = root_journal_history_snapshot(root.path(), None);
    let db_path = root.path().join("state.db");
    let custody = || {
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        [
            "SELECT * FROM primary_layout_bindings ORDER BY family",
            "SELECT * FROM primary_layout_operations ORDER BY operation",
            "SELECT * FROM primary_layout_members ORDER BY operation,member",
            "SELECT * FROM primary_layout_claims ORDER BY path_key",
            "SELECT * FROM primary_layout_preserved ORDER BY operation,member",
            "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,native_path",
            "SELECT * FROM download_receipts ORDER BY id",
            "SELECT library,id,version_size,local_path,provider_checksum,source_checksum,local_checksum FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
        ]
        .into_iter()
        .map(|sql| {
            let mut stmt = conn.prepare(sql).unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |row| {
                (0..columns)
                    .map(|column| row.get::<_, rusqlite::types::Value>(column))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        })
        .collect::<Vec<_>>()
    };
    let original_custody = custody();
    assert_eq!(original_custody[0].len(), 1);
    assert_eq!(original_custody[1].len(), 1);
    assert_eq!(original_custody[2].len(), 2);
    assert_eq!(original_custody[3].len(), 2);
    assert_eq!(original_custody[6].len(), 2);
    let media = root.path().join("media");
    let moved = root.path().join("moved-media");
    let files = |directory: &std::path::Path| {
        [
            "IMG_0501.JPG",
            "IMG_0501_original.JPG",
            "IMG_0501.JPG.xmp",
            "IMG_0501_original.JPG.xmp",
        ]
        .into_iter()
        .filter(|name| cfg!(feature = "xmp") || !name.ends_with(".xmp"))
        .map(|name| {
            let path = directory.join(name);
            let file = std::fs::File::open(&path).unwrap();
            (
                name,
                std::fs::read(&path).unwrap(),
                crate::fs_util::file_identity(&file).unwrap(),
                file.metadata().unwrap().modified().unwrap(),
            )
        })
        .collect::<Vec<_>>()
    };
    let original_files = files(&media);
    let checkpoint = || {
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        conn.query_row(
            "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
    };
    assert_eq!(checkpoint(), "root-parent-before");
    {
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        for (version, name, bytes) in [
            ("original", "IMG_0501_original.JPG", ORIGINAL),
            ("adjusted", "IMG_0501.JPG", EDIT_ONE),
        ] {
            let destination = media.join(name);
            let native = serde_json::to_string(
                &state::db::provider_selection::SelectionPath::from_path(&destination),
            )
            .unwrap();
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key WHERE r.scope=?1 AND r.library='PrimarySync' AND r.child='asset-501' AND r.master='501' AND r.state_id='asset-501' AND r.origin_pass=?2 AND r.version_size=?3 AND r.provider_checksum=?4 AND r.provider_size=?5 AND r.native_path=?6 AND r.source_checksum=?7 AND r.local_checksum=?7 AND length(r.proof_hash)=64", rusqlite::params![scope,pass_key,version,provider_hash(bytes),i64::try_from(bytes.len()).unwrap(),native,hash], |row| row.get(0)).unwrap();
            assert_eq!(
                count, 1,
                "missing authenticated recorded-path receipt for {version}"
            );
        }
    }
    let new_root_is_empty = || {
        assert!(
            std::fs::read_dir(root.path().join("new-root"))
                .unwrap()
                .next()
                .is_none()
        );
        let conn = rusqlite::Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let admitted: i64 = conn
            .query_row(
                "SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND compat_path LIKE ?1",
                [format!("{}/%", root.path().join("new-root").display())],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(admitted, 0);
    };
    std::fs::rename(&media, &moved).unwrap();
    assert_eq!(
        root_journal_history_snapshot(root.path(), Some((&media, &moved))),
        history
    );
    let requests = server.received_requests().await.unwrap().len();
    for (round, token) in ["root-parent-held-one", "root-parent-held-two"]
        .into_iter()
        .enumerate()
    {
        assert_eq!(
            std::fs::symlink_metadata(&media).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let (held, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            options("new-root", token),
        )
        .await;
        assert!(held.failed_count > 0, "{:?}", held.stats);
        assert!(!held.db_sync_token_advance_safe);
        assert_eq!(held.stats.state_write_failures, 1);
        assert_eq!(held.stats.enumeration_errors, 0);
        assert_eq!(
            held.stats.sync_token_blocked_reason,
            Some("selection_generation_deferred")
        );
        logs_assert(|lines| {
            let refusals = lines
                .iter()
                .filter(|line| line.contains("Selection projection refused canonical mutation error=Recorded old-root file is unavailable"))
                .count();
            if refusals == round + 1 {
                Ok(())
            } else {
                Err(format!(
                    "expected {} exact old-root refusals, got {refusals}",
                    round + 1
                ))
            }
        });
        assert_eq!(held.stats.downloaded, 0);
        assert_eq!(checkpoint(), "root-parent-before");
        assert_eq!(custody(), original_custody);
        assert_eq!(files(&moved), original_files);
        assert_eq!(
            root_journal_history_snapshot(root.path(), Some((&media, &moved))),
            history
        );
        new_root_is_empty();
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        eprintln!(
            "ROOT_PARENT_HELD round={round} candidate={token} checkpoint={} unchanged_custody=true unchanged_files=true GETs={requests}",
            checkpoint()
        );
    }
    // Restore only this fixture's exact recorded namespace, not a discovered path.
    std::fs::rename(&moved, &media).unwrap();
    assert!(!moved.exists());
    assert_eq!(files(&media), original_files);
    assert_eq!(root_journal_history_snapshot(root.path(), None), history);
    let (restored, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options("new-root", "root-parent-restored"),
    )
    .await;
    assert_eq!(restored.failed_count, 0, "{:?}", restored.stats);
    assert!(restored.db_sync_token_advance_safe);
    assert_eq!(restored.stats.downloaded, 0);
    assert_eq!(checkpoint(), "root-parent-restored");
    assert_eq!(custody(), original_custody);
    assert_eq!(files(&media), original_files);
    assert_eq!(root_journal_history_snapshot(root.path(), None), history);
    new_root_is_empty();
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    eprintln!(
        "ROOT_PARENT_RESTORED checkpoint={} unchanged_custody=true unchanged_files=true GETs={requests}",
        checkpoint()
    );
    for round in 0..2 {
        let (quiet, lookups) = cycle_options(
            root.path(),
            edited.clone(),
            true,
            EditedNaming::Primary,
            options("new-root", "root-parent-restored"),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_eq!(checkpoint(), "root-parent-restored");
        assert_eq!(custody(), original_custody);
        assert_eq!(files(&media), original_files);
        assert_eq!(root_journal_history_snapshot(root.path(), None), history);
        new_root_is_empty();
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        eprintln!(
            "ROOT_PARENT_QUIET round={round} unchanged_custody=true unchanged_files=true GETs={requests} downloads=0 lookups=0"
        );
    }
}

#[tokio::test]
async fn primary_layout_stale_writers_and_downloaded_receipts_cannot_finalize_current_slots() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (initial, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        CycleOptions {
            private: true,
            baseline_successor: Some("guard-checkpoint"),
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    assert!(initial.db_sync_token_advance_safe);
    let media = root.path().join("media");
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let source = records(&server, Some(("edit-one", EDIT_ONE)));
    let provider = Session {
        reported_count: None,
        records: source,
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
        Arc::from("private"),
    );
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album,
        exclude_ids: Arc::default(),
    };
    let expected_scope = json!({
        "zone": {"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
        "name":"TestAlbum",
        "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
        "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
        "query_filter":null, "container_id":null, "cross_zone_sources":[]
    });
    assert_eq!(pass.album.selection_scope(), expected_scope);
    let expected_pass = format!(
        "{:x}",
        Sha256::digest(
            json!({"kind":"Unfiled","scope":expected_scope,"excludes":[]})
                .to_string()
                .as_bytes()
        )
    );
    let store: Arc<dyn download::DownloadStore> = db.clone();
    let build = make_run_cycle_download_config_builder(&media, store);
    let mut config = build(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    let mut op = download::seed_prepared_edited_compat(&config, &pass).await;

    assert_eq!(op.phase, "prepared");
    assert_eq!(op.generation, 0);
    assert_eq!(op.library, "PrimarySync");
    assert_eq!(op.child, "asset-501");
    assert_eq!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .len(),
        1
    );
    let current = root.path().join("media/IMG_0501.JPG");
    assert!(op.members.iter().any(|m| {
        m.destination
            .as_ref()
            .is_some_and(|p| p.to_path() == current)
    }));
    assert!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
        ) > 0
    );
    let current_member = op
        .members
        .iter()
        .find(|m| {
            m.destination
                .as_ref()
                .is_some_and(|p| p.to_path() == current)
        })
        .unwrap();
    let current_path_key = current_member.path_key.clone().unwrap();
    let original_path = root.path().join("media/IMG_0501_original.JPG");
    let original_path_key = op
        .members
        .iter()
        .find(|m| {
            m.destination
                .as_ref()
                .is_some_and(|p| p.to_path() == original_path)
        })
        .unwrap()
        .path_key
        .clone()
        .unwrap();
    let record = current_member.record.as_ref().unwrap();
    assert_eq!(record.version_size.as_str(), "adjusted");
    assert_eq!(record.checksum.as_ref(), provider_hash(EDIT_ONE));
    assert_eq!(op.source_library, "PrimarySync");
    assert_eq!(op.pass, expected_pass);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let claim: (String, String, String, String, String) = conn.query_row(
        "SELECT operation, library, child, version, provider_checksum FROM primary_layout_claims WHERE path_key=?1",
        [current_member.path_key.as_deref().unwrap()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    ).unwrap();
    assert_eq!(
        claim,
        (
            op.operation.clone(),
            "PrimarySync".into(),
            "asset-501".into(),
            "adjusted".into(),
            provider_hash(EDIT_ONE)
        )
    );
    // Snapshot complete rows, including encoded operation/member evidence,
    // all claims, selected-work retry/progress flags and all receipt fields.
    let tables = [
        "primary_layout_operations",
        "primary_layout_members",
        "primary_layout_claims",
        "primary_layout_preserved",
        "primary_layout_bindings",
        "provider_selection_rank_pages",
        "provider_selection_rank_records",
        "provider_active_generations",
        "provider_active_decisions",
        "provider_active_sources",
        "provider_active_destinations",
        "provider_selection_generations",
        "provider_selection_sources",
        "provider_selection_decisions",
        "provider_selection_destinations",
        "download_receipts",
        "asset_metadata_paths",
        "assets",
        "metadata",
    ];
    let snapshot = || -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        tables
            .iter()
            .map(|table| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                let columns = stmt.column_count();
                let mut rows = stmt
                    .query_map([], |row| {
                        (0..columns)
                            .map(|i| row.get(i))
                            .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                rows.sort_by_key(|row| format!("{row:?}"));
                rows
            })
            .collect()
    };
    let before = snapshot();
    for (table, expected) in [
        ("provider_active_generations", 1),
        ("provider_active_decisions", 1),
        ("provider_active_sources", 0),
        ("provider_active_destinations", 2),
    ] {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, expected, "unexpected selected work in {table}");
    }
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("guard-checkpoint")
    );
    let original = root.path().join("media/IMG_0501_original.JPG");
    assert_bytes(&current, EDIT_ONE);
    assert_bytes(&original, ORIGINAL);
    let current_mtime = std::fs::metadata(&current).unwrap().modified().unwrap();
    let original_mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    use crate::state::db::MetadataRewriteStore as _;
    assert!(db.guard_primary_writer(&current).await.is_err());
    assert_eq!(snapshot(), before, "writer guard changed durable state");
    for version in ["original", "adjusted"] {
        assert!(
            db.mark_downloaded(
                "PrimarySync",
                "asset-501",
                version,
                &current,
                "forged-local-sha",
                None
            )
            .await
            .is_err()
        );
        assert_eq!(
            snapshot(),
            before,
            "{version} refusal changed durable state"
        );
        assert_bytes(&current, EDIT_ONE);
        assert_bytes(&original, ORIGINAL);
        assert_eq!(
            std::fs::metadata(&current).unwrap().modified().unwrap(),
            current_mtime
        );
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            original_mtime
        );
    }
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("guard-checkpoint")
    );
    download::finish_prepared_edited_compat(&config, &mut op).await;
    assert!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert_bytes(&current, EDIT_ONE);
    assert_bytes(&original, ORIGINAL);
    let binding = db
        .primary_layout_binding(op.family.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.generation, 1);
    assert_eq!(
        (
            binding.library.as_str(),
            binding.source_library.as_str(),
            binding.child.as_str()
        ),
        ("PrimarySync", "PrimarySync", "asset-501")
    );
    for (version, path, bytes) in [
        ("adjusted", &current, EDIT_ONE),
        ("original", &original, ORIGINAL),
    ] {
        let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        let file = binding
            .files
            .iter()
            .find(|f| f.version == version && f.path.to_path() == *path)
            .unwrap();
        assert_eq!(
            data_encoding::HEXLOWER.encode(&file.fingerprint.sha256),
            checksum
        );
        assert_eq!(file.provider_checksum, provider_hash(bytes));
        let row: (String, String, String, String) = conn.query_row(
            "SELECT status, local_path, checksum, local_checksum FROM assets WHERE library=?1 AND id=?2 AND version_size=?3",
            ["PrimarySync", "asset-501", version], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
        ).unwrap();
        assert_eq!(
            row,
            (
                "downloaded".into(),
                path.to_str().unwrap().into(),
                provider_hash(bytes),
                checksum.clone()
            )
        );
        let receipt: (String, String) = conn.query_row(
            "SELECT provider_checksum, local_checksum FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4",
            ["PrimarySync", "asset-501", version, path.to_str().unwrap()], |r| Ok((r.get(0)?,r.get(1)?))
        ).unwrap();
        assert_eq!(receipt, (provider_hash(bytes), checksum));
    }
    let phase: String = conn
        .query_row(
            "SELECT phase FROM primary_layout_operations WHERE operation=?1",
            [&op.operation],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(phase, "committed");
    let claim_after: (Option<String>, String) = conn
        .query_row(
            "SELECT operation, local_checksum FROM primary_layout_claims WHERE path_key=?1",
            [&current_path_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        claim_after,
        (
            None,
            data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_ONE))
        )
    );
    let original_claim: (Option<String>, String) = conn
        .query_row(
            "SELECT operation, local_checksum FROM primary_layout_claims WHERE path_key=?1",
            [&original_path_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        original_claim,
        (
            None,
            data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL))
        )
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("guard-checkpoint")
    );
}

// Source metadata retries read valid JPEG structure; truncated marker bytes
// are suitable for byte-only transitions but cannot qualify sidecar migration.
const VALID_ORIGINAL: &[u8] = include_bytes!("../../../../tests/data/media/metadata.jpg");
const VALID_EDIT: &[u8] = include_bytes!("../../../../tests/data/media/pattern.jpg");
fn records_with_original(
    server: &MockServer,
    original: &[u8],
    edit: Option<(&str, &[u8])>,
) -> Vec<Value> {
    let mut result = records(server, edit);
    result[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(original.len());
    result[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(provider_hash(original));
    result
}
async fn server_with_media(original: &[u8], edit_one: &[u8], edit_two: &[u8]) -> MockServer {
    let server = MockServer::start().await;
    for (name, bytes) in [
        ("original", original),
        ("edit-one", edit_one),
        ("edit-two", edit_two),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
    }
    server
}

// Regression cases supplied by the independent review of c202d07, extended
// with exact sidecar and catalogue oracles and two quiet reopened cycles.
#[tokio::test]
async fn review501_adjusted_only_preserves_owned_original_at_recorded_path() {
    use state::db::provider_selection::SelectionPath;

    #[cfg(feature = "xmp")]
    let sidecar_modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let sidecar_modes = [false];
    for sidecar in sidecar_modes {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let mut root = tempfile::tempdir().unwrap();
        if std::env::var_os("KEI_ADJUSTED_ONLY_RETAIN_FIXTURE").is_some() {
            root.disable_cleanup(true);
        }
        eprintln!(
            "ADJUSTED_ONLY_ROOT sidecar={sidecar} root={}",
            root.path().display()
        );
        let media = root.path().join("media");
        let options = |resolution| CycleOptions {
            private: true,
            resolution,
            #[cfg(feature = "xmp")]
            sidecar,
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            records_with_original(&server, VALID_ORIGINAL, None),
            false,
            EditedNaming::Suffix,
            options(crate::types::PhotoResolution::Original),
        )
        .await;
        assert_eq!(
            (initial.failed_count, initial.stats.downloaded),
            (0, 1),
            "{initial:?}"
        );
        assert!(initial.db_sync_token_advance_safe);
        let p = media.join("IMG_0501.JPG");
        assert_bytes(&p, VALID_ORIGINAL);
        let rows = |sql: &str| {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut stmt = conn.prepare(sql).unwrap();
            let columns = stmt.column_count();
            stmt.query_map([], |row| {
                (0..columns)
                    .map(|i| row.get::<_, rusqlite::types::Value>(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        };
        let file = |path: &std::path::Path| {
            let handle = std::fs::File::open(path).unwrap();
            (
                std::fs::read(path).unwrap(),
                crate::fs_util::file_identity(&handle).unwrap(),
                handle.metadata().unwrap().modified().unwrap(),
            )
        };
        let original_file = file(&p);
        let original_sidecar = sidecar.then(|| file(&media.join("IMG_0501.JPG.xmp")));
        let original_sql = "SELECT * FROM download_receipts WHERE child='asset-501' AND version_size='original' ORDER BY id";
        let original_receipt = rows(original_sql);
        assert_eq!(original_receipt.len(), 1);
        let local_sql = "SELECT library,id,version_size,status,checksum,size_bytes,local_path,local_checksum,download_checksum FROM assets WHERE id='asset-501' AND version_size='original'";
        let original_local = rows(local_sql);
        let path_sql = "SELECT library,id,version_size,local_path,provider_checksum,source_checksum,local_checksum,download_checksum FROM asset_metadata_paths WHERE id='asset-501' AND version_size='original' ORDER BY local_path";
        let original_paths = rows(path_sql);
        assert_eq!(original_paths.len(), 1);
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let (account, provider) = owner.authenticated_keys().unwrap();
        let scope = json!({"container":"com.apple.photos.cloud","database":"private","environment":"production","format":1,"realm":"com","zone":{"ownerRecordName":"_defaultOwner","zoneName":"PrimarySync"}}).to_string();
        let selection = json!({"zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"name":"TestAlbum","list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted","obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted","query_filter":null,"container_id":null,"cross_zone_sources":[]});
        let pass = format!(
            "{:x}",
            Sha256::digest(
                json!({"kind":"Unfiled","scope":selection,"excludes":[]})
                    .to_string()
                    .as_bytes()
            )
        );
        let prove_receipt = |version: &str, path: &std::path::Path, bytes: &[u8]| {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let native = SelectionPath::from_path(path).key().unwrap();
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key WHERE r.account_key=?1 AND r.provider_key=?2 AND r.scope=?3 AND r.library='PrimarySync' AND r.child='asset-501' AND r.master='501' AND r.state_id='asset-501' AND r.origin_pass=?4 AND r.version_size=?5 AND r.provider_checksum=?6 AND r.provider_size=?7 AND r.native_path=?8 AND r.source_checksum=?9 AND r.local_checksum=?9 AND length(r.proof_hash)=64 AND r.trust_receipt IS NULL AND r.selected_metadata_hash IS NULL AND r.generation_id IS NULL",
                rusqlite::params![account,provider,scope,pass,version,provider_hash(bytes),i64::try_from(bytes.len()).unwrap(),native,sha], |r|r.get(0),
            ).unwrap();
            assert_eq!(count, 1, "full scoped {version} proof at {path:?}");
            let local: (String, String, String) = conn.query_row(
                "SELECT source_checksum,local_checksum,download_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size=?1 AND local_path=?2 AND provider_checksum=?3",
                rusqlite::params![version,path.to_str().unwrap(),provider_hash(bytes)], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
            ).unwrap();
            assert_eq!(local, (sha.clone(), sha.clone(), sha));
        };
        prove_receipt("original", &p, VALID_ORIGINAL);
        let edited = records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
        let (published, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None),
        )
        .await;
        assert_eq!(
            (published.failed_count, published.stats.downloaded),
            (0, 1),
            "{published:?}"
        );
        assert!(published.db_sync_token_advance_safe);
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let s: String = conn.query_row("SELECT local_path FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND status='downloaded' AND checksum=?1", [provider_hash(VALID_EDIT)], |r|r.get(0)).unwrap();
        let s = std::path::PathBuf::from(s);
        drop(conn);
        assert_ne!(s, p);
        assert_bytes(&s, VALID_EDIT);
        prove_receipt("adjusted", &s, VALID_EDIT);
        let prove_selected = || {
            let selected: i64 = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ).unwrap().query_row(
                "SELECT count(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.asset_id='asset-501' AND d.master='501' AND d.version_size='adjusted' AND d.checksum=?1 AND d.size_bytes=?2 AND d.path=?3 AND d.pass_key=?4 AND d.source_checksum=?5 AND d.local_checksum=?5 AND d.admitted=1 AND d.verified_media=1 AND g.account_key=?6 AND g.provider_key=?7 AND g.scope=?8 AND g.sealed=1 AND g.checkpoint_ready=1",
                rusqlite::params![provider_hash(VALID_EDIT),i64::try_from(VALID_EDIT.len()).unwrap(),SelectionPath::from_path(&s).key().unwrap(),pass,data_encoding::HEXLOWER.encode(&Sha256::digest(VALID_EDIT)),account,provider,scope], |r|r.get(0),
            ).unwrap();
            assert!(
                selected > 0,
                "current Adjusted must reach verified scoped selection"
            );
        };
        let adjusted_file = file(&s);
        let mut sidecar_path = s.as_os_str().to_os_string();
        sidecar_path.push(".xmp");
        let sidecar_path = std::path::PathBuf::from(sidecar_path);
        let adjusted_sidecar = sidecar.then(|| file(&sidecar_path));
        let receipts_sql = "SELECT * FROM download_receipts WHERE child='asset-501' ORDER BY id";
        let receipts = rows(receipts_sql);
        assert_eq!(
            receipts.len(),
            2,
            "no Original recertification or duplicate copy"
        );
        let adjusted_sql = "SELECT library,id,version_size,status,checksum,size_bytes,local_path,local_checksum,download_checksum FROM assets WHERE id='asset-501' AND version_size='adjusted'";
        let adjusted_local = rows(adjusted_sql);
        let checkpoint_sql = "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'";
        let checkpoint = rows(checkpoint_sql);
        assert_eq!(checkpoint.len(), 1);
        let debt_sql = "SELECT version_size,local_path,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM assets WHERE id='asset-501' ORDER BY version_size";
        let path_debt_sql = "SELECT version_size,local_path,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM asset_metadata_paths WHERE id='asset-501' ORDER BY version_size,local_path";
        let durable_debt = (rows(debt_sql), rows(path_debt_sql));
        let prove_custody = || {
            prove_receipt("original", &p, VALID_ORIGINAL);
            prove_receipt("adjusted", &s, VALID_EDIT);
            prove_selected();
            assert_eq!(file(&p), original_file);
            assert_eq!(file(&s), adjusted_file);
            assert_eq!(rows(original_sql), original_receipt);
            assert_eq!(rows(local_sql), original_local);
            assert_eq!(rows(path_sql), original_paths);
            assert_eq!(rows(receipts_sql), receipts);
            assert_eq!(rows(adjusted_sql), adjusted_local);
            assert_eq!((rows(debt_sql), rows(path_debt_sql)), durable_debt);
            if let Some(expected) = &original_sidecar {
                assert_eq!(file(&media.join("IMG_0501.JPG.xmp")), *expected);
            }
            if let Some(expected) = &adjusted_sidecar {
                assert_eq!(file(&sidecar_path), *expected);
            }
            let mut expected = vec![p.clone(), s.clone()];
            if sidecar {
                expected.extend([media.join("IMG_0501.JPG.xmp"), sidecar_path.clone()]);
            }
            expected.sort();
            let mut actual = std::fs::read_dir(&media)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            actual.sort();
            assert_eq!(
                actual, expected,
                "no archive, history, or duplicate outputs"
            );
            for table in [
                "primary_layout_operations",
                "primary_layout_bindings",
                "primary_layout_members",
                "primary_layout_claims",
                "primary_layout_preserved",
                "primary_layout_superseded_paths",
            ] {
                assert_eq!(
                    db_count(root.path(), &format!("SELECT count(*) FROM {table}")),
                    0,
                    "unexpected {table}"
                );
            }
        };
        prove_custody();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/original")
                .count(),
            1,
            "adjusted-only selection invented an unselected Original download"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/edit-one")
                .count(),
            1
        );
        // Independent optional metadata debt is not authority to rewrite P/S.
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let debt = db
            .get_pending_metadata_rewrites(100)
            .await
            .unwrap()
            .into_iter()
            .map(|a| {
                (
                    a.id.to_string(),
                    a.version_size.as_str().to_owned(),
                    a.local_path,
                )
            })
            .collect::<Vec<_>>();
        drop(db);
        eprintln!(
            "ADJUSTED_ONLY_PUBLISHED sidecar={sidecar} P={p:?} S={s:?} receipts={receipts:?} debt={debt:?} checkpoint={checkpoint:?}"
        );
        for reopen in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                edited.clone(),
                true,
                EditedNaming::Primary,
                options(crate::types::PhotoResolution::None),
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(lookups, 0);
            prove_custody();
            assert_eq!(rows(checkpoint_sql), checkpoint);
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                requests.len()
            );
            let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
                .await
                .unwrap();
            let remaining = db
                .get_pending_metadata_rewrites(100)
                .await
                .unwrap()
                .into_iter()
                .map(|a| {
                    (
                        a.id.to_string(),
                        a.version_size.as_str().to_owned(),
                        a.local_path,
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                remaining, debt,
                "quiet reopen must not discard independent metadata debt"
            );
            eprintln!(
                "ADJUSTED_ONLY_QUIET sidecar={sidecar} reopen={reopen} checkpoint_safe={} GETs={} debt={remaining:?}",
                quiet.db_sync_token_advance_safe,
                requests.len()
            );
        }
    }
}

#[tokio::test]
async fn review501_committed_archive_prepared_capture_repair_blocks_source_deletion() {
    use state::db::MetadataRewriteQueue;
    use state::db::primary_layout::{LayoutBinding, LayoutMember, LayoutOperation};

    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    if std::env::var_os("KEI_ADJUSTED_ONLY_RETAIN_FIXTURE").is_some() {
        root.disable_cleanup(true);
    }
    eprintln!("ARCHIVE_GUARD_ROOT root={}", root.path().display());
    let (initial, _) = cycle_options(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Suffix,
        CycleOptions {
            private: true,
            #[cfg(feature = "xmp")]
            sidecar: true,
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(
        (initial.failed_count, initial.stats.downloaded),
        (0, 2),
        "{initial:?}"
    );
    assert!(initial.db_sync_token_advance_safe);
    eprintln!(
        "ARCHIVE_GUARD_PHASE initial_owned_suffix_complete GETs={}",
        server.received_requests().await.unwrap().len()
    );
    // A synthetic recorded compatibility handover, not an ordinary naming
    // migration or a claim about a released database's chronology.
    let (scope, pass) = seed_committed_root_journal_family(root.path(), &server).await;
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let bytes: Vec<u8> = conn
        .query_row("SELECT binding FROM primary_layout_bindings", [], |r| {
            r.get(0)
        })
        .unwrap();
    let binding: LayoutBinding = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(binding.library, "PrimarySync");
    assert_eq!(binding.source_library, "PrimarySync");
    assert_eq!(binding.child, "asset-501");
    let archives = binding
        .files
        .iter()
        .filter(|f| f.archive_original)
        .collect::<Vec<_>>();
    assert_eq!(archives.len(), 1);
    let archive = archives[0];
    let archive_path = archive.path.to_path();
    assert_eq!(archive.version, "original");
    assert_eq!(archive.provider_checksum, provider_hash(ORIGINAL));
    let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL));
    let size = i64::try_from(ORIGINAL.len()).unwrap();
    assert_eq!(archive.source_checksum.as_deref(), Some(checksum.as_str()));
    assert_eq!(
        archive.fingerprint.sha256.as_slice(),
        Sha256::digest(ORIGINAL).as_slice()
    );
    assert_eq!(
        archive.fingerprint.size,
        u64::try_from(ORIGINAL.len()).unwrap()
    );
    assert_bytes(&archive_path, ORIGINAL);
    assert_eq!(
        crate::fs_util::file_identity(&std::fs::File::open(&archive_path).unwrap()).unwrap(),
        archive.fingerprint.identity
    );
    let current = binding
        .files
        .iter()
        .find(|f| f.version == "adjusted" && !f.archive_original)
        .unwrap();
    assert_bytes(&current.path.to_path(), EDIT_ONE);
    assert_eq!(current.provider_checksum, provider_hash(EDIT_ONE));
    assert_ne!(current.path, archive.path);
    let (operation, header): (String,Vec<u8>) = conn.query_row("SELECT operation,header FROM primary_layout_operations WHERE family=?1 AND phase='committed'", [&binding.family], |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    let op: LayoutOperation = serde_json::from_slice(&header).unwrap();
    assert_eq!(op.operation, operation);
    assert_eq!(op.family, binding.family);
    assert_eq!(op.decision, binding.decision);
    assert_eq!(op.generation + 1, binding.generation);
    assert_eq!(op.pass, pass);
    let members = conn
        .prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member")
        .unwrap()
        .query_map([&operation], |r| r.get::<_, Vec<u8>>(0))
        .unwrap()
        .map(|r| serde_json::from_slice::<LayoutMember>(&r.unwrap()).unwrap())
        .collect::<Vec<_>>();
    let member = members
        .iter()
        .find(|m| {
            m.installed
                .as_ref()
                .is_some_and(|f| f.archive_original && f.path == archive.path)
        })
        .unwrap();
    let record = member
        .record
        .as_ref()
        .expect("compatibility archive has a selected Original member");
    assert_eq!(record.version_size.as_str(), archive.version);
    assert_eq!(record.checksum.as_ref(), archive.provider_checksum);
    assert_eq!(member.destination.as_ref(), Some(&archive.path));
    assert_eq!(
        serde_json::to_value(member.installed.as_ref().unwrap()).unwrap(),
        serde_json::to_value(archive).unwrap()
    );
    for saved in binding
        .files
        .iter()
        .chain(members.iter().filter_map(|m| m.preserved.as_ref()))
    {
        let path = saved.path.to_path();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(u64::try_from(bytes.len()).unwrap(), saved.fingerprint.size);
        assert_eq!(
            Sha256::digest(&bytes).as_slice(),
            saved.fingerprint.sha256.as_slice()
        );
        assert_eq!(
            crate::fs_util::file_identity(&std::fs::File::open(&path).unwrap()).unwrap(),
            saved.fingerprint.identity
        );
        if let Some(sidecar) = &saved.sidecar {
            let mut path = path.into_os_string();
            path.push(".xmp");
            let path = std::path::PathBuf::from(path);
            let bytes = std::fs::read(&path).unwrap();
            assert_eq!(u64::try_from(bytes.len()).unwrap(), sidecar.size);
            assert_eq!(Sha256::digest(&bytes).as_slice(), sidecar.sha256.as_slice());
            assert_eq!(
                crate::fs_util::file_identity(&std::fs::File::open(&path).unwrap()).unwrap(),
                sidecar.identity
            );
        }
        #[cfg(feature = "xmp")]
        assert!(saved.sidecar.is_some());
    }
    assert!(
        members.iter().any(|m| m.preserved.is_some()),
        "archive fixture must retain real displaced history"
    );
    let active: i64 = conn.query_row("SELECT count(*) FROM primary_layout_claims WHERE family=?1 AND library=?2 AND child=?3 AND version=?4 AND provider_checksum=?5 AND local_checksum=?6 AND native_path=?7 AND operation IS NULL AND path_key=?8", rusqlite::params![binding.family,binding.library,binding.child,archive.version,archive.provider_checksum,checksum,serde_json::to_vec(&archive.path).unwrap(),state::db::primary_layout::path_key(&archive_path).unwrap()], |r|r.get(0)).unwrap();
    assert_eq!(
        active, 1,
        "archive exclusion must not be inactive superseded-path exclusion"
    );
    let master: String = conn.query_row("SELECT master_record_name FROM asset_master_mappings WHERE library=?1 AND asset_record_name=?2", rusqlite::params![binding.library,binding.child], |r|r.get(0)).unwrap();
    assert_eq!(master, "501");
    let local: (String,String,String) = conn.query_row("SELECT source_checksum,local_checksum,download_checksum FROM asset_metadata_paths WHERE library=?1 AND id=?2 AND version_size=?3 AND provider_checksum=?4 AND local_path=?5", rusqlite::params![binding.library,binding.child,archive.version,archive.provider_checksum,archive_path.to_str().unwrap()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        local,
        (checksum.clone(), checksum.clone(), checksum.clone())
    );
    let prepared_count = || {
        conn.query_row("SELECT (SELECT count(*) FROM assets WHERE capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL) + (SELECT count(*) FROM asset_metadata_paths WHERE capture_repair_output_checksum IS NOT NULL OR capture_repair_output_size IS NOT NULL)", [], |r|r.get::<_,i64>(0)).unwrap()
    };
    assert_eq!(prepared_count(), 0, "no other repair may explain refusal");
    // Explicit historical-debt test-state input, not a new archive writer.
    // Without this marker, an empty ordinary queue could pass vacuously.
    for (table, resource) in [
        ("assets", "checksum"),
        ("asset_metadata_paths", "provider_checksum"),
    ] {
        assert_eq!(conn.execute(&format!("UPDATE {table} SET metadata_write_failed_at=1 WHERE library=?1 AND id=?2 AND version_size=?3 AND {resource}=?4 AND local_path=?5 AND local_checksum=?6"), rusqlite::params![binding.library,binding.child,archive.version,archive.provider_checksum,archive_path.to_str().unwrap(),checksum]).unwrap(), 1);
    }
    let eligible_without_archive_filter: i64 = conn.query_row("SELECT count(*) FROM assets a WHERE library=?1 AND id=?2 AND version_size=?3 AND checksum=?4 AND local_path=?5 AND metadata_write_failed_at=1 AND status='downloaded' AND is_deleted=0 AND NOT EXISTS(SELECT 1 FROM unattributed_legacy u WHERE u.library=a.library AND u.asset_id=a.id)", rusqlite::params![binding.library,binding.child,archive.version,archive.provider_checksum,archive_path.to_str().unwrap()], |r|r.get(0)).unwrap();
    assert_eq!(eligible_without_archive_filter, 1);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
        .await
        .unwrap();
    let queue = db.get_pending_metadata_rewrites(100).await.unwrap();
    assert!(
        queue
            .iter()
            .all(|a| a.local_path.as_deref() != Some(archive_path.as_path())),
        "marked active archive must not reach the ordinary writer: {queue:?}"
    );
    // The excluded archive has no selected capture-repair receipt. Do not
    // fabricate a PendingMetadataRewrite to bypass the real API's eligibility.
    let repairs = db
        .get_pending_metadata_rewrites_page_for_queue(
            MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            100,
        )
        .await
        .unwrap();
    assert!(
        repairs
            .iter()
            .all(|r| r.asset.local_path.as_deref() != Some(archive_path.as_path())),
        "no genuinely eligible archive capture-repair API input: {repairs:?}"
    );
    eprintln!(
        "ARCHIVE_GUARD_EXCLUSION archive={archive_path:?} family={} operation={operation} active_claims={active} scope={scope} pass={pass} ordinary={queue:?} prepared_before={}",
        binding.family,
        prepared_count()
    );
    // Narrow durable-state fault from the old regression footer: only these
    // three prepared fields at the actual committed archive/resource/path.
    for (table, resource) in [
        ("assets", "checksum"),
        ("asset_metadata_paths", "provider_checksum"),
    ] {
        assert_eq!(conn.execute(&format!("UPDATE {table} SET capture_repair_metadata_hash='synthetic-prepared-repair',capture_repair_output_checksum=?6,capture_repair_output_size=?7 WHERE library=?1 AND id=?2 AND version_size=?3 AND {resource}=?4 AND local_path=?5 AND local_checksum=?6"), rusqlite::params![binding.library,binding.child,archive.version,archive.provider_checksum,archive_path.to_str().unwrap(),checksum,size]).unwrap(), 1);
        let receipt: (String,String,i64) = conn.query_row(&format!("SELECT capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM {table} WHERE library=?1 AND id=?2 AND version_size=?3 AND {resource}=?4 AND local_path=?5"), rusqlite::params![binding.library,binding.child,archive.version,archive.provider_checksum,archive_path.to_str().unwrap()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(
            receipt,
            ("synthetic-prepared-repair".into(), checksum.clone(), size)
        );
    }
    assert_eq!(
        prepared_count(),
        2,
        "only the two projections of this archive repair"
    );
    let durable = || {
        let tables = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        tables
            .into_iter()
            .map(|table| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                let columns = stmt.column_count();
                let mut rows = stmt
                    .query_map([], |r| {
                        (0..columns)
                            .map(|i| {
                                r.get::<_, rusqlite::types::Value>(i)
                                    .map(|v| format!("{v:?}"))
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                rows.sort();
                (table, rows)
            })
            .collect::<Vec<_>>()
    };
    type Physical = (
        std::path::PathBuf,
        Option<Vec<u8>>,
        crate::fs_util::FileIdentity,
        std::time::SystemTime,
    );
    fn physical(root: &std::path::Path) -> Vec<Physical> {
        let mut paths = vec![root.to_path_buf()];
        let mut census = Vec::new();
        while let Some(path) = paths.pop() {
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink());
            #[cfg(windows)]
            let handle = {
                use std::os::windows::fs::OpenOptionsExt;
                use windows_sys::Win32::Storage::FileSystem::{
                    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
                    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                };

                // Attribute-only directory/file handles can coexist with the
                // read_dir and full-byte readers below, without following links.
                std::fs::OpenOptions::new()
                    .access_mode(FILE_READ_ATTRIBUTES)
                    .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(&path)
                    .unwrap()
            };
            #[cfg(not(windows))]
            let handle = std::fs::File::open(&path).unwrap();
            let bytes = if metadata.is_dir() {
                paths.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                None
            } else {
                assert!(metadata.is_file());
                Some(std::fs::read(&path).unwrap())
            };
            census.push((
                path,
                bytes,
                crate::fs_util::file_identity(&handle).unwrap(),
                metadata.modified().unwrap(),
            ));
        }
        census.sort_by(|a, b| a.0.cmp(&b.0));
        census
    }
    let files = physical(&root.path().join("media"));
    let before = durable();
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 2);
    let error = db
        .mark_master_family_soft_deleted(&binding.library, &master, None)
        .await
        .expect_err("archival must not hide the prepared repair");
    assert!(
        matches!(&error, state::error::StateError::Invariant { operation: "mark_master_family_soft_deleted", detail } if detail == "source deletion cannot hide a prepared capture-repair receipt"),
        "wrong archive deletion failure: {error:?}"
    );
    assert_eq!(
        durable(),
        before,
        "deletion flags, debt, receipts and every durable row must survive refusal"
    );
    assert_eq!(
        physical(&root.path().join("media")),
        files,
        "all archive/current/history/stage/XMP bytes, IDs and mtimes must survive"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    eprintln!(
        "ARCHIVE_GUARD_REFUSAL error={error:?} archive={archive_path:?} master={master} files={} preserved_members={} all_durable_rows_unchanged=true GETs={requests}",
        files.len(),
        members.iter().filter(|m| m.preserved.is_some()).count()
    );
}

#[tokio::test]
async fn review501_legacy_reverted_edit_is_preserved_outside_visible_library() {
    // The historical test name predates the no-convergence policy. The
    // safety obligation is that a reverted edit remains independently owned
    // and byte-identical, at its recorded native path.
    #[cfg(feature = "xmp")]
    let sidecar_modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let sidecar_modes = [false];
    for sidecar in sidecar_modes {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let options = || CycleOptions {
            private: true,
            #[cfg(feature = "xmp")]
            sidecar,
            ..CycleOptions::default()
        };
        #[cfg(not(feature = "xmp"))]
        let _ = sidecar;
        let initial_records =
            records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
        let (initial, _) = cycle_options(
            root.path(),
            initial_records,
            false,
            EditedNaming::Suffix,
            options(),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert!(initial.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_ORIGINAL);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), VALID_EDIT);
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let wrong_owner = state::db::account::AccountOwner::authenticated(
            "different@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"different-provider"}})).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            state::SqliteStateDb::open_owned(&root.path().join("state.db"), &wrong_owner).await,
            Err(state::error::StateError::AccountOwnerMismatch)
        ));
        let owned = Arc::new(
            state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
                .await
                .unwrap(),
        );
        let expected_scope =
            crate::icloud::photos::inbox::ShadowCapture::new(owned.clone(), owner.clone(), "com")
                .scope(
                    "private",
                    &json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                )
                .unwrap();
        let expected_selection = json!({
            "zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
            "name":"TestAlbum",
            "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
            "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
            "query_filter":null, "container_id":null, "cross_zone_sources":[]
        });
        let expected_pass = format!(
            "{:x}",
            Sha256::digest(
                json!({"kind":"Unfiled","scope":expected_selection,"excludes":[]})
                    .to_string()
                    .as_bytes()
            )
        );
        let (account_key, provider_key) = owner.authenticated_keys().unwrap();
        let snapshot = || {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut stmt = conn.prepare(
                "SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,trust_receipt,selected_metadata_hash,generation_id) FROM download_receipts WHERE library='PrimarySync' AND child='asset-501' ORDER BY version_size"
            ).unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let receipts = snapshot();
        assert_eq!(receipts.len(), 2);
        for (receipt, version, path, bytes) in [
            (
                &receipts[0],
                "adjusted",
                media.join("IMG_0501_edited.JPG"),
                VALID_EDIT,
            ),
            (
                &receipts[1],
                "original",
                media.join("IMG_0501.JPG"),
                VALID_ORIGINAL,
            ),
        ] {
            let row: serde_json::Value = serde_json::from_str(receipt).unwrap();
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            assert_eq!(row[1], account_key);
            assert_eq!(row[2], provider_key);
            assert_eq!(row[3], expected_scope);
            assert_eq!(row[4], "PrimarySync");
            assert_eq!(row[5], "asset-501");
            assert_eq!(row[6], "501");
            assert_eq!(row[7], "asset-501");
            assert_eq!(row[8], expected_pass);
            assert_eq!(row[9], version);
            assert_eq!(row[10], provider_hash(bytes));
            assert_eq!(row[11], bytes.len());
            assert_eq!(
                row[12],
                crate::state::db::provider_selection::SelectionPath::from_path(&path)
                    .key()
                    .unwrap()
            );
            assert_eq!(row[13], hash);
            assert_eq!(row[14], hash);
            assert!(!row[0].as_str().unwrap().is_empty());
            assert!(!row[15].as_str().unwrap().is_empty());
            assert!(row[16].is_null() && row[17].is_null() && row[18].is_null());
        }
        drop(owned);
        let tree = || {
            let mut files = std::fs::read_dir(&media)
                .unwrap()
                .map(|entry| {
                    let entry = entry.unwrap();
                    let meta = entry.metadata().unwrap();
                    assert!(meta.is_file());
                    (
                        entry.file_name(),
                        std::fs::read(entry.path()).unwrap(),
                        meta.modified().unwrap(),
                    )
                })
                .collect::<Vec<_>>();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        };
        if sidecar {
            for name in ["IMG_0501.JPG.xmp", "IMG_0501_edited.JPG.xmp"] {
                assert!(
                    !std::fs::read(media.join(name)).unwrap().is_empty(),
                    "sidecar-enabled initial download must create {name}"
                );
            }
        }
        let original_tree = tree();
        let names: Vec<_> = original_tree
            .iter()
            .map(|entry| entry.0.to_string_lossy().into_owned())
            .collect();
        let expected_names = if sidecar {
            vec![
                "IMG_0501.JPG",
                "IMG_0501.JPG.xmp",
                "IMG_0501_edited.JPG",
                "IMG_0501_edited.JPG.xmp",
            ]
        } else {
            vec!["IMG_0501.JPG", "IMG_0501_edited.JPG"]
        };
        assert_eq!(
            names, expected_names,
            "initial selection must not create extra copies"
        );
        let requests = server.received_requests().await.unwrap().len();
        assert_eq!(requests, 2);
        let current = records_with_original(&server, VALID_ORIGINAL, None);
        let (reverted, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(reverted.failed_count, 0, "{:?}", reverted.stats);
        assert!(reverted.db_sync_token_advance_safe);
        assert_eq!(reverted.stats.downloaded, 0);
        assert_eq!(
            tree(),
            original_tree,
            "ordinary revert must preserve both recorded files, sidecars and mtimes"
        );
        assert_eq!(
            snapshot(),
            receipts,
            "ordinary revert must retain both exact scoped receipts"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let original_path_key = crate::state::db::provider_selection::SelectionPath::from_path(
            &media.join("IMG_0501.JPG"),
        )
        .key()
        .unwrap();
        let original_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(VALID_ORIGINAL));
        let selected: i64 = conn.query_row(
            "SELECT COUNT(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.asset_id='asset-501' AND d.master='501' AND d.version_size='original' AND d.pass_key=?1 AND d.path=?2 AND d.checksum=?3 AND d.size_bytes=?4 AND d.local_checksum=?5 AND d.source_checksum=?5 AND g.account_key=?6 AND g.provider_key=?7 AND g.scope=?8 AND d.admitted=1 AND d.verified_media=1",
            rusqlite::params![expected_pass, original_path_key, provider_hash(VALID_ORIGINAL),
                i64::try_from(VALID_ORIGINAL.len()).unwrap(), original_hash, account_key, provider_key, expected_scope],
            |row| row.get(0),
        ).unwrap();
        assert!(
            selected > 0,
            "current original must actually reach verified selection"
        );
        let checkpoint: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        for _ in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Primary,
                options(),
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(lookups, 0);
            assert_eq!(tree(), original_tree);
            assert_eq!(snapshot(), receipts);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let current_checkpoint: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(current_checkpoint, checkpoint);
        }
        eprintln!("REVERT_PRESERVATION_REACHED sidecar={sidecar}");
    }
}

#[tokio::test]
async fn zero_write_recorded_recovery_preserves_debt_across_native_paths() {
    use crate::state::db::primary_layout::{LayoutBinding, LayoutFile, LayoutMember};
    use crate::state::db::provider_selection::SelectionPath;
    use rusqlite::types::Value as SqlValue;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    type Physical = (Vec<u8>, crate::fs_util::FileIdentity, std::time::SystemTime);
    fn tree(root: &Path) -> BTreeMap<PathBuf, Physical> {
        let mut out = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(parent) = pending.pop() {
            for entry in std::fs::read_dir(parent).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else {
                    assert!(entry.file_type().unwrap().is_file());
                    let file = std::fs::File::open(entry.path()).unwrap();
                    out.insert(
                        entry.path(),
                        (
                            std::fs::read(entry.path()).unwrap(),
                            crate::fs_util::file_identity(&file).unwrap(),
                            file.metadata().unwrap().modified().unwrap(),
                        ),
                    );
                }
            }
        }
        out
    }
    fn rows(root: &Path, sql: &str) -> Vec<Vec<SqlValue>> {
        let conn = rusqlite::Connection::open_with_flags(
            root.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = conn.prepare(sql).unwrap();
        let columns = stmt.column_count();
        stmt.query_map([], |r| {
            (0..columns)
                .map(|i| r.get(i))
                .collect::<Result<Vec<SqlValue>, _>>()
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
    }
    fn json_rows(root: &Path, sql: &str) -> Vec<Value> {
        rows(root, sql)
            .into_iter()
            .map(|r| {
                let [SqlValue::Text(text)] = r.as_slice() else {
                    panic!("expected JSON row")
                };
                serde_json::from_str(text).unwrap()
            })
            .collect()
    }
    fn owner() -> state::db::account::AccountOwner {
        state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap()
    }
    async fn context(
        root: &Path,
        current: Vec<Value>,
        folder: &str,
        quiet: bool,
    ) -> (
        Arc<state::SqliteStateDb>,
        AlbumPass,
        download::DownloadConfig,
        Session,
    ) {
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
                .await
                .unwrap(),
        );
        let provider = Session {
            reported_count: Some(1),
            records: current,
            quiet,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: Some("zero-write-debt-token"),
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(""),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider.clone()),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner(), "com"),
            Arc::from("private"),
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        let observed =
            state::MembershipStore::get_asset_groupings(db.as_ref(), "PrimarySync", &["asset-501"])
                .await
                .unwrap();
        let mut groups = download::AssetGroupings::default();
        for (asset, name) in observed.albums {
            groups.albums.entry(asset).or_default().push(name);
        }
        for (asset, name) in observed.people {
            groups.people.entry(asset).or_default().push(name);
        }
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let media = root.join("media");
        let build = make_run_cycle_download_config_builder(&media, store);
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::new(groups),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = folder.into();
        config.folder_structure_albums = Arc::from(folder);
        config.folder_structure_smart_folders = Arc::from(folder);
        config.edited = true;
        config.edited_naming = EditedNaming::Suffix;
        assert_eq!(format!("{:?}", config.capture_timestamp_repair), "Preserve");
        assert!(
            !config.metadata.set_exif_datetime
                && !config.metadata.set_exif_rating
                && !config.metadata.set_exif_gps
                && !config.metadata.set_exif_description
        );
        #[cfg(feature = "xmp")]
        {
            assert!(!config.metadata.xmp_sidecar && !config.metadata.embed_xmp);
        }
        (db, pass, config, provider)
    }
    async fn cycle(
        root: &Path,
        current: Vec<Value>,
        folder: &str,
        quiet: bool,
    ) -> anyhow::Result<(crate::sync_cycle::CycleResult, usize)> {
        let (db, pass, template, provider) = context(root, current, folder, quiet).await;
        let lookups = provider.lookups.clone();
        let mut library = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            vec![pass],
        );
        library.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(provider),
            "PrimarySync",
        );
        let media = root.join("media");
        std::fs::create_dir_all(&media).unwrap();
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let base = make_run_cycle_download_config_builder(&media, store);
        let build = |mode, exclude, groups, zone| {
            let mut c = base(mode, exclude, groups, zone).as_ref().clone();
            c.folder_structure = template.folder_structure.clone();
            c.folder_structure_albums = template.folder_structure_albums.clone();
            c.folder_structure_smart_folders = template.folder_structure_smart_folders.clone();
            c.edited = true;
            c.edited_naming = EditedNaming::Suffix;
            Arc::new(c)
        };
        let globals = crate::config::GlobalArgs {
            username: Some("synthetic@example.invalid".into()),
            domain: None,
            data_dir: Some(root.to_str().unwrap().into()),
        };
        let mut config = crate::config::Config::build(
            &globals,
            &crate::cli::PasswordArgs::default(),
            crate::cli::SyncArgs::default(),
            None,
        )
        .unwrap();
        config.download.directory = media.clone();
        config.download.folder_structure = folder.into();
        config.download.folder_structure_albums = folder.into();
        config.download.folder_structure_smart_folders = folder.into();
        config.photos.edited = true;
        config.photos.edited_naming = EditedNaming::Suffix;
        let shared = Arc::new(tokio::sync::RwLock::new(
            crate::auth::session::Session::new(
                root,
                "synthetic@example.invalid",
                "https://example.com",
                None,
            )
            .await
            .unwrap(),
        ));
        let result = Box::pin(crate::sync_cycle::run_cycle(
            &[&library],
            &config,
            Some(db.as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &shared,
            &CancellationToken::new(),
        ))
        .await?;
        Ok((result, lookups.load(Ordering::SeqCst)))
    }
    const ASSETS: &str = "SELECT json_object('version',version_size,'status',status,'path',local_path,'checksum',checksum,'hash',metadata_hash,'marker',metadata_write_failed_at,'local',local_checksum,'source',download_checksum,'capture_hash',capture_repair_metadata_hash,'capture_checksum',capture_repair_output_checksum,'capture_size',capture_repair_output_size) FROM assets WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size";
    const PATHS: &str = "SELECT json_object('version',version_size,'path',local_path,'checksum',provider_checksum,'local',local_checksum,'download',download_checksum,'source',source_checksum,'marker',metadata_write_failed_at,'capture_hash',capture_repair_metadata_hash,'capture_checksum',capture_repair_output_checksum,'capture_size',capture_repair_output_size,'preserved',preserved_path_key) FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size,local_path";
    fn stable(root: &Path) -> Vec<Vec<Vec<SqlValue>>> {
        ["SELECT * FROM primary_layout_operations ORDER BY operation","SELECT * FROM primary_layout_members ORDER BY operation,member","SELECT * FROM primary_layout_preserved ORDER BY operation,member","SELECT * FROM primary_layout_bindings ORDER BY family","SELECT * FROM primary_layout_claims ORDER BY path_key","SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,provider_checksum,native_path","SELECT * FROM download_receipts ORDER BY id","SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path","SELECT * FROM asset_albums ORDER BY library,asset_id,album_name","SELECT * FROM album_containers ORDER BY library,container_id","SELECT * FROM asset_album_memberships ORDER BY library,asset_record_name,container_id"].map(|sql|rows(root,sql)).to_vec()
    }
    async fn snapshot(root: &Path, db: &state::SqliteStateDb, phase: &str) -> Value {
        let queue=match db.get_pending_metadata_rewrites_page_for_queue(state::db::MetadataRewriteQueue::Ordinary,Some(&["PrimarySync"]),0,100).await {
            Ok(items)=>json!(items.into_iter().map(|q|json!({"asset":q.asset,"source":q.source_checksum,"capture":q.capture_repair_receipt.map(|r|format!("{r:?}")),"selection":q.selection_receipt.map(|r|format!("{r:?}"))})).collect::<Vec<_>>()),Err(e)=>json!({"error":format!("{e:?}")})};
        let g = state::MembershipStore::get_asset_groupings(db, "PrimarySync", &["asset-501"])
            .await
            .unwrap();
        let packet = json!({"phase":phase,"assets":json_rows(root,ASSETS),"paths":json_rows(root,PATHS),"queue":queue,"groupings":{"albums":g.albums,"people":g.people},"proofs":format!("{:?}",stable(root)),"media":tree(&root.join("media"))});
        std::fs::write(
            root.join(format!("phase-{phase}.json")),
            serde_json::to_vec_pretty(&packet).unwrap(),
        )
        .unwrap();
        packet
    }
    fn verify(file: &LayoutFile) {
        let path = file.path.to_path();
        let handle = std::fs::File::open(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, file.fingerprint.size);
        assert_eq!(
            Sha256::digest(&bytes).as_slice(),
            file.fingerprint.sha256.as_slice()
        );
        assert_eq!(
            crate::fs_util::file_identity(&handle).unwrap(),
            file.fingerprint.identity
        );
        assert!(file.sidecar.is_none());
        assert_eq!(
            file.source_checksum.as_deref(),
            Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
        );
    }
    fn require_current_debt(packet: &Value, expected: &BTreeMap<String, Value>, parent: &Path) {
        let queue = packet["queue"].as_array().unwrap();
        for row in packet["assets"].as_array().unwrap() {
            let version = row["version"].as_str().unwrap();
            assert_eq!(row["marker"], expected[version]);
            let filename = if version == "original" {
                "IMG_0501.JPG"
            } else {
                assert_eq!(version, "adjusted");
                "IMG_0501_edited.JPG"
            };
            let path = parent.join(filename);
            assert_eq!(row["path"], path.to_str().unwrap());
            let proof = packet["paths"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["version"] == version && p["path"] == row["path"])
                .unwrap();
            assert_eq!(proof["marker"], expected[version]);
            assert_eq!(proof["checksum"], row["checksum"]);
            assert_eq!(proof["source"], row["source"]);
            assert_eq!(proof["local"], row["local"]);
            let selected: Vec<_> = queue
                .iter()
                .filter(|q| {
                    q["asset"]["version_size"]
                        .as_str()
                        .unwrap()
                        .eq_ignore_ascii_case(version)
                        && q["asset"]["local_path"] == row["path"]
                })
                .collect();
            if expected[version].is_null() {
                assert!(selected.is_empty());
            } else {
                assert_eq!(
                    selected.len(),
                    1,
                    "installed exact-resource metadata work must remain rediscoverable"
                );
                let q = selected[0];
                assert_eq!(q["asset"]["checksum"], row["checksum"]);
                assert_eq!(q["asset"]["metadata"]["metadata_hash"], row["hash"]);
                assert_eq!(q["source"], proof["source"]);
                assert!(q["capture"].is_null());
            }
        }
        for key in ["assets", "paths"] {
            for row in packet[key].as_array().unwrap() {
                for field in ["capture_hash", "capture_checksum", "capture_size"] {
                    assert!(row[field].is_null());
                }
            }
        }
    }
    for group in [Some("GroupName"), None] {
        for folder in ["", "new-parent"] {
            let server = server().await;
            let mut fixture = tempfile::tempdir().unwrap();
            if std::env::var_os("KEI_ZERO_WRITE_DEBT_RETAIN_FIXTURES").is_some() {
                fixture.disable_cleanup(true);
            }
            let root = fixture.path();
            eprintln!(
                "ZERO_WRITE_DEBT_FIXTURE root={} folder={folder:?} group={group:?}",
                root.display()
            );
            let media = root.join("media");
            let current = records(&server, Some(("edit-one", EDIT_ONE)));
            let (initial, _) = cycle(root, current.clone(), "", false).await.unwrap();
            assert_eq!(initial.failed_count, 0, "{initial:?}");
            assert_eq!(initial.stats.downloaded, 2);
            assert!(initial.db_sync_token_advance_safe);
            let db = Arc::new(
                state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
                    .await
                    .unwrap(),
            );
            let baseline = snapshot(root, &db, "initial").await;
            assert_eq!(baseline["queue"], json!([]));
            assert_eq!(baseline["groupings"], json!({"albums":[],"people":[]}));
            for key in ["assets", "paths"] {
                assert_eq!(baseline[key].as_array().unwrap().len(), 2);
                for row in baseline[key].as_array().unwrap() {
                    assert!(row["marker"].is_null());
                }
            }
            let original_tree = tree(&media);
            assert_eq!(original_tree.len(), 2);
            let original_receipts = rows(root, "SELECT * FROM download_receipts ORDER BY id");
            assert_eq!(original_receipts.len(), 2);
            if let Some(group) = group {
                let container = state::MembershipStore::upsert_album_container(
                    db.as_ref(),
                    "PrimarySync",
                    "genuine-group-501",
                    group,
                    "Album",
                )
                .await;
                std::fs::write(root.join("container-result.txt"), format!("{container:?}"))
                    .unwrap();
                container.unwrap();
                let membership = state::MembershipStore::upsert_album_membership_delta(
                    db.as_ref(),
                    "PrimarySync",
                    "genuine-group-501",
                    "asset-501",
                    Some("501"),
                    "icloud",
                )
                .await;
                std::fs::write(
                    root.join("membership-result.txt"),
                    format!("{membership:?}"),
                )
                .unwrap();
                assert!(membership.unwrap());
            }
            let debt = snapshot(root, &db, "before-plan").await;
            let expected: BTreeMap<_, _> = debt["assets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| {
                    (
                        r["version"].as_str().unwrap().to_owned(),
                        r["marker"].clone(),
                    )
                })
                .collect();
            for marker in expected.values() {
                if group.is_some() {
                    assert!(marker.is_number());
                } else {
                    assert!(marker.is_null());
                }
            }
            assert_eq!(
                debt["queue"].as_array().unwrap().len(),
                if group.is_some() { 2 } else { 0 }
            );
            require_current_debt(&debt, &expected, &media);
            assert_eq!(tree(&media), original_tree);
            assert_eq!(
                rows(root, "SELECT * FROM download_receipts ORDER BY id"),
                original_receipts
            );
            drop(db);
            let (op, scope, preparation_start, preparation_end) = {
                let (db, pass, config, _) = context(root, current.clone(), folder, false).await;
                let actual = config.with_pass(&pass);
                assert_eq!(actual.album_name.as_deref(), Some(""));
                let (_, scope, _) = pass
                    .album
                    .publication_scope("PrimarySync")
                    .unwrap()
                    .unwrap();
                let preparation_start = chrono::Utc::now().timestamp();
                let op = download::seed_prepared_edited_compat(&config, &pass).await;
                let prepared = snapshot(root, &db, "after-prepare").await;
                std::fs::write(root.join("prepared-operation.json"),serde_json::to_vec_pretty(&json!({"operation":op,"scope":scope,"group":group,"folder":folder,"metadata":format!("{:?}",actual.metadata),"capture_timestamp_repair":format!("{:?}",actual.capture_timestamp_repair)})).unwrap()).unwrap();
                assert_eq!(op.metadata_flags, 0);
                assert_eq!(op.phase, "prepared");
                assert_eq!(op.policy, "suffix");
                assert!(!op.refresh_metadata);
                assert_eq!(op.pass, actual.primary_layout_pass.as_deref().unwrap());
                assert_eq!(
                    op.root,
                    SelectionPath::from_path(&std::path::absolute(&media).unwrap())
                );
                assert_eq!(prepared["paths"], debt["paths"]);
                assert_eq!(prepared["assets"], debt["assets"]);
                assert_eq!(prepared["groupings"], debt["groupings"]);
                let asset = pass
                    .album
                    .confirm_layout_asset("asset-501", "PrimarySync")
                    .await
                    .unwrap()
                    .asset;
                let mut payloads = Vec::new();
                for member in &op.members {
                    let record = member.record.as_ref().unwrap();
                    let selected = download::filter::metadata_for_selected_version(
                        &asset,
                        &actual,
                        record.version_size,
                    );
                    assert_eq!(
                        serde_json::to_value(&record.metadata).unwrap(),
                        serde_json::to_value(&selected).unwrap()
                    );
                    assert!(selected.keywords.is_none());
                    let keywords = actual
                        .asset_groupings
                        .albums
                        .get("asset-501")
                        .cloned()
                        .unwrap_or_default();
                    assert_eq!(
                        keywords,
                        group.into_iter().map(str::to_owned).collect::<Vec<_>>()
                    );
                    let payload = json!({"timezone_offset":selected.timezone_offset,"rating":selected.rating,"latitude":selected.latitude,"longitude":selected.longitude,"altitude":selected.altitude,"title":selected.title,"description":selected.description.as_ref().or(selected.title.as_ref()),"keywords":keywords,"people":[],"is_hidden":selected.is_hidden,"is_archived":selected.is_archived,"media_subtype":selected.media_subtype,"burst_id":selected.burst_id});
                    let decision = format!(
                        "{:x}",
                        Sha256::digest(
                            serde_json::to_vec(&json!([
                                1,
                                record.version_size.as_str(),
                                record.checksum,
                                0,
                                "Preserve",
                                asset.created(),
                                payload
                            ]))
                            .unwrap()
                        )
                    );
                    let file = member.prepared.as_ref().unwrap();
                    assert_eq!(file.metadata_decision.as_deref(), Some(decision.as_str()));
                    verify(file);
                    if let Some(old) = &member.old {
                        verify(old);
                    }
                    assert!(member.installed.is_none());
                    assert_eq!(
                        member.destination.as_ref().unwrap().to_path().parent(),
                        Some(media.join(folder).as_path())
                    );
                    payloads.push(json!({"version":record.version_size.as_str(),"hash":record.metadata.metadata_hash,"payload":payload,"decision":decision}));
                }
                std::fs::write(
                    root.join("prepared-payloads.json"),
                    serde_json::to_vec_pretty(&payloads).unwrap(),
                )
                .unwrap();
                for (path, proof) in &original_tree {
                    assert_eq!(tree(&media).get(path), Some(proof));
                }
                assert!(
                    db.capture_layout_recovery(op.clone())
                        .await
                        .unwrap()
                        .is_none()
                );
                let preparation_end = chrono::Utc::now().timestamp();
                (op, scope, preparation_start, preparation_end)
            };
            let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
                .await
                .unwrap();
            let before = snapshot(root, &db, "before-recovery-reopened").await;
            assert_eq!(before["paths"], debt["paths"]);
            // The real preparation owner upserts each selected record. Validate
            // only that observation-time transition; all other queue evidence
            // stays byte-for-byte equal, including its exact native resource.
            assert!(preparation_start <= preparation_end);
            let canonical = json_rows(
                root,
                "SELECT json_object('library',library,'id',id,'version',version_size,'path',local_path,'checksum',checksum,'size',size_bytes,'hash',metadata_hash,'local',local_checksum,'download',download_checksum,'last_seen_at',last_seen_at) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND status='downloaded' ORDER BY version_size",
            );
            let queue_key = |q: &Value| {
                json!([
                    q["asset"]["library"],
                    q["asset"]["id"],
                    q["asset"]["version_size"],
                    q["asset"]["local_path"],
                    q["asset"]["checksum"],
                    q["asset"]["size_bytes"],
                    q["asset"]["metadata"]["metadata_hash"],
                    q["asset"]["local_checksum"],
                    q["asset"]["download_checksum"],
                    q["source"]
                ])
            };
            let original_queue = debt["queue"].as_array().unwrap();
            let actual_queue = before["queue"].as_array().unwrap();
            assert_eq!(actual_queue.len(), original_queue.len());
            let mut paired = BTreeSet::new();
            let mut updates = Vec::new();
            let mut observation_proof = Vec::new();
            for actual in actual_queue {
                let key = queue_key(actual);
                let expected_entries: Vec<_> = original_queue
                    .iter()
                    .enumerate()
                    .filter(|(_, q)| queue_key(q) == key)
                    .collect();
                assert_eq!(
                    expected_entries.len(),
                    1,
                    "queued resource must have one exact pre-preparation peer"
                );
                let (index, original) = expected_entries[0];
                assert!(paired.insert(index), "duplicate queued resource");
                let asset = &actual["asset"];
                let matching: Vec<_> = canonical
                    .iter()
                    .filter(|row| {
                        row["library"] == asset["library"]
                            && row["id"] == asset["id"]
                            && row["version"]
                                .as_str()
                                .unwrap()
                                .eq_ignore_ascii_case(asset["version_size"].as_str().unwrap())
                            && row["path"] == asset["local_path"]
                            && row["checksum"] == asset["checksum"]
                            && row["size"] == asset["size_bytes"]
                            && row["hash"] == asset["metadata"]["metadata_hash"]
                            && row["local"] == asset["local_checksum"]
                            && row["download"] == asset["download_checksum"]
                    })
                    .collect();
                assert_eq!(
                    matching.len(),
                    1,
                    "observation must belong to the exact canonical resource/path"
                );
                let canonical_row = matching[0];
                let seconds = canonical_row["last_seen_at"].as_i64().unwrap();
                let observed: chrono::DateTime<chrono::Utc> =
                    serde_json::from_value(asset["last_seen_at"].clone()).unwrap();
                assert_eq!(observed.timestamp_subsec_nanos(), 0);
                assert_eq!(
                    observed.timestamp(),
                    seconds,
                    "queue observation must equal its actual canonical row"
                );
                assert!(
                    (preparation_start..=preparation_end).contains(&seconds),
                    "canonical upsert observation must fall inside actual preparation calls"
                );
                updates.push((index, asset["last_seen_at"].clone()));
                observation_proof.push(json!({"resource":key,"canonical":canonical_row,"before":original["asset"]["last_seen_at"],"after":asset["last_seen_at"]}));
            }
            let mut expected_queue = debt["queue"].clone();
            for (index, observed) in updates {
                expected_queue[index]["asset"]["last_seen_at"] = observed;
            }
            assert_eq!(before["queue"], expected_queue);
            std::fs::write(root.join("preparation-observation-proof.json"),serde_json::to_vec_pretty(&json!({"operation":op.operation,"start_utc_seconds":preparation_start,"end_utc_seconds":preparation_end,"observations":observation_proof,"all_other_queue_fields_exact":true})).unwrap()).unwrap();
            assert!(
                db.primary_layout_binding(op.family.clone())
                    .await
                    .unwrap()
                    .is_none()
            );
            drop(db);
            let recovery = cycle(root, current.clone(), folder, false).await;
            std::fs::write(root.join("recovery-result.txt"), format!("{recovery:?}")).unwrap();
            let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
                .await
                .unwrap();
            let after = snapshot(root, &db, "after-recovery").await;
            let (recovered, _) = recovery.unwrap();
            assert_eq!(recovered.failed_count, 0, "{recovered:?}");
            assert_eq!(recovered.stats.downloaded, 0);
            assert!(recovered.db_sync_token_advance_safe);
            assert_eq!(server.received_requests().await.unwrap().len(), 2);
            require_current_debt(&after, &expected, &media.join(folder));
            assert_eq!(after["groupings"], debt["groupings"]);
            for old in debt["paths"].as_array().unwrap() {
                let retained = after["paths"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|p| p["path"] == old["path"] && p["version"] == old["version"])
                    .unwrap();
                for field in [
                    "marker",
                    "checksum",
                    "source",
                    "local",
                    "download",
                    "capture_hash",
                    "capture_checksum",
                    "capture_size",
                ] {
                    assert_eq!(
                        retained[field], old[field],
                        "old native-path evidence: {field}"
                    );
                }
            }
            let receipts = rows(root, "SELECT * FROM download_receipts ORDER BY id");
            for receipt in &original_receipts {
                assert!(
                    receipts.contains(receipt),
                    "historical receipt packet changed"
                );
            }
            assert_eq!(
                rows(
                    root,
                    "SELECT operation,phase FROM primary_layout_operations ORDER BY operation"
                ),
                vec![vec![
                    SqlValue::Text(op.operation.clone()),
                    SqlValue::Text("committed".into())
                ]]
            );
            let binding = db
                .primary_layout_binding(op.family.clone())
                .await
                .unwrap()
                .unwrap();
            let expected_binding = LayoutBinding {
                family: op.family.clone(),
                library: op.library.clone(),
                source_library: op.source_library.clone(),
                child: op.child.clone(),
                generation: op.generation + 1,
                decision: op.decision.clone(),
                policy: op.policy.clone(),
                qualifier: op.qualifier.clone(),
                files: op
                    .members
                    .iter()
                    .map(|m| {
                        let mut f = m.prepared.as_ref().unwrap().clone();
                        f.path = m.destination.clone().unwrap();
                        f
                    })
                    .collect(),
            };
            assert_eq!(
                serde_json::to_value(&binding).unwrap(),
                serde_json::to_value(&expected_binding).unwrap()
            );
            let members: Vec<LayoutMember> = rows(
                root,
                "SELECT evidence FROM primary_layout_members ORDER BY operation,member",
            )
            .into_iter()
            .map(|r| {
                let [SqlValue::Blob(bytes)] = r.as_slice() else {
                    panic!("missing member evidence")
                };
                serde_json::from_slice(bytes).unwrap()
            })
            .collect();
            let mut accounted = BTreeSet::new();
            let final_tree = tree(&media);
            for (member, recorded) in members.iter().zip(&op.members) {
                assert_eq!(
                    serde_json::to_value(&member.prepared).unwrap(),
                    serde_json::to_value(&recorded.prepared).unwrap()
                );
                assert_eq!(
                    serde_json::to_value(&member.record).unwrap(),
                    serde_json::to_value(&recorded.record).unwrap()
                );
                let installed = member.installed.as_ref().unwrap();
                verify(installed);
                assert_eq!(
                    installed.fingerprint,
                    recorded.prepared.as_ref().unwrap().fingerprint
                );
                if let Some(old) = &recorded.old {
                    let preserved = member.preserved.as_ref().unwrap();
                    verify(preserved);
                    assert_eq!(preserved.fingerprint.sha256, old.fingerprint.sha256);
                    assert_ne!(preserved.fingerprint.identity, old.fingerprint.identity);
                } else {
                    assert!(member.preserved.is_none());
                }
                for path in [
                    &member.destination,
                    &member.stage,
                    &member.preserved_path,
                    &member.retirement,
                ]
                .into_iter()
                .flatten()
                {
                    if path.to_path().exists() {
                        accounted.insert(path.to_path());
                    }
                }
            }
            for (path, proof) in &original_tree {
                let placements: Vec<_> = final_tree.iter().filter(|(_, p)| *p == proof).collect();
                assert!(
                    !placements.is_empty(),
                    "old identity/mtime lost: {}",
                    path.display()
                );
                for (path, _) in placements {
                    accounted.insert(path.clone());
                }
            }
            assert_eq!(accounted, final_tree.keys().cloned().collect());
            std::fs::write(root.join("committed-proof.json"),serde_json::to_vec_pretty(&json!({"operation":op.operation,"scope":scope,"binding":binding,"members":members,"files":final_tree,"expected_markers":expected,"source_finalized":true})).unwrap()).unwrap();
            drop(db);
            let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
                .await
                .unwrap();
            let reopened = snapshot(root, &db, "after-commit-reopened").await;
            require_current_debt(&reopened, &expected, &media.join(folder));
            assert_eq!(reopened["paths"], after["paths"]);
            assert_eq!(reopened["queue"], after["queue"]);
            drop(db);
            let frozen = stable(root);
            for round in 0..2 {
                let (quiet, lookups) = cycle(root, current.clone(), folder, true).await.unwrap();
                assert_eq!(quiet.failed_count, 0, "{quiet:?}");
                assert_eq!((quiet.stats.downloaded, lookups), (0, 0));
                assert!(quiet.db_sync_token_advance_safe);
                assert_eq!(server.received_requests().await.unwrap().len(), 2);
                let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner())
                    .await
                    .unwrap();
                let packet = snapshot(root, &db, &format!("quiet-{round}")).await;
                require_current_debt(&packet, &expected, &media.join(folder));
                assert_eq!(packet["queue"], reopened["queue"]);
                assert_eq!(stable(root), frozen);
                assert_eq!(tree(&media), final_tree);
                assert_eq!(
                    db.get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("zero-write-debt-token")
                );
            }
            eprintln!(
                "ZERO_WRITE_DEBT_COMPLETE root={} folder={folder:?} group={group:?} operation={} generation={} quiet_reopens=2",
                root.display(),
                op.operation,
                binding.generation
            );
        }
    }
}

#[tokio::test]
async fn recorded_old_root_finishes_before_new_resource_download() {
    use crate::state::db::primary_layout::{
        LayoutBinding, LayoutFile, LayoutFingerprint, LayoutOperation,
    };
    use crate::state::db::provider_selection::SelectionPath;
    use rusqlite::types::Value as SqlValue;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    type Physical = (Vec<u8>, crate::fs_util::FileIdentity, std::time::SystemTime);
    fn file(path: &Path) -> Physical {
        let handle = std::fs::File::open(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&handle).unwrap(),
            handle.metadata().unwrap().modified().unwrap(),
        )
    }
    fn tree(root: &Path) -> BTreeMap<PathBuf, Physical> {
        let mut files = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(parent) = pending.pop() {
            for entry in std::fs::read_dir(parent).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else {
                    assert!(entry.file_type().unwrap().is_file());
                    files.insert(entry.path(), file(&entry.path()));
                }
            }
        }
        files
    }
    fn xmp(path: &Path) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".xmp");
        name.into()
    }
    fn fingerprint(path: &Path, expected: &LayoutFingerprint) {
        let actual = file(path);
        assert_eq!(actual.0.len() as u64, expected.size);
        assert_eq!(
            Sha256::digest(&actual.0).as_slice(),
            expected.sha256.as_slice()
        );
        assert_eq!(actual.1, expected.identity);
    }
    fn verified(saved: &LayoutFile) {
        fingerprint(&saved.path.to_path(), &saved.fingerprint);
        #[cfg(feature = "xmp")]
        assert!(saved.sidecar.is_some(), "recorded fixture must retain XMP");
        if let Some(proof) = &saved.sidecar {
            fingerprint(&xmp(&saved.path.to_path()), proof);
        }
    }
    fn rows(root: &Path, sql: &str) -> Vec<Vec<SqlValue>> {
        let conn = rusqlite::Connection::open_with_flags(
            root.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |r| {
                (0..columns)
                    .map(|i| r.get(i))
                    .collect::<Result<Vec<SqlValue>, _>>()
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
    fn frozen_proofs(root: &Path) -> Vec<Vec<Vec<SqlValue>>> {
        [
            "SELECT operation,family,library,source_library,prior_generation,decision,policy,header,conflict FROM primary_layout_operations ORDER BY operation",
            "SELECT * FROM primary_layout_members ORDER BY operation,member",
            "SELECT * FROM primary_layout_preserved ORDER BY operation,member",
            "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,provider_checksum,native_path",
            "SELECT * FROM download_receipts WHERE child='asset-501' ORDER BY id",
            "SELECT library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size,source_checksum FROM asset_metadata_paths WHERE id='asset-501' ORDER BY library,version_size,local_path",
        ].map(|sql| rows(root,sql)).to_vec()
    }
    fn settled_proofs(root: &Path) -> Vec<Vec<Vec<SqlValue>>> {
        let mut proof = frozen_proofs(root);
        for sql in [
            "SELECT * FROM primary_layout_operations ORDER BY operation",
            "SELECT * FROM primary_layout_bindings ORDER BY family",
            "SELECT * FROM primary_layout_claims ORDER BY path_key",
            "SELECT * FROM download_receipts ORDER BY id",
            "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
            "SELECT * FROM owned_temp_files ORDER BY path",
        ] {
            proof.push(rows(root, sql));
        }
        proof
    }
    fn membership_custody(root: &Path) -> Vec<(String, Vec<String>, Vec<Vec<SqlValue>>)> {
        let conn = rusqlite::Connection::open_with_flags(
            root.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = conn.prepare("SELECT name FROM sqlite_schema WHERE type='table' AND (name IN ('assets','asset_metadata_paths','asset_metadata_capture_revisions','download_receipts','metadata','owned_temp_files') OR name LIKE 'provider_%' OR name LIKE 'primary_layout_%') ORDER BY name").unwrap();
        let tables = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        tables
            .into_iter()
            .map(|table| {
                let mut schema = conn
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .unwrap();
                let columns = schema
                    .query_map([], |r| r.get::<_, String>(1))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap()
                    .into_iter()
                    .filter(|name| {
                        !(matches!(table.as_str(), "assets" | "asset_metadata_paths")
                            && name == "metadata_write_failed_at")
                    })
                    .collect::<Vec<_>>();
                let order = (1..=columns.len())
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let values = rows(
                    root,
                    &format!("SELECT {} FROM {table} ORDER BY {order}", columns.join(",")),
                );
                (table, columns, values)
            })
            .collect()
    }
    async fn metadata_queue(db: &state::SqliteStateDb) -> Value {
        json!(db.get_pending_metadata_rewrites_page_for_queue(state::db::MetadataRewriteQueue::Ordinary,Some(&["PrimarySync"]),0,usize::MAX).await.unwrap().into_iter().map(|q|json!({"asset":q.asset,"source":q.source_checksum,"capture":q.capture_repair_receipt.map(|r|format!("{r:?}")),"selection":q.selection_receipt.map(|r|format!("{r:?}"))})).collect::<Vec<_>>())
    }
    fn queued_resources(queue: &Value) -> Vec<Value> {
        queue
            .as_array()
            .unwrap()
            .iter()
            .map(|q| {
                let a = &q["asset"];
                json!([
                    a["library"],
                    a["id"],
                    a["version_size"],
                    a["local_path"],
                    a["checksum"],
                    a["size_bytes"],
                    a["metadata"],
                    a["local_checksum"],
                    a["download_checksum"],
                    q["source"],
                    q["capture"],
                    q["selection"]
                ])
            })
            .collect()
    }
    fn prepared_metadata(
        asset: &crate::icloud::photos::PhotoAsset,
        config: &download::DownloadConfig,
        op: &LayoutOperation,
    ) -> Vec<Value> {
        // Real trusted membership creates the debt; this fixture requests only
        // sidecars, or no metadata writes in a build without XMP.
        const SIDECAR_FLAG: u8 = 1 << 5;
        let flags = if cfg!(feature = "xmp") {
            SIDECAR_FLAG
        } else {
            0
        };
        assert_eq!(op.metadata_flags, flags);
        assert!(
            !config.metadata.set_exif_datetime
                && !config.metadata.set_exif_rating
                && !config.metadata.set_exif_gps
                && !config.metadata.set_exif_description
        );
        #[cfg(feature = "xmp")]
        assert!(!config.metadata.embed_xmp);
        assert_eq!(format!("{:?}", config.capture_timestamp_repair), "Preserve");
        assert_eq!(config.album_name.as_deref(), Some("TestAlbum"));
        assert!(
            config.asset_groupings.albums.is_empty() && config.asset_groupings.people.is_empty()
        );
        op.members.iter().map(|member| {
            let record = member.record.as_ref().unwrap();
            let prepared = member.prepared.as_ref().unwrap();
            let selected = download::filter::metadata_for_selected_version(asset, config, record.version_size);
            assert_eq!(serde_json::to_value(&record.metadata).unwrap(), serde_json::to_value(&selected).unwrap());
            assert!(selected.metadata_hash.is_some());
            assert!(selected.keywords.is_none());
            let payload = json!({
                "timezone_offset": selected.timezone_offset, "rating": selected.rating,
                "latitude": selected.latitude, "longitude": selected.longitude, "altitude": selected.altitude,
                "title": selected.title, "description": selected.description.as_ref().or(selected.title.as_ref()),
                "keywords": ["TestAlbum"], "people": [], "is_hidden": selected.is_hidden,
                "is_archived": selected.is_archived, "media_subtype": selected.media_subtype, "burst_id": selected.burst_id,
            });
            assert_eq!(payload, json!({
                "timezone_offset": null, "rating": null, "latitude": null, "longitude": null, "altitude": null,
                "title": null, "description": null, "keywords": ["TestAlbum"], "people": [],
                "is_hidden": false, "is_archived": false, "media_subtype": null, "burst_id": null,
            }), "new metadata obligations require reconciliation, not marker clearing");
            let decision = format!("{:x}", Sha256::digest(serde_json::to_vec(&json!([
                1, record.version_size.as_str(), record.checksum, flags, "Preserve", asset.created(), payload,
            ])).unwrap()));
            assert_eq!(prepared.metadata_decision.as_deref(), Some(decision.as_str()));
            let bytes = match record.version_size {
                state::VersionSizeKey::Original => ORIGINAL,
                state::VersionSizeKey::Adjusted => EDIT_ONE,
                other => panic!("unexpected recorded rendition {other:?}"),
            };
            assert_bytes(&prepared.path.to_path(), bytes);
            verified(prepared);
            #[cfg(feature = "xmp")]
            {
                let text = std::fs::read_to_string(xmp(&prepared.path.to_path())).unwrap();
                let _: xmp_toolkit::XmpMeta = text.parse().unwrap();
                let date = asset.created_local().format("%Y-%m-%dT%H:%M:%S").to_string();
                for tag in ["xmp:CreateDate", "xmp:ModifyDate", "exif:DateTimeOriginal", "photoshop:DateCreated"] {
                    assert!(text.contains(&format!("<{tag}>{date}</{tag}>")), "missing prepared {tag}");
                }
                assert!(text.contains("<rdf:li>TestAlbum</rdf:li>"));
                assert!(text.contains("<kei:managedFields>"));
            }
            #[cfg(not(feature = "xmp"))]
            {
                assert!(prepared.sidecar.is_none());
                assert!(!xmp(&prepared.path.to_path()).exists());
            }
            json!({"library": op.library, "child": op.child, "version": record.version_size.as_str(),
                "path": member.destination.as_ref().unwrap(), "checksum": record.checksum,
                "metadata_hash": selected.metadata_hash, "flags": flags, "decision": decision,
                "prepared": prepared, "obligations_validated": true})
        }).collect()
    }
    fn committed_proofs(
        root: &Path,
        op: &LayoutOperation,
        preparation: &[Value],
        fault: &[Vec<Vec<SqlValue>>],
    ) -> Vec<Vec<Vec<SqlValue>>> {
        // Caller must first prove this exact operation committed and its complete
        // binding has generation + 1. Only its prepared record tuples can settle.
        let actual = frozen_proofs(root);
        let mut expected = fault.to_vec();
        assert_eq!(preparation.len(), op.members.len());
        let conn = rusqlite::Connection::open_with_flags(
            root.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut transitions = Vec::new();
        for (member, proof) in op.members.iter().zip(preparation) {
            let record = member.record.as_ref().unwrap();
            let installed = member.installed.as_ref().unwrap();
            let prepared = member.prepared.as_ref().unwrap();
            assert_eq!(record.library.as_ref(), op.library);
            assert_eq!(record.id.as_ref(), op.child);
            assert_eq!(proof["metadata_hash"], json!(record.metadata.metadata_hash));
            assert_eq!(proof["flags"], op.metadata_flags);
            assert_eq!(proof["decision"], json!(installed.metadata_decision));
            assert_eq!(proof["prepared"], serde_json::to_value(prepared).unwrap());
            assert_eq!(installed.metadata_decision, prepared.metadata_decision);
            assert_eq!(installed.fingerprint, prepared.fingerprint);
            assert_eq!(installed.sidecar, prepared.sidecar);
            assert_eq!(installed.source_checksum, prepared.source_checksum);
            assert_eq!(member.destination.as_ref(), Some(&installed.path));
            assert_eq!(installed.version, record.version_size.as_str());
            assert_eq!(installed.provider_checksum, record.checksum.as_ref());
            assert_eq!(
                proof["path"],
                serde_json::to_value(&installed.path).unwrap()
            );
            let native = installed.path.to_path();
            let path = native.to_str().unwrap();
            let key = vec![
                SqlValue::Text(op.library.clone()),
                SqlValue::Text(op.child.clone()),
                SqlValue::Text(record.version_size.as_str().into()),
                SqlValue::Text(path.into()),
                SqlValue::Text(record.checksum.to_string()),
            ];
            let matching: Vec<_> = expected[5]
                .iter_mut()
                .enumerate()
                .filter(|(_, row)| row[..5] == key)
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "exact prepared tuple must have one frozen path row: {proof}"
            );
            let (index, row) = matching.into_iter().next().unwrap();
            assert!(matches!(row[7], SqlValue::Null | SqlValue::Integer(_)));
            let before_marker = row[7].clone();
            if op.metadata_flags != 0 {
                row[7] = SqlValue::Null;
            }
            let settled: bool = conn.query_row("SELECT count(*)=1 FROM assets WHERE library=?1 AND id=?2 AND version_size=?3 AND local_path=?4 AND checksum=?5 AND metadata_hash=?6 AND status='downloaded' AND metadata_write_failed_at IS ?7",rusqlite::params![op.library,op.child,record.version_size.as_str(),path,record.checksum.as_ref(),record.metadata.metadata_hash,row[7]], |r| r.get(0)).unwrap();
            assert!(
                settled,
                "exact installed tuple must retain zero-write debt or settle prepared metadata: {proof}"
            );
            transitions.push(json!({"tuple":proof,"before":format!("{before_marker:?}"),"after":format!("{:?}",row[7])}));
            assert_eq!(
                actual[5][index][7], row[7],
                "zero-write markers stay exact; prepared nonzero markers clear: {proof}"
            );
        }
        assert_eq!(
            actual, expected,
            "only exact prepared metadata markers may change"
        );
        std::fs::write(root.join("marker-transition.json"), serde_json::to_vec_pretty(&json!({"operation":op.operation,"generation":op.generation+1,"transitions":transitions,"all_other_proofs_exact":true})).unwrap()).unwrap();
        actual
    }
    let ledger_sql = "SELECT json_object('generation',g.id,'account',g.account_key,'provider',g.provider_key,'scope',g.scope,'format',json_extract(CAST(g.specification AS TEXT),'$.format'),'pass',d.pass_key,'child',d.child,'master',d.master,'state',d.asset_id,'library',d.library,'version',d.version_size,'path',d.path,'checksum',d.checksum,'size',d.size_bytes,'admitted',d.admitted,'verified',d.verified_media,'sealed',g.sealed,'ready',g.checkpoint_ready) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child IN ('asset-501','asset-502') ORDER BY g.id,d.child,d.version_size,d.path";
    for (directory, template) in [("new-root", ""), ("media", "new-template")] {
        for naming in [EditedNaming::Primary, EditedNaming::Suffix] {
            let server = server().await;
            let root = tempfile::tempdir().unwrap().keep();
            eprintln!(
                "RECORDED_OLD_ROOT_FIXTURE root={} directory={directory} template={template} naming={naming:?}",
                root.display()
            );
            let old_media = root.join("media");
            let current = records(&server, Some(("edit-one", EDIT_ONE)));
            let options = |media_directory, folder_structure, token, count| CycleOptions {
                private: true,
                media_directory,
                folder_structure,
                cycle_token: Some(token),
                reported_count: Some(count),
                #[cfg(feature = "xmp")]
                sidecar: true,
                ..CycleOptions::default()
            };
            let (initial, _) = cycle_options(
                &root,
                current.clone(),
                false,
                EditedNaming::Suffix,
                options("media", "", "recorded-old-start", 1),
            )
            .await;
            assert_eq!(initial.failed_count, 0, "{initial:?}");
            assert_eq!(initial.stats.downloaded, 2);
            assert!(initial.db_sync_token_advance_safe);
            assert_bytes(&old_media.join("IMG_0501.JPG"), ORIGINAL);
            assert_bytes(&old_media.join("IMG_0501_edited.JPG"), EDIT_ONE);
            let requests = server.received_requests().await.unwrap().len();
            assert_eq!(requests, 2);
            let owner = state::db::account::AccountOwner::authenticated(
                "synthetic@example.invalid",
                "com",
                &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
            )
            .unwrap();
            let (op, scope, old_store, preparation, membership_queue) = {
                let db = Arc::new(
                    state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
                        .await
                        .unwrap(),
                );
                let initial_files = tree(&old_media);
                let initial_custody = membership_custody(&root);
                let marker_rows = || {
                    [
                        rows(
                            &root,
                            "SELECT library,id,version_size,local_path,checksum,metadata_hash,metadata_write_failed_at FROM assets WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size",
                        ),
                        rows(
                            &root,
                            "SELECT library,id,version_size,local_path,provider_checksum,source_checksum,metadata_write_failed_at FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size,local_path",
                        ),
                    ]
                };
                let initial_markers = marker_rows();
                let initial_groups = state::MembershipStore::get_asset_groupings(
                    db.as_ref(),
                    "PrimarySync",
                    &["asset-501"],
                )
                .await
                .unwrap();
                let initial_containers = rows(
                    &root,
                    "SELECT * FROM album_containers ORDER BY library,container_id",
                );
                let initial_relations = rows(
                    &root,
                    "SELECT * FROM asset_album_memberships ORDER BY library,asset_record_name,container_id",
                );
                let readiness = rows(
                    &root,
                    "SELECT g.id,g.metadata_enabled,g.sealed,g.checkpoint_ready,w.library,w.child,w.version_size,w.admitted,w.verified_media,w.verified_metadata,w.frozen_metadata,w.grouping_hash FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation ORDER BY g.id,w.child,w.version_size,w.path",
                );
                std::fs::write(root.join("membership-before.json"),serde_json::to_vec_pretty(&json!({"files":initial_files,"custody":format!("{initial_custody:?}"),"markers":format!("{initial_markers:?}"),"containers":format!("{initial_containers:?}"),"relations":format!("{initial_relations:?}"),"albums":initial_groups.albums,"people":initial_groups.people,"guard_readiness":format!("{readiness:?}")})).unwrap()).unwrap();
                assert!(
                    initial_groups.albums.is_empty() && initial_groups.people.is_empty(),
                    "Unfiled label is not album membership"
                );
                assert!(initial_containers.is_empty() && initial_relations.is_empty());
                for group in &initial_markers {
                    assert_eq!(group.len(), 2);
                    assert!(group.iter().all(|row| row[6] == SqlValue::Null));
                }
                assert_eq!(readiness.len(), 2);
                for row in &readiness {
                    assert_eq!(row[7], SqlValue::Integer(1));
                    assert_eq!(row[8], SqlValue::Integer(1));
                    if row[1] == SqlValue::Integer(1) {
                        assert_eq!(row[9], SqlValue::Integer(1));
                    }
                }
                const CONTAINER: &str = "old-root-trusted-album-501";
                let container = state::MembershipStore::upsert_album_container(
                    db.as_ref(),
                    "PrimarySync",
                    CONTAINER,
                    "TestAlbum",
                    "Album",
                )
                .await;
                std::fs::write(
                    root.join("membership-container-result.txt"),
                    format!("{container:?}"),
                )
                .unwrap();
                container.unwrap();
                let relation = state::MembershipStore::upsert_album_membership_delta(
                    db.as_ref(),
                    "PrimarySync",
                    CONTAINER,
                    "asset-501",
                    Some("501"),
                    "icloud",
                )
                .await;
                let containers = rows(
                    &root,
                    "SELECT library,container_id,album_name,pass_kind,is_deleted FROM album_containers ORDER BY library,container_id",
                );
                let relations = rows(
                    &root,
                    "SELECT library,asset_record_name,master_record_name,container_id,generation,is_deleted,source FROM asset_album_memberships ORDER BY library,asset_record_name,container_id",
                );
                let observed =
                    state::MembershipStore::get_live_selected_album_memberships_for_asset(
                        db.as_ref(),
                        "PrimarySync",
                        "asset-501",
                        &[CONTAINER],
                    )
                    .await
                    .unwrap();
                let groups = state::MembershipStore::get_asset_groupings(
                    db.as_ref(),
                    "PrimarySync",
                    &["asset-501"],
                )
                .await
                .unwrap();
                let after_markers = marker_rows();
                let after_custody = membership_custody(&root);
                let after_files = tree(&old_media);
                let membership_queue = metadata_queue(db.as_ref()).await;
                std::fs::write(root.join("membership-update.json"),serde_json::to_vec_pretty(&json!({"producer":"synthetic trusted provider membership via MembershipStore","container_result":"Ok(())","relation_result":format!("{relation:?}"),"containers":format!("{containers:?}"),"relations":format!("{relations:?}"),"owner_relations":format!("{observed:?}"),"albums":groups.albums,"people":groups.people,"markers":format!("{after_markers:?}"),"custody":format!("{after_custody:?}"),"files":after_files,"queue":membership_queue})).unwrap()).unwrap();
                assert!(
                    relation.unwrap(),
                    "trusted container must be known; no guard bypass"
                );
                assert_eq!(
                    containers,
                    vec![vec![
                        SqlValue::Text("PrimarySync".into()),
                        SqlValue::Text(CONTAINER.into()),
                        SqlValue::Text("TestAlbum".into()),
                        SqlValue::Text("Album".into()),
                        SqlValue::Integer(0)
                    ]]
                );
                assert_eq!(
                    relations,
                    vec![vec![
                        SqlValue::Text("PrimarySync".into()),
                        SqlValue::Text("asset-501".into()),
                        SqlValue::Text("501".into()),
                        SqlValue::Text(CONTAINER.into()),
                        SqlValue::Integer(0),
                        SqlValue::Integer(0),
                        SqlValue::Text("icloud".into())
                    ]]
                );
                assert_eq!(observed.len(), 1);
                assert_eq!(
                    (
                        &*observed[0].library,
                        &*observed[0].asset_record_name,
                        observed[0].master_record_name.as_deref(),
                        &*observed[0].container_id,
                        observed[0].generation,
                        &*observed[0].source
                    ),
                    (
                        "PrimarySync",
                        "asset-501",
                        Some("501"),
                        CONTAINER,
                        0,
                        "icloud"
                    )
                );
                assert_eq!(
                    groups.albums,
                    vec![("asset-501".into(), "TestAlbum".into())]
                );
                assert!(groups.people.is_empty());
                for group in &after_markers {
                    assert_eq!(group.len(), 2);
                    assert!(
                        group
                            .iter()
                            .all(|row| matches!(row[6], SqlValue::Integer(_)))
                    );
                }
                assert_eq!(
                    after_custody, initial_custody,
                    "membership may only add grouping and retry markers"
                );
                assert_eq!(
                    after_files, initial_files,
                    "trusted state update must not rewrite media or raw XMP"
                );
                assert_eq!(membership_queue.as_array().unwrap().len(), 2);
                let weak = Arc::downgrade(&db);
                let provider = Session {
                    reported_count: None,
                    records: current.clone(),
                    quiet: true,
                    lookups: Arc::new(AtomicUsize::new(0)),
                    current: None,
                    lookup_change: None,
                    indexing: None,
                    baseline_successor: None,
                    cycle_token: None,
                    trace: None,
                };
                let mut album = crate::icloud::photos::PhotoAlbum::new(
                    crate::icloud::photos::PhotoAlbumConfig {
                        params: Arc::default(),
                        service_endpoint: Arc::from("https://example.com"),
                        name: Arc::from("TestAlbum"),
                        list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                        obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                        query_filter: None,
                        page_size: 100,
                        zone_id: Arc::new(
                            json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                        ),
                        retry_config: crate::retry::RetryConfig::default(),
                        container_id: None,
                        cross_zone_sources: Vec::new(),
                    },
                    Box::new(provider),
                );
                album.set_shadow_capture(
                    crate::icloud::photos::inbox::ShadowCapture::new(
                        db.clone(),
                        owner.clone(),
                        "com",
                    ),
                    Arc::from("private"),
                );
                let pass = AlbumPass {
                    kind: PassKind::Unfiled,
                    album,
                    exclude_ids: Arc::default(),
                };
                let (capture, scope, _) = pass
                    .album
                    .publication_scope("PrimarySync")
                    .unwrap()
                    .unwrap();
                assert!(Arc::ptr_eq(&db, &capture.db));
                drop(capture);
                let store: Arc<dyn download::DownloadStore> = db.clone();
                let build = make_run_cycle_download_config_builder(&old_media, store);
                let mut config = build(
                    download::SyncMode::Full,
                    Arc::default(),
                    Arc::default(),
                    Arc::from("PrimarySync"),
                )
                .as_ref()
                .clone();
                config.folder_structure = String::new();
                config.folder_structure_albums = Arc::from("");
                config.folder_structure_smart_folders = Arc::from("");
                config.edited = true;
                config.edited_naming = EditedNaming::Suffix;
                #[cfg(feature = "xmp")]
                {
                    config.metadata.xmp_sidecar = true;
                }
                let actual = config.with_pass(&pass);
                let op = download::seed_prepared_edited_compat(&config, &pass).await;
                let asset = pass
                    .album
                    .confirm_layout_asset("asset-501", "PrimarySync")
                    .await
                    .unwrap()
                    .asset;
                let preparation = prepared_metadata(&asset, &actual, &op);
                assert_eq!(op.phase, "prepared");
                assert_eq!(op.policy, "suffix");
                assert_eq!(
                    op.root,
                    SelectionPath::from_path(&std::path::absolute(&old_media).unwrap())
                );
                assert_eq!(op.pass, actual.primary_layout_pass.as_deref().unwrap());
                assert_eq!(
                    rows(
                        &root,
                        "SELECT DISTINCT json_extract(CAST(specification AS TEXT),'$.metadata_flags') FROM provider_active_generations ORDER BY 1"
                    ),
                    vec![vec![SqlValue::Integer(i64::from(op.metadata_flags))]],
                    "recorded flags must match the actual owned baseline"
                );
                assert!(!op.refresh_metadata);
                assert_eq!(
                    (
                        &*op.library,
                        &*op.source_library,
                        &*op.child,
                        &*op.asset_record_name
                    ),
                    ("PrimarySync", "PrimarySync", "asset-501", "asset-501")
                );
                assert_eq!(op.members.len(), 2);
                for member in &op.members {
                    verified(member.old.as_ref().unwrap());
                    verified(member.prepared.as_ref().unwrap());
                    assert!(
                        member.installed.is_none()
                            && member.preserved.is_none()
                            && member.retirement.is_none()
                    );
                    let record = member.record.as_ref().unwrap();
                    let catalogue:(String,i64)=db.acquire_lock("verify real recorded catalogue").unwrap().query_row("SELECT checksum,size_bytes FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size=?1",[record.version_size.as_str()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                    assert_eq!(catalogue.0, record.checksum.as_ref());
                    assert_eq!(catalogue.1, record.size_bytes as i64);
                }
                assert!(
                    db.primary_layout_binding(op.family.clone())
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(
                    db.capture_layout_recovery(op.clone())
                        .await
                        .unwrap()
                        .is_none()
                );
                assert_eq!(
                    db.primary_layout_operations("PrimarySync".into())
                        .await
                        .unwrap()
                        .len(),
                    1
                );
                std::fs::write(root.join("prepared.json"),serde_json::to_vec_pretty(&json!({"operation":op,"scope":scope,"catalogue":format!("{:?}",rows(&root,"SELECT * FROM assets ORDER BY version_size")),"checkpoint":format!("{:?}",rows(&root,"SELECT value FROM metadata WHERE key='sync_token:PrimarySync'")),"capture_layout_recovery":"None; matching real catalogue"})).unwrap()).unwrap();
                (op, scope, weak, preparation, membership_queue)
            };
            assert!(
                old_store.upgrade().is_none(),
                "setup handles must close before recovery"
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let checkpoint = rows(
                &root,
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
            );
            assert_eq!(
                checkpoint,
                vec![vec![SqlValue::Text("recorded-old-start".into())]]
            );
            rusqlite::Connection::open(root.join("state.db")).unwrap().execute_batch("CREATE TRIGGER fail_old_root_binding BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'recorded old-root binding fault'); END").unwrap();
            let outcome = try_cycle_options(
                &root,
                current.clone(),
                false,
                EditedNaming::Suffix,
                options("media", "", "fault-attempt-token", 1),
            )
            .await;
            std::fs::write(root.join("fault-attempt.json"),serde_json::to_vec_pretty(&json!({"outcome":format!("{outcome:?}"),"operations":format!("{:?}",rows(&root,"SELECT * FROM primary_layout_operations")),"GETs":server.received_requests().await.unwrap().len(),"checkpoint":format!("{:?}",rows(&root,"SELECT value FROM metadata WHERE key='sync_token:PrimarySync'"))})).unwrap()).unwrap();
            let error = outcome.expect_err("normal dispatch must propagate the binding trigger");
            assert!(
                matches!(error.downcast_ref::<state::error::StateError>(),Some(state::error::StateError::Migration(rusqlite::Error::SqliteFailure(code,Some(message)))) if code.extended_code==rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER && message=="recorded old-root binding fault"),
                "{error:#}"
            );
            let publishing = {
                let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
                    .await
                    .unwrap();
                let pending = db
                    .primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap();
                assert_eq!(pending.len(), 1);
                assert!(
                    db.primary_layout_binding(op.family.clone())
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(
                    db.capture_layout_recovery(pending[0].clone())
                        .await
                        .unwrap()
                        .is_none()
                );
                pending.into_iter().next().unwrap()
            };
            assert_eq!(publishing.operation, op.operation);
            assert_eq!(publishing.phase, "publishing");
            for key in [
                "family",
                "root",
                "pass",
                "generation",
                "decision",
                "policy",
                "metadata_flags",
                "library",
                "source_library",
                "child",
                "asset_record_name",
            ] {
                assert_eq!(
                    serde_json::to_value(&publishing).unwrap()[key],
                    serde_json::to_value(&op).unwrap()[key],
                    "{key}"
                );
            }
            for member in &publishing.members {
                verified(member.installed.as_ref().unwrap());
                verified(member.preserved.as_ref().unwrap());
            }
            assert_eq!(
                rows(
                    &root,
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'"
                ),
                checkpoint
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let fault_tree = tree(&old_media);
            let fault_proofs = frozen_proofs(&root);
            let stage_presence: Vec<_> = publishing
                .members
                .iter()
                .map(|m| {
                    let p = m.stage.as_ref().unwrap().to_path();
                    let exists = p.exists();
                    if exists {
                        let actual = file(&p);
                        let expected =
                            if actual.1 == m.prepared.as_ref().unwrap().fingerprint.identity {
                                &m.prepared.as_ref().unwrap().fingerprint
                            } else {
                                &m.old.as_ref().unwrap().fingerprint
                            };
                        fingerprint(&p, expected);
                    }
                    (p, exists)
                })
                .collect();
            std::fs::write(
                root.join("fault-files.json"),
                serde_json::to_vec_pretty(&fault_tree).unwrap(),
            )
            .unwrap();
            std::fs::write(root.join("fault-state.json"),serde_json::to_vec_pretty(&json!({"operation":publishing,"scope":scope,"error":format!("{error:#}"),"proofs":format!("{fault_proofs:?}"),"stages":stage_presence,"checkpoint":format!("{checkpoint:?}"),"GETs":requests})).unwrap()).unwrap();
            assert_eq!(
                frozen_proofs(&root),
                fault_proofs,
                "fault-time evidence stays strict while trigger is installed"
            );
            rusqlite::Connection::open(root.join("state.db"))
                .unwrap()
                .execute_batch("DROP TRIGGER fail_old_root_binding")
                .unwrap();
            let expected_binding = LayoutBinding {
                family: publishing.family.clone(),
                library: publishing.library.clone(),
                source_library: publishing.source_library.clone(),
                child: publishing.child.clone(),
                generation: publishing.generation + 1,
                decision: publishing.decision.clone(),
                policy: publishing.policy.clone(),
                qualifier: publishing.qualifier.clone(),
                files: publishing
                    .members
                    .iter()
                    .map(|m| m.installed.as_ref().unwrap().clone())
                    .collect(),
            };
            let calls = Arc::new(AtomicUsize::new(0));
            let seen = calls.clone();
            let callback_root = root.clone();
            let callback_op = publishing.clone();
            let callback_binding = serde_json::to_value(&expected_binding).unwrap();
            let callback_proofs = fault_proofs.clone();
            let callback_preparation = preparation.clone();
            let committed_snapshot = Arc::new(std::sync::Mutex::new(None));
            let callback_snapshot = committed_snapshot.clone();
            let callback_files = fault_tree.clone();
            let callback_owner = owner
                .authenticated_keys()
                .map(|(a, p)| (a.to_owned(), p.to_owned()))
                .unwrap();
            Mock::given(method("GET")).and(path("/new-resource")).respond_with(move |_:&wiremock::Request| {
                let conn=rusqlite::Connection::open_with_flags(callback_root.join("state.db"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
                let phase:String=conn.query_row("SELECT phase FROM primary_layout_operations WHERE operation=?1",[&callback_op.operation],|r|r.get(0)).unwrap();
                let encoded:Vec<u8>=conn.query_row("SELECT binding FROM primary_layout_bindings WHERE family=?1 AND generation=?2 AND library=?3 AND source_library=?4 AND child=?5 AND decision=?6 AND policy=?7",rusqlite::params![callback_op.family,callback_op.generation+1,callback_op.library,callback_op.source_library,callback_op.child,callback_op.decision,callback_op.policy],|r|r.get(0)).unwrap();
                let binding:LayoutBinding=serde_json::from_slice(&encoded).unwrap();
                let owner:(String,String)=conn.query_row("SELECT account_key,provider_key FROM account_owner WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                std::fs::write(callback_root.join("new-GET-ordering.json"),serde_json::to_vec_pretty(&json!({"operation":callback_op.operation,"phase":phase,"binding":binding,"expected_generation":callback_op.generation+1,"owner":owner})).unwrap()).unwrap();
                assert_eq!(owner,callback_owner);
                assert_eq!(phase,"committed","old journal must commit before new502 GET");
                assert_eq!(serde_json::to_value(&binding).unwrap(),callback_binding);
                let committed = committed_proofs(&callback_root, &callback_op, &callback_preparation, &callback_proofs);
                for member in &callback_op.members {verified(member.installed.as_ref().unwrap());verified(member.preserved.as_ref().unwrap());}
                for (path,snapshot) in &callback_files {assert_eq!(&file(path),snapshot);}
                *callback_snapshot.lock().unwrap() = Some((committed, rows(&callback_root,"SELECT * FROM asset_metadata_paths WHERE id='asset-501' ORDER BY library,version_size,local_path")));
                std::fs::write(callback_root.join("new-GET-200.json"),serde_json::to_vec_pretty(&json!({"operation":callback_op.operation,"generation":binding.generation,"all_ordering_and_custody_assertions_passed":true,"response_status":200})).unwrap()).unwrap();
                seen.fetch_add(1,Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_bytes(EDIT_TWO)
            }).expect(1).mount(&server).await;
            let mut page = full_album_page_with_download(
                "PrimarySync",
                "502",
                "selected-token",
                &format!("{}/new-resource", server.uri()),
                EDIT_TWO.len() as u64,
                &provider_hash(EDIT_TWO),
            );
            page["records"][0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
            let mut complete = current.clone();
            complete.extend(page["records"].as_array().unwrap().iter().cloned());
            let new_root = root.join(directory);
            let new_path = new_root.join(template).join("IMG_0502.JPG");
            let (recovered, _) = cycle_options(
                &root,
                complete.clone(),
                false,
                naming,
                options(directory, template, "new-resource-token", 2),
            )
            .await;
            std::fs::write(root.join("recovered-state.json"),serde_json::to_vec_pretty(&json!({"result":format!("{recovered:?}"),"GET_callbacks":calls.load(Ordering::SeqCst),"ledger":format!("{:?}",rows(&root,ledger_sql)),"proofs":format!("{:?}",settled_proofs(&root))})).unwrap()).unwrap();
            assert_eq!(
                recovered.failed_count, 0,
                "{directory}/{template} {naming:?}: {recovered:?}"
            );
            assert!(recovered.db_sync_token_advance_safe);
            assert_eq!(recovered.stats.downloaded, 1);
            assert!(recovered.stats.assets_seen >= 2);
            assert!(!recovered.stats.enumeration_incomplete);
            assert_eq!(
                recovered.stats.full_enumeration_reason,
                Some(download::FullEnumerationReason::DownloadConfigHashDrift)
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "ordering responder must actually execute"
            );
            assert_bytes(&new_path, EDIT_TWO);
            let (committed, committed_paths) = committed_snapshot.lock().unwrap().clone().unwrap();
            assert_eq!(frozen_proofs(&root), committed);
            assert_eq!(
                rows(
                    &root,
                    "SELECT * FROM asset_metadata_paths WHERE id='asset-501' ORDER BY library,version_size,local_path"
                ),
                committed_paths
            );
            assert_eq!(
                rows(&root, "SELECT phase FROM primary_layout_operations"),
                vec![vec![SqlValue::Text("committed".into())]]
            );
            assert_eq!(
                db_count(&root, "SELECT count(*) FROM primary_layout_operations"),
                1
            );
            assert_eq!(
                db_count(&root, "SELECT count(*) FROM primary_layout_bindings"),
                1
            );
            assert_eq!(
                db_count(
                    &root,
                    "SELECT count(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
                ),
                0
            );
            let mut expected_tree = fault_tree.clone();
            expected_tree.insert(new_path.clone(), file(&new_path));
            #[cfg(feature = "xmp")]
            {
                expected_tree.insert(xmp(&new_path), file(&xmp(&new_path)));
            }
            let mut actual_tree = tree(&old_media);
            if new_root != old_media {
                actual_tree.extend(tree(&new_root));
            }
            assert_eq!(actual_tree, expected_tree, "only new502 may add media/XMP");
            for (path, existed) in &stage_presence {
                assert_eq!(path.exists(), *existed, "recorded stage custody changed");
            }
            let (account_key, provider_key) = owner.authenticated_keys().unwrap();
            let ledger: Vec<Value> = rows(&root, ledger_sql)
                .into_iter()
                .map(|r| {
                    let [SqlValue::Text(text)] = r.as_slice() else {
                        panic!("invalid ledger projection")
                    };
                    serde_json::from_str(text).unwrap()
                })
                .collect();
            let mut new_generations = std::collections::BTreeSet::new();
            for d in &ledger {
                assert_eq!(d["account"], account_key);
                assert_eq!(d["provider"], provider_key);
                assert_eq!(d["scope"], scope);
                assert_eq!(d["format"], 2);
                assert_eq!(d["pass"], op.pass);
                assert_eq!(d["library"], "PrimarySync");
                for key in ["admitted", "verified", "sealed", "ready"] {
                    assert_eq!(d[key], 1, "{key}: {d:?}");
                }
                let native: SelectionPath =
                    serde_json::from_str(d["path"].as_str().unwrap()).unwrap();
                if d["child"] == "asset-502" {
                    assert_eq!(d["master"], "502");
                    assert_eq!(d["state"], "asset-502");
                    assert_eq!(d["version"], "original");
                    assert_eq!(d["checksum"], provider_hash(EDIT_TWO));
                    assert_eq!(d["size"], EDIT_TWO.len());
                    assert_eq!(native.to_path(), new_path);
                    new_generations.insert(d["generation"].as_str().unwrap().to_owned());
                } else {
                    assert_eq!(d["child"], "asset-501");
                    assert_eq!(d["master"], "501");
                    assert_eq!(d["state"], "asset-501");
                    let (path, bytes) = if d["version"] == "original" {
                        (old_media.join("IMG_0501.JPG"), ORIGINAL)
                    } else {
                        assert_eq!(d["version"], "adjusted");
                        (old_media.join("IMG_0501_edited.JPG"), EDIT_ONE)
                    };
                    assert_eq!(native.to_path(), path);
                    assert_eq!(d["checksum"], provider_hash(bytes));
                    assert_eq!(d["size"], bytes.len());
                }
            }
            assert!(!new_generations.is_empty());
            assert!(
                new_generations.iter().any(|g| ledger
                    .iter()
                    .filter(|d| d["generation"] == *g && d["child"] == "asset-501")
                    .count()
                    == 2),
                "complete unchanged501 inventory must be admitted with new502"
            );
            let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
                .await
                .unwrap();
            let recovered_queue = metadata_queue(&db).await;
            std::fs::write(
                root.join("metadata-queue-recovered.json"),
                serde_json::to_vec_pretty(&recovered_queue).unwrap(),
            )
            .unwrap();
            if op.metadata_flags == 0 {
                assert_eq!(
                    queued_resources(&recovered_queue),
                    queued_resources(&membership_queue)
                );
            } else {
                assert_eq!(recovered_queue, json!([]));
            }
            assert_eq!(
                serde_json::to_value(
                    db.primary_layout_binding(op.family.clone())
                        .await
                        .unwrap()
                        .unwrap()
                )
                .unwrap(),
                serde_json::to_value(&expected_binding).unwrap()
            );
            assert!(
                db.primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(db.get_pending().await.unwrap().is_empty());
            let new_binding = state::db::PublicationBinding {
                account_key: account_key.into(),
                provider_key: provider_key.into(),
                scope: scope.clone(),
                library: "PrimarySync".into(),
                child: "asset-502".into(),
                master: "502".into(),
                state_id: "asset-502".into(),
                origin_pass: op.pass.clone(),
            };
            let receipts = db
                .bound_download_receipts(
                    new_binding.clone(),
                    "original".into(),
                    provider_hash(EDIT_TWO),
                    EDIT_TWO.len() as u64,
                )
                .await
                .unwrap();
            assert_eq!(receipts.len(), 1);
            assert_eq!(receipts[0].native_path.to_path(), new_path);
            assert_eq!(
                receipts[0].source_checksum,
                format!("{:x}", Sha256::digest(EDIT_TWO))
            );
            assert_eq!(receipts[0].local_checksum, receipts[0].source_checksum);
            drop(db);
            for row in rows(
                &root,
                "SELECT local_path,preserved_path_key FROM asset_metadata_paths WHERE id='asset-501' ORDER BY version_size",
            ) {
                let [SqlValue::Text(path), marker] = row.as_slice() else {
                    panic!("invalid old path marker")
                };
                assert!(
                    matches!(marker, SqlValue::Null)
                        || *marker
                            == SqlValue::Text(
                                state::db::primary_layout::path_key(Path::new(path)).unwrap()
                            ),
                    "only exact read-only path marker may be filled"
                );
            }
            let settled = settled_proofs(&root);
            let settled_ledger = rows(&root, ledger_sql);
            let after_requests = server.received_requests().await.unwrap().len();
            assert_eq!(after_requests, requests + 1);
            for round in 0..2 {
                let (result, lookups) = cycle_options(
                    &root,
                    complete.clone(),
                    true,
                    naming,
                    options(directory, template, "new-resource-token", 2),
                )
                .await;
                assert_eq!(result.failed_count, 0, "{result:?}");
                assert!(result.db_sync_token_advance_safe);
                assert_eq!((result.stats.downloaded, lookups), (0, 0));
                assert_eq!(
                    server.received_requests().await.unwrap().len(),
                    after_requests
                );
                assert_eq!(settled_proofs(&root), settled);
                assert_eq!(rows(&root, ledger_sql), settled_ledger);
                let mut actual = tree(&old_media);
                if new_root != old_media {
                    actual.extend(tree(&new_root));
                }
                assert_eq!(actual, expected_tree);
                assert_eq!(
                    rows(
                        &root,
                        "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'"
                    ),
                    vec![vec![SqlValue::Text("new-resource-token".into())]]
                );
                let db = state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
                    .await
                    .unwrap();
                let quiet_queue = metadata_queue(&db).await;
                std::fs::write(
                    root.join(format!("metadata-queue-quiet-{round}.json")),
                    serde_json::to_vec_pretty(&quiet_queue).unwrap(),
                )
                .unwrap();
                assert_eq!(quiet_queue, recovered_queue);
                assert!(db.get_pending().await.unwrap().is_empty());
                assert_eq!(
                    db.bound_download_receipts(
                        new_binding.clone(),
                        "original".into(),
                        provider_hash(EDIT_TWO),
                        EDIT_TWO.len() as u64
                    )
                    .await
                    .unwrap(),
                    receipts
                );
                std::fs::write(root.join(format!("quiet-{round}.json")),serde_json::to_vec_pretty(&json!({"result":format!("{result:?}"),"proofs_unchanged":true,"physical_unchanged":true,"GETs":after_requests,"operation":op.operation,"generation":expected_binding.generation})).unwrap()).unwrap();
            }
            let received = server.received_requests().await.unwrap();
            for endpoint in ["/original", "/edit-one", "/new-resource"] {
                assert_eq!(
                    received
                        .iter()
                        .filter(|r| r.method == "GET" && r.url.path() == endpoint)
                        .count(),
                    1
                );
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            std::fs::write(
                root.join("final-files.json"),
                serde_json::to_vec_pretty(&expected_tree).unwrap(),
            )
            .unwrap();
            eprintln!(
                "RECORDED_OLD_ROOT_COMPLETE root={} operation={} prior_generation={} committed_generation={} ordering_GET=1 quiet_reopens=2",
                root.display(),
                op.operation,
                op.generation,
                expected_binding.generation
            );
        }
    }
}

#[tokio::test]
async fn review501_changed_unselected_original_keeps_failed_adjusted_pending() {
    use crate::state::db::match_diagnostics::MatchDiagnostics;
    use crate::state::db::provider_selection::SelectionPath;
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicBool;

    fn provider_hash(bytes: &[u8]) -> String {
        use sha1::{Digest as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        base64::engine::general_purpose::STANDARD.encode([&[1][..], &hash.finalize()[..]].concat())
    }
    fn rows(db_path: &std::path::Path, sql: &str) -> Vec<Value> {
        let conn = rusqlite::Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str(&r.unwrap()).unwrap())
            .collect()
    }
    let server = server().await;
    let root = tempfile::tempdir().unwrap().keep();
    eprintln!("CHANGED_ORIGINAL_DEBT_ROOT={}", root.display());
    let db_path = root.join("state.db");
    let media = root.join("media");
    let original = media.join("IMG_0501.JPG");
    let file_snapshot = |path: &std::path::Path| {
        let file = std::fs::File::open(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&file).unwrap(),
            file.metadata().unwrap().modified().unwrap(),
        )
    };
    let media_paths = || {
        std::fs::read_dir(&media)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<BTreeSet<_>>()
    };
    let page = |edit: Option<(&str, &[u8])>| {
        let mut page = records(&server, edit);
        page[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(provider_hash(ORIGINAL));
        if let Some((_, bytes)) = edit {
            page[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                json!(provider_hash(bytes));
        }
        page
    };
    let receipt_sql = "SELECT json_object('id',id,'account',account_key,'provider',provider_key,'scope',scope,'library',library,'child',child,'master',master,'state',state_id,'pass',origin_pass,'version',version_size,'checksum',provider_checksum,'size',provider_size,'path',native_path,'source',source_checksum,'local',local_checksum,'proof',proof_hash,'trust',trust_receipt,'metadata',selected_metadata_hash,'generation',generation_id) FROM download_receipts WHERE library='PrimarySync' AND child='asset-501' AND version_size='original' ORDER BY id";
    let adjusted_sql = receipt_sql.replace("version_size='original'", "version_size='adjusted'");
    let intent_sql = "SELECT json_object('library',library,'child',id,'version',version_size,'requested',requested_path_key,'destination_key',destination_path_key,'destination',destination_path,'checksum',provider_checksum,'size',provider_size) FROM reconciliation_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' ORDER BY provider_checksum,provider_size";
    let debt_sql = "SELECT json_object('generation',g.id,'format',json_extract(CAST(g.specification AS TEXT),'$.format'),'account',g.account_key,'provider',g.provider_key,'scope',g.scope,'pass',d.pass_key,'library',d.library,'child',d.child,'state',d.asset_id,'master',d.master,'version',d.version_size,'path',d.path,'compat_path',d.compat_path,'checksum',d.checksum,'size',d.size_bytes,'metadata',d.metadata_hash,'admitted',d.admitted,'verified',d.verified_media,'sealed',g.sealed,'ready',g.checkpoint_ready) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.version_size='adjusted' ORDER BY g.id,d.pass_key,d.path";
    let error_sql = "SELECT json_quote(last_error) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'";
    let checkpoint_sql =
        "SELECT json_quote(value) FROM metadata WHERE key='sync_token:PrimarySync'";
    let temp_sql =
        "SELECT json_array(hex(path),claimed_at,publication) FROM owned_temp_files ORDER BY path";
    let mapping_sql = "SELECT json_array(library,asset_record_name,master_record_name) FROM asset_master_mappings WHERE asset_record_name='asset-501' ORDER BY library";
    let options = |resolution, token| CycleOptions {
        retry: Some(crate::retry::RetryConfig {
            max_retries: 1,
            base_delay_secs: 0,
            max_delay_secs: 0,
        }),
        private: true,
        resolution,
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        &root,
        page(None),
        false,
        EditedNaming::Suffix,
        options(crate::types::PhotoResolution::Original, "original-token"),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    assert_bytes(&original, ORIGINAL);
    let original_file = file_snapshot(&original);
    let original_receipts = rows(&db_path, receipt_sql);
    assert_eq!(original_receipts.len(), 1);
    let original_mapping = rows(&db_path, mapping_sql);
    assert_eq!(rows(&db_path, checkpoint_sql), [json!("original-token")]);
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let fault = Arc::new(AtomicBool::new(true));
    let fail_requests = fault.clone();
    Mock::given(method("GET"))
        .and(path("/edit-one"))
        .respond_with(move |_: &wiremock::Request| {
            if fail_requests.load(Ordering::SeqCst) {
                seen.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(500)
            } else {
                ResponseTemplate::new(200).set_body_bytes(EDIT_ONE)
            }
        })
        .with_priority(2)
        .mount(&server)
        .await;
    let mut edited = page(Some(("edit-one", EDIT_ONE)));
    let (failed, _) = cycle_options(
        &root,
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "debt-token"),
    )
    .await;
    assert!(failed.failed_count > 0, "{failed:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    assert!(failed.db_sync_token_advance_safe);
    let intent = rows(&db_path, intent_sql);
    assert_eq!(intent.len(), 1);
    let sibling = std::path::PathBuf::from(intent[0]["destination"].as_str().unwrap());
    assert_ne!(sibling, original);
    assert!(sibling.starts_with(&media));
    assert!(!sibling.exists());
    assert_eq!(
        intent[0]["requested"],
        crate::fs_util::confined_path_key(&original).unwrap()
    );
    assert_eq!(
        intent[0]["destination_key"],
        crate::fs_util::confined_path_key(&sibling).unwrap()
    );
    assert_eq!(intent[0]["checksum"], provider_hash(EDIT_ONE));
    assert_eq!(intent[0]["size"], EDIT_ONE.len());
    let debt = rows(&db_path, debt_sql);
    let assert_debt = |debt: &[Value]| {
        for d in debt {
            for key in [
                "account", "provider", "scope", "pass", "library", "child", "state", "master",
            ] {
                assert_eq!(d[key], original_receipts[0][key], "{key}: {d:?}");
            }
            assert_eq!(d["format"], 2);
            assert_eq!(d["version"], "adjusted");
            let native: SelectionPath = serde_json::from_str(d["path"].as_str().unwrap()).unwrap();
            assert_eq!(native.to_path(), sibling);
            assert_eq!(d["compat_path"], sibling.to_str().unwrap());
            assert_eq!(d["checksum"], provider_hash(EDIT_ONE));
            assert_eq!(d["size"], EDIT_ONE.len());
            for key in ["admitted", "sealed", "ready"] {
                assert_eq!(d[key], 1);
            }
            assert_eq!(d["verified"], 0);
        }
    };
    assert_debt(&debt);
    let generations: BTreeSet<_> = debt
        .iter()
        .map(|d| d["generation"].as_str().unwrap())
        .collect();
    assert!(
        generations.len() >= 2,
        "multiple genuine admitted obligations: {debt:?}"
    );
    assert_eq!(generations.len(), debt.len());
    assert!(rows(&db_path, &adjusted_sql).is_empty());
    let failed_error = rows(&db_path, error_sql);
    assert_eq!(failed_error.len(), 1);
    assert!(failed_error[0].as_str().unwrap().contains("HTTP 500"));
    assert!(MatchDiagnostics::decode(failed_error[0].as_str().unwrap()).is_none());
    assert_eq!(rows(&db_path, checkpoint_sql), [json!("debt-token")]);
    assert_eq!(rows(&db_path, receipt_sql), original_receipts);
    assert_eq!(file_snapshot(&original), original_file);
    let temp_custody = rows(&db_path, temp_sql);
    let failed_paths = media_paths();
    let other_files: Vec<_> = failed_paths
        .iter()
        .filter(|p| **p != original)
        .map(|p| (p.clone(), file_snapshot(p)))
        .collect();
    std::fs::write(root.join("failed-transfer-evidence.json"), serde_json::to_vec_pretty(&json!({"intent":intent,"debt":debt,"original_receipt":original_receipts,"failed_gets":calls.load(Ordering::SeqCst),"error":failed_error,"original_physical":format!("{:?}",original_file),"temps":temp_custody})).unwrap()).unwrap();
    fault.store(false, Ordering::SeqCst);
    // Intentional fixture edit on the existing inode, only after real transfer debt.
    let modified = b"locally modified original bytes";
    std::fs::write(&original, modified).unwrap();
    let changed_file = file_snapshot(&original);
    assert_eq!(changed_file.0, modified);
    assert_eq!(changed_file.1, original_file.1);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    assert_eq!(debt[0]["account"], account_key);
    assert_eq!(debt[0]["provider"], provider_key);
    let binding = state::db::PublicationBinding {
        account_key: account_key.into(),
        provider_key: provider_key.into(),
        scope: debt[0]["scope"].as_str().unwrap().into(),
        origin_pass: debt[0]["pass"].as_str().unwrap().into(),
        library: "PrimarySync".into(),
        child: "asset-501".into(),
        master: "501".into(),
        state_id: "asset-501".into(),
    };
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let mut healthy = page(None);
    for record in &mut healthy {
        record["recordName"] = json!(record["recordName"].as_str().unwrap().replace("501", "502"));
        if let Some(reference) = record["fields"].get_mut("masterRef") {
            reference["value"]["recordName"] = json!("502");
        }
    }
    healthy[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    healthy[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)});
    healthy[1]["fields"]["resJPEGFullRes"] = json!({"value":{"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)}});
    healthy[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.jpeg"});
    healthy[1]["fields"]["adjustmentRenderType"] = json!({"value":1});
    edited.extend(healthy);
    let (held, lookups) = cycle_options(
        &root,
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "held-token"),
    )
    .await;
    let held_debt = rows(&db_path, debt_sql);
    let error = rows(&db_path, error_sql);
    let requests = server.received_requests().await.unwrap();
    let adjusted_gets = requests
        .iter()
        .filter(|r| r.method == "GET" && r.url.path() == "/edit-one")
        .count();
    std::fs::write(root.join("changed-original-evidence.json"), serde_json::to_vec_pretty(&json!({"cycle":format!("{held:?}"),"lookups":lookups,"debt":held_debt,"intent":rows(&db_path,intent_sql),"adjusted_receipts":rows(&db_path,&adjusted_sql),"diagnostic":error,"adjusted_gets":adjusted_gets,"requests":requests.iter().map(|r| r.url.path()).collect::<Vec<_>>(),"s_absent":!sibling.exists(),"changed_original_physical":format!("{:?}",file_snapshot(&original)),"original_receipt":rows(&db_path,receipt_sql),"temps":rows(&db_path,temp_sql),"checkpoint":rows(&db_path,checkpoint_sql)})).unwrap()).unwrap();
    assert!(
        lookups > 0,
        "actual due retry must confirm the initial admitted child"
    );
    assert!(held.failed_count > 0, "{held:?}");
    assert!(held.db_sync_token_advance_safe);
    assert_eq!(
        held.stats.downloaded, 1,
        "only healthy502 may publish: {held:?}"
    );
    assert_eq!(file_snapshot(&original), changed_file);
    assert!(!sibling.exists());
    assert_eq!(adjusted_gets, 8);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.method == "GET" && r.url.path() == "/edit-two")
            .count(),
        1
    );
    assert_ne!(error, failed_error);
    let diagnostic = MatchDiagnostics::decode(error[0].as_str().unwrap()).unwrap();
    // Changed P fails the fresh Original witness; its raw bytes cannot prove Adjusted.
    assert!(
        diagnostic.attempts.iter().any(|a| {
            a.child == "asset-501"
                && a.destination.to_path() == sibling
                && a.checksum == provider_hash(EDIT_ONE)
                && a.generation
                    .as_deref()
                    .is_some_and(|g| generations.contains(g))
                && a.pass.as_deref() == Some(binding.origin_pass.as_str())
                && a.explanation.candidates.iter().any(|c| {
                    c.path.as_ref().is_some_and(|p| p.to_path() == original)
                        && c.reason == "candidate_bytes_not_verified_for_current_resource"
                })
        }),
        "{}",
        error[0]
    );
    assert_debt(&held_debt);
    for initial in &debt {
        assert!(
            held_debt.contains(initial),
            "lost initial debt: {initial:?}; {held_debt:?}"
        );
    }
    let held_generations: BTreeSet<_> = held_debt
        .iter()
        .map(|d| d["generation"].as_str().unwrap())
        .collect();
    assert!(generations.is_subset(&held_generations));
    assert_eq!(held_generations.len(), held_debt.len());
    assert_eq!(rows(&db_path, intent_sql), intent);
    assert!(rows(&db_path, &adjusted_sql).is_empty());
    assert_eq!(rows(&db_path, receipt_sql), original_receipts);
    assert_eq!(rows(&db_path, mapping_sql), original_mapping);
    assert_eq!(rows(&db_path, temp_sql), temp_custody);
    assert_eq!(rows(&db_path, checkpoint_sql), [json!("held-token")]);
    for (path, before) in &other_files {
        assert_eq!(&file_snapshot(path), before);
    }
    let healthy_path = media.join("IMG_0502.JPG");
    assert_bytes(&healthy_path, EDIT_TWO);
    let healthy_file = file_snapshot(&healthy_path);
    let mut expected_paths = failed_paths;
    expected_paths.insert(healthy_path.clone());
    assert_eq!(media_paths(), expected_paths);
    let healthy_binding = state::db::PublicationBinding {
        child: "asset-502".into(),
        master: "502".into(),
        state_id: "asset-502".into(),
        ..binding.clone()
    };
    let healthy_receipts = {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.bound_download_receipts(
            healthy_binding.clone(),
            "adjusted".into(),
            provider_hash(EDIT_TWO),
            EDIT_TWO.len() as u64,
        )
        .await
        .unwrap()
    };
    assert_eq!(healthy_receipts.len(), 1);
    assert_eq!(healthy_receipts[0].native_path.to_path(), healthy_path);
    assert_eq!(
        healthy_receipts[0].source_checksum,
        format!("{:x}", Sha256::digest(EDIT_TWO))
    );
    // Restore authorized old bytes on that same inode. Its new mtime is explicit,
    // not reset to or represented as the pre-edit publication timestamp.
    std::fs::write(&original, &original_file.0).unwrap();
    let restored_file = file_snapshot(&original);
    assert_eq!(restored_file.0, original_file.0);
    assert_eq!(restored_file.1, original_file.1);
    std::fs::write(root.join("restored-original-evidence.json"),serde_json::to_vec_pretty(&json!({"restored_physical":format!("{:?}",restored_file),"original_receipt":rows(&db_path,receipt_sql),"intent":rows(&db_path,intent_sql)})).unwrap()).unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let (recovered, _) = cycle_options(
        &root,
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "recovered-token"),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.db_sync_token_advance_safe);
    assert_eq!(recovered.stats.downloaded, 1);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests.len() + 1
    );
    assert_bytes(&sibling, EDIT_ONE);
    let sibling_file = file_snapshot(&sibling);
    let adjusted_receipts = rows(&db_path, &adjusted_sql);
    assert_eq!(adjusted_receipts.len(), 1);
    for (key, expected) in [
        ("account", &binding.account_key),
        ("provider", &binding.provider_key),
        ("scope", &binding.scope),
        ("pass", &binding.origin_pass),
        ("library", &binding.library),
        ("child", &binding.child),
        ("master", &binding.master),
        ("state", &binding.state_id),
    ] {
        assert_eq!(adjusted_receipts[0][key], expected.as_str(), "{key}");
    }
    assert_eq!(adjusted_receipts[0]["version"], "adjusted");
    let native: SelectionPath =
        serde_json::from_str(adjusted_receipts[0]["path"].as_str().unwrap()).unwrap();
    assert_eq!(native.to_path(), sibling);
    assert_eq!(adjusted_receipts[0]["checksum"], provider_hash(EDIT_ONE));
    assert_eq!(adjusted_receipts[0]["size"], EDIT_ONE.len());
    assert_eq!(
        adjusted_receipts[0]["source"],
        format!("{:x}", Sha256::digest(EDIT_ONE))
    );
    assert_eq!(
        adjusted_receipts[0]["local"],
        adjusted_receipts[0]["source"]
    );
    for key in ["trust", "metadata", "generation"] {
        assert!(adjusted_receipts[0][key].is_null());
    }
    let verified_receipts = {
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        db.bound_download_receipts(
            binding.clone(),
            "adjusted".into(),
            provider_hash(EDIT_ONE),
            EDIT_ONE.len() as u64,
        )
        .await
        .unwrap()
    };
    assert_eq!(verified_receipts.len(), 1);
    assert_eq!(
        adjusted_receipts[0]["id"],
        verified_receipts[0].id().unwrap()
    );
    // Check recovery and then two actual quiet owned reopens, using restored P facts.
    for round in 0..=2 {
        if round > 0 {
            let before = server.received_requests().await.unwrap().len();
            let (quiet, lookups) = cycle_options(
                &root,
                edited.clone(),
                true,
                EditedNaming::Primary,
                options(crate::types::PhotoResolution::None, "recovered-token"),
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{quiet:?}");
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!((quiet.stats.downloaded, lookups), (0, 0));
            assert_eq!(server.received_requests().await.unwrap().len(), before);
        }
        assert_eq!(file_snapshot(&original), restored_file);
        assert_eq!(file_snapshot(&sibling), sibling_file);
        assert_eq!(file_snapshot(&healthy_path), healthy_file);
        assert_eq!(rows(&db_path, receipt_sql), original_receipts);
        assert_eq!(rows(&db_path, &adjusted_sql), adjusted_receipts);
        assert_eq!(rows(&db_path, mapping_sql), original_mapping);
        assert_eq!(rows(&db_path, intent_sql), intent);
        assert_eq!(rows(&db_path, checkpoint_sql), [json!("recovered-token")]);
        let finished = rows(&db_path, debt_sql);
        for initial in &held_debt {
            let mut completed = initial.clone();
            completed["verified"] = json!(1);
            assert!(
                finished.contains(&completed),
                "initial destination not verified at exact S: {initial:?}; {finished:?}"
            );
        }
        assert_eq!(
            db_count(
                &root,
                "SELECT count(*) FROM provider_active_destinations WHERE library='PrimarySync' AND child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0"
            ),
            0
        );
        assert_eq!(
            db_count(&root, "SELECT count(*) FROM primary_layout_operations"),
            0
        );
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        assert_eq!(
            db.bound_download_receipts(
                binding.clone(),
                "adjusted".into(),
                provider_hash(EDIT_ONE),
                EDIT_ONE.len() as u64
            )
            .await
            .unwrap(),
            verified_receipts
        );
        assert_eq!(
            db.bound_download_receipts(
                healthy_binding.clone(),
                "adjusted".into(),
                provider_hash(EDIT_TWO),
                EDIT_TWO.len() as u64
            )
            .await
            .unwrap(),
            healthy_receipts
        );
        assert_eq!(media_paths().len(), 3);
    }
}

#[tokio::test]
async fn review501_legacy_changed_extensions_and_aliases_preserve_after_revert() {
    use state::db::provider_selection::SelectionPath;

    let heic = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/pattern.heic"
    ));
    #[cfg(feature = "xmp")]
    let sidecar_modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let sidecar_modes = [false];
    for sidecar in sidecar_modes {
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/edit-heic"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(heic.as_slice()))
            .mount(&server)
            .await;
        let mut root = tempfile::tempdir().unwrap();
        if std::env::var_os("KEI_LEGACY_ALIAS_RETAIN_FIXTURE").is_some() {
            root.disable_cleanup(true);
        }
        eprintln!(
            "LEGACY_ALIAS_A_ROOT sidecar={sidecar} root={}",
            root.path().display()
        );
        let media = root.path().join("media");
        let options = || CycleOptions {
            private: true,
            #[cfg(feature = "xmp")]
            sidecar,
            ..CycleOptions::default()
        };
        let rows = |sql: &str| {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let file = |path: &std::path::Path| {
            let handle = std::fs::File::open(path).unwrap();
            (
                std::fs::read(path).unwrap(),
                crate::fs_util::file_identity(&handle).unwrap(),
                handle.metadata().unwrap().modified().unwrap(),
            )
        };
        let physical = || {
            let mut files = std::collections::BTreeMap::new();
            let mut pending = vec![media.clone()];
            while let Some(path) = pending.pop() {
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                assert!(!metadata.file_type().is_symlink());
                #[cfg(windows)]
                let handle = {
                    use std::os::windows::fs::OpenOptionsExt;
                    use windows_sys::Win32::Storage::FileSystem::{
                        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                    };
                    std::fs::OpenOptions::new()
                        .access_mode(FILE_READ_ATTRIBUTES)
                        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                        .open(&path)
                        .unwrap()
                };
                #[cfg(not(windows))]
                let handle = std::fs::File::open(&path).unwrap();
                let bytes = if metadata.is_dir() {
                    pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                    None
                } else {
                    assert!(metadata.is_file());
                    Some(std::fs::read(&path).unwrap())
                };
                files.insert(
                    path,
                    (
                        bytes,
                        crate::fs_util::file_identity(&handle).unwrap(),
                        metadata.modified().unwrap(),
                    ),
                );
            }
            files
        };
        let receipt_sql = "SELECT id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,trust_receipt,selected_metadata_hash,generation_id FROM download_receipts ORDER BY id";
        let path_sql = "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,provider_checksum,local_path";
        let catalogue_sql = "SELECT library,id,version_size,status,checksum,size_bytes,local_path,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM assets ORDER BY library,id,version_size";
        let custody = || {
            [
                rows("SELECT * FROM primary_layout_operations ORDER BY operation"),
                rows("SELECT * FROM primary_layout_bindings ORDER BY family"),
                rows("SELECT * FROM primary_layout_members ORDER BY operation,member"),
                rows("SELECT * FROM primary_layout_claims ORDER BY path_key"),
                rows("SELECT * FROM primary_layout_preserved ORDER BY operation,member"),
                rows(
                    "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,provider_checksum,native_path",
                ),
            ]
        };
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let (account, provider) = owner.authenticated_keys().unwrap();
        let scope = json!({"container":"com.apple.photos.cloud","database":"private","environment":"production","format":1,"realm":"com","zone":{"ownerRecordName":"_defaultOwner","zoneName":"PrimarySync"}}).to_string();
        let selection = json!({"zone":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"name":"TestAlbum","list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted","obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted","query_filter":null,"container_id":null,"cross_zone_sources":[]});
        let pass = format!(
            "{:x}",
            Sha256::digest(
                json!({"kind":"Unfiled","scope":selection,"excludes":[]})
                    .to_string()
                    .as_bytes()
            )
        );
        let prove = |version: &str, path: &std::path::Path, bytes: &[u8]| {
            let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key WHERE r.account_key=?1 AND r.provider_key=?2 AND r.scope=?3 AND r.origin_pass=?4 AND r.library='PrimarySync' AND r.child='asset-501' AND r.master='501' AND r.state_id='asset-501' AND r.version_size=?5 AND r.provider_checksum=?6 AND r.provider_size=?7 AND r.native_path=?8 AND r.source_checksum=?9 AND r.local_checksum=?9 AND length(r.id)>0 AND length(r.proof_hash)=64 AND r.trust_receipt IS NULL AND r.selected_metadata_hash IS NULL AND r.generation_id IS NULL",
                rusqlite::params![account,provider,scope,pass,version,provider_hash(bytes),i64::try_from(bytes.len()).unwrap(),SelectionPath::from_path(path).key().unwrap(),sha], |r|r.get(0),
            ).unwrap();
            assert_eq!(count, 1, "full scoped publication at {path:?}");
            let local: (String,String,String) = conn.query_row("SELECT source_checksum,local_checksum,download_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size=?1 AND provider_checksum=?2 AND local_path=?3", rusqlite::params![version,provider_hash(bytes),path.to_str().unwrap()], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(local, (sha.clone(), sha.clone(), sha));
        };
        let mut first = records(&server, Some(("edit-heic", heic.as_slice())));
        first[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.heic"});
        let first_provider = first.clone();
        let (initial, _) =
            cycle_options(root.path(), first, false, EditedNaming::Suffix, options()).await;
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert!(initial.db_sync_token_advance_safe);
        assert_eq!(
            (
                initial.stats.downloaded,
                initial.stats.skipped.uncertain,
                initial.stats.state_write_failures
            ),
            (2, 0, 0)
        );
        let original = media.join("IMG_0501.JPG");
        let old_heic = media.join("IMG_0501_edited.HEIC");
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&media.join("IMG_0501_edited.HEIC"), heic);
        prove("original", &original, ORIGINAL);
        prove("adjusted", &old_heic, heic);
        let initial_files = [file(&original), file(&old_heic)];
        let initial_xmp = sidecar.then(|| {
            [
                file(&media.join("IMG_0501.JPG.xmp")),
                file(&media.join("IMG_0501_edited.HEIC.xmp")),
            ]
        });
        let initial_receipts = rows(receipt_sql);
        let initial_paths = rows(path_sql);
        assert_eq!(initial_receipts.len(), 2);
        let second = records(&server, Some(("edit-two", EDIT_TWO)));
        let named_rows = |table: &str| {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut statement = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let columns: Vec<_> = statement
                .column_names()
                .into_iter()
                .map(str::to_owned)
                .collect();
            statement
                .query_map([], |row| {
                    let mut fields = serde_json::Map::new();
                    for (index, name) in columns.iter().enumerate() {
                        let value = match row.get::<_, rusqlite::types::Value>(index)? {
                            rusqlite::types::Value::Null => Value::Null,
                            rusqlite::types::Value::Integer(value) => json!(value),
                            rusqlite::types::Value::Real(value) => json!(value),
                            rusqlite::types::Value::Text(value) => json!(value),
                            rusqlite::types::Value::Blob(value) => {
                                json!({"blob_hex": data_encoding::HEXLOWER.encode(&value)})
                            }
                        };
                        fields.insert(name.clone(), value);
                    }
                    Ok(Value::Object(fields))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let phase_rows = || {
            json!({
                "paths": named_rows("asset_metadata_paths"),
                "assets": named_rows("assets"),
                "receipts": named_rows("download_receipts"),
                "destinations": named_rows("provider_active_destinations"),
                "generations": named_rows("provider_active_generations"),
                "groupings": named_rows("asset_albums"),
                "metadata": named_rows("metadata"),
            })
        };
        let before_jpeg = phase_rows();
        eprintln!(
            "LEGACY_ALIAS_A_ROW_BEFORE sidecar={sidecar} root={} rows={before_jpeg} provider_before={first_provider:?} provider_after={second:?} policy=Suffix resolution=Original refresh_metadata=false capture_policy=Preserve private=true GETs={:?} files={:?}",
            root.path().display(),
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|r| (&r.method, r.url.path()))
                .collect::<Vec<_>>(),
            physical()
        );
        let (updated, _) =
            cycle_options(root.path(), second, false, EditedNaming::Suffix, options()).await;
        let after_jpeg = phase_rows();
        eprintln!(
            "LEGACY_ALIAS_A_ROW_AFTER sidecar={sidecar} root={} rows={after_jpeg} result={updated:?} GETs={:?} files={:?}",
            root.path().display(),
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .map(|r| (&r.method, r.url.path()))
                .collect::<Vec<_>>(),
            physical()
        );
        assert_eq!(updated.failed_count, 0, "{:?}", updated.stats);
        assert!(updated.db_sync_token_advance_safe);
        assert_eq!(
            (
                updated.stats.downloaded,
                updated.stats.skipped.uncertain,
                updated.stats.state_write_failures
            ),
            (1, 0, 0)
        );
        assert_eq!([file(&original), file(&old_heic)], initial_files);
        if let Some(expected) = &initial_xmp {
            assert_eq!(
                &[
                    file(&media.join("IMG_0501.JPG.xmp")),
                    file(&media.join("IMG_0501_edited.HEIC.xmp"))
                ],
                expected
            );
        }
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let statement = conn.prepare(path_sql).unwrap();
        let path_column = statement.column_index("local_path").unwrap();
        let marker_column = statement.column_index("preserved_path_key").unwrap();
        let version_column = statement.column_index("version_size").unwrap();
        let library_column = statement.column_index("library").unwrap();
        let child_column = statement.column_index("id").unwrap();
        let published_paths = rows(path_sql);
        for mut expected in initial_paths {
            if expected[path_column]
                == rusqlite::types::Value::Text(original.to_str().unwrap().to_owned())
            {
                let actual = published_paths
                    .iter()
                    .find(|row| {
                        row[path_column] == expected[path_column]
                            && row[version_column] == expected[version_column]
                    })
                    .unwrap();
                if actual[marker_column] != expected[marker_column] {
                    // The measured second-publication transition is exact
                    // read-only Original reuse, not a media/metadata write or
                    // debt completion. Every other full-row field stays strict.
                    assert!(
                        !sidecar,
                        "only the measured zero-write reuse transition is authorized"
                    );
                    assert_eq!(
                        expected[library_column],
                        rusqlite::types::Value::Text("PrimarySync".into())
                    );
                    assert_eq!(
                        expected[child_column],
                        rusqlite::types::Value::Text("asset-501".into())
                    );
                    assert_eq!(
                        expected[version_column],
                        rusqlite::types::Value::Text("original".into())
                    );
                    assert_eq!(expected[marker_column], rusqlite::types::Value::Null);
                    let canonical = state::db::primary_layout::path_key(&original).unwrap();
                    assert_eq!(
                        actual[marker_column],
                        rusqlite::types::Value::Text(canonical.clone())
                    );
                    assert_eq!(file(&original), initial_files[0]);
                    prove("original", &original, ORIGINAL);
                    let verified_readonly: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation JOIN assets a ON a.library=d.library AND a.id=d.asset_id AND a.version_size=d.version_size WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.asset_id='asset-501' AND d.master='501' AND d.version_size='original' AND d.pass_key=?1 AND d.path=?2 AND d.checksum=?3 AND d.size_bytes=?4 AND d.source_checksum=?5 AND d.local_checksum=?5 AND d.admitted=1 AND d.verified_media=1 AND d.metadata_hash=a.metadata_hash AND g.account_key=?6 AND g.provider_key=?7 AND g.scope=?8 AND g.metadata_enabled=0 AND json_extract(CAST(g.specification AS TEXT),'$.metadata_flags')=0", rusqlite::params![pass,SelectionPath::from_path(&original).key().unwrap(),provider_hash(ORIGINAL),i64::try_from(ORIGINAL.len()).unwrap(),data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL)),account,provider,scope], |r|r.get(0)).unwrap();
                    assert_eq!(
                        verified_readonly, 1,
                        "exact scoped Original read-only reuse owner basis"
                    );
                    expected[marker_column] = rusqlite::types::Value::Text(canonical);
                    eprintln!(
                        "LEGACY_ALIAS_A_EXACT_READONLY_TRANSITION sidecar={sidecar} native={original:?} expected_full_row={expected:?} verified_readonly={verified_readonly} debt_and_all_other_columns_unchanged=true"
                    );
                }
            }
            assert!(
                published_paths.contains(&expected),
                "unexpected second-publication path-row change: expected={expected:?}; actual={published_paths:?}"
            );
        }
        drop(statement);
        drop(conn);
        for row in initial_receipts {
            assert!(rows(receipt_sql).contains(&row));
        }
        assert_bytes(&media.join("IMG_0501_edited.HEIC"), heic);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_TWO);
        prove("original", &original, ORIGINAL);
        prove("adjusted", &old_heic, heic);
        prove("adjusted", &media.join("IMG_0501_edited.JPG"), EDIT_TWO);
        let authentic_receipts = rows(receipt_sql);
        assert_eq!(authentic_receipts.len(), 3);
        // Explicit synthetic durable legacy association. Copy + mark_downloaded
        // is catalogue-only evidence, NOT a second authenticated publication.
        let alias_name = format!("IMG_0501_edited-{}.JPG", EDIT_TWO.len());
        let alias = media.join(&alias_name);
        std::fs::copy(media.join("IMG_0501_edited.JPG"), &alias).unwrap();
        if sidecar {
            std::fs::copy(
                media.join("IMG_0501_edited.JPG.xmp"),
                media.join(format!("{alias_name}.xmp")),
            )
            .unwrap();
        }
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "asset-501",
            "adjusted",
            &alias,
            &data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_TWO)),
            None,
        )
        .await
        .unwrap();
        drop(db);
        assert_ne!(file(&alias).1, file(&media.join("IMG_0501_edited.JPG")).1);
        assert_eq!(
            rows(receipt_sql),
            authentic_receipts,
            "no ScopedAlias receipt may be invented"
        );
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let alias_receipts: i64 = conn
            .query_row(
                "SELECT count(*) FROM download_receipts WHERE native_path=?1",
                [SelectionPath::from_path(&alias).key().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alias_receipts, 0);
        let alias_association: (String,String) = conn.query_row("SELECT provider_checksum,local_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND local_path=?1", [alias.to_str().unwrap()], |r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(
            alias_association,
            (
                provider_hash(EDIT_TWO),
                data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_TWO))
            )
        );
        drop(conn);
        let before_files = physical();
        assert_eq!(before_files.len(), 1 + 4 * if sidecar { 2 } else { 1 });
        let before_paths = rows(path_sql);
        let before_catalogue = rows(catalogue_sql);
        let before_custody = custody();
        assert!(before_custody.iter().all(Vec::is_empty));
        assert_eq!(before_paths.len(), 4);
        let requests = server.received_requests().await.unwrap().len();
        assert_eq!(requests, 3);
        for endpoint in ["/original", "/edit-heic", "/edit-two"] {
            assert_eq!(
                server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|r| r.method == "GET" && r.url.path() == endpoint)
                    .count(),
                1,
                "{endpoint}"
            );
        }
        eprintln!(
            "LEGACY_ALIAS_A_BEFORE sidecar={sidecar} files={before_files:?} path_proofs={before_paths:?} scoped19={authentic_receipts:?} catalogue={before_catalogue:?} GETs=original:1,edit-heic:1,edit-two:1 synthetic_alias_no_scoped_receipt=true"
        );
        let current = records(&server, None);
        let before_revert = phase_rows();
        let pending = async || {
            let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
                .await
                .unwrap();
            db.get_pending_metadata_rewrites_page_for_queue(
                state::db::MetadataRewriteQueue::Ordinary,
                None,
                0,
                100,
            )
            .await
            .unwrap()
        };
        assert!(
            pending().await.is_empty(),
            "unexpected pre-revert metadata queue peer"
        );
        let current_metadata =
            crate::icloud::photos::PhotoAsset::new(current[0].clone(), current[1].clone())
                .metadata_arc(state::VersionSizeKey::Original);
        let old_metadata = crate::icloud::photos::PhotoAsset::new(
            first_provider[0].clone(),
            first_provider[1].clone(),
        )
        .metadata_arc(state::VersionSizeKey::Original);
        eprintln!(
            "LEGACY_ALIAS_A_ROW_BEFORE_REVERT sidecar={sidecar} root={} rows={before_revert}",
            root.path().display()
        );
        let revert_started = chrono::Utc::now().timestamp();
        let (migrated, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        let revert_finished = chrono::Utc::now().timestamp();
        let after_revert = phase_rows();
        eprintln!(
            "LEGACY_ALIAS_A_ROW_AFTER_REVERT sidecar={sidecar} root={} rows={after_revert} result={migrated:?}",
            root.path().display()
        );
        assert_eq!(
            migrated.failed_count, 0,
            "sidecar={sidecar}: {:?}",
            migrated.stats
        );
        assert!(migrated.db_sync_token_advance_safe);
        assert_eq!(
            (
                migrated.stats.downloaded,
                migrated.stats.skipped.uncertain,
                migrated.stats.state_write_failures
            ),
            (0, 0, 0)
        );
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert_eq!(
            physical(),
            before_files,
            "all original/native alias/XMP bytes, IDs and mtimes survive ordinary revert"
        );
        let mut expected_paths = before_paths.clone();
        let mut expected_catalogue = before_catalogue.clone();
        let pending_after = pending().await;
        let before_original = before_revert["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["version_size"] == "original")
            .unwrap();
        let after_original = after_revert["assets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["version_size"] == "original")
            .unwrap();
        assert_eq!(
            before_original["metadata_hash"],
            json!(old_metadata.metadata_hash)
        );
        assert_eq!(
            after_original["metadata_hash"],
            json!(current_metadata.metadata_hash)
        );
        assert_ne!(old_metadata.metadata_hash, current_metadata.metadata_hash);
        assert_eq!(
            after_original["provider_data"],
            json!(current_metadata.provider_data)
        );
        assert!(before_revert["groupings"].as_array().unwrap().is_empty());
        assert!(after_revert["groupings"].as_array().unwrap().is_empty());
        let generations = after_revert["generations"].as_array().unwrap();
        assert_eq!(generations.len(), 1);
        let generation = &generations[0];
        assert_eq!(generation["metadata_enabled"], i64::from(sidecar));
        let spec: Value = serde_json::from_slice(
            &data_encoding::HEXLOWER
                .decode(
                    generation["specification"]["blob_hex"]
                        .as_str()
                        .unwrap()
                        .as_bytes(),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(spec["metadata_enabled"], sidecar);
        assert_eq!(spec["metadata_flags"].as_u64().unwrap() != 0, sidecar);
        let destinations = after_revert["destinations"].as_array().unwrap();
        assert_eq!(destinations.len(), 1);
        let selected_original = &destinations[0];
        for (key, value) in [
            ("generation", &generation["id"]),
            ("library", &json!("PrimarySync")),
            ("child", &json!("asset-501")),
            ("asset_id", &json!("asset-501")),
            ("master", &json!("501")),
            ("pass_key", &json!(pass)),
            ("version_size", &json!("original")),
            ("metadata_hash", &json!(current_metadata.metadata_hash)),
            ("checksum", &json!(provider_hash(ORIGINAL))),
            ("size_bytes", &json!(ORIGINAL.len())),
            ("admitted", &json!(1)),
            ("verified_media", &json!(1)),
        ] {
            assert_eq!(&selected_original[key], value, "{key}");
        }
        assert_eq!(
            serde_json::from_str::<SelectionPath>(selected_original["path"].as_str().unwrap())
                .unwrap()
                .to_path(),
            original
        );
        let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL));
        assert_eq!(selected_original["source_checksum"], sha);
        assert_eq!(selected_original["local_checksum"], sha);
        if sidecar {
            // Admission records new work on provider-current metadata hash drift.
            // This is a pending addition, never proof of a local metadata write.
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            assert_eq!(
                conn.query_row("SELECT count(*) FROM asset_album_memberships", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
                0
            );
            let stamp = after_original["metadata_write_failed_at"].as_i64().unwrap();
            assert!((revert_started..=revert_finished).contains(&stamp));
            let paths = conn.prepare(path_sql).unwrap();
            let debt_column = paths.column_index("metadata_write_failed_at").unwrap();
            let expected = expected_paths
                .iter_mut()
                .find(|row| {
                    row[path_column]
                        == rusqlite::types::Value::Text(original.to_str().unwrap().into())
                        && row[version_column] == rusqlite::types::Value::Text("original".into())
                })
                .unwrap();
            assert_eq!(expected[debt_column], rusqlite::types::Value::Null);
            assert_eq!(before_original["metadata_write_failed_at"], Value::Null);
            expected[debt_column] = rusqlite::types::Value::Integer(stamp);
            let catalogue = conn.prepare(catalogue_sql).unwrap();
            let debt_column = catalogue.column_index("metadata_write_failed_at").unwrap();
            let version_column = catalogue.column_index("version_size").unwrap();
            let expected = expected_catalogue
                .iter_mut()
                .find(|row| row[version_column] == rusqlite::types::Value::Text("original".into()))
                .unwrap();
            assert_eq!(expected[debt_column], rusqlite::types::Value::Null);
            expected[debt_column] = rusqlite::types::Value::Integer(stamp);
            assert_eq!(
                pending_after.len(),
                1,
                "unexpected unfiltered metadata queue peer"
            );
            let queued = &pending_after[0];
            assert_eq!(
                (
                    queued.asset.library.as_ref(),
                    queued.asset.id.as_ref(),
                    queued.asset.version_size
                ),
                ("PrimarySync", "asset-501", state::VersionSizeKey::Original)
            );
            assert_eq!(queued.asset.local_path.as_ref(), Some(&original));
            assert_eq!(queued.asset.checksum.as_ref(), provider_hash(ORIGINAL));
            assert_eq!(queued.asset.size_bytes, ORIGINAL.len() as u64);
            assert_eq!(
                queued.asset.metadata.metadata_hash,
                current_metadata.metadata_hash
            );
            assert_eq!(
                queued.asset.metadata.provider_data,
                current_metadata.provider_data
            );
            assert_eq!(queued.source_checksum.as_deref(), Some(sha.as_str()));
            assert_eq!(queued.asset.local_checksum.as_deref(), Some(sha.as_str()));
            assert_eq!(
                queued.asset.download_checksum.as_deref(),
                Some(sha.as_str())
            );
            assert!(queued.selection_receipt.is_none() && queued.capture_repair_receipt.is_none());
            eprintln!(
                "LEGACY_ALIAS_A_PENDING_ADDITION root={} bounds={revert_started}..={revert_finished} stamp={stamp} old_hash={:?} current_metadata={current_metadata:?} selected={selected_original} generation={generation} normal_queue={pending_after:?}",
                root.path().display(),
                old_metadata.metadata_hash
            );
        } else {
            assert!(pending_after.is_empty());
        }
        assert_eq!(rows(path_sql), expected_paths);
        assert_eq!(rows(catalogue_sql), expected_catalogue);
        assert_eq!(rows(receipt_sql), authentic_receipts);
        assert_eq!(custody(), before_custody);
        assert!(!media.join(".kei-history").exists());
        assert!(!media.join("IMG_0501_original.JPG").exists());
        let checkpoint_sql = "SELECT key,value FROM metadata WHERE key='sync_token:PrimarySync'";
        let checkpoint = rows(checkpoint_sql);
        assert_eq!(checkpoint.len(), 1);
        let selection_sql = "SELECT d.* FROM provider_active_destinations d ORDER BY generation,pass_key,child,version_size,path";
        let selected = rows(selection_sql);
        assert!(!selected.is_empty());
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let selected_original: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.asset_id='asset-501' AND d.master='501' AND d.version_size='original' AND d.pass_key=?1 AND d.path=?2 AND d.checksum=?3 AND d.size_bytes=?4 AND d.source_checksum=?5 AND d.local_checksum=?5 AND d.admitted=1 AND d.verified_media=1 AND g.account_key=?6 AND g.provider_key=?7 AND g.scope=?8", rusqlite::params![pass,SelectionPath::from_path(&original).key().unwrap(),provider_hash(ORIGINAL),i64::try_from(ORIGINAL.len()).unwrap(),data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL)),account,provider,scope], |r|r.get(0)).unwrap();
        assert!(
            selected_original > 0,
            "reverted Original must actually reach verified selection"
        );
        drop(conn);
        for round in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Primary,
                options(),
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(
                (
                    quiet.stats.skipped.uncertain,
                    quiet.stats.state_write_failures
                ),
                (0, 0)
            );
            assert_eq!(lookups, 0);
            assert_eq!(physical(), before_files);
            assert_eq!(rows(path_sql), expected_paths);
            assert_eq!(rows(catalogue_sql), expected_catalogue);
            let quiet_pending = pending().await;
            assert_eq!(format!("{quiet_pending:?}"), format!("{pending_after:?}"));
            assert_eq!(phase_rows()["generations"], after_revert["generations"]);
            assert_eq!(rows(receipt_sql), authentic_receipts);
            assert_eq!(custody(), before_custody);
            assert_eq!(rows(checkpoint_sql), checkpoint);
            assert_eq!(rows(selection_sql), selected);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            eprintln!(
                "LEGACY_ALIAS_A_QUIET sidecar={sidecar} round={round} files={} path_proofs={} scoped19={} custody=empty checkpoint={checkpoint:?} selection={selected:?} GETs=3 lookups=0",
                before_files.len(),
                before_paths.len(),
                authentic_receipts.len()
            );
            eprintln!(
                "LEGACY_ALIAS_A_PENDING_QUIET sidecar={sidecar} round={round} root={} full_queue={quiet_pending:?} full_paths={expected_paths:?} full_catalogue={expected_catalogue:?}",
                root.path().display()
            );
        }
    }
}

#[tokio::test]
async fn review501_recorded_legacy_extension_aliases_require_each_saved_path_identity() {
    use state::db::primary_layout::{LayoutFile, LayoutMember, LayoutOperation};
    use state::db::provider_selection::SelectionPath;

    fn sidecar_path(path: &std::path::Path) -> std::path::PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(".xmp");
        name.into()
    }
    fn file(
        path: &std::path::Path,
    ) -> (Vec<u8>, crate::fs_util::FileIdentity, std::time::SystemTime) {
        let handle = std::fs::File::open(path).unwrap();
        (
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&handle).unwrap(),
            handle.metadata().unwrap().modified().unwrap(),
        )
    }
    fn check_file(proof: &LayoutFile, path: &std::path::Path) {
        let (bytes, identity, _) = file(path);
        assert_eq!(identity, proof.fingerprint.identity, "{path:?}");
        assert_eq!(bytes.len() as u64, proof.fingerprint.size);
        assert_eq!(
            Sha256::digest(&bytes).as_slice(),
            proof.fingerprint.sha256.as_slice()
        );
        assert_eq!(proof.provider_checksum, provider_hash(&bytes));
        match proof.version.as_str() {
            "original" => assert_eq!(
                proof.source_checksum.as_deref(),
                Some(
                    data_encoding::HEXLOWER
                        .encode(&Sha256::digest(&bytes))
                        .as_str()
                )
            ),
            "adjusted" => assert!(
                proof.source_checksum.is_none(),
                "synthetic legacy media has no authenticated download source"
            ),
            other => panic!("unexpected fixture rendition {other}"),
        }
        if let Some(xmp) = &proof.sidecar {
            let (bytes, identity, _) = file(&sidecar_path(path));
            assert_eq!(identity, xmp.identity);
            assert_eq!(bytes.len() as u64, xmp.size);
            assert_eq!(Sha256::digest(&bytes).as_slice(), xmp.sha256.as_slice());
        } else {
            assert!(!sidecar_path(path).exists());
        }
    }
    // An empty-name Unfiled pass avoids inventing TestAlbum membership/debt.
    // Both publication and every recovery reopen derive their pass from this
    // real album context; no manually hashed alternate pass is substituted.
    async fn context(
        root: &std::path::Path,
        records: Vec<Value>,
        sidecar: bool,
        folder: &str,
    ) -> (download::DownloadConfig, AlbumPass, Session) {
        assert!(!sidecar || cfg!(feature = "xmp"));
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
                .await
                .unwrap(),
        );
        let provider = Session {
            reported_count: None,
            records,
            quiet: false,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from(""),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider.clone()),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
            Arc::from("private"),
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        let media = root.join("media");
        let store: Arc<dyn download::DownloadStore> = db;
        let build = make_run_cycle_download_config_builder(&media, store);
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = folder.into();
        config.folder_structure_albums = Arc::from(folder);
        config.folder_structure_smart_folders = Arc::from(folder);
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        config.refresh_metadata = false;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = sidecar;
        }
        (config.with_pass(&pass), pass, provider)
    }
    async fn publish_source(
        root: &std::path::Path,
        records: Vec<Value>,
        sidecar: bool,
        naming: EditedNaming,
    ) -> (crate::sync_cycle::CycleResult, String) {
        let (mut download, pass, provider) = context(root, records, sidecar, "").await;
        download.edited_naming = naming;
        let pass_key = download.primary_layout_pass.as_deref().unwrap().to_owned();
        let mut library = make_run_cycle_library_state_with_passes(
            "PrimarySync",
            "sync_token:PrimarySync",
            vec![pass],
        );
        library.library = crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(provider),
            "PrimarySync",
        );
        std::fs::create_dir_all(root.join("media")).unwrap();
        let build = |mode, exclude, groups, zone| {
            let mut next = download.clone();
            next.sync_mode = mode;
            next.exclude_asset_ids = exclude;
            next.asset_groupings = groups;
            next.library = zone;
            Arc::new(next)
        };
        let mut config = make_run_cycle_config();
        config.download.directory = root.join("media");
        config.download.folder_structure = String::new();
        config.download.folder_structure_albums = String::new();
        config.download.folder_structure_smart_folders = String::new();
        config.photos.edited = true;
        config.photos.edited_naming = naming;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = sidecar;
        }
        let (_session_root, session) = make_shared_session_for_run_cycle().await;
        let result = Box::pin(crate::sync_cycle::run_cycle(
            &[&library],
            &config,
            Some(download.state_db.as_ref().unwrap().as_ref()),
            false,
            &build,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        ))
        .await
        .unwrap();
        (result, pass_key)
    }
    let heic = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/pattern.heic"
    ));
    #[cfg(feature = "xmp")]
    let sidecar_modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let sidecar_modes = [false];
    for sidecar in sidecar_modes {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let server = server().await;
        Mock::given(method("GET"))
            .and(path("/edit-heic"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(heic.as_slice()))
            .mount(&server)
            .await;
        let mut root = tempfile::tempdir().unwrap();
        if std::env::var_os("KEI_LEGACY_ALIAS_RETAIN_FIXTURE").is_some() {
            root.disable_cleanup(true);
        }
        eprintln!(
            "LEGACY_ALIAS_B_ROOT sidecar={sidecar} root={}",
            root.path().display()
        );
        let media = root.path().join("media");
        let rows = |sql: &str| {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |r| {
                    (0..columns)
                        .map(|i| r.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let operation = || {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let header: Vec<u8> = conn
                .query_row("SELECT header FROM primary_layout_operations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let mut op: LayoutOperation = serde_json::from_slice(&header).unwrap();
            let mut statement = conn
                .prepare("SELECT evidence FROM primary_layout_members ORDER BY member")
                .unwrap();
            op.members = statement
                .query_map([], |r| r.get::<_, Vec<u8>>(0))
                .unwrap()
                .map(|bytes| serde_json::from_slice::<LayoutMember>(&bytes.unwrap()).unwrap())
                .collect();
            op
        };
        let physical = || {
            let mut files = std::collections::BTreeMap::new();
            let mut pending = vec![media.clone()];
            while let Some(path) = pending.pop() {
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                assert!(!metadata.file_type().is_symlink());
                #[cfg(windows)]
                let handle = {
                    use std::os::windows::fs::OpenOptionsExt;
                    use windows_sys::Win32::Storage::FileSystem::{
                        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
                        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
                    };
                    std::fs::OpenOptions::new()
                        .access_mode(FILE_READ_ATTRIBUTES)
                        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                        .open(&path)
                        .unwrap()
                };
                #[cfg(not(windows))]
                let handle = std::fs::File::open(&path).unwrap();
                let bytes = if metadata.is_dir() {
                    pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                    None
                } else {
                    assert!(metadata.is_file());
                    Some(std::fs::read(&path).unwrap())
                };
                files.insert(
                    path,
                    (
                        bytes,
                        crate::fs_util::file_identity(&handle).unwrap(),
                        metadata.modified().unwrap(),
                    ),
                );
            }
            files
        };
        let receipt_sql = "SELECT id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,trust_receipt,selected_metadata_hash,generation_id FROM download_receipts ORDER BY id";
        let path_sql = "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,provider_checksum,local_path";
        let checkpoint_sql = "SELECT key,value FROM metadata WHERE key='sync_token:PrimarySync'";
        let current = records(&server, None);
        let (initial, saved_pass) =
            publish_source(root.path(), current.clone(), sidecar, EditedNaming::Suffix).await;
        assert_eq!(
            (
                initial.failed_count,
                initial.stats.downloaded,
                initial.stats.skipped.uncertain,
                initial.stats.state_write_failures
            ),
            (0, 1, 0, 0),
            "{initial:?}"
        );
        assert!(initial.db_sync_token_advance_safe);
        let original = media.join("IMG_0501.JPG");
        assert_bytes(&original, ORIGINAL);
        let source_file = file(&original);
        let source_xmp = sidecar.then(|| file(&sidecar_path(&original)));
        let published_receipt = rows(receipt_sql);
        let source_debt_sql = "SELECT library,id,version_size,local_path,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM asset_metadata_paths WHERE version_size='original'";
        let published_debt = rows(source_debt_sql);
        eprintln!(
            "LEGACY_ALIAS_B_SOURCE_PUBLISHED sidecar={sidecar} root={} actual_pass={saved_pass} full_path_rows={:?} full_scoped19={published_receipt:?} debt={published_debt:?} media={source_file:?} xmp={source_xmp:?} result={initial:?}",
            root.path().display(),
            rows(path_sql)
        );
        // Read-only reuse disables optional metadata writes, retaining any raw
        // source XMP from its genuine publication. Prep/recovery below both use
        // the requested sidecar mode, but only on an independent new copy.
        let (reused, reused_pass) =
            publish_source(root.path(), current.clone(), false, EditedNaming::Primary).await;
        assert_eq!(
            (
                reused.failed_count,
                reused.stats.downloaded,
                reused.stats.skipped.uncertain,
                reused.stats.state_write_failures
            ),
            (0, 0, 0, 0),
            "{reused:?}"
        );
        assert!(reused.db_sync_token_advance_safe);
        assert_eq!(reused_pass, saved_pass);
        assert_eq!(file(&original), source_file);
        if let Some(xmp) = &source_xmp {
            assert_eq!(&file(&sidecar_path(&original)), xmp);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let source_sql = format!(
            "SELECT * FROM asset_metadata_paths WHERE local_path='{}'",
            original.to_str().unwrap().replace('\'', "''")
        );
        let source_receipt_sql = format!(
            "SELECT * FROM download_receipts WHERE native_path='{}' ORDER BY id",
            SelectionPath::from_path(&original)
                .key()
                .unwrap()
                .replace('\'', "''")
        );
        let source_path = rows(&source_sql);
        let authentic = rows(&source_receipt_sql);
        eprintln!(
            "LEGACY_ALIAS_B_SOURCE_PROTECTED sidecar={sidecar} root={} actual_pass={reused_pass} full_source_path={source_path:?} full_scoped19={authentic:?} debt={:?} result={reused:?}",
            root.path().display(),
            rows(source_debt_sql)
        );
        assert_eq!(
            rows(source_debt_sql),
            published_debt,
            "read-only source reuse cannot clear or replace existing debt"
        );
        assert_eq!(rows(receipt_sql), published_receipt);
        assert_eq!(source_path.len(), 1);
        assert_eq!(authentic.len(), 1);
        assert_eq!(rows(receipt_sql).len(), 1);
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let marker: String = conn
            .query_row(
                "SELECT preserved_path_key FROM asset_metadata_paths WHERE local_path=?1",
                [original.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            marker,
            state::db::primary_layout::path_key(&original).unwrap()
        );
        let receipt_pass: String = conn
            .query_row("SELECT origin_pass FROM download_receipts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(receipt_pass, saved_pass);
        drop(conn);
        let assert_source = || {
            assert_eq!(
                rows(&source_sql),
                source_path,
                "protected source full path/debt/provenance row changed"
            );
            assert_eq!(
                rows(&source_receipt_sql),
                authentic,
                "protected source authentic scoped receipt changed"
            );
            assert_eq!(file(&original), source_file);
            if let Some(xmp) = &source_xmp {
                assert_eq!(&file(&sidecar_path(&original)), xmp);
            }
        };
        let legacy = media.join("legacy");
        std::fs::create_dir(&legacy).unwrap();
        let old_heic = legacy.join("IMG_0501_edited.HEIC");
        let jpeg = legacy.join("IMG_0501_edited.JPG");
        let alias = legacy.join(format!("IMG_0501_edited-{}.JPG", EDIT_TWO.len()));
        let destination = legacy.join("IMG_0501.JPG");
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        // These are explicit synthetic historical catalogue associations, not
        // authenticated downloads or a claimed released-writer chronology.
        for (name, bytes, path) in [
            ("edit-heic", heic.as_slice(), &old_heic),
            ("edit-two", EDIT_TWO, &jpeg),
        ] {
            let mut observation = records(&server, Some((name, bytes)));
            if name == "edit-heic" {
                observation[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.heic"});
            }
            let (mut config, pass, _) = context(root.path(), observation, sidecar, "legacy").await;
            config.edited_naming = EditedNaming::Suffix;
            assert_eq!(
                config.primary_layout_pass.as_deref(),
                Some(saved_pass.as_str())
            );
            let asset = pass
                .album
                .confirm_layout_asset("asset-501", "PrimarySync")
                .await
                .unwrap()
                .asset;
            let paths = download::filter::expected_paths_for(&asset, &config);
            let selected = paths
                .iter()
                .find(|p| p.version_size == state::VersionSizeKey::Adjusted)
                .unwrap();
            assert_eq!(&selected.path, path);
            assert_eq!(selected.checksum.as_ref(), provider_hash(bytes));
            let record = state::AssetRecord::new_pending(
                Arc::from(asset.source_zone().unwrap()),
                asset.state_id().to_owned(),
                selected.version_size,
                selected.checksum.to_string(),
                selected
                    .path
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned(),
                asset.created(),
                Some(asset.added_date()),
                selected.size,
                download::determine_media_type(selected.version_size, &asset),
            )
            .with_metadata_arc(download::filter::metadata_for_selected_version(
                &asset,
                &config,
                selected.version_size,
            ));
            std::fs::write(path, bytes).unwrap();
            if let Some(xmp) = &source_xmp {
                std::fs::write(sidecar_path(path), &xmp.0).unwrap();
            }
            let db = config.state_db.as_ref().unwrap();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                "asset-501",
                "adjusted",
                path,
                &data_encoding::HEXLOWER.encode(&Sha256::digest(bytes)),
                None,
            )
            .await
            .unwrap();
        }
        std::fs::copy(&jpeg, &alias).unwrap();
        if sidecar {
            std::fs::copy(sidecar_path(&jpeg), sidecar_path(&alias)).unwrap();
        }
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "asset-501",
            "adjusted",
            &alias,
            &data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_TWO)),
            None,
        )
        .await
        .unwrap();
        drop(db);
        assert_eq!(rows(&source_receipt_sql), authentic);
        assert_eq!(
            rows(receipt_sql).len(),
            1,
            "no authenticated Adjusted publication was created"
        );
        assert_source();
        assert_ne!(file(&alias).1, file(&jpeg).1);
        let unassociated = legacy.join("unassociated-equal-bytes.JPG");
        std::fs::copy(&jpeg, &unassociated).unwrap();
        if sidecar {
            std::fs::copy(sidecar_path(&jpeg), sidecar_path(&unassociated)).unwrap();
        }
        let unassociated_file = file(&unassociated);
        let unassociated_xmp = sidecar.then(|| file(&sidecar_path(&unassociated)));
        let old_physical = physical();
        let path_proofs = rows(path_sql);
        assert_eq!(path_proofs.len(), 4);
        let checkpoint = rows(checkpoint_sql);
        let debt_sql = "SELECT library,id,version_size,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size FROM assets ORDER BY library,id,version_size";
        let catalogue_debt = rows(debt_sql);
        let recover = async || {
            let (config, pass, provider) =
                context(root.path(), current.clone(), sidecar, "legacy").await;
            assert_eq!(
                config.primary_layout_pass.as_deref(),
                Some(saved_pass.as_str())
            );
            let report = download::recover_recorded_sources(
                &reqwest::Client::new(),
                std::slice::from_ref(&pass),
                Arc::new(config),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            (report, provider.lookups.load(Ordering::SeqCst))
        };
        let (op, protected) = {
            let (config, pass, _) = context(root.path(), current.clone(), sidecar, "legacy").await;
            let (capture, scope, _) = pass
                .album
                .publication_scope("PrimarySync")
                .unwrap()
                .unwrap();
            let db = capture.db;
            let (account, provider) = capture.owner.authenticated_keys().unwrap();
            let pass_key = config.primary_layout_pass.as_deref().unwrap();
            assert_eq!(pass_key, saved_pass);
            assert!(
                rows("SELECT * FROM primary_layout_bindings").is_empty(),
                "a binding would bypass legacy per-path discovery"
            );
            assert!(rows("SELECT * FROM primary_layout_operations").is_empty());
            let receipts =
                state::db::DownloadContextStateStore::get_primary_layout_receipts(db.as_ref())
                    .await
                    .unwrap();
            assert_eq!(receipts.len(), 4);
            for (path, version, bytes) in [
                (&original, "original", ORIGINAL),
                (&old_heic, "adjusted", heic.as_slice()),
                (&jpeg, "adjusted", EDIT_TWO),
                (&alias, "adjusted", EDIT_TWO),
            ] {
                let receipt = receipts
                    .iter()
                    .find(|r| r.local_path.as_ref() == Some(path))
                    .unwrap();
                let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
                assert_eq!(receipt.library, "PrimarySync");
                assert_eq!(receipt.id, "asset-501");
                assert_eq!(receipt.version_size.as_str(), version);
                assert_eq!(receipt.checksum, provider_hash(bytes));
                assert_eq!(receipt.local_checksum.as_deref(), Some(sha.as_str()));
                assert_eq!(
                    receipt.download_checksum.as_deref(),
                    if path == &original {
                        Some(sha.as_str())
                    } else {
                        None
                    }
                );
                assert!(!receipt.is_preserved_history);
                assert_eq!(
                    path.parent(),
                    Some(if path == &original {
                        media.as_path()
                    } else {
                        legacy.as_path()
                    })
                );
                let conn = db
                    .acquire_lock("inspect genuine scoped publication versus synthetic alias")
                    .unwrap();
                let count: i64 = conn.query_row("SELECT count(*) FROM download_receipts r JOIN account_owner a ON a.account_key=r.account_key AND a.provider_key=r.provider_key WHERE r.account_key=?1 AND r.provider_key=?2 AND r.scope=?3 AND r.origin_pass=?4 AND r.library='PrimarySync' AND r.child='asset-501' AND r.master='501' AND r.state_id='asset-501' AND r.version_size=?5 AND r.provider_checksum=?6 AND r.provider_size=?7 AND r.native_path=?8 AND r.source_checksum=?9 AND r.local_checksum=?9 AND length(r.id)>0 AND length(r.proof_hash)=64 AND r.trust_receipt IS NULL AND r.selected_metadata_hash IS NULL AND r.generation_id IS NULL", rusqlite::params![account,provider,scope,pass_key,version,provider_hash(bytes),i64::try_from(bytes.len()).unwrap(),SelectionPath::from_path(path).key().unwrap(),sha], |r|r.get(0)).unwrap();
                assert_eq!(
                    count,
                    i64::from(path == &original),
                    "all three Adjusted paths are synthetic catalogue-only inputs"
                );
                let provenance:(Option<String>,Option<String>)=conn.query_row("SELECT source_checksum,download_checksum FROM asset_metadata_paths WHERE local_path=?1",[path.to_str().unwrap()],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                assert_eq!(
                    provenance,
                    if path == &original {
                        (Some(sha.clone()), Some(sha))
                    } else {
                        (None, None)
                    },
                    "actual synthetic provenance must not be source-stamped"
                );
            }
            assert!(
                !receipts
                    .iter()
                    .any(|r| r.local_path.as_ref() == Some(&unassociated))
            );
            eprintln!(
                "LEGACY_ALIAS_B_PREPARE_ENTER sidecar={sidecar} capture_policy={:?} metadata={:?} refresh={} no_prior_binding=true scoped19={authentic:?} paths={path_proofs:?} files={old_physical:?} legacy_receipts={receipts:?}",
                config.capture_timestamp_repair, config.metadata, config.refresh_metadata
            );
            let protected = config
                .state_db
                .as_ref()
                .unwrap()
                .protected_legacy_paths()
                .await
                .unwrap();
            assert!(protected.contains(&original));
            assert!(protected.contains(&sidecar_path(&original)));
            (
                download::seed_prepared_edited_compat(&config, &pass).await,
                protected,
            )
        };
        eprintln!("LEGACY_ALIAS_B_PREPARED sidecar={sidecar} operation={op:?}");
        assert_eq!(op.phase, "prepared");
        assert_eq!(op.generation, 0);
        assert_eq!(op.policy, "primary");
        assert!(!op.refresh_metadata);
        assert_eq!(op.members.len(), 4);
        assert_eq!(
            serde_json::to_value(operation()).unwrap(),
            serde_json::to_value(&op).unwrap()
        );
        let mut preservation_paths = std::collections::BTreeSet::new();
        let mut retirement_paths = std::collections::BTreeSet::new();
        let mut prepared_files = Vec::new();
        for member in &op.members {
            assert!(member.preserved.is_none() && member.installed.is_none());
            if let Some(old) = &member.old {
                check_file(old, &old.path.to_path());
                assert_eq!(old.sidecar.is_some(), sidecar);
                assert!(!old.archive_original);
                assert!(old.source_checksum.is_none());
                assert!(
                    preservation_paths.insert(member.preserved_path.as_ref().unwrap().to_path())
                );
                let retirement = member.retirement.as_ref().unwrap();
                assert!(retirement_paths.insert(retirement.to_path()));
                assert!(member.destination.is_none() && member.record.is_none());
                assert!(
                    [old_heic.clone(), jpeg.clone(), alias.clone()].contains(&old.path.to_path())
                );
            } else {
                assert_eq!(member.destination.as_ref().unwrap().to_path(), destination);
                assert!(member.retirement.is_none() && member.preserved_path.is_none());
                assert!(!destination.exists());
                assert_eq!(
                    member.record.as_ref().unwrap().version_size,
                    state::VersionSizeKey::Original
                );
            }
            for path in [
                member.destination.as_ref(),
                member.old.as_ref().map(|old| &old.path),
                member.preserved_path.as_ref(),
                member.retirement.as_ref(),
                member.stage.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                for candidate in [path.to_path(), sidecar_path(&path.to_path())] {
                    let key = crate::fs_util::confined_path_key(&candidate).unwrap();
                    assert!(
                        protected
                            .iter()
                            .all(|p| crate::fs_util::confined_path_key(p).unwrap() != key),
                        "recorded path overlaps protected source: {candidate:?}"
                    );
                }
            }
            if let Some(prepared) = &member.prepared {
                check_file(prepared, &prepared.path.to_path());
                prepared_files.push((prepared.path.to_path(), file(&prepared.path.to_path())));
                if sidecar {
                    prepared_files.push((
                        sidecar_path(&prepared.path.to_path()),
                        file(&sidecar_path(&prepared.path.to_path())),
                    ));
                }
            }
        }
        assert_eq!((preservation_paths.len(), retirement_paths.len()), (3, 3));
        assert!(!op.members.iter().any(|m| {
            m.old
                .as_ref()
                .is_some_and(|f| f.path.to_path() == unassociated)
        }));
        for path in [&old_heic, &jpeg, &alias] {
            assert_eq!(
                op.members
                    .iter()
                    .filter(|m| m.old.as_ref().is_some_and(|f| f.path.to_path() == *path))
                    .count(),
                1
            );
        }
        let jpeg_member = op
            .members
            .iter()
            .find(|m| m.old.as_ref().is_some_and(|old| old.path.to_path() == jpeg))
            .unwrap();
        let alias_index = op
            .members
            .iter()
            .position(|m| {
                m.old
                    .as_ref()
                    .is_some_and(|old| old.path.to_path() == alias)
            })
            .unwrap();
        let alias_member = &op.members[alias_index];
        let jpeg_old = jpeg_member.old.as_ref().unwrap();
        let alias_old = alias_member.old.as_ref().unwrap();
        assert_eq!(jpeg_old.version, alias_old.version);
        assert_eq!(jpeg_old.provider_checksum, alias_old.provider_checksum);
        assert_eq!(jpeg_old.fingerprint.sha256, alias_old.fingerprint.sha256);
        assert_ne!(jpeg_old.path, alias_old.path);
        assert_ne!(
            jpeg_old.fingerprint.identity,
            alias_old.fingerprint.identity
        );
        assert_ne!(jpeg_member.preserved_path, alias_member.preserved_path);
        assert_ne!(jpeg_member.retirement, alias_member.retirement);
        if sidecar {
            assert_eq!(
                jpeg_old.sidecar.as_ref().unwrap().sha256,
                alias_old.sidecar.as_ref().unwrap().sha256
            );
            assert_ne!(
                jpeg_old.sidecar.as_ref().unwrap().identity,
                alias_old.sidecar.as_ref().unwrap().identity
            );
        }
        assert_eq!(rows(&source_receipt_sql), authentic);
        let claims = rows("SELECT * FROM primary_layout_claims ORDER BY path_key");
        assert_eq!(claims.len(), 4);
        let original_before = file(&original);
        assert_source();
        eprintln!(
            "LEGACY_ALIAS_B_PROTECTED_SOURCE sidecar={sidecar} source={original:?} marker={marker} path_row={source_path:?} scoped19={authentic:?} source_file={source_file:?} source_xmp={source_xmp:?} actual_pass={saved_pass} protected={protected:?} operation={op:?}"
        );
        // Park the real inode outside every discovered/recorded path. No hardlink
        // or rewritten journal fingerprint participates in this negative.
        let backup = root.path().join("fixture-original-alias.backup");
        let replacement_archive = root.path().join("fixture-equal-byte-replacement.retained");
        std::fs::rename(&alias, &backup).unwrap();
        std::fs::copy(&jpeg, &alias).unwrap();
        let replacement = file(&alias);
        assert_eq!(replacement.0, EDIT_TWO);
        assert_ne!(replacement.1, alias_old.fingerprint.identity);
        assert_eq!(file(&backup).1, alias_old.fingerprint.identity);
        let events_before = capture.events().len();
        eprintln!(
            "LEGACY_ALIAS_B_MUTATION_ENTER sidecar={sidecar} operation={} parked={backup:?} replacement={replacement:?} paths={:?}",
            op.operation,
            rows(path_sql)
        );
        let (held, _) = recover().await;
        let errors: Vec<_> = capture
            .events()
            .into_iter()
            .skip(events_before)
            .filter(|e| e.field("error").is_some())
            .collect();
        eprintln!(
            "LEGACY_ALIAS_B_MUTATION_HOLD sidecar={sidecar} result={held:?} errors={errors:?} operation={:?}",
            operation()
        );
        assert!(
            held.stats.failed == 1 && !held.complete && !held.complete_after_smart_query,
            "{held:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.field("error") == Some("Owned layout file changed locally")),
            "{errors:?}"
        );
        assert_eq!(held.stats.downloaded, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        let partial = operation();
        assert_eq!(partial.operation, op.operation);
        assert_eq!(partial.phase, "prepared");
        assert!(partial.members.iter().all(|m| m.installed.is_none()));
        assert!(partial.members[alias_index].preserved.is_none());
        assert_eq!(
            rows("SELECT * FROM primary_layout_preserved").len(),
            partial
                .members
                .iter()
                .filter(|m| m.preserved.is_some())
                .count()
        );
        assert_eq!(rows("SELECT * FROM primary_layout_bindings").len(), 0);
        assert_eq!(
            rows("SELECT * FROM primary_layout_claims ORDER BY path_key"),
            claims
        );
        assert_eq!(rows(&source_receipt_sql), authentic);
        assert_eq!(rows(checkpoint_sql), checkpoint);
        assert_eq!(file(&original), original_before);
        assert_source();
        assert!(!destination.exists());
        assert_eq!(rows(path_sql), path_proofs);
        assert_eq!(rows(debt_sql), catalogue_debt);
        assert_eq!(file(&alias), replacement);
        for (path, (bytes, identity, modified)) in &old_physical {
            if let Some(bytes) = bytes
                && path != &alias
            {
                assert_eq!(file(path), (bytes.clone(), *identity, *modified));
            }
        }
        for (index, member) in partial.members.iter().enumerate() {
            assert_eq!(
                serde_json::to_value(&member.old).unwrap(),
                serde_json::to_value(&op.members[index].old).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&member.prepared).unwrap(),
                serde_json::to_value(&op.members[index].prepared).unwrap()
            );
            if let Some(preserved) = &member.preserved {
                check_file(preserved, &preserved.path.to_path());
                let old = member.old.as_ref().unwrap();
                assert_eq!(preserved.fingerprint.sha256, old.fingerprint.sha256);
                assert_ne!(preserved.fingerprint.identity, old.fingerprint.identity);
                eprintln!(
                    "LEGACY_ALIAS_B_PARTIAL_PRESERVATION member={index} proof={preserved:?} retirement_not_started=true"
                );
            }
            if let Some(retired) = &member.retirement {
                assert!(!retired.to_path().exists());
            }
        }
        for (path, snapshot) in &prepared_files {
            assert_eq!(&file(path), snapshot);
        }
        assert_eq!(file(&unassociated), unassociated_file);
        if let Some(xmp) = &unassociated_xmp {
            assert_eq!(&file(&sidecar_path(&unassociated)), xmp);
        }
        // Restore only the real inode/name. Retain the replacement separately;
        // never rewrite a frozen fingerprint, source checksum or saved claim.
        std::fs::rename(&alias, &replacement_archive).unwrap();
        std::fs::rename(&backup, &alias).unwrap();
        check_file(alias_old, &alias);
        assert_eq!(file(&replacement_archive), replacement);
        // Inject one test-only phase interruption to observe ALL durable per-path
        // preservation receipts while every old inode is still at its old name.
        // Recovery must resume the same operation after this barrier is removed.
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.execute_batch("CREATE TRIGGER legacy_alias_preservation_barrier BEFORE UPDATE OF phase ON primary_layout_operations WHEN NEW.phase='publishing' BEGIN SELECT RAISE(FAIL,'legacy alias preservation barrier'); END").unwrap();
        drop(conn);
        let events_before = capture.events().len();
        let (preserved_hold, _) = recover().await;
        assert!(
            preserved_hold.stats.failed == 1 && !preserved_hold.complete,
            "{preserved_hold:?}"
        );
        let barrier_errors: Vec<_> = capture
            .events()
            .into_iter()
            .skip(events_before)
            .filter(|event| event.field("error").is_some())
            .collect();
        assert!(
            barrier_errors.iter().any(|event| event
                .field("error")
                .is_some_and(|error| error.contains("legacy alias preservation barrier"))),
            "unexpected refusal, not the injected preservation barrier: {barrier_errors:?}"
        );
        let preserved = operation();
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        eprintln!(
            "LEGACY_ALIAS_B_PRESERVED_BEFORE_RETIREMENT sidecar={sidecar} operation={preserved:?}"
        );
        assert_eq!(preserved.operation, op.operation);
        assert_eq!(preserved.phase, "preserved");
        assert_eq!(rows("SELECT * FROM primary_layout_preserved").len(), 3);
        assert!(rows("SELECT * FROM primary_layout_bindings").is_empty());
        assert_eq!(rows(&source_receipt_sql), authentic);
        assert_eq!(rows(checkpoint_sql), checkpoint);
        assert_source();
        assert_eq!(rows(path_sql), path_proofs);
        assert_eq!(rows(debt_sql), catalogue_debt);
        assert!(!destination.exists());
        for member in &preserved.members {
            let Some(old) = &member.old else {
                assert!(member.preserved.is_none() && member.installed.is_none());
                continue;
            };
            let receipt = member.preserved.as_ref().unwrap();
            check_file(old, &old.path.to_path());
            check_file(receipt, &receipt.path.to_path());
            assert_eq!(file(&old.path.to_path()).0, file(&receipt.path.to_path()).0);
            assert_eq!(receipt.provider_checksum, old.provider_checksum);
            assert_eq!(receipt.source_checksum, old.source_checksum);
            assert_eq!(receipt.fingerprint.sha256, old.fingerprint.sha256);
            assert_ne!(receipt.fingerprint.identity, old.fingerprint.identity);
            if sidecar {
                assert_eq!(
                    file(&sidecar_path(&old.path.to_path())).0,
                    file(&sidecar_path(&receipt.path.to_path())).0
                );
            }
            assert!(member.installed.is_none());
            if let Some(retired) = &member.retirement {
                assert!(!retired.to_path().exists());
            }
        }
        for (path, (bytes, identity, modified)) in &old_physical {
            if let Some(bytes) = bytes {
                assert_eq!(file(path), (bytes.clone(), *identity, *modified));
            }
        }
        for (path, snapshot) in &prepared_files {
            assert_eq!(&file(path), snapshot);
        }
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.execute_batch("DROP TRIGGER legacy_alias_preservation_barrier")
            .unwrap();
        drop(conn);
        let (recovered, _) = recover().await;
        eprintln!(
            "LEGACY_ALIAS_B_RECOVERED sidecar={sidecar} result={recovered:?} operation={:?}",
            operation()
        );
        assert_eq!(
            (
                recovered.stats.failed,
                recovered.stats.downloaded,
                recovered.stats.skipped.uncertain,
                recovered.stats.state_write_failures
            ),
            (0, 0, 0, 0),
            "{recovered:?}"
        );
        assert!(recovered.complete);
        let committed = operation();
        let committed_paths = rows(path_sql);
        eprintln!(
            "LEGACY_ALIAS_B_DEBT_COMMIT sidecar={sidecar} operation={} flags={} prepared_decisions={:?} full_path_proofs_before={path_proofs:?} full_path_proofs_after={committed_paths:?}",
            op.operation,
            op.metadata_flags,
            op.members
                .iter()
                .map(|m| m.prepared.as_ref().map(|f| (&f.path, &f.metadata_decision)))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            committed_paths.len(),
            path_proofs.len() + 1,
            "only the new Original destination may add a path row"
        );
        for old in &path_proofs {
            assert!(
                committed_paths.contains(old),
                "STOP: recorded commit changed old path debt/provenance: {old:?}; actual={committed_paths:?}"
            );
        }
        assert_eq!(
            rows(debt_sql),
            catalogue_debt,
            "STOP: recorded commit changed independent catalogue debt"
        );
        assert_source();
        assert_eq!(committed.operation, op.operation);
        // Commit advances the SQL phase, not the frozen publishing header.
        // Inspect both independently; never overlay status on the raw header.
        assert_eq!(committed.phase, "publishing");
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let (sql_phase, raw_header): (String, Vec<u8>) = conn.query_row(
            "SELECT phase,header FROM primary_layout_operations WHERE operation=?1 AND family=?2 AND library=?3 AND source_library=?4",
            rusqlite::params![op.operation,op.family,op.library,op.source_library],
            |row| Ok((row.get(0)?,row.get(1)?)),
        ).unwrap();
        assert_eq!(sql_phase, "committed");
        let mut expected_publishing_header = op.clone();
        expected_publishing_header.members.clear();
        expected_publishing_header.phase = "publishing".into();
        assert_eq!(
            raw_header,
            serde_json::to_vec(&expected_publishing_header).unwrap()
        );
        drop(conn);
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let binding = db
            .primary_layout_binding(op.family.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.generation, op.generation + 1);
        assert_eq!(
            (
                &binding.family,
                &binding.library,
                &binding.source_library,
                &binding.child,
                &binding.decision,
                &binding.policy,
                &binding.qualifier
            ),
            (
                &op.family,
                &op.library,
                &op.source_library,
                &op.child,
                &op.decision,
                &op.policy,
                &op.qualifier
            )
        );
        assert!(
            db.primary_layout_operations(op.source_library.clone())
                .await
                .unwrap()
                .is_empty()
        );
        eprintln!(
            "LEGACY_ALIAS_B_AUTHORITATIVE_PHASE sidecar={sidecar} operation={} sql_phase={sql_phase} raw_header_phase={} raw_header_hex={} binding_generation={} binding={binding:?} normal_pending_reader=empty",
            op.operation,
            committed.phase,
            data_encoding::HEXLOWER.encode(&raw_header),
            binding.generation
        );
        drop(db);
        assert_eq!(rows("SELECT * FROM primary_layout_bindings").len(), 1);
        assert_eq!(rows("SELECT * FROM primary_layout_operations").len(), 1);
        for (index, member) in committed.members.iter().enumerate() {
            let Some(old) = &member.old else {
                assert!(member.preserved.is_none() && member.retirement.is_none());
                let installed = member.installed.as_ref().unwrap();
                assert_eq!(installed.path.to_path(), destination);
                check_file(installed, &destination);
                assert_bytes(&destination, ORIGINAL);
                assert_ne!(installed.fingerprint.identity, source_file.1);
                assert_eq!(
                    installed.fingerprint,
                    member.prepared.as_ref().unwrap().fingerprint
                );
                eprintln!(
                    "LEGACY_ALIAS_B_ORIGINAL_COPY source={original:?} installed={installed:?} protected_source_unchanged=true"
                );
                continue;
            };
            let receipt = member.preserved.as_ref().unwrap();
            assert_eq!(
                serde_json::to_value(receipt).unwrap(),
                serde_json::to_value(preserved.members[index].preserved.as_ref().unwrap()).unwrap()
            );
            check_file(receipt, &receipt.path.to_path());
            if let Some(retired) = &member.retirement {
                check_file(old, &retired.to_path());
                let (bytes, identity, modified) = &old_physical[&old.path.to_path()];
                assert_eq!(
                    file(&retired.to_path()),
                    (bytes.clone().unwrap(), *identity, *modified)
                );
                if sidecar {
                    let (bytes, identity, modified) =
                        &old_physical[&sidecar_path(&old.path.to_path())];
                    assert_eq!(
                        file(&sidecar_path(&retired.to_path())),
                        (bytes.clone().unwrap(), *identity, *modified)
                    );
                }
                assert!(!old.path.to_path().exists());
                assert!(!sidecar_path(&old.path.to_path()).exists());
                eprintln!(
                    "LEGACY_ALIAS_B_DISTINCT_SLOT member={index} old={old:?} preservation={receipt:?} retirement={retired:?}"
                );
            }
        }
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let histories = state::db::DownloadContextStateStore::get_primary_layout_receipts(&db)
            .await
            .unwrap();
        for member in &committed.members {
            let Some(preserved) = &member.preserved else {
                continue;
            };
            let receipt = histories
                .iter()
                .find(|r| {
                    r.local_path.as_ref() == Some(&preserved.path.to_path())
                        && r.is_preserved_history
                })
                .unwrap();
            assert!(!receipt.is_current_path);
            assert_eq!(receipt.library, "PrimarySync");
            assert_eq!(receipt.id, "asset-501");
            assert_eq!(receipt.version_size.as_str(), preserved.version);
            assert_eq!(receipt.checksum, preserved.provider_checksum);
            assert_eq!(receipt.download_checksum, preserved.source_checksum);
            assert_eq!(
                receipt.local_checksum.as_deref(),
                Some(
                    data_encoding::HEXLOWER
                        .encode(&preserved.fingerprint.sha256)
                        .as_str()
                )
            );
        }
        assert_eq!(
            histories.iter().filter(|r| r.is_preserved_history).count(),
            3
        );
        let current_original = histories
            .iter()
            .find(|r| r.local_path.as_ref() == Some(&destination))
            .unwrap();
        assert!(current_original.is_current_path && !current_original.is_preserved_history);
        assert_eq!(
            current_original.version_size,
            state::VersionSizeKey::Original
        );
        assert_eq!(current_original.checksum, provider_hash(ORIGINAL));
        assert_eq!(
            current_original.local_checksum.as_deref(),
            Some(
                data_encoding::HEXLOWER
                    .encode(&Sha256::digest(ORIGINAL))
                    .as_str()
            )
        );
        let retained_source = histories
            .iter()
            .find(|r| r.local_path.as_ref() == Some(&original))
            .unwrap();
        assert!(!retained_source.is_current_path && !retained_source.is_preserved_history);
        drop(db);
        assert_eq!(file(&unassociated), unassociated_file);
        if let Some(xmp) = &unassociated_xmp {
            assert_eq!(&file(&sidecar_path(&unassociated)), xmp);
        }
        assert_eq!(file(&replacement_archive), replacement);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        for (endpoint, count) in [("/original", 1), ("/edit-heic", 0), ("/edit-two", 0)] {
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r.method == "GET" && r.url.path() == endpoint)
                    .count(),
                count
            );
        }
        let quiet_files = physical();
        let quiet_receipts = rows(receipt_sql);
        let quiet_paths = rows(path_sql);
        let custody = || {
            [
                rows("SELECT * FROM primary_layout_operations ORDER BY operation"),
                rows("SELECT * FROM primary_layout_members ORDER BY operation,member"),
                rows("SELECT * FROM primary_layout_bindings ORDER BY family"),
                rows("SELECT * FROM primary_layout_claims ORDER BY path_key"),
                rows("SELECT * FROM primary_layout_preserved ORDER BY operation,member"),
                rows(
                    "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,provider_checksum,native_path",
                ),
            ]
        };
        let quiet_custody = custody();
        let quiet_checkpoint = rows(checkpoint_sql);
        for round in 0..2 {
            let (quiet, lookups) = recover().await;
            assert_eq!(
                (
                    quiet.stats.failed,
                    quiet.stats.downloaded,
                    quiet.stats.skipped.uncertain,
                    quiet.stats.state_write_failures
                ),
                (0, 0, 0, 0),
                "{quiet:?}"
            );
            assert!(quiet.complete);
            assert_eq!(lookups, 0);
            assert_eq!(physical(), quiet_files);
            assert_eq!(rows(receipt_sql), quiet_receipts);
            assert_eq!(rows(path_sql), quiet_paths);
            assert_eq!(custody(), quiet_custody);
            assert_eq!(rows(checkpoint_sql), quiet_checkpoint);
            assert_eq!(file(&replacement_archive), replacement);
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            assert_source();
            assert_eq!(rows(debt_sql), catalogue_debt);
            let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
                .await
                .unwrap();
            let actual = state::db::DownloadContextStateStore::get_primary_layout_receipts(&db)
                .await
                .unwrap();
            assert_eq!(format!("{actual:?}"), format!("{histories:?}"));
            eprintln!(
                "LEGACY_ALIAS_B_QUIET sidecar={sidecar} round={round} operation={} files={} history=3 distinct_retirement_slots=3 full_scoped19={} path_proofs={} checkpoint={quiet_checkpoint:?} GETs=1 lookups=0 protected_source_unassociated_and_replacement_unchanged=true",
                op.operation,
                quiet_files.len(),
                quiet_receipts.len(),
                quiet_paths.len()
            );
        }
    }
}

#[tokio::test]
async fn review501_retained_source_mutation_during_compatibility_preparation_holds_publication() {
    use rusqlite::types::Value as SqlValue;

    let fixture_metadata = |config: download::DownloadConfig| {
        #[cfg(feature = "xmp")]
        let config = {
            let mut config = config;
            config.metadata.xmp_sidecar = true;
            config
        };
        config
    };

    for replace_inode in [false, true] {
        let server = server().await;
        let mut root = tempfile::tempdir().unwrap();
        if std::env::var_os("KEI_RETAINED_SOURCE_COMPAT_RETAIN_FIXTURE").is_some() {
            root.disable_cleanup(true);
        }
        eprintln!(
            "RETAINED_COMPAT root={} replacement={replace_inode}",
            root.path().display()
        );
        let media = root.path().join("media");
        let visible = media.join("IMG_0501.JPG");
        let original = media.join("IMG_0501_original.JPG");
        let rows = |sql: &str| {
            let conn = rusqlite::Connection::open_with_flags(
                root.path().join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut statement = conn.prepare(sql).unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| row.get::<_, SqlValue>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let custody = || {
            [
                "SELECT * FROM primary_layout_operations ORDER BY operation",
                "SELECT * FROM primary_layout_members ORDER BY operation,member",
                "SELECT * FROM primary_layout_claims ORDER BY path_key",
                "SELECT * FROM primary_layout_bindings ORDER BY family",
                "SELECT * FROM primary_layout_preserved ORDER BY operation,member",
                "SELECT * FROM primary_layout_superseded_paths ORDER BY library,child,version,provider_checksum,native_path",
                "SELECT * FROM owned_temp_files ORDER BY path",
            ].map(rows)
        };
        let proofs = || {
            [
                "SELECT * FROM assets ORDER BY library,id,version_size",
                "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
                "SELECT * FROM asset_metadata_capture_revisions ORDER BY rowid",
                "SELECT * FROM download_receipts ORDER BY id",
                "SELECT * FROM metadata ORDER BY key",
            ]
            .map(rows)
        };
        let file = |path: &std::path::Path| {
            let handle = std::fs::File::open(path).unwrap();
            assert_eq!(crate::fs_util::file_link_count(&handle).unwrap(), 1);
            (
                std::fs::read(path).unwrap(),
                crate::fs_util::file_identity(&handle).unwrap(),
                handle.metadata().unwrap().modified().unwrap(),
            )
        };
        let tree = || {
            let mut files = std::collections::BTreeMap::new();
            let mut directories = vec![media.clone()];
            while let Some(directory) = directories.pop() {
                for entry in std::fs::read_dir(directory).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_dir() {
                        directories.push(entry.path());
                    } else {
                        assert!(entry.file_type().unwrap().is_file());
                        files.insert(entry.path(), file(&entry.path()));
                    }
                }
            }
            files
        };
        let (initial, _) = cycle_options(
            root.path(),
            records(&server, None),
            false,
            EditedNaming::Primary,
            CycleOptions {
                private: true,
                baseline_successor: Some("retained-original"),
                cycle_token: Some("retained-original"),
                #[cfg(feature = "xmp")]
                sidecar: true,
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert!(initial.db_sync_token_advance_safe);
        assert_eq!(initial.stats.downloaded, 1);
        let first = records(&server, Some(("edit-one", EDIT_ONE)));
        let current = Arc::new(std::sync::Mutex::new(first.clone()));
        let lookups = Arc::new(AtomicUsize::new(0));
        let mut committed = Vec::new();
        for selected in [
            first.clone(),
            records(&server, Some(("edit-two", EDIT_TWO))),
        ] {
            *current.lock().unwrap() = selected;
            let (config, pass) =
                source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
            let config = fixture_metadata(config);
            let mut op = download::plan_edited_compat(&config, &pass).await.unwrap();
            assert_eq!(op.pass, config.primary_layout_pass.as_deref().unwrap());
            assert!(!op.refresh_metadata);
            eprintln!(
                "RETAINED_COMPAT baseline preparing operation={} generation={} phase={} pass={}",
                op.operation, op.generation, op.phase, op.pass
            );
            download::prepare_edited_compat(&config, &pass, &mut op)
                .await
                .unwrap();
            download::finish_prepared_edited_compat(&config, &mut op).await;
            assert_eq!(op.phase, "committed");
            eprintln!(
                "RETAINED_COMPAT baseline operation={} generation={} phase={}",
                op.operation, op.generation, op.phase
            );
            committed.push(op);
        }
        assert_bytes(&visible, EDIT_TWO);
        assert_bytes(&original, ORIGINAL);
        let retained = committed[1]
            .members
            .iter()
            .filter_map(|m| m.preserved.as_ref())
            .find(|f| f.provider_checksum == provider_hash(EDIT_ONE))
            .unwrap()
            .clone();
        let history = retained.path.to_path();
        let original_history = file(&history);
        assert_eq!(original_history.0, EDIT_ONE);
        assert_eq!(retained.fingerprint.identity, original_history.1);
        assert_eq!(retained.fingerprint.size, EDIT_ONE.len() as u64);
        assert_eq!(
            retained.fingerprint.sha256.as_slice(),
            Sha256::digest(EDIT_ONE).as_slice()
        );
        assert_eq!(
            retained.source_checksum.as_deref(),
            Some(
                data_encoding::HEXLOWER
                    .encode(&Sha256::digest(EDIT_ONE))
                    .as_str()
            )
        );
        #[cfg(feature = "xmp")]
        assert!(retained.sidecar.is_some());
        let requests = server.received_requests().await.unwrap().len();
        assert_eq!(requests, 3);
        *current.lock().unwrap() = first;
        let mut op = {
            let (config, pass) =
                source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
            let config = fixture_metadata(config);
            let op = download::plan_edited_compat(&config, &pass).await.unwrap();
            assert_eq!(op.generation, committed[1].generation + 1);
            assert_eq!(op.phase, "planned");
            assert!(
                op.members
                    .iter()
                    .filter_map(|m| m.old.as_ref())
                    .any(|f| f.provider_checksum == provider_hash(EDIT_TWO))
            );
            assert!(
                op.members
                    .iter()
                    .filter_map(|m| m.old.as_ref())
                    .all(|f| f.provider_checksum != provider_hash(EDIT_ONE))
            );
            assert!(
                op.members
                    .iter()
                    .filter_map(|m| m.record.as_ref())
                    .any(|r| r.checksum.as_ref() == provider_hash(EDIT_ONE))
            );
            op
        };
        // Planning legitimately changes catalogue observations. Freeze proofs only now.
        let before_proofs = proofs();
        let before_custody = custody();
        let before_tree = tree();
        let planned = op.clone();
        let evidence = root.path().join("mutation-evidence");
        std::fs::create_dir(&evidence).unwrap();
        let parked = evidence.join("original-inode");
        let changed = b"unrelated local bytes introduced between source snapshots";
        let hook = if replace_inode {
            crate::test_helpers::primary_source_replacement_with_parked_original(
                history.clone(),
                changed.to_vec(),
                parked.clone(),
            )
        } else {
            crate::test_helpers::primary_source_mutation(history.clone(), changed.to_vec(), false)
        };
        let error = {
            let (config, pass) =
                source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
            let config = fixture_metadata(config);
            eprintln!(
                "RETAINED_COMPAT mutation preparing operation={} generation={} phase={} source={}",
                op.operation,
                op.generation,
                op.phase,
                history.display()
            );
            download::prepare_edited_compat(&config, &pass, &mut op)
                .await
                .unwrap_err()
        };
        assert!(hook.fired(), "receipt fallback mutation hook not reached");
        assert_eq!(
            error.to_string(),
            "Retained primary source changed while proving its receipt"
        );
        eprintln!(
            "RETAINED_COMPAT refusal operation={} generation={} phase={} hook={} error={error:#}",
            op.operation,
            op.generation,
            op.phase,
            hook.fired()
        );
        assert_eq!(op.phase, "planned");
        assert_eq!(proofs(), before_proofs);
        // The earlier Original member may already have a durable private stage.
        // Only real preparation receipts can account for those additional files.
        let mut expected = planned;
        let mut partial_stages = 0;
        let mut changed_tree = before_tree.clone();
        for (before, after) in expected.members.iter_mut().zip(&op.members) {
            if let Some(prepared) = &after.prepared {
                partial_stages += 1;
                assert_eq!(prepared.version, "original");
                assert_eq!(before.stage.as_ref(), Some(&prepared.path));
                let stage = prepared.path.to_path();
                assert_bytes(&stage, ORIGINAL);
                let snapshot = file(&stage);
                assert_eq!(snapshot.1, prepared.fingerprint.identity);
                assert_eq!(prepared.fingerprint.size, ORIGINAL.len() as u64);
                assert_eq!(
                    prepared.fingerprint.sha256.as_slice(),
                    Sha256::digest(ORIGINAL).as_slice()
                );
                changed_tree.insert(stage.clone(), snapshot);
                if let Some(sidecar) = &prepared.sidecar {
                    let mut path = stage.as_os_str().to_os_string();
                    path.push(".xmp");
                    let path = std::path::PathBuf::from(path);
                    let packet = file(&path);
                    assert_eq!(packet.1, sidecar.identity);
                    assert_eq!(packet.0.len() as u64, sidecar.size);
                    assert_eq!(
                        Sha256::digest(&packet.0).as_slice(),
                        sidecar.sha256.as_slice()
                    );
                    changed_tree.insert(path, packet);
                }
                before.prepared = Some(prepared.clone());
            }
        }
        assert_eq!(partial_stages, 1);
        assert_eq!(
            serde_json::to_value(expected).unwrap(),
            serde_json::to_value(&op).unwrap()
        );
        let held_custody = custody();
        for index in [0, 2, 3, 4, 5, 6] {
            assert_eq!(held_custody[index], before_custody[index]);
        }
        let other_members = |snapshot: &[Vec<SqlValue>]| {
            snapshot
                .iter()
                .filter(|row| row[0] != SqlValue::Text(op.operation.clone()))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            other_members(&held_custody[1]),
            other_members(&before_custody[1])
        );
        changed_tree.insert(history.clone(), file(&history));
        assert_eq!(
            tree(),
            changed_tree,
            "no unaccounted partial stage or publication"
        );
        assert_bytes(&history, changed);
        assert_bytes(&visible, EDIT_TWO);
        if replace_inode {
            assert_eq!(file(&parked), original_history);
            assert_ne!(file(&history).1, original_history.1);
        } else {
            assert_eq!(file(&history).1, original_history.1);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);

        // Normal recorded recovery must not perform compatibility preparation.
        {
            let (config, pass) =
                source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
            let config = fixture_metadata(config);
            let unused = crate::test_helpers::primary_source_mutation(
                history.clone(),
                b"must not run".to_vec(),
                false,
            );
            let held = download::recover_recorded_sources(
                &reqwest::Client::new(),
                std::slice::from_ref(&pass),
                Arc::new(config.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(!held.complete && !held.complete_after_smart_query);
            assert_eq!(held.stats.downloaded, 0);
            assert!(!unused.fired());
            let pending = config
                .state_db
                .as_ref()
                .unwrap()
                .primary_layout_operations("PrimarySync".into())
                .await
                .unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(
                serde_json::to_value(&pending[0]).unwrap(),
                serde_json::to_value(&op).unwrap()
            );
            eprintln!(
                "RETAINED_COMPAT normal reopened recovery held operation={} phase={} automatic_hook=false",
                op.operation, op.phase
            );
        }
        assert_eq!(proofs(), before_proofs);
        assert_eq!(custody(), held_custody);
        assert_eq!(tree(), changed_tree);
        let changed_evidence = evidence.join("changed-source");
        assert!(!changed_evidence.try_exists().unwrap());
        if replace_inode {
            std::fs::rename(&history, &changed_evidence).unwrap();
            std::fs::rename(&parked, &history).unwrap();
            assert_eq!(file(&history), original_history);
        } else {
            std::fs::copy(&history, &changed_evidence).unwrap();
            std::fs::write(&history, EDIT_ONE).unwrap();
            let restored = file(&history);
            assert_eq!(restored.0, original_history.0);
            assert_eq!(restored.1, original_history.1);
            eprintln!(
                "RETAINED_COMPAT in-place restored mtime={:?} previous={:?}",
                restored.2, original_history.2
            );
        }
        assert_bytes(&changed_evidence, changed);
        eprintln!(
            "RETAINED_COMPAT restored replacement={replace_inode} original_identity={:?} restored_identity={:?} changed_identity={:?} evidence={}",
            original_history.1,
            file(&history).1,
            file(&changed_evidence).1,
            changed_evidence.display()
        );
        {
            let (config, pass) =
                source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
            let config = fixture_metadata(config);
            let saved = config
                .state_db
                .as_ref()
                .unwrap()
                .primary_layout_operations("PrimarySync".into())
                .await
                .unwrap();
            op = saved.into_iter().next().unwrap();
            let operation = op.operation.clone();
            download::prepare_edited_compat(&config, &pass, &mut op)
                .await
                .unwrap();
            assert_eq!(op.operation, operation);
            assert_eq!(op.phase, "prepared");
            let recovered = download::recover_recorded_sources(
                &reqwest::Client::new(),
                std::slice::from_ref(&pass),
                Arc::new(config.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(recovered.complete);
            assert_eq!(recovered.stats.failed, 0);
            assert_eq!(recovered.stats.downloaded, 0);
            assert!(
                config
                    .state_db
                    .as_ref()
                    .unwrap()
                    .primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap()
                    .is_empty()
            );
            eprintln!(
                "RETAINED_COMPAT same-operation reprepare+normal-finish operation={operation}"
            );
        }
        assert_bytes(&visible, EDIT_ONE);
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&history, EDIT_ONE);
        assert_eq!(
            rows("SELECT generation FROM primary_layout_bindings"),
            vec![vec![SqlValue::Integer(op.generation + 1)]]
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='committed'"
            ),
            3
        );
        let final_members = custody()[1]
            .iter()
            .filter(|row| matches!(row.first(), Some(SqlValue::Text(operation)) if operation == &op.operation))
            .map(|row| {
                let [
                    SqlValue::Text(_),
                    SqlValue::Integer(_),
                    SqlValue::Blob(bytes),
                ] = row.as_slice()
                else {
                    panic!("unexpected durable layout member shape")
                };
                serde_json::from_slice::<state::db::primary_layout::LayoutMember>(bytes).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(final_members.len(), 2);
        for member in &final_members {
            for (receipt, adjusted) in [
                (member.installed.as_ref().unwrap(), EDIT_ONE),
                (member.preserved.as_ref().unwrap(), EDIT_TWO),
            ] {
                let expected = if receipt.version == "original" {
                    ORIGINAL
                } else {
                    adjusted
                };
                let snapshot = file(&receipt.path.to_path());
                assert_eq!(snapshot.0, expected);
                assert_eq!(snapshot.1, receipt.fingerprint.identity);
                assert_eq!(receipt.fingerprint.size, expected.len() as u64);
                assert_eq!(
                    receipt.fingerprint.sha256.as_slice(),
                    Sha256::digest(expected).as_slice()
                );
                assert_eq!(receipt.provider_checksum, provider_hash(expected));
                assert_eq!(
                    receipt.source_checksum.as_deref(),
                    Some(
                        data_encoding::HEXLOWER
                            .encode(&Sha256::digest(expected))
                            .as_str()
                    )
                );
                assert_eq!(receipt.sidecar.is_some(), cfg!(feature = "xmp"));
                if let Some(sidecar) = &receipt.sidecar {
                    let mut path = receipt.path.to_path().into_os_string();
                    path.push(".xmp");
                    let packet = file(std::path::Path::new(&path));
                    assert!(!packet.0.is_empty());
                    assert_eq!(packet.1, sidecar.identity);
                    assert_eq!(packet.0.len() as u64, sidecar.size);
                    assert_eq!(
                        Sha256::digest(&packet.0).as_slice(),
                        sidecar.sha256.as_slice()
                    );
                }
            }
        }
        let quiet_proofs = proofs();
        let quiet_custody = custody();
        let quiet_tree = tree();
        for quiet in 0..2 {
            let (config, pass) =
                source_mutation_layout_context(root.path(), current.clone(), lookups.clone()).await;
            let config = fixture_metadata(config);
            let recovered = download::recover_recorded_sources(
                &reqwest::Client::new(),
                std::slice::from_ref(&pass),
                Arc::new(config.clone()),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(recovered.complete);
            assert_eq!(recovered.stats.failed, 0);
            assert_eq!(recovered.stats.downloaded, 0);
            drop(pass);
            drop(config);
            assert_eq!(proofs(), quiet_proofs);
            assert_eq!(custody(), quiet_custody);
            assert_eq!(tree(), quiet_tree);
            assert_bytes(&changed_evidence, changed);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            eprintln!(
                "RETAINED_COMPAT quiet reopen={quiet} operation={} GETs={requests}",
                op.operation
            );
        }
        assert_eq!(
            rows("SELECT value FROM metadata WHERE key='sync_token:PrimarySync'"),
            vec![vec![SqlValue::Text("retained-original".into())]],
            "recovery-only cannot advance source checkpoints"
        );
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn review501_suffix_album_collecting_delta_uses_managed_metadata_owner() {
    use crate::state::db::ManifestAssetRow;
    use std::path::{Path, PathBuf};
    let recorded_path = |manifest: &[ManifestAssetRow], version: &str, bytes: &[u8]| {
        let checksum = provider_hash(bytes);
        let mut rows = manifest.iter().filter(|row| {
            row.library == "PrimarySync"
                && row.asset_id == "asset-501"
                && row.version == version
                && row.checksum == checksum
                && row.status == "downloaded"
        });
        let row = rows
            .next()
            .expect("completed exact resource must be in the manifest");
        assert!(
            rows.next().is_none(),
            "current resource must have one catalog row"
        );
        row.local_path
            .clone()
            .expect("current resource must retain its recorded path")
    };
    let sidecar = |path: &Path| {
        let mut path = path.as_os_str().to_os_string();
        path.push(".xmp");
        PathBuf::from(path)
    };
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
    let root = tempfile::tempdir().unwrap();
    let catalog = || {
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut stmt = conn.prepare("SELECT json_object('library',a.library,'id',a.id,'version',a.version_size,'provider_checksum',a.checksum,'native_path',a.local_path,'source_sha',p.source_checksum,'local_sha',p.local_checksum,'catalog_local_sha',a.local_checksum,'path_download_sha',p.download_checksum,'catalog_download_sha',a.download_checksum,'catalog_marker',a.metadata_write_failed_at,'path_marker',p.metadata_write_failed_at,'catalog_capture_hash',a.capture_repair_metadata_hash,'path_capture_hash',p.capture_repair_metadata_hash,'catalog_output_sha',a.capture_repair_output_checksum,'path_output_sha',p.capture_repair_output_checksum,'catalog_output_size',a.capture_repair_output_size,'path_output_size',p.capture_repair_output_size,'title',a.title,'metadata_hash',a.metadata_hash) FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.provider_checksum=a.checksum AND p.local_path=a.local_path WHERE a.library='PrimarySync' AND a.id='asset-501' AND a.version_size IN ('original','adjusted') AND a.status='downloaded' AND a.is_deleted=0 ORDER BY a.version_size").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| serde_json::from_str::<Value>(&r.unwrap()).unwrap())
            .collect::<Vec<_>>()
    };
    let pending = async || {
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let mut obligations = std::collections::BTreeMap::new();
        for row in db.get_pending_metadata_rewrites(usize::MAX).await.unwrap() {
            let key = (
                row.library.to_string(),
                row.id.to_string(),
                row.version_size.as_str().to_owned(),
                row.checksum.to_string(),
                row.local_path
                    .expect("rewrite obligation must name its path"),
            );
            let proof = (row.local_checksum, row.download_checksum);
            assert!(obligations.insert(key, proof).is_none(), "duplicate debt");
        }
        obligations
    };
    let metadata = |records: &[Value]| {
        download::filter::metadata_capture(&crate::icloud::photos::PhotoAsset::new(
            records[0].clone(),
            records[1].clone(),
        ))
    };
    let snap = |path: &Path| {
        let metadata = std::fs::metadata(path).unwrap();
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
        #[cfg(not(unix))]
        let inode = 0_u64;
        (
            std::fs::read(path).unwrap(),
            metadata.modified().unwrap(),
            inode,
        )
    };
    let options = || CycleOptions {
        pass_kind: PassKind::Album,
        sidecar: true,
        // No private capture context: selected album deltas use collecting routing.
        private: false,
        ..CycleOptions::default()
    };
    let current = records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
    for (naming, quiet) in [(EditedNaming::Primary, false), (EditedNaming::Suffix, true)] {
        let (result, _) =
            cycle_options(root.path(), current.clone(), quiet, naming, options()).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
    }
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let manifest = db.get_manifest_assets().await.unwrap();
    let original_path = recorded_path(&manifest, "original", VALID_ORIGINAL);
    let adjusted_path = recorded_path(&manifest, "adjusted", VALID_EDIT);
    assert_ne!(original_path, adjusted_path);
    assert_bytes(&original_path, VALID_ORIGINAL);
    assert_bytes(&adjusted_path, VALID_EDIT);
    // Album membership creates retained retry markers even after successful initial writes.
    let initial_debt = pending().await;
    assert_eq!(initial_debt.len(), 2);
    drop(db);
    let adjusted_sidecar = sidecar(&adjusted_path);
    let original_sidecar = sidecar(&original_path);
    let old_xmp = std::fs::read(&adjusted_sidecar).unwrap();
    let original_xmp = std::fs::read(&original_sidecar).unwrap();
    let paths = [
        &original_path,
        &adjusted_path,
        &original_sidecar,
        &adjusted_sidecar,
    ];
    let untouched = paths.map(|path| snap(path));
    let before = catalog();
    assert_eq!(before.len(), 2);
    let initial_capture = metadata(&current);
    for (version, bytes, path) in [
        ("original", VALID_ORIGINAL, &original_path),
        ("adjusted", VALID_EDIT, &adjusted_path),
    ] {
        let row = before.iter().find(|row| row["version"] == version).unwrap();
        assert_eq!(row["library"], "PrimarySync");
        assert_eq!(row["id"], "asset-501");
        assert_eq!(row["provider_checksum"], provider_hash(bytes));
        assert_eq!(row["native_path"], path.to_str().unwrap());
        let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        assert_eq!(row["source_sha"], sha);
        assert_eq!(row["local_sha"], sha);
        assert_eq!(row["catalog_local_sha"], sha);
        assert_eq!(row["path_download_sha"], sha);
        assert_eq!(row["catalog_download_sha"], sha);
        let key = (
            "PrimarySync".to_owned(),
            "asset-501".to_owned(),
            version.to_owned(),
            provider_hash(bytes),
            path.clone(),
        );
        assert_eq!(
            initial_debt.get(&key),
            Some(&(Some(sha.clone()), Some(sha)))
        );
        let marker = row["catalog_marker"].as_i64().unwrap();
        assert!(marker > 0);
        assert_eq!(row["path_marker"], row["catalog_marker"]);
        for field in [
            "catalog_capture_hash",
            "path_capture_hash",
            "catalog_output_sha",
            "path_output_sha",
            "catalog_output_size",
            "path_output_size",
        ] {
            assert!(row[field].is_null(), "unexpected prepared repair: {row:?}");
        }
        let resolved = initial_capture.resolve(
            state::VersionSizeKey::from_str(version).unwrap(),
            &provider_hash(bytes),
        );
        assert_eq!(row["title"], json!(resolved.title));
        assert_eq!(row["metadata_hash"], json!(resolved.metadata_hash));
    }
    let requests = server.received_requests().await.unwrap().len();
    let mut changed = current;
    changed[1]["recordChangeTag"] = json!("ct-asset-caption");
    changed[1]["fields"]["captionEnc"] =
        json!({"type":"STRING","value":"caption changed after suffix reversal"});
    let changed_capture = metadata(&changed);
    let mut expected = before.clone();
    for row in &mut expected {
        let version = state::VersionSizeKey::from_str(row["version"].as_str().unwrap()).unwrap();
        let resolved = changed_capture.resolve(version, row["provider_checksum"].as_str().unwrap());
        assert_eq!(
            resolved.title.as_deref(),
            Some("caption changed after suffix reversal")
        );
        let hash = json!(resolved.metadata_hash);
        assert_ne!(row["metadata_hash"], hash);
        row["metadata_hash"] = hash;
        row["title"] = json!(resolved.title);
    }
    let (result, _) = cycle_options(
        root.path(),
        changed.clone(),
        false,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert_eq!(result.failed_count, 0, "{:?}", result.stats);
    assert!(result.db_sync_token_advance_safe);
    assert_eq!(result.stats.downloaded, 0);
    assert!(result.can_advance_database_checkpoint());
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert_bytes(&original_path, VALID_ORIGINAL);
    assert_bytes(&adjusted_path, VALID_EDIT);
    // Catalog capture is not consent to rewrite either existing packet or media file.
    assert_bytes(&adjusted_sidecar, &old_xmp);
    assert_bytes(&original_sidecar, &original_xmp);
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        pending().await,
        initial_debt,
        "caption changed rewrite obligations"
    );
    let manifest = db.get_manifest_assets().await.unwrap();
    assert_eq!(
        recorded_path(&manifest, "original", VALID_ORIGINAL),
        original_path
    );
    assert_eq!(
        recorded_path(&manifest, "adjusted", VALID_EDIT),
        adjusted_path
    );
    assert_eq!(catalog(), expected, "{:?}", capture.events());
    for (path, before) in paths.iter().zip(&untouched) {
        assert_eq!(snap(path), *before);
    }
    drop(db);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let title: String = conn.query_row("SELECT title FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'", [], |row| row.get(0)).unwrap();
    assert_eq!(title, "caption changed after suffix reversal");
    drop(conn);
    for _ in 0..2 {
        let (quiet, lookups) = cycle_options(
            root.path(),
            changed.clone(),
            true,
            EditedNaming::Suffix,
            options(),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert!(quiet.can_advance_database_checkpoint());
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            pending().await,
            initial_debt,
            "quiet reopen changed rewrite obligations"
        );
        let manifest = db.get_manifest_assets().await.unwrap();
        assert_eq!(
            recorded_path(&manifest, "original", VALID_ORIGINAL),
            original_path
        );
        assert_eq!(
            recorded_path(&manifest, "adjusted", VALID_EDIT),
            adjusted_path
        );
        assert_bytes(&original_path, VALID_ORIGINAL);
        assert_bytes(&adjusted_path, VALID_EDIT);
        assert_bytes(&adjusted_sidecar, &old_xmp);
        assert_bytes(&original_sidecar, &original_xmp);
        assert_eq!(catalog(), expected);
        for (path, before) in paths.iter().zip(&untouched) {
            assert_eq!(snap(path), *before);
        }
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
async fn recorded_authorized_metadata_refresh_recovers_without_repeating_flag() {
    use crate::state::db::primary_layout::{LayoutFile, LayoutFingerprint};

    // Real supported provider verification, local to this positive fixture.
    fn provider_hash(bytes: &[u8]) -> String {
        use sha1::{Digest as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        base64::engine::general_purpose::STANDARD.encode([&[1][..], &hash.finalize()[..]].concat())
    }

    fn assert_file(path: &std::path::Path, proof: &LayoutFingerprint) {
        let file = std::fs::File::open(path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len() as u64, proof.size);
        assert_eq!(Sha256::digest(&bytes).as_slice(), proof.sha256.as_slice());
        assert_eq!(
            crate::fs_util::file_identity(&file).unwrap(),
            proof.identity
        );
    }
    fn xmp_path(path: &std::path::Path) -> std::path::PathBuf {
        let mut path = path.as_os_str().to_os_string();
        path.push(".xmp");
        path.into()
    }
    fn assert_receipt(file: &LayoutFile) {
        let path = file.path.to_path();
        assert_file(&path, &file.fingerprint);
        if let Some(proof) = &file.sidecar {
            assert_file(&xmp_path(&path), proof);
        } else {
            assert!(!xmp_path(&path).try_exists().unwrap());
        }
        let source = match file.version.as_str() {
            "original" => VALID_ORIGINAL,
            "adjusted" => VALID_EDIT,
            version => panic!("unexpected fixture version: {version}"),
        };
        assert_eq!(file.provider_checksum, provider_hash(source));
        assert_eq!(
            file.source_checksum,
            Some(format!("{:x}", Sha256::digest(source)))
        );
    }
    fn tree(
        root: &std::path::Path,
    ) -> Vec<(
        std::path::PathBuf,
        Option<Vec<u8>>,
        String,
        std::time::SystemTime,
    )> {
        fn walk(
            root: &std::path::Path,
            dir: &std::path::Path,
            out: &mut Vec<(
                std::path::PathBuf,
                Option<Vec<u8>>,
                String,
                std::time::SystemTime,
            )>,
        ) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                assert!(!meta.file_type().is_symlink());
                let (bytes, identity) = if meta.is_dir() {
                    (None, String::new())
                } else {
                    assert!(meta.is_file());
                    let file = std::fs::File::open(&path).unwrap();
                    (
                        Some(std::fs::read(&path).unwrap()),
                        format!("{:?}", crate::fs_util::file_identity(&file).unwrap()),
                    )
                };
                out.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    bytes,
                    identity,
                    meta.modified().unwrap(),
                ));
                if meta.is_dir() {
                    walk(root, &path, out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
    fn rows(root: &std::path::Path) -> Vec<(String, Vec<String>)> {
        let conn = rusqlite::Connection::open_with_flags(
            root.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        [
            "account_owner",
            "assets",
            "asset_metadata_paths",
            "asset_albums",
            "primary_layout_operations",
            "primary_layout_members",
            "primary_layout_claims",
            "primary_layout_bindings",
            "primary_layout_preserved",
            "primary_layout_superseded_paths",
            "download_receipts",
        ]
        .iter()
        .map(|table| {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let count = stmt.column_count();
            let mut values = stmt
                .query_map([], |row| {
                    (0..count)
                        .map(|i| {
                            row.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|v| v.join("|"))
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            values.sort();
            ((*table).to_owned(), values)
        })
        .collect()
    }

    async fn snapshot(
        root: &std::path::Path,
        db: &state::SqliteStateDb,
        operation: &crate::state::db::primary_layout::LayoutOperation,
        phase: &str,
    ) -> Value {
        let durable = {
            let conn = rusqlite::Connection::open_with_flags(
                root.join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut tables = serde_json::Map::new();
            for (table, order) in [
                ("assets", "library,id,version_size"),
                ("asset_metadata_paths", "library,id,version_size,local_path"),
                ("asset_albums", "library,asset_id,album_name"),
                ("primary_layout_operations", "operation"),
                ("primary_layout_members", "operation,member"),
                ("primary_layout_bindings", "family"),
                ("primary_layout_preserved", "operation,member"),
                ("primary_layout_claims", "path_key"),
                ("download_receipts", "id"),
            ] {
                let mut stmt = conn
                    .prepare(&format!("SELECT * FROM {table} ORDER BY {order}"))
                    .unwrap();
                let names: Vec<_> = stmt.column_names().into_iter().map(str::to_owned).collect();
                let values = stmt.query_map([], |row| {
                    let mut fields = serde_json::Map::new();
                    for (index, name) in names.iter().enumerate() {
                        let value = match row.get_ref(index)? {
                            rusqlite::types::ValueRef::Null => Value::Null,
                            rusqlite::types::ValueRef::Integer(n) => json!(n),
                            rusqlite::types::ValueRef::Real(n) => json!(n),
                            rusqlite::types::ValueRef::Text(s) => json!(std::str::from_utf8(s).unwrap()),
                            rusqlite::types::ValueRef::Blob(b) => json!({"hex":data_encoding::HEXLOWER.encode(b),"json":serde_json::from_slice::<Value>(b).ok()}),
                        };
                        fields.insert(name.clone(), value);
                    }
                    Ok(Value::Object(fields))
                }).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
                tables.insert(table.to_owned(), json!(values));
            }
            tables
        };
        let queue = db.get_pending_metadata_rewrites_page_for_queue(
            state::db::MetadataRewriteQueue::Ordinary,None,0,usize::MAX,
        ).await.unwrap().into_iter().map(|q|json!({"asset":q.asset,"source":q.source_checksum,"capture":q.capture_repair_receipt.map(|r|format!("{r:?}")),"selection":q.selection_receipt.map(|r|format!("{r:?}"))})).collect::<Vec<_>>();
        let packet = json!({"phase":phase,"operation":operation,"durable":durable,"queue":queue,"files":tree(&root.join("media")),"checkpoint":db.get_metadata("sync_token:PrimarySync").await.unwrap()});
        std::fs::write(
            root.join(format!("refresh-{phase}.json")),
            serde_json::to_vec_pretty(&packet).unwrap(),
        )
        .unwrap();
        packet
    }

    #[cfg(feature = "xmp")]
    let modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let modes = [false];
    for sidecar in modes {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let mut root = tempfile::tempdir().unwrap();
        if std::env::var_os("KEI_RECORDED_REFRESH_RETAIN_FIXTURE").is_some() {
            root.disable_cleanup(true);
        }
        eprintln!(
            "RECORDED_REFRESH_FIXTURE sidecar={sidecar} root={}",
            root.path().display()
        );
        let media = root.path().join("media");
        // This fixture's actual selection is Unfiled, with TestAlbum grouping.
        let options = || CycleOptions {
            private: true,
            #[cfg(feature = "xmp")]
            sidecar,
            ..CycleOptions::default()
        };
        let records_for = |edited: bool| {
            let mut values = records_with_original(
                &server,
                VALID_ORIGINAL,
                edited.then_some(("edit-one", VALID_EDIT)),
            );
            values[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(provider_hash(VALID_ORIGINAL));
            if edited {
                values[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                    json!(provider_hash(VALID_EDIT));
            }
            values
        };
        let mut current = records_for(true);
        let (initial, _) = cycle_options(
            root.path(),
            records_for(false),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert!(initial.can_advance_database_checkpoint());
        assert_eq!(initial.stats.downloaded, 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        eprintln!("RECORDED_REFRESH_BASELINE sidecar={sidecar} Original_only=true media_GETs=1");
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
                .await
                .unwrap(),
        );
        let provider_records = Arc::new(std::sync::Mutex::new(current.clone()));
        let provider = Session {
            reported_count: None,
            records: current.clone(),
            quiet: true,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: Some(provider_records.clone()),
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner.clone(), "com"),
            Arc::from("private"),
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store);
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = sidecar;
        }
        let expected_pass = config.with_pass(&pass).primary_layout_pass.unwrap();
        eprintln!("RECORDED_REFRESH_LEGACY_PREPARE_BEGIN sidecar={sidecar} actual_Unfiled=true");
        let mut family = download::seed_prepared_edited_compat(&config, &pass).await;
        eprintln!(
            "RECORDED_REFRESH_LEGACY_PREPARED sidecar={sidecar} operation={} phase={}",
            family.operation, family.phase
        );
        download::finish_prepared_edited_compat(&config, &mut family).await;
        eprintln!(
            "RECORDED_REFRESH_LEGACY_COMMITTED sidecar={sidecar} operation={} total_media_GETs={}",
            family.operation,
            server.received_requests().await.unwrap().len()
        );
        assert_eq!(family.phase, "committed");
        let binding = db
            .primary_layout_binding(family.family.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(binding.generation, family.generation + 1);
        assert_eq!(binding.files.len(), 2);
        for file in &binding.files {
            assert_receipt(file);
        }
        let previous_xmp = sidecar.then(|| std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap());
        let old_files = tree(&media);
        let requests = server.received_requests().await.unwrap().len();

        snapshot(root.path(), &db, &family, "before-plan").await;

        // Authentic provider metadata is decoded by confirm_layout_asset in the
        // compatibility owner. Capture those exact records through upsert_seen,
        // just as seed_prepared_edited_compat does. No hash or proof is invented.
        current[1]["recordChangeTag"] = json!("recorded-refresh-favorite");
        current[1]["fields"]["isFavorite"] = json!({"value":1});
        *provider_records.lock().unwrap() = current.clone();
        config.refresh_metadata = true;
        eprintln!("RECORDED_REFRESH_UNPREPARED_BEGIN sidecar={sidecar}");
        let operation = download::seed_unprepared_refresh(&config, &pass).await;
        for record in operation.members.iter().filter_map(|m| m.record.as_ref()) {
            db.upsert_seen(record).await.unwrap();
        }
        assert_eq!(operation.phase, "planned");
        assert!(operation.refresh_metadata);
        assert_eq!(operation.pass, expected_pass);
        assert_eq!(operation.family, binding.family);
        assert_eq!(operation.generation, binding.generation);
        assert_eq!(
            operation.root.to_path(),
            std::fs::canonicalize(&media).unwrap()
        );
        assert_eq!(operation.policy, "primary");
        // MetadataFlags::XMP_SIDECAR is private to download; mirror only its
        // recorded bit, not a new encoder or metadata-policy adapter.
        const XMP_SIDECAR_FLAG: u8 = 1 << 5;
        assert_eq!(
            operation.metadata_flags,
            if sidecar { XMP_SIDECAR_FLAG } else { 0 }
        );
        assert!(operation.members.iter().any(|m| m.old.is_some()));
        assert!(operation.members.iter().all(|m| m.prepared.is_none()));
        let stages: Vec<_> = operation
            .members
            .iter()
            .filter_map(|m| m.stage.as_ref())
            .map(|p| p.to_path())
            .collect();
        assert!(!stages.is_empty());
        for stage in &stages {
            assert!(!stage.try_exists().unwrap());
            assert!(!xmp_path(stage).try_exists().unwrap());
        }
        let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
        assert!(checkpoint.is_some());
        let protected_rows = rows(root.path());
        let planned = snapshot(root.path(), &db, &operation, "planned").await;
        assert_eq!(tree(&media), old_files);
        eprintln!(
            "RECORDED_REFRESH_PLANNED sidecar={sidecar} operation={} stages={} all_absent=true",
            operation.operation,
            stages.len()
        );
        // Install only after seeding. Same-phase planned UPDATE at recover:2094
        // fails before prepare_recorded_refresh creates any media/XMP stages.
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.execute_batch(&format!("CREATE TRIGGER fail_recorded_refresh_planned BEFORE UPDATE OF phase ON primary_layout_operations WHEN OLD.operation='{}' AND NEW.operation=OLD.operation AND OLD.phase='planned' AND NEW.phase='planned' BEGIN SELECT RAISE(FAIL,'injected recorded refresh pre-stage save failure'); END", operation.operation)).unwrap();
        drop(conn);
        drop(config);
        drop(build);
        drop(pass);
        drop(db);
        let held = try_cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await
        .expect_err("the actual pre-stage planned save must fail");
        assert!(
            matches!(held.downcast_ref::<state::error::StateError>(), Some(state::error::StateError::Migration(rusqlite::Error::SqliteFailure(error, Some(message)))) if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER && message == "injected recorded refresh pre-stage save failure"),
            "{held:#}"
        );
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            checkpoint
        );
        let pending = db
            .primary_layout_operations("PrimarySync".into())
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            serde_json::to_value(&pending[0]).unwrap(),
            serde_json::to_value(&operation).unwrap()
        );
        assert_eq!(
            rows(root.path()),
            protected_rows,
            "operation/header/members, catalogue, receipts, paths and claims changed"
        );
        assert_eq!(
            tree(&media),
            old_files,
            "pre-stage failure changed physical custody"
        );
        for stage in &stages {
            assert!(!stage.try_exists().unwrap());
            assert!(!xmp_path(stage).try_exists().unwrap());
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        eprintln!(
            "RECORDED_REFRESH_FAULT_PROVEN sidecar={sidecar} same_operation=true typed_trigger=true stages_absent=true custody_checkpoint_unchanged=true"
        );
        let before_recovery = snapshot(root.path(), &db, &operation, "after-fault").await;
        assert_eq!(before_recovery["durable"], planned["durable"]);
        assert_eq!(before_recovery["queue"], planned["queue"]);
        drop(db);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.execute_batch("DROP TRIGGER fail_recorded_refresh_planned")
            .unwrap();
        drop(conn);

        // Reopen the same owned state, without repeating current authorization.
        let recovery_start = chrono::Utc::now().timestamp();
        let (refreshed, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await;
        let recovery_end = chrono::Utc::now().timestamp();
        assert_eq!(refreshed.failed_count, 0, "{:?}", refreshed.stats);
        assert!(refreshed.can_advance_database_checkpoint());
        assert_eq!(refreshed.stats.downloaded, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let recovered = snapshot(root.path(), &db, &operation, "after-recovery").await;
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        let fresh_binding = db
            .primary_layout_binding(operation.family.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh_binding.generation, operation.generation + 1);
        assert_eq!(fresh_binding.decision, operation.decision);
        assert_eq!(fresh_binding.files.len(), binding.files.len());
        for file in &fresh_binding.files {
            assert_receipt(file);
        }
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
        assert_bytes(&media.join("IMG_0501_original.JPG"), VALID_ORIGINAL);
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let phase: String = conn
            .query_row(
                "SELECT phase FROM primary_layout_operations WHERE operation=?1",
                [&operation.operation],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "committed");
        eprintln!(
            "RECORDED_REFRESH_RECOVERED sidecar={sidecar} same_operation={} no_current_flag=true no_GET=true",
            operation.operation
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            2
        );
        let committed_members: Vec<crate::state::db::primary_layout::LayoutMember> = conn
            .prepare(
                "SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member",
            )
            .unwrap()
            .query_map([&operation.operation], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|r| serde_json::from_slice(&r.unwrap()).unwrap())
            .collect();
        for (before, after) in operation.members.iter().zip(&committed_members) {
            if let Some(old) = &before.old {
                let preserved = after.preserved.as_ref().unwrap();
                assert_eq!(Some(&preserved.path), before.preserved_path.as_ref());
                assert_receipt(preserved);
                assert_eq!(preserved.fingerprint.sha256, old.fingerprint.sha256);
                assert_eq!(
                    preserved.sidecar.as_ref().map(|p| p.sha256),
                    old.sidecar.as_ref().map(|p| p.sha256)
                );
            }
        }
        assert_eq!(committed_members.len(), operation.members.len());
        for file in &fresh_binding.files {
            let (path, provider, local, source, hash): (String, String, String, String, Option<String>) = conn.query_row("SELECT local_path,checksum,local_checksum,download_checksum,metadata_hash FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size=?1 AND status='downloaded'", [&file.version], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
            assert_eq!(std::path::Path::new(&path), file.path.to_path());
            assert_eq!(provider, file.provider_checksum);
            assert_eq!(
                local,
                format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()))
            );
            assert_eq!(Some(&source), file.source_checksum.as_ref());
            let path_proofs: (String, String, String) = conn.query_row("SELECT provider_checksum,local_checksum,download_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size=?1 AND local_path=?2", rusqlite::params![file.version, path], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(path_proofs, (provider, local.clone(), source));
            let claim: (String, String, Option<String>) = conn.query_row("SELECT provider_checksum,local_checksum,operation FROM primary_layout_claims WHERE family=?1 AND path_key=?2", rusqlite::params![operation.family, crate::state::db::primary_layout::path_key(&file.path.to_path()).unwrap()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(claim, (file.provider_checksum.clone(), local, None));
            let record = operation
                .members
                .iter()
                .filter_map(|m| m.record.as_ref())
                .find(|r| r.version_size.as_str() == file.version)
                .unwrap();
            assert_eq!(hash, record.metadata.metadata_hash);
        }
        drop(conn);
        let mut expected_assets = before_recovery["durable"]["assets"].clone();
        let mut expected_paths = before_recovery["durable"]["asset_metadata_paths"].clone();
        let mut expected_queue = before_recovery["queue"].clone();
        let mut transitions = Vec::new();
        for member in &committed_members {
            let record = member.record.as_ref().unwrap();
            let prepared = member.prepared.as_ref().unwrap();
            let installed = member.installed.as_ref().unwrap();
            assert_eq!(prepared.fingerprint, installed.fingerprint);
            assert_eq!(prepared.sidecar, installed.sidecar);
            assert_eq!(prepared.metadata_decision, installed.metadata_decision);
            assert_eq!(prepared.source_checksum, installed.source_checksum);
            assert_eq!(member.destination.as_ref(), Some(&installed.path));
            let path = installed.path.to_path();
            let path = path.to_str().unwrap();
            let owns_asset = |row: &&mut Value| {
                row["library"] == record.library.as_ref()
                    && row["id"] == record.id.as_ref()
                    && row["version_size"] == record.version_size.as_str()
                    && row["local_path"] == path
                    && row["checksum"] == record.checksum.as_ref()
                    && row["metadata_hash"] == json!(record.metadata.metadata_hash)
            };
            let assets: Vec<_> = expected_assets
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .filter(owns_asset)
                .collect();
            assert_eq!(assets.len(), 1);
            let asset = assets.into_iter().next().unwrap();
            let actual: Vec<_> = recovered["durable"]["assets"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| {
                    row["library"] == asset["library"]
                        && row["id"] == asset["id"]
                        && row["version_size"] == asset["version_size"]
                })
                .collect();
            assert_eq!(actual.len(), 1);
            let actual = actual[0];
            for field in ["last_seen_at", "downloaded_at"] {
                let seconds = actual[field].as_i64().unwrap();
                assert!((recovery_start..=recovery_end).contains(&seconds));
                asset[field] = actual[field].clone();
            }
            let before_marker = asset["metadata_write_failed_at"].clone();
            if operation.metadata_flags != 0 {
                asset["metadata_write_failed_at"] = Value::Null;
            }
            let paths: Vec<_> = expected_paths
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .filter(|row| {
                    row["library"] == record.library.as_ref()
                        && row["id"] == record.id.as_ref()
                        && row["version_size"] == record.version_size.as_str()
                        && row["local_path"] == path
                        && row["provider_checksum"] == record.checksum.as_ref()
                })
                .collect();
            assert_eq!(paths.len(), 1);
            let path_row = paths.into_iter().next().unwrap();
            let before_path_marker = path_row["metadata_write_failed_at"].clone();
            if operation.metadata_flags != 0 {
                path_row["metadata_write_failed_at"] = Value::Null;
            }
            assert_eq!(
                path_row["source_checksum"],
                json!(installed.source_checksum)
            );
            assert_eq!(
                path_row["local_checksum"],
                format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
            );
            transitions.push(json!({"version":record.version_size.as_str(),"path":path,"asset_before":before_marker,"asset_after":asset["metadata_write_failed_at"],"path_before":before_path_marker,"path_after":path_row["metadata_write_failed_at"],"prepared":prepared,"installed":installed}));
        }
        // The entire cohort must be owned; an unknown resource/path is not an
        // excuse to clear it or silently exempt it from this control.
        for pending in expected_queue.as_array_mut().unwrap() {
            let asset = &mut pending["asset"];
            let matching: Vec<_> = committed_members
                .iter()
                .filter(|member| {
                    let record = member.record.as_ref().unwrap();
                    asset["library"] == record.library.as_ref()
                        && asset["id"] == record.id.as_ref()
                        && asset["version_size"] == json!(record.version_size)
                        && asset["local_path"]
                            == member
                                .destination
                                .as_ref()
                                .unwrap()
                                .to_path()
                                .to_str()
                                .unwrap()
                        && asset["checksum"] == record.checksum.as_ref()
                        && asset["metadata"]["metadata_hash"]
                            == json!(record.metadata.metadata_hash)
                })
                .collect();
            assert_eq!(matching.len(), 1, "unexpected queued peer or native path");
            let canonical: Vec<_> = recovered["durable"]["assets"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| {
                    row["library"] == asset["library"]
                        && row["id"] == asset["id"]
                        && row["version_size"]
                            .as_str()
                            .unwrap()
                            .eq_ignore_ascii_case(asset["version_size"].as_str().unwrap())
                })
                .collect();
            assert_eq!(canonical.len(), 1);
            for field in ["last_seen_at", "downloaded_at"] {
                asset[field] = json!(
                    chrono::DateTime::<chrono::Utc>::from_timestamp(
                        canonical[0][field].as_i64().unwrap(),
                        0
                    )
                    .unwrap()
                );
            }
        }
        assert_eq!(recovered["durable"]["assets"], expected_assets);
        assert_eq!(recovered["durable"]["asset_metadata_paths"], expected_paths);
        if operation.metadata_flags != 0 {
            expected_queue = json!([]);
        }
        assert_eq!(recovered["queue"], expected_queue);
        std::fs::write(root.path().join("refresh-settlement.json"),serde_json::to_vec_pretty(&json!({"operation":operation.operation,"SQL_phase":phase,"generation":fresh_binding.generation,"flags":operation.metadata_flags,"start_utc_seconds":recovery_start,"end_utc_seconds":recovery_end,"transitions":transitions,"queue":recovered["queue"],"all_other_asset_and_path_fields_exact":true})).unwrap()).unwrap();
        #[cfg(feature = "xmp")]
        if let Some(previous_xmp) = &previous_xmp {
            let fresh_xmp = std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap();
            assert_ne!(&fresh_xmp, previous_xmp);
            assert!(
                std::str::from_utf8(&fresh_xmp)
                    .unwrap()
                    .contains("Rating>5")
            );
            let fresh: xmp_toolkit::XmpMeta =
                std::str::from_utf8(&fresh_xmp).unwrap().parse().unwrap();
            let old: xmp_toolkit::XmpMeta =
                std::str::from_utf8(previous_xmp).unwrap().parse().unwrap();
            let expected_date = chrono::DateTime::from_timestamp_millis(
                crate::sync_loop::test_support::RUN_CYCLE_ASSET_DATE_MS,
            )
            .unwrap()
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
            for (namespace, key) in [
                (xmp_toolkit::xmp_ns::XMP, "CreateDate"),
                (xmp_toolkit::xmp_ns::EXIF, "DateTimeOriginal"),
            ] {
                assert_eq!(
                    fresh.property(namespace, key).unwrap().value,
                    old.property(namespace, key).unwrap().value
                );
                assert_eq!(fresh.property(namespace, key).unwrap().value, expected_date);
            }
            let keywords: Vec<_> = (1..)
                .map_while(|i| fresh.array_item(xmp_toolkit::xmp_ns::DC, "subject", i))
                .map(|item| item.value)
                .collect();
            assert!(keywords.iter().any(|value| value == "TestAlbum"));
            assert!(!keywords.iter().any(|value| value == "Unfiled"));
            assert!(
                committed_members
                    .iter()
                    .filter_map(|m| m.preserved.as_ref())
                    .any(|file| std::fs::read(xmp_path(&file.path.to_path()))
                        .is_ok_and(|bytes| bytes == *previous_xmp))
            );
        }
        #[cfg(not(feature = "xmp"))]
        let _ = previous_xmp;
        drop(db);
        let settled_tree = tree(&media);
        for prior in old_files.iter().filter(|entry| {
            entry.1.is_some()
                && entry
                    .0
                    .components()
                    .any(|c| c.as_os_str() == ".kei-history")
        }) {
            assert!(
                settled_tree.contains(prior),
                "pre-existing history changed: {:?}",
                prior.0
            );
        }
        let settled_rows = rows(root.path());
        for round in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Primary,
                options(),
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
            assert!(quiet.can_advance_database_checkpoint());
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(lookups, 0);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(
                tree(&media),
                settled_tree,
                "quiet reopen changed full physical census"
            );
            assert_eq!(
                rows(root.path()),
                settled_rows,
                "quiet reopen grew history or changed proofs/metadata"
            );
            let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
                .await
                .unwrap();
            let quiet = snapshot(root.path(), &db, &operation, &format!("quiet-{round}")).await;
            assert_eq!(quiet["queue"], recovered["queue"]);
            eprintln!(
                "RECORDED_REFRESH_QUIET sidecar={sidecar} physical_rows_stable=true downloads=0 lookups=0 new_GETs=0"
            );
            let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
                .await
                .unwrap();
            let owned_queue = db.get_pending_metadata_rewrites(usize::MAX).await.unwrap();
            let recovered_assets: Vec<_> = recovered["queue"]
                .as_array()
                .unwrap()
                .iter()
                .map(|pending| pending["asset"].clone())
                .collect();
            assert_eq!(
                serde_json::to_value(owned_queue).unwrap(),
                json!(recovered_assets)
            );
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
#[tokio::test]
async fn interrupted_unprepared_embedded_refresh_remains_held() {
    let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let mut current = records_with_original(&server, VALID_ORIGINAL, None);
    current[1]["fields"]["isFavorite"] = json!({"value":1});
    let options = || CycleOptions {
        embed_rating: true,
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        options(),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let original = media.join("IMG_0501.JPG");
    let old_bytes = std::fs::read(&original).unwrap();
    let provider = Session {
        reported_count: None,
        records: current.clone(),
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
        exclude_ids: Arc::default(),
    };
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let db_store: Arc<dyn download::DownloadStore> = db.clone();
    let base = make_run_cycle_download_config_builder(&media, db_store);
    let mut config = base(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    config.refresh_metadata = true;
    config.metadata.set_exif_rating = true;
    let operation = download::seed_unprepared_refresh(&config, &pass).await;
    let stage = operation
        .members
        .iter()
        .find_map(|m| m.stage.as_ref())
        .unwrap()
        .to_path();
    drop(config);
    drop(base);
    drop(db);
    let result =
        try_cycle_options(root.path(), current, true, EditedNaming::Primary, options()).await;
    if let Ok((result, _)) = result {
        assert!(!result.db_sync_token_advance_safe || result.failed_count > 0);
    }
    assert!(!stage.exists());
    assert_eq!(std::fs::read(&original).unwrap(), old_bytes);
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let pending = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]
            .members
            .iter()
            .any(|m| m.stage.is_some() && m.prepared.is_none())
    );
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
#[tokio::test]
async fn interrupted_unprepared_embedded_refresh_retries_on_ordinary_sync() {
    async fn snapshot(
        root: &std::path::Path,
        db: &state::SqliteStateDb,
        operation: &crate::state::db::primary_layout::LayoutOperation,
        phase: &str,
    ) -> Value {
        let durable = {
            let conn = rusqlite::Connection::open_with_flags(
                root.join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            let mut assets = conn.prepare("SELECT json_object('library',library,'id',id,'version',version_size,'path',local_path,'checksum',checksum,'size',size_bytes,'hash',metadata_hash,'rating',rating,'marker',metadata_write_failed_at,'local',local_checksum,'source',download_checksum,'capture_hash',capture_repair_metadata_hash,'capture_checksum',capture_repair_output_checksum,'capture_size',capture_repair_output_size) FROM assets ORDER BY library,id,version_size").unwrap();
            let assets = assets
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| serde_json::from_str::<Value>(&r.unwrap()).unwrap())
                .collect::<Vec<_>>();
            let mut paths = conn.prepare("SELECT json_object('library',library,'id',id,'version',version_size,'path',local_path,'checksum',provider_checksum,'local',local_checksum,'download',download_checksum,'source',source_checksum,'marker',metadata_write_failed_at,'capture_hash',capture_repair_metadata_hash,'capture_checksum',capture_repair_output_checksum,'capture_size',capture_repair_output_size,'preserved',preserved_path_key) FROM asset_metadata_paths ORDER BY library,id,version_size,local_path").unwrap();
            let paths = paths
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| serde_json::from_str::<Value>(&r.unwrap()).unwrap())
                .collect::<Vec<_>>();
            let (sql_phase, header): (String, Vec<u8>) = conn
                .query_row(
                    "SELECT phase,header FROM primary_layout_operations WHERE operation=?1",
                    [&operation.operation],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            let mut members = conn.prepare("SELECT evidence FROM primary_layout_members WHERE operation=?1 ORDER BY member").unwrap();
            let members = members
                .query_map([&operation.operation], |r| r.get::<_, Vec<u8>>(0))
                .unwrap()
                .map(|r| {
                    serde_json::from_slice::<crate::state::db::primary_layout::LayoutMember>(
                        &r.unwrap(),
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>();
            json!({"assets":assets,"paths":paths,"SQL_phase":sql_phase,"header":serde_json::from_slice::<Value>(&header).unwrap(),"members":members})
        };
        let mut files = serde_json::Map::new();
        let mut pending = vec![root.join("media")];
        while let Some(parent) = pending.pop() {
            for entry in std::fs::read_dir(parent).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    pending.push(entry.path());
                } else {
                    assert!(entry.file_type().unwrap().is_file());
                    let file = std::fs::File::open(entry.path()).unwrap();
                    files.insert(entry.path().to_str().unwrap().to_owned(),json!({"bytes":std::fs::read(entry.path()).unwrap(),"identity":crate::fs_util::file_identity(&file).unwrap(),"mtime":file.metadata().unwrap().modified().unwrap()}));
                }
            }
        }
        let queue=db.get_pending_metadata_rewrites_page_for_queue(state::db::MetadataRewriteQueue::Ordinary,None,0,usize::MAX).await.unwrap().into_iter().map(|q|json!({"asset":q.asset,"source":q.source_checksum,"capture":q.capture_repair_receipt.map(|r|format!("{r:?}")),"selection":q.selection_receipt.map(|r|format!("{r:?}"))})).collect::<Vec<_>>();
        let packet = json!({"phase":phase,"operation":operation.operation,"flags":operation.metadata_flags,"durable":durable,"binding":db.primary_layout_binding(operation.family.clone()).await.unwrap(),"queue":queue,"files":files});
        std::fs::write(
            root.join(format!("embedded-{phase}.json")),
            serde_json::to_vec_pretty(&packet).unwrap(),
        )
        .unwrap();
        packet
    }

    let cases: &[(&str, &[u8], bool)] = &[
        ("IMG_0501.JPG", VALID_ORIGINAL, false),
        #[cfg(feature = "xmp")]
        (
            "IMG_0501.HEIC",
            include_bytes!("../../../../tests/data/sample.heic"),
            true,
        ),
    ];
    for &(filename, original_bytes, is_heif) in cases {
        let server = server_with_media(original_bytes, VALID_EDIT, EDIT_TWO).await;
        let mut root = tempfile::tempdir().unwrap();
        if std::env::var_os("KEI_RECORDED_EMBED_RETAIN_FIXTURE").is_some() {
            root.disable_cleanup(true);
        }
        eprintln!(
            "RECORDED_EMBED_FIXTURE root={} filename={filename}",
            root.path().display()
        );
        let media = root.path().join("media");
        let mut current = records_with_original(&server, original_bytes, None);
        current[0]["fields"]["filenameEnc"]["value"] = json!(filename);
        if is_heif {
            current[0]["fields"]["itemType"] = json!({"value":"public.heic"});
            current[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
        }
        let (initial, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            CycleOptions::default(),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{filename}: {:?}", initial.stats);
        let original = media.join(filename);
        assert_eq!(std::fs::read(&original).unwrap(), original_bytes);
        let sidecar = media.join(format!("{filename}.xmp"));
        let old_sidecar = b"independent synthetic sidecar";
        std::fs::write(&sidecar, old_sidecar).unwrap();
        // Capture genuine provider rating drift through the same upsert owner;
        // do not manufacture retry markers or add unrelated grouping authority.
        current[1]["fields"]["isFavorite"] = json!({"value":1});
        current[1]["recordChangeTag"] = json!("recorded-embedded-favorite");
        let provider = Session {
            reported_count: None,
            records: current.clone(),
            quiet: true,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
            exclude_ids: Arc::default(),
        };
        let db = Arc::new(
            state::SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap(),
        );
        let db_store: Arc<dyn download::DownloadStore> = db.clone();
        let base = make_run_cycle_download_config_builder(&media, db_store);
        let mut config = base(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        config.refresh_metadata = true;
        config.metadata.set_exif_rating = true;
        let operation = download::seed_unprepared_refresh(&config, &pass).await;
        let before_upsert = snapshot(root.path(), &db, &operation, "before-upsert").await;
        for record in operation
            .members
            .iter()
            .filter_map(|member| member.record.as_ref())
        {
            db.upsert_seen(record).await.unwrap();
        }
        let before = snapshot(root.path(), &db, &operation, "before-recovery").await;
        assert_ne!(operation.metadata_flags, 0);
        assert!(
            operation
                .members
                .iter()
                .all(|member| member.prepared.is_none())
        );
        let stage = operation
            .members
            .iter()
            .find_map(|member| member.stage.as_ref())
            .unwrap()
            .to_path();
        assert!(!stage.exists());
        drop(config);
        drop(base);
        drop(db);
        let options = || CycleOptions {
            embed_rating: true,
            ..CycleOptions::default()
        };
        let (recovered, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await;
        let proof_db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let after = snapshot(root.path(), &proof_db, &operation, "after-recovery").await;
        drop(proof_db);
        assert_eq!(
            recovered.failed_count, 0,
            "{filename}: {:?}",
            recovered.stats
        );
        assert!(recovered.db_sync_token_advance_safe);
        assert_eq!(recovered.stats.downloaded, 0);
        assert_eq!(after["durable"]["SQL_phase"], "committed");
        assert_eq!(after["binding"]["generation"], operation.generation + 1);
        assert_eq!(after["binding"]["decision"], operation.decision);
        let mut transitions = Vec::new();
        for saved in after["durable"]["members"].as_array().unwrap() {
            let member: crate::state::db::primary_layout::LayoutMember =
                serde_json::from_value(saved.clone()).unwrap();
            let record = member.record.as_ref().unwrap();
            let prepared = member.prepared.as_ref().unwrap();
            let installed = member.installed.as_ref().unwrap();
            assert_eq!(member.destination.as_ref(), Some(&installed.path));
            assert_eq!(prepared.fingerprint, installed.fingerprint);
            assert_eq!(prepared.metadata_decision, installed.metadata_decision);
            assert_eq!(prepared.source_checksum, installed.source_checksum);
            let output = std::fs::read(installed.path.to_path()).unwrap();
            assert_eq!(
                installed.fingerprint.sha256.as_slice(),
                Sha256::digest(&output).as_slice()
            );
            assert_eq!(installed.fingerprint.size, output.len() as u64);
            assert_eq!(
                installed.fingerprint.identity,
                crate::fs_util::file_identity(
                    &std::fs::File::open(installed.path.to_path()).unwrap()
                )
                .unwrap()
            );
            assert_eq!(installed.provider_checksum, record.checksum.as_ref());
            assert_eq!(
                installed.source_checksum,
                Some(format!("{:x}", Sha256::digest(original_bytes)))
            );
            let matches = |row: &&Value| {
                row["library"] == record.library.as_ref()
                    && row["id"] == record.id.as_ref()
                    && row["version"] == record.version_size.as_str()
                    && row["path"] == installed.path.to_path().to_str().unwrap()
                    && row["checksum"] == record.checksum.as_ref()
            };
            let old: Vec<_> = before["durable"]["assets"]
                .as_array()
                .unwrap()
                .iter()
                .filter(matches)
                .collect();
            let new: Vec<_> = after["durable"]["assets"]
                .as_array()
                .unwrap()
                .iter()
                .filter(matches)
                .collect();
            assert_eq!(old.len(), 1);
            assert_eq!(new.len(), 1);
            assert_eq!(old[0]["hash"], json!(record.metadata.metadata_hash));
            assert_eq!(old[0]["rating"], 5);
            assert_eq!(new[0]["marker"], Value::Null);
            assert_eq!(new[0]["hash"], old[0]["hash"]);
            assert_eq!(new[0]["local"], format!("{:x}", Sha256::digest(&output)));
            assert_eq!(new[0]["source"], json!(installed.source_checksum));
            let old_paths: Vec<_> = before["durable"]["paths"]
                .as_array()
                .unwrap()
                .iter()
                .filter(matches)
                .collect();
            let new_paths: Vec<_> = after["durable"]["paths"]
                .as_array()
                .unwrap()
                .iter()
                .filter(matches)
                .collect();
            assert_eq!(old_paths.len(), 1);
            assert_eq!(new_paths.len(), 1);
            assert_eq!(new_paths[0]["marker"], Value::Null);
            assert_eq!(new_paths[0]["local"], new[0]["local"]);
            assert_eq!(new_paths[0]["source"], new[0]["source"]);
            for field in ["capture_hash", "capture_checksum", "capture_size"] {
                assert_eq!(new[0][field], old[0][field]);
                assert_eq!(new_paths[0][field], old_paths[0][field]);
            }
            let initial_hash = &before_upsert["durable"]["assets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["version"] == record.version_size.as_str())
                .unwrap()["hash"];
            assert_ne!(
                initial_hash, &old[0]["hash"],
                "provider rating must create an actual metadata transition"
            );
            transitions.push(json!({"record":record,"asset_before":old[0],"asset_after":new[0],"path_before":old_paths[0],"path_after":new_paths[0],"prepared":prepared,"installed":installed,"nonvacuous_marker_clear":!old[0]["marker"].is_null() && !old_paths[0]["marker"].is_null()}));
        }
        std::fs::write(root.path().join("embedded-settlement.json"),serde_json::to_vec_pretty(&json!({"operation":operation.operation,"SQL_phase":after["durable"]["SQL_phase"],"generation":after["binding"]["generation"],"flags":operation.metadata_flags,"transitions":transitions})).unwrap()).unwrap();
        assert_ne!(std::fs::read(&original).unwrap(), original_bytes);
        assert_eq!(std::fs::read(&sidecar).unwrap(), old_sidecar);
        #[cfg(feature = "xmp")]
        {
            let meta: xmp_toolkit::XmpMeta = if is_heif {
                let mut file = std::fs::File::open(&original).unwrap();
                let len = file.metadata().unwrap().len();
                let layout = download::heif::FileLayout::read(&mut file, len).unwrap();
                std::str::from_utf8(&layout.xmp(&mut file).unwrap().unwrap())
                    .unwrap()
                    .parse()
                    .unwrap()
            } else {
                let mut file = xmp_toolkit::XmpFile::new().unwrap();
                file.open_file(
                    &original,
                    xmp_toolkit::OpenFileOptions::default().for_read(),
                )
                .unwrap();
                file.xmp().unwrap()
            };
            assert_eq!(
                meta.property_i32(xmp_toolkit::xmp_ns::XMP, "Rating")
                    .unwrap()
                    .value,
                5
            );
        }
        #[cfg(not(feature = "xmp"))]
        {
            use little_exif::exif_tag::ExifTag;
            use little_exif::ifd::ExifTagGroup;
            let metadata = little_exif::metadata::Metadata::new_from_path(&original).unwrap();
            let rating = metadata
                .get_tag(&ExifTag::UnknownINT16U(
                    Vec::new(),
                    0x4746,
                    ExifTagGroup::GENERIC,
                ))
                .next();
            assert!(
                matches!(rating, Some(ExifTag::UnknownINT16U(values, _, _)) if values.first() == Some(&5))
            );
        }
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );
        let manifest = db.get_manifest_assets().await.unwrap();
        assert!(
            manifest
                .iter()
                .flat_map(|asset| &asset.preserved_files)
                .any(|item| std::fs::read(item.native_path.to_path())
                    .is_ok_and(|bytes| bytes == original_bytes))
        );
        drop(db);
        assert!(
            std::fs::read_dir(stage.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kei-embedded-attempt-"))
        );
        let output = std::fs::read(&original).unwrap();
        let (quiet, lookups) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(quiet.failed_count, 0);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_eq!(std::fs::read(&original).unwrap(), output);
        assert_eq!(std::fs::read(&sidecar).unwrap(), old_sidecar);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let quiet = snapshot(root.path(), &db, &operation, "quiet").await;
        assert_eq!(quiet["durable"], after["durable"]);
        assert_eq!(quiet["queue"], after["queue"]);
        assert_eq!(quiet["files"], after["files"]);
        eprintln!(
            "RECORDED_EMBED_COMPLETE root={} filename={filename} operation={} SQL_phase=committed parsed_rating=5",
            root.path().display(),
            operation.operation
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
#[tokio::test]
#[ignore = "dedicated process-death fixture, spawned by its parent test"]
async fn recorded_embedded_scratch_death_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    let uri = std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap();
    let mut current = records_at(&uri, None);
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(VALID_ORIGINAL.len());
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(provider_hash(VALID_ORIGINAL));
    current[1]["fields"]["isFavorite"] = json!({"value":1});
    cycle_options(
        &root,
        current,
        true,
        EditedNaming::Primary,
        CycleOptions {
            embed_rating: true,
            ..CycleOptions::default()
        },
    )
    .await;
    panic!("embedded scratch death worker passed its rendezvous");
}

#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "dedicated private-temp retry fixture, spawned by its parent test"]
async fn recorded_embedded_scratch_retry_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    let uri = std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap();
    let mut current = records_at(&uri, None);
    current[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(VALID_ORIGINAL.len());
    current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(provider_hash(VALID_ORIGINAL));
    current[1]["fields"]["isFavorite"] = json!({"value":1});
    let (recovered, _) = cycle_options(
        &root,
        current,
        true,
        EditedNaming::Primary,
        CycleOptions {
            embed_rating: true,
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.db_sync_token_advance_safe);
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
#[tokio::test]
async fn interrupted_embedded_scratch_reopens_without_adopting_or_removing_unknown_files() {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let mut current = records_with_original(&server, VALID_ORIGINAL, None);
    current[1]["fields"]["isFavorite"] = json!({"value":1});
    let (initial, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        CycleOptions::default(),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let original = media.join("IMG_0501.JPG");
    assert_eq!(std::fs::read(&original).unwrap(), VALID_ORIGINAL);
    let provider = Session {
        reported_count: None,
        records: current.clone(),
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
        exclude_ids: Arc::default(),
    };
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let db_store: Arc<dyn download::DownloadStore> = db.clone();
    let base = make_run_cycle_download_config_builder(&media, db_store);
    let mut config = base(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    config.refresh_metadata = true;
    config.metadata.set_exif_rating = true;
    let operation = download::seed_unprepared_refresh(&config, &pass).await;
    let stage = operation
        .members
        .iter()
        .find_map(|member| member.stage.as_ref())
        .unwrap()
        .to_path();
    drop(config);
    drop(base);
    drop(db);
    std::fs::write(
        root.path().join("fixture-owner"),
        b"kei-synthetic-process-death",
    )
    .unwrap();
    let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
    #[cfg(target_os = "macos")]
    let attempt_parent = {
        use std::os::unix::fs::DirBuilderExt;
        let path = root.path().join("private-temp");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    };
    #[cfg(not(target_os = "macos"))]
    let attempt_parent = stage.parent().unwrap().to_path_buf();
    let mut command = Command::new(std::env::current_exe().unwrap());
    #[cfg(target_os = "macos")]
    command.env("TMPDIR", &attempt_parent);
    let mut worker = Worker(
        command
            .args([
                "--exact",
                "sync_loop::runner::tests::primary_layout::recorded_embedded_scratch_death_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
            .env("KEI_TEST_PROCESS_DEATH_POINT", "recorded-embed-scratch")
            .env("KEI_TEST_PROCESS_DEATH_URL", server.uri())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while !root.path().join("ready").exists() {
        assert!(
            worker.0.try_wait().unwrap().is_none(),
            "worker exited: {}",
            std::fs::read_to_string(root.path().join("worker.log")).unwrap()
        );
        assert!(
            std::time::Instant::now() < deadline,
            "scratch death worker timed out"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let attempts: Vec<_> = std::fs::read_dir(&attempt_parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".kei-embedded-attempt-")
        })
        .collect();
    assert_eq!(attempts.len(), 1);
    let old_attempt = &attempts[0];
    let old_input = old_attempt.join("IMG_0501.JPG");
    assert_eq!(std::fs::read(&old_input).unwrap(), VALID_ORIGINAL);
    std::fs::write(old_attempt.join("unknown-marker"), b"do not delete").unwrap();
    worker.0.kill().unwrap();
    let status = worker.0.wait().unwrap();
    #[cfg(unix)]
    assert_eq!(status.signal(), Some(9));
    #[cfg(windows)]
    assert!(!status.success());
    assert!(!stage.exists());
    assert_eq!(std::fs::read(&original).unwrap(), VALID_ORIGINAL);
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let pending = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]
            .members
            .iter()
            .any(|member| member.stage.is_some() && member.prepared.is_none())
    );
    drop(db);
    let options = || CycleOptions {
        embed_rating: true,
        ..CycleOptions::default()
    };
    #[cfg(not(target_os = "macos"))]
    {
        let (recovered, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
        assert!(recovered.db_sync_token_advance_safe);
    }
    #[cfg(target_os = "macos")]
    {
        let log_path = root.path().join("retry-worker.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut retry = Worker(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sync_loop::runner::tests::primary_layout::recorded_embedded_scratch_retry_worker", "--ignored", "--nocapture"])
            .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
            .env("KEI_TEST_PROCESS_DEATH_URL", server.uri())
            .env("TMPDIR", &attempt_parent)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
        loop {
            if let Some(status) = retry.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "retry worker exited: {}",
                    std::fs::read_to_string(&log_path).unwrap()
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "retry worker timed out"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    assert_eq!(std::fs::read(&old_input).unwrap(), VALID_ORIGINAL);
    assert_eq!(
        std::fs::read(old_attempt.join("unknown-marker")).unwrap(),
        b"do not delete"
    );
    let remaining: Vec<_> = std::fs::read_dir(&attempt_parent)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".kei-embedded-attempt-")
        })
        .collect();
    assert_eq!(
        remaining,
        vec![old_attempt.clone()],
        "successful retry must clean only its own attempt"
    );
    let output = std::fs::read(&original).unwrap();
    assert_ne!(output, VALID_ORIGINAL);
    let (quiet, lookups) =
        cycle_options(root.path(), current, true, EditedNaming::Primary, options()).await;
    assert_eq!(quiet.failed_count, 0);
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(lookups, 0);
    assert_eq!(std::fs::read(&original).unwrap(), output);
    assert_eq!(
        std::fs::read(old_attempt.join("unknown-marker")).unwrap(),
        b"do not delete"
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn interrupted_unprepared_refresh_retries_on_ordinary_sync() {
    let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let mut current = records_with_original(&server, VALID_ORIGINAL, None);
    current[1]["fields"]["isFavorite"] = json!({"value":1});
    let options = |refresh_metadata| CycleOptions {
        refresh_metadata,
        sidecar: true,
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        options(false),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let original = media.join("IMG_0501.JPG");
    let old_media = std::fs::read(&original).unwrap();
    let sidecar = media.join("IMG_0501.JPG.xmp");
    let original_packet = std::fs::read_to_string(&sidecar)
        .unwrap()
        .replace("Rating>5", "Rating>4");
    assert!(original_packet.contains("Rating>4"));
    std::fs::write(&sidecar, original_packet.as_bytes()).unwrap();
    let old_sidecar = std::fs::read(&sidecar).unwrap();
    let provider = Session {
        reported_count: None,
        records: current.clone(),
        quiet: true,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        lookup_change: None,
        indexing: None,
        baseline_successor: None,
        cycle_token: None,
        trace: None,
    };
    let pass = AlbumPass {
        kind: PassKind::Unfiled,
        album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
        exclude_ids: Arc::default(),
    };
    let db = Arc::new(
        state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    let db_store: Arc<dyn download::DownloadStore> = db.clone();
    let base = make_run_cycle_download_config_builder(&media, db_store);
    let mut config = base(
        download::SyncMode::Full,
        Arc::default(),
        Arc::default(),
        Arc::from("PrimarySync"),
    )
    .as_ref()
    .clone();
    config.folder_structure = String::new();
    config.folder_structure_albums = Arc::from("");
    config.folder_structure_smart_folders = Arc::from("");
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    config.refresh_metadata = true;
    config.metadata.xmp_sidecar = true;
    let op = download::seed_unprepared_refresh(&config, &pass).await;
    assert!(
        op.members
            .iter()
            .any(|m| m.stage.is_some() && m.prepared.is_none())
    );
    drop(config);
    drop(base);
    drop(db);
    let (recovered, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        options(false),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.db_sync_token_advance_safe);
    assert_eq!(std::fs::read(&original).unwrap(), old_media);
    let fresh = std::fs::read(&sidecar).unwrap();
    assert_ne!(fresh, old_sidecar);
    assert!(std::str::from_utf8(&fresh).unwrap().contains("Rating>5"));
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        db.get_manifest_assets()
            .await
            .unwrap()
            .iter()
            .flat_map(|asset| &asset.preserved_files)
            .any(|file| {
                let mut path = file.native_path.to_path().as_os_str().to_os_string();
                path.push(".xmp");
                std::fs::read(std::path::PathBuf::from(path))
                    .is_ok_and(|bytes| bytes == old_sidecar)
            }),
        "prior sidecar must be independently preserved"
    );
    drop(db);
    let (quiet, lookups) = cycle_options(
        root.path(),
        current,
        true,
        EditedNaming::Primary,
        options(false),
    )
    .await;
    assert_eq!(quiet.failed_count, 0);
    assert_eq!(lookups, 0);
    assert_eq!(std::fs::read(&sidecar).unwrap(), fresh);
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn interrupted_unprepared_refresh_rejects_drift_and_occupied_paths() {
    for scenario in [
        "changed_source",
        "option_drift",
        "readonly",
        "occupied_stage",
        "occupied_sidecar",
        "changed_after_preparation",
        "embedded_changed_after_preparation",
        "embedded_occupied_stage",
        "embedded_occupied_sidecar",
    ] {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let mut current = records_with_original(&server, VALID_ORIGINAL, None);
        current[1]["fields"]["isFavorite"] = json!({"value":1});
        let initial = CycleOptions {
            sidecar: true,
            ..CycleOptions::default()
        };
        let (result, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            initial,
        )
        .await;
        assert_eq!(result.failed_count, 0);
        let original = media.join("IMG_0501.JPG");
        let old_media = std::fs::read(&original).unwrap();
        let original_sidecar = media.join("IMG_0501.JPG.xmp");
        std::fs::remove_file(&original_sidecar).unwrap();
        let provider = Session {
            reported_count: None,
            records: current.clone(),
            quiet: true,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
            exclude_ids: Arc::default(),
        };
        let db = Arc::new(
            state::SqliteStateDb::open(&root.path().join("state.db"))
                .await
                .unwrap(),
        );
        let db_store: Arc<dyn download::DownloadStore> = db.clone();
        let base = make_run_cycle_download_config_builder(&media, db_store);
        let mut config = base(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        config.refresh_metadata = true;
        config.metadata.xmp_sidecar = true;
        config.metadata.set_exif_rating = scenario.starts_with("embedded_");
        let op = download::seed_unprepared_refresh(&config, &pass).await;
        let stage = op
            .members
            .iter()
            .find_map(|m| m.stage.as_ref())
            .unwrap()
            .to_path();
        let mut sidecar_name = stage.as_os_str().to_os_string();
        sidecar_name.push(".xmp");
        let stage_sidecar = std::path::PathBuf::from(sidecar_name);
        if scenario == "readonly" {
            let conn = db.acquire_lock("mark source read-only").unwrap();
            assert!(conn.execute("UPDATE asset_metadata_paths SET preserved_path_key='foreign-marker' WHERE local_path=?1", [original.to_str().unwrap()]).unwrap() > 0);
        }
        if scenario.ends_with("occupied_stage") || scenario.ends_with("occupied_sidecar") {
            std::fs::create_dir_all(stage.parent().unwrap()).unwrap();
            let path = if scenario.ends_with("occupied_stage") {
                &stage
            } else {
                &stage_sidecar
            };
            std::fs::write(path, b"foreign entry").unwrap();
        }
        drop(config);
        drop(base);
        drop(db);
        if scenario == "changed_source" {
            current[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                json!(provider_hash(EDIT_TWO));
        }
        let mut later = current.clone();
        later[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(provider_hash(EDIT_TWO));
        let options = CycleOptions {
            sidecar: scenario != "option_drift",
            lookup_change: matches!(
                scenario,
                "changed_after_preparation" | "embedded_changed_after_preparation"
            )
            .then_some((2, later)),
            embed_rating: scenario.starts_with("embedded_"),
            ..CycleOptions::default()
        };
        let result = try_cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options,
        )
        .await;
        if let Ok((result, _)) = result {
            assert!(
                !result.db_sync_token_advance_safe || result.failed_count > 0,
                "{scenario}"
            );
        }
        assert_eq!(std::fs::read(&original).unwrap(), old_media, "{scenario}");
        assert!(!original_sidecar.exists(), "{scenario}");
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let pending = db
            .primary_layout_operations("PrimarySync".into())
            .await
            .unwrap();
        assert_eq!(pending.len(), 1, "{scenario}");
        assert!(
            pending[0]
                .members
                .iter()
                .any(|m| m.stage.is_some() && m.prepared.is_none()),
            "{scenario}"
        );
        if scenario.starts_with("embedded_occupied_") {
            assert!(
                std::fs::read_dir(stage.parent().unwrap())
                    .unwrap()
                    .all(|entry| {
                        !entry
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".kei-embedded-attempt-")
                    }),
                "{scenario}: collision created scratch"
            );
        }
        match scenario {
            "occupied_stage" | "embedded_occupied_stage" => {
                assert_eq!(std::fs::read(&stage).unwrap(), b"foreign entry")
            }
            "occupied_sidecar" | "embedded_occupied_sidecar" => {
                assert_eq!(std::fs::read(&stage_sidecar).unwrap(), b"foreign entry")
            }
            "changed_after_preparation" | "embedded_changed_after_preparation" => {
                let media_stage = std::fs::read(&stage).unwrap();
                let sidecar_stage = std::fs::read(&stage_sidecar).unwrap();
                drop(db);
                let retry = try_cycle_options(
                    root.path(),
                    current.clone(),
                    true,
                    EditedNaming::Primary,
                    CycleOptions {
                        sidecar: true,
                        embed_rating: scenario.starts_with("embedded_"),
                        ..CycleOptions::default()
                    },
                )
                .await;
                if let Ok((retry, _)) = retry {
                    assert!(!retry.db_sync_token_advance_safe || retry.failed_count > 0);
                }
                let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
                    .await
                    .unwrap();
                let pending = db
                    .primary_layout_operations("PrimarySync".into())
                    .await
                    .unwrap();
                assert_eq!(pending.len(), 1);
                assert!(
                    pending[0]
                        .members
                        .iter()
                        .any(|member| member.stage.is_some() && member.prepared.is_none())
                );
                assert_eq!(std::fs::read(&stage).unwrap(), media_stage);
                assert_eq!(std::fs::read(&stage_sidecar).unwrap(), sidecar_stage);
            }
            _ => assert!(!stage.exists(), "{scenario}"),
        }
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn review501_explicit_metadata_refresh_keeps_existing_naming_paths() {
    let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = |refresh_metadata| CycleOptions {
        refresh_metadata,
        sidecar: true,
        private: true,
        pass_kind: PassKind::Album,
        ..CycleOptions::default()
    };
    let initial_records =
        records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
    let (initial, _) = cycle_options(
        root.path(),
        initial_records.clone(),
        true,
        EditedNaming::Suffix,
        options(false),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    {
        let conn = rusqlite::Connection::open_with_flags(
            root.path().join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = conn.prepare(
            "SELECT c.container_id, c.album_name, m.asset_record_name, m.master_record_name, m.source \
             FROM asset_album_memberships m JOIN album_containers c \
             ON c.library = m.library AND c.container_id = m.container_id \
             WHERE m.library = 'PrimarySync' AND c.is_deleted = 0 AND m.is_deleted = 0 \
             ORDER BY c.container_id, m.asset_record_name",
        ).unwrap();
        let memberships = statement
            .query_map([], |row| {
                Ok([
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ])
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            memberships,
            [["album-501", "TestAlbum", "asset-501", "501", "icloud"].map(str::to_owned)]
        );
    }
    let original = media.join("IMG_0501.JPG");
    let edited = media.join("IMG_0501_edited.JPG");
    assert_bytes(&original, VALID_ORIGINAL);
    assert_bytes(&edited, VALID_EDIT);
    let original_mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    let edited_mtime = std::fs::metadata(&edited).unwrap().modified().unwrap();
    let sidecar = media.join("IMG_0501_edited.JPG.xmp");
    let previous_xmp = std::fs::read(&sidecar).unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let (renamed, _) = cycle_options(
        root.path(),
        initial_records.clone(),
        true,
        EditedNaming::Primary,
        options(false),
    )
    .await;
    assert_eq!(renamed.failed_count, 0, "{:?}", renamed.stats);
    assert!(renamed.db_sync_token_advance_safe);
    assert_eq!(renamed.stats.downloaded, 0);
    assert_bytes(&original, VALID_ORIGINAL);
    assert_bytes(&edited, VALID_EDIT);
    assert_eq!(std::fs::read(&sidecar).unwrap(), previous_xmp);
    assert!(!media.join("IMG_0501_original.JPG").exists());
    let mut changed = initial_records;
    changed[1]["fields"]["isFavorite"] = json!({"value":1});
    let (refreshed, _) = cycle_options(
        root.path(),
        changed.clone(),
        true,
        EditedNaming::Primary,
        options(true),
    )
    .await;
    assert_eq!(refreshed.failed_count, 0, "{:?}", refreshed.stats);
    assert!(refreshed.db_sync_token_advance_safe);
    assert_eq!(refreshed.stats.downloaded, 0);
    assert_bytes(&original, VALID_ORIGINAL);
    assert_bytes(&edited, VALID_EDIT);
    assert_eq!(
        std::fs::metadata(&original).unwrap().modified().unwrap(),
        original_mtime
    );
    assert_eq!(
        std::fs::metadata(&edited).unwrap().modified().unwrap(),
        edited_mtime
    );
    let fresh_xmp = std::fs::read(&sidecar).unwrap();
    assert_ne!(
        fresh_xmp, previous_xmp,
        "explicit refresh must update metadata"
    );
    assert!(
        std::str::from_utf8(&fresh_xmp)
            .unwrap()
            .contains("Rating>5")
    );
    let parsed: xmp_toolkit::XmpMeta = std::str::from_utf8(&fresh_xmp).unwrap().parse().unwrap();
    let old: xmp_toolkit::XmpMeta = std::str::from_utf8(&previous_xmp).unwrap().parse().unwrap();
    for (namespace, key) in [
        (xmp_toolkit::xmp_ns::XMP, "CreateDate"),
        (xmp_toolkit::xmp_ns::EXIF, "DateTimeOriginal"),
    ] {
        assert_eq!(
            parsed.property(namespace, key).unwrap().value,
            old.property(namespace, key).unwrap().value
        );
    }
    let expected_date = chrono::DateTime::from_timestamp_millis(
        crate::sync_loop::test_support::RUN_CYCLE_ASSET_DATE_MS,
    )
    .unwrap()
    .with_timezone(&chrono::Local)
    .format("%Y-%m-%dT%H:%M:%S")
    .to_string();
    for (namespace, key) in [
        (xmp_toolkit::xmp_ns::XMP, "CreateDate"),
        (xmp_toolkit::xmp_ns::EXIF, "DateTimeOriginal"),
    ] {
        assert_eq!(
            parsed.property(namespace, key).unwrap().value,
            expected_date
        );
    }
    let keywords: Vec<_> = (1..)
        .map_while(|index| parsed.array_item(xmp_toolkit::xmp_ns::DC, "subject", index))
        .map(|item| item.value)
        .collect();
    assert!(keywords.iter().any(|value| value == "TestAlbum"));
    assert!(!keywords.iter().any(|value| value == "Unfiled"));
    assert!(!media.join("IMG_0501_original.JPG").exists());
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    for (version, expected) in [("original", &original), ("adjusted", &edited)] {
        let recorded: String = conn.query_row(
            "SELECT local_path FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size=?1 AND status='downloaded'",
            [version], |row| row.get(0),
        ).unwrap();
        assert_eq!(std::path::Path::new(&recorded), expected);
    }
    drop(conn);
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.get_pending_metadata_rewrites(100)
            .await
            .unwrap()
            .is_empty()
    );
    drop(db);
    let catalogue = || {
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let mut statement = conn.prepare(
            "SELECT version_size, local_path, metadata_hash FROM assets WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size"
        ).unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<(String, String, Option<String>)>>>()
            .unwrap()
    };
    let refreshed_catalogue = catalogue();
    for _ in 0..2 {
        let (quiet, lookups) = cycle_options(
            root.path(),
            changed.clone(),
            true,
            EditedNaming::Primary,
            options(false),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_eq!(catalogue(), refreshed_catalogue);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(
            db.get_pending_metadata_rewrites(100)
                .await
                .unwrap()
                .is_empty()
        );
        drop(db);
        assert_bytes(&original, VALID_ORIGINAL);
        assert_bytes(&edited, VALID_EDIT);
        assert_eq!(std::fs::read(&sidecar).unwrap(), fresh_xmp);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
async fn recorded_suffix_album_replacement_recovers_without_redownload() {
    use crate::state::db::primary_layout::{LayoutFile, LayoutOperation};
    use crate::state::db::provider_selection::SelectionPath;
    use std::path::{Path, PathBuf};

    fn provider_hash(bytes: &[u8]) -> String {
        use sha1::{Digest as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        base64::engine::general_purpose::STANDARD.encode([&[1][..], &hash.finalize()[..]].concat())
    }

    // Match cycle_options' actual Album, database, source records and flags.
    // This adapter only prepares a released journal; it never changes the plan.
    async fn prepare_album(
        root: &Path,
        current: Vec<Value>,
    ) -> (
        LayoutOperation,
        Arc<state::SqliteStateDb>,
        download::DownloadConfig,
    ) {
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
                .await
                .unwrap(),
        );
        let provider = Session {
            reported_count: None,
            records: current,
            quiet: true,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: Some(Arc::from("album-501")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider),
        );
        // Production attaches account-owned capture to every Album. The
        // fixture's private=true installs that context; it is not a route flag.
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
            Arc::from("private"),
        );
        let (capture, _, _) = album.publication_scope("PrimarySync").unwrap().unwrap();
        assert!(Arc::ptr_eq(&db, &capture.db));
        let pass = AlbumPass {
            kind: PassKind::Album,
            album,
            exclude_ids: Arc::default(),
        };
        let media = root.join("media");
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store);
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Suffix;
        config.resolution = crate::types::PhotoResolution::Original;
        let actual = config.with_pass(&pass);
        assert_eq!(actual.primary_layout_pass_kind, Some(PassKind::Album));
        assert_eq!(actual.album_name.as_deref(), Some("TestAlbum"));
        assert!(!actual.refresh_metadata);
        assert!(actual.selection_run.is_none());
        eprintln!(
            "RECORDED_ALBUM_OWNER_BEGIN actual_Album501=true suffix=true pass={} flags=0",
            actual.primary_layout_pass.as_ref().unwrap()
        );
        let op = download::seed_prepared_edited_compat(&config, &pass).await;
        assert_eq!(op.pass, actual.primary_layout_pass.unwrap());
        assert_eq!(op.policy, "suffix");
        assert_eq!(op.phase, "prepared");
        assert_eq!(
            op.root,
            SelectionPath::from_path(&std::path::absolute(&media).unwrap())
        );
        assert_eq!(
            (
                &*op.source_library,
                &*op.library,
                &*op.child,
                &*op.asset_record_name
            ),
            ("PrimarySync", "PrimarySync", "asset-501", "asset-501")
        );
        assert!(!op.refresh_metadata);
        assert_eq!(op.metadata_flags, 0);
        assert_eq!(op.members.len(), 2);
        eprintln!(
            "RECORDED_ALBUM_PREPARED operation={} family={} generation={} pass={} phase={} flags={}",
            op.operation, op.family, op.generation, op.pass, op.phase, op.metadata_flags
        );
        drop(build);
        (op, db, config)
    }

    fn rows(root: &Path, sql: &str) -> Vec<Vec<rusqlite::types::Value>> {
        let conn = rusqlite::Connection::open_with_flags(
            root.join("state.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map([], |row| {
                (0..columns)
                    .map(|column| row.get(column))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn physical(
        root: &Path,
    ) -> Vec<(
        PathBuf,
        Vec<u8>,
        crate::fs_util::FileIdentity,
        std::time::SystemTime,
    )> {
        let mut pending = vec![root.to_path_buf()];
        let mut files = Vec::new();
        while let Some(parent) = pending.pop() {
            for entry in std::fs::read_dir(parent).unwrap() {
                let path = entry.unwrap().path();
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                assert!(!metadata.file_type().is_symlink());
                if metadata.is_dir() {
                    pending.push(path);
                } else {
                    assert!(metadata.is_file());
                    let file = std::fs::File::open(&path).unwrap();
                    files.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(&path).unwrap(),
                        crate::fs_util::file_identity(&file).unwrap(),
                        metadata.modified().unwrap(),
                    ));
                }
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        files
    }

    fn verify_file(file: &LayoutFile, bytes: &[u8]) {
        let opened = std::fs::File::open(file.path.to_path()).unwrap();
        assert_eq!(std::fs::read(file.path.to_path()).unwrap(), bytes);
        assert_eq!(file.provider_checksum, provider_hash(bytes));
        assert_eq!(
            file.source_checksum.as_deref(),
            Some(
                data_encoding::HEXLOWER
                    .encode(&Sha256::digest(bytes))
                    .as_str()
            )
        );
        assert_eq!(
            file.fingerprint.sha256.as_slice(),
            Sha256::digest(bytes).as_slice()
        );
        assert_eq!(file.fingerprint.size, bytes.len() as u64);
        assert_eq!(
            file.fingerprint.identity,
            crate::fs_util::file_identity(&opened).unwrap()
        );
        assert!(file.sidecar.is_none());
    }

    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    if std::env::var_os("KEI_RECORDED_ALBUM_RETAIN_FIXTURE").is_some() {
        root.disable_cleanup(true);
    }
    eprintln!("RECORDED_ALBUM_FIXTURE root={}", root.path().display());
    let media = root.path().join("media");
    let original = media.join("IMG_0501.JPG");
    let adjusted = media.join("IMG_0501_edited.JPG");
    let options = || CycleOptions {
        pass_kind: PassKind::Album,
        private: true,
        ..CycleOptions::default()
    };
    let records_for = |edit: Option<(&str, &[u8])>| {
        let mut values = records(&server, edit);
        values[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(provider_hash(ORIGINAL));
        if let Some((_, bytes)) = edit {
            values[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] =
                json!(provider_hash(bytes));
        }
        values
    };
    let first = records_for(Some(("edit-one", EDIT_ONE)));
    // Avoid an ordinary first-Adjusted prerequisite: the released compatibility
    // owner below performs that genuine GET and creates the legacy family.
    let (initial, _) = cycle_options(
        root.path(),
        records_for(None),
        false,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert_eq!(initial.stats.downloaded, 1);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&original, ORIGINAL);
    assert!(!adjusted.try_exists().unwrap());
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations"
        ),
        0
    );
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 1);
    assert_eq!(
        rows(root.path(), "SELECT * FROM download_receipts ORDER BY id").len(),
        1
    );
    eprintln!(
        "RECORDED_ALBUM_BASELINE Original_GET=1 first_edit_absent=true managed_operations=0 actual_Album501=true suffix=true"
    );

    eprintln!(
        "RECORDED_ALBUM_LEGACY_PREPARE_BEGIN first_edit_GET_owner=compatibility ordinary_first_edit_publication=false"
    );
    let (mut legacy, db, config) = prepare_album(root.path(), first).await;
    for (version, bytes, path) in [
        ("original", ORIGINAL, &original),
        ("adjusted", EDIT_ONE, &adjusted),
    ] {
        let member = legacy
            .members
            .iter()
            .find(|m| m.record.as_ref().unwrap().version_size.as_str() == version)
            .unwrap();
        assert_eq!(member.destination.as_ref().unwrap().to_path(), *path);
        if version == "original" {
            verify_file(member.old.as_ref().unwrap(), bytes);
        } else {
            assert!(member.old.is_none());
        }
        verify_file(member.prepared.as_ref().unwrap(), bytes);
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests + 1
    );
    download::finish_prepared_edited_compat(&config, &mut legacy).await;
    assert_eq!(legacy.phase, "committed");
    let binding = db
        .primary_layout_binding(legacy.family.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.generation, legacy.generation + 1);
    assert_eq!(binding.policy, "suffix");
    assert_eq!(binding.files.len(), 2);
    eprintln!(
        "RECORDED_ALBUM_LEGACY_COMMITTED operation={} binding_generation={} first_edit_GET_owner=compatibility total_GETs=2",
        legacy.operation, binding.generation
    );
    for file in &binding.files {
        verify_file(
            file,
            if file.version == "original" {
                ORIGINAL
            } else {
                EDIT_ONE
            },
        );
    }
    assert!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
    assert!(checkpoint.is_some());
    let old_store = Arc::downgrade(&db);
    drop(config);
    drop(db);
    assert!(old_store.upgrade().is_none());
    let before = physical(&media);
    let old_proof_sql = format!(
        "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND provider_checksum='{}' ORDER BY local_path",
        provider_hash(EDIT_ONE)
    );
    let old_proof = rows(root.path(), &old_proof_sql);
    assert!(!old_proof.is_empty());
    let compact_sql = "SELECT * FROM download_receipts ORDER BY id";
    let compact = rows(root.path(), compact_sql);
    assert_eq!(
        compact.len(),
        1,
        "only the ordinary Original minted a compact receipt; legacy preparation is not ordinary first-edit publication"
    );

    let second = records_for(Some(("edit-two", EDIT_TWO)));
    eprintln!(
        "RECORDED_ALBUM_REPLACEMENT_PREPARE_BEGIN C2_GET_owner=compatibility ordinary_C2_publication=false"
    );
    let (op, db, config) = prepare_album(root.path(), second.clone()).await;
    assert_eq!(op.family, legacy.family);
    assert_eq!(op.pass, legacy.pass);
    assert_eq!(op.generation, binding.generation);
    assert_ne!(op.operation, legacy.operation);
    assert_ne!(op.decision, legacy.decision);
    assert_eq!(
        physical(&media)
            .iter()
            .filter(|f| !f.0.starts_with(".kei-history"))
            .cloned()
            .collect::<Vec<_>>(),
        before
            .iter()
            .filter(|f| !f.0.starts_with(".kei-history"))
            .cloned()
            .collect::<Vec<_>>()
    );
    for (version, bytes, path) in [
        ("original", ORIGINAL, &original),
        ("adjusted", EDIT_TWO, &adjusted),
    ] {
        let member = op
            .members
            .iter()
            .find(|m| m.record.as_ref().unwrap().version_size.as_str() == version)
            .unwrap();
        assert_eq!(member.destination.as_ref().unwrap().to_path(), *path);
        assert_eq!(
            member.stage.as_ref(),
            Some(&member.prepared.as_ref().unwrap().path)
        );
        verify_file(member.prepared.as_ref().unwrap(), bytes);
        verify_file(
            member.old.as_ref().unwrap(),
            if version == "original" {
                ORIGINAL
            } else {
                EDIT_ONE
            },
        );
        assert!(member.installed.is_none());
    }
    let saved = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(
        serde_json::to_value(&saved[0]).unwrap(),
        serde_json::to_value(&op).unwrap()
    );
    let expected_id = op.operation.clone();
    let stage_paths: Vec<_> = op
        .members
        .iter()
        .map(|m| m.stage.as_ref().unwrap().to_path())
        .collect();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    for endpoint in ["/original", "/edit-one", "/edit-two"] {
        assert_eq!(
            requests.iter().filter(|r| r.url.path() == endpoint).count(),
            1
        );
    }
    // The only EDIT_TWO GET occurred in compatibility preparation, not discovery.
    eprintln!(
        "RECORDED_ALBUM_GETS original=1 C1=1 C2=1 C1_C2_owner=compatibility ordinary_C2_publication=false"
    );
    let old_store = Arc::downgrade(&db);
    drop(config);
    drop(db);
    assert!(old_store.upgrade().is_none());
    assert_eq!(rows(root.path(), &old_proof_sql), old_proof);
    assert_eq!(rows(root.path(), compact_sql), compact);
    rusqlite::Connection::open(root.path().join("state.db")).unwrap().execute_batch(
        "CREATE TRIGGER fail_collecting_layout_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected collecting layout commit failure'); END"
    ).unwrap();
    let held = try_cycle_options(
        root.path(),
        second.clone(),
        true,
        EditedNaming::Suffix,
        options(),
    )
    .await
    .expect_err("commit fault must reach normal recorded recovery");
    eprintln!("RECORDED_ALBUM_RECOVERY_ERROR operation={expected_id} error={held:#}");
    assert!(format!("{held:#}").contains("injected collecting layout commit failure"));
    assert!(
        matches!(held.downcast_ref::<state::error::StateError>(), Some(state::error::StateError::Migration(rusqlite::Error::SqliteFailure(error, Some(message)))) if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER && message == "injected collecting layout commit failure")
    );
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync").await.unwrap(),
        checkpoint
    );
    let held_ops = db
        .primary_layout_operations("PrimarySync".into())
        .await
        .unwrap();
    assert_eq!(held_ops.len(), 1);
    let held_op = &held_ops[0];
    assert_eq!(held_op.operation, expected_id);
    assert_eq!(held_op.phase, "publishing");
    assert_eq!(held_op.generation, binding.generation);
    assert_eq!(held_op.pass, op.pass);
    assert_eq!(held_op.policy, op.policy);
    assert_eq!(held_op.decision, op.decision);
    assert_eq!(held_op.metadata_flags, op.metadata_flags);
    assert_eq!(
        db.primary_layout_binding(op.family.clone())
            .await
            .unwrap()
            .unwrap()
            .generation,
        binding.generation
    );
    assert_eq!(held_op.members.len(), op.members.len());
    for (index, (frozen, member)) in op.members.iter().zip(&held_op.members).enumerate() {
        assert_eq!(
            serde_json::to_value(&member.prepared).unwrap(),
            serde_json::to_value(&frozen.prepared).unwrap(),
            "prepared proof changed at member {index}"
        );
        assert_eq!(
            serde_json::to_value(&member.old).unwrap(),
            serde_json::to_value(&frozen.old).unwrap(),
            "old proof changed at member {index}"
        );
        assert_eq!(
            serde_json::to_value(&member.record).unwrap(),
            serde_json::to_value(&frozen.record).unwrap(),
            "resource changed at member {index}"
        );
        assert_eq!(
            member.destination, frozen.destination,
            "destination changed at member {index}"
        );
        assert_eq!(
            member.stage, frozen.stage,
            "stage changed at member {index}"
        );
        assert_eq!(
            member.preserved_path, frozen.preserved_path,
            "preservation reservation changed at member {index}"
        );
        let bytes = if frozen.record.as_ref().unwrap().version_size.as_str() == "original" {
            ORIGINAL
        } else {
            EDIT_TWO
        };
        let installed = member
            .installed
            .as_ref()
            .expect("commit fault must follow durable installation");
        verify_file(installed, bytes);
        assert_eq!(frozen.destination.as_ref(), Some(&installed.path));
        let prepared = frozen.prepared.as_ref().unwrap();
        assert_eq!(prepared.fingerprint, installed.fingerprint);
        assert_eq!(prepared.sidecar, installed.sidecar);
        let old = frozen.old.as_ref().unwrap();
        let preserved = member
            .preserved
            .as_ref()
            .expect("old bytes must precede replacement");
        verify_file(
            preserved,
            if bytes == ORIGINAL {
                ORIGINAL
            } else {
                EDIT_ONE
            },
        );
        assert_eq!(frozen.preserved_path.as_ref(), Some(&preserved.path));
        assert_eq!(preserved.version, old.version);
        assert_eq!(preserved.provider_checksum, old.provider_checksum);
        assert_eq!(preserved.source_checksum, old.source_checksum);
        assert_eq!(preserved.fingerprint.size, old.fingerprint.size);
        assert_eq!(preserved.fingerprint.sha256, old.fingerprint.sha256);
        assert_eq!(
            preserved.sidecar.as_ref().map(|p| (p.size, p.sha256)),
            old.sidecar.as_ref().map(|p| (p.size, p.sha256))
        );
        // Preservation is an independent copy. verify_file checks its OWN
        // receipt identity, not equality with the frozen old source inode.
        assert!(member.retirement.is_none());
        // Exchange publication may retain the displaced inode at the stage;
        // the fallback owner may instead consume it. Never delete either form.
        let stage = frozen.stage.as_ref().unwrap().to_path();
        match std::fs::symlink_metadata(&stage) {
            Ok(metadata) => {
                assert!(metadata.is_file());
                let opened = std::fs::File::open(&stage).unwrap();
                assert_eq!(
                    crate::fs_util::file_identity(&opened).unwrap(),
                    old.fingerprint.identity
                );
                assert_eq!(
                    Sha256::digest(std::fs::read(&stage).unwrap()).as_slice(),
                    old.fingerprint.sha256.as_slice()
                );
                assert_eq!(metadata.len(), old.fingerprint.size);
            }
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::NotFound),
        }
    }
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&adjusted, EDIT_TWO);
    assert_eq!(rows(root.path(), &old_proof_sql), old_proof);
    assert_eq!(rows(root.path(), compact_sql), compact);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM assets WHERE version_size='adjusted' AND status='downloaded'"
        ),
        0
    );
    assert!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
        ) > 0
    );
    drop(db);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests.len()
    );
    let held_members = rows(
        root.path(),
        "SELECT * FROM primary_layout_members ORDER BY operation,member",
    );
    let held_files = physical(&media);
    eprintln!(
        "RECORDED_ALBUM_HOLD operation={expected_id} phase=publishing binding_generation={} members_installed_preserved=true checkpoint_unchanged=true total_GETs=3",
        binding.generation
    );
    for file in &before {
        if file.0.starts_with(".kei-history") {
            assert!(held_files.contains(file));
        }
    }
    assert!(
        held_files
            .iter()
            .all(|f| f.0.extension().is_none_or(|extension| extension != "xmp"))
    );
    rusqlite::Connection::open(root.path().join("state.db"))
        .unwrap()
        .execute_batch("DROP TRIGGER fail_collecting_layout_commit")
        .unwrap();

    let (recovered, _) = cycle_options(
        root.path(),
        second.clone(),
        true,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.db_sync_token_advance_safe);
    assert!(recovered.can_advance_database_checkpoint());
    assert_eq!(recovered.stats.downloaded, 0);
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.primary_layout_operations("PrimarySync".into())
            .await
            .unwrap()
            .is_empty()
    );
    let committed = db
        .primary_layout_binding(op.family.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(committed.generation, binding.generation + 1);
    assert_eq!(committed.decision, op.decision);
    assert_eq!(committed.policy, "suffix");
    assert_eq!(committed.files.len(), 2);
    for file in &committed.files {
        verify_file(
            file,
            if file.version == "original" {
                ORIGINAL
            } else {
                EDIT_TWO
            },
        );
    }
    let (old_edit_index, old_edit_member) = op
        .members
        .iter()
        .enumerate()
        .find(|(_, member)| {
            member.old.as_ref().is_some_and(|old| {
                old.version == "adjusted" && old.provider_checksum == provider_hash(EDIT_ONE)
            })
        })
        .unwrap();
    let reserved_edit = old_edit_member.preserved_path.as_ref().unwrap();
    let (native, evidence): (Vec<u8>, Vec<u8>) = db.acquire_lock("exact frozen Album preservation member").unwrap().query_row(
        "SELECT p.native_path,p.evidence FROM primary_layout_operations o JOIN primary_layout_members m ON m.operation=o.operation JOIN primary_layout_preserved p ON p.operation=m.operation AND p.member=m.member WHERE o.operation=?1 AND o.family=?2 AND o.phase='committed' AND m.member=?3",
        rusqlite::params![expected_id,op.family,i64::try_from(old_edit_index).unwrap()],
        |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    let native: SelectionPath = serde_json::from_slice(&native).unwrap();
    assert_eq!(&native, reserved_edit);
    let durable: LayoutFile = serde_json::from_slice(&evidence).unwrap();
    assert_eq!(&durable.path, reserved_edit);
    assert_eq!(
        serde_json::to_value(&durable).unwrap(),
        serde_json::to_value(held_op.members[old_edit_index].preserved.as_ref().unwrap()).unwrap()
    );
    verify_file(&durable, EDIT_ONE);
    let manifest = db.get_manifest_assets().await.unwrap();
    assert!(
        manifest
            .iter()
            .flat_map(|row| &row.preserved_files)
            .any(|file| {
                file.operation == expected_id
                    && file.family == op.family
                    && &file.native_path == reserved_edit
                    && file.previous_generation == binding.generation
                    && file.provider_checksum == provider_hash(EDIT_ONE)
                    && file.source_checksum.as_deref()
                        == Some(
                            data_encoding::HEXLOWER
                                .encode(&Sha256::digest(EDIT_ONE))
                                .as_str(),
                        )
                    && file.local_checksum
                        == data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_ONE))
                    && file.size_bytes == EDIT_ONE.len() as u64
                    && std::fs::read(file.native_path.to_path()).unwrap() == EDIT_ONE
            })
    );
    drop(db);
    assert_eq!(
        physical(&media),
        held_files,
        "commit-only recovery changed media/history/stages"
    );
    // Current-slot path receipts legitimately advance to EDIT_TWO; the full
    // old source/local/path proof remains in the exact recorded members/history.
    assert_eq!(
        rows(
            root.path(),
            "SELECT * FROM primary_layout_members ORDER BY operation,member"
        ),
        held_members
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='committed'"
        ),
        2
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
        ),
        0
    );
    let final_sql = "SELECT a.library,a.id,a.version_size,a.status,a.checksum,a.local_checksum,a.download_checksum,a.local_path,p.provider_checksum,p.local_checksum,p.source_checksum FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path AND p.provider_checksum=a.checksum WHERE a.library='PrimarySync' AND a.id='asset-501' ORDER BY a.version_size";
    let final_rows = rows(root.path(), final_sql);
    assert_eq!(final_rows.len(), 2);
    for (row, version, bytes, path) in [
        (&final_rows[0], "adjusted", EDIT_TWO, &adjusted),
        (&final_rows[1], "original", ORIGINAL, &original),
    ] {
        let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        let expected = [
            "PrimarySync".to_owned(),
            "asset-501".to_owned(),
            version.to_owned(),
            "downloaded".to_owned(),
            provider_hash(bytes),
            sha.clone(),
            sha.clone(),
            path.to_str().unwrap().to_owned(),
            provider_hash(bytes),
            sha.clone(),
            sha,
        ];
        assert_eq!(
            *row,
            expected
                .into_iter()
                .map(rusqlite::types::Value::Text)
                .collect::<Vec<_>>()
        );
    }
    let same_op_sql =
        format!("SELECT phase FROM primary_layout_operations WHERE operation='{expected_id}'");
    assert_eq!(
        rows(root.path(), &same_op_sql),
        vec![vec![rusqlite::types::Value::Text("committed".into())]]
    );
    eprintln!(
        "RECORDED_ALBUM_RECOVERED operation={expected_id} generation={} same_operation=true downloads=0 new_GETs=0 full_physical_stable=true",
        committed.generation
    );
    let proof_sql = "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path,provider_checksum";
    let proof = rows(root.path(), proof_sql);
    let journal_sql = "SELECT * FROM primary_layout_members ORDER BY operation,member";
    let journal = rows(root.path(), journal_sql);
    let claims_sql = "SELECT * FROM primary_layout_claims ORDER BY path_key";
    let claims = rows(root.path(), claims_sql);
    let history_sql = "SELECT * FROM primary_layout_preserved ORDER BY operation,member";
    let history = rows(root.path(), history_sql);
    let binding_sql = "SELECT * FROM primary_layout_bindings ORDER BY family";
    let bindings = rows(root.path(), binding_sql);
    let operations_sql = "SELECT * FROM primary_layout_operations ORDER BY operation";
    let operations = rows(root.path(), operations_sql);
    let compact_final = rows(root.path(), compact_sql);
    assert!(compact.iter().all(|row| compact_final.contains(row)));
    let retained_stages: Vec<_> = stage_paths.iter().map(|p| p.exists()).collect();
    for _ in 0..2 {
        let (quiet, lookups) = cycle_options(
            root.path(),
            second.clone(),
            true,
            EditedNaming::Suffix,
            options(),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert!(quiet.db_sync_token_advance_safe);
        assert!(quiet.can_advance_database_checkpoint());
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            requests.len()
        );
        assert_eq!(physical(&media), held_files);
        assert_eq!(rows(root.path(), final_sql), final_rows);
        assert_eq!(rows(root.path(), proof_sql), proof);
        assert_eq!(rows(root.path(), journal_sql), journal);
        assert_eq!(rows(root.path(), claims_sql), claims);
        assert_eq!(rows(root.path(), history_sql), history);
        assert_eq!(rows(root.path(), binding_sql), bindings);
        assert_eq!(rows(root.path(), operations_sql), operations);
        assert_eq!(rows(root.path(), compact_sql), compact_final);
        assert_eq!(
            rows(root.path(), &same_op_sql),
            vec![vec![rusqlite::types::Value::Text("committed".into())]]
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            2
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
            ),
            0
        );
        assert_eq!(
            stage_paths.iter().map(|p| p.exists()).collect::<Vec<_>>(),
            retained_stages
        );
        eprintln!(
            "RECORDED_ALBUM_QUIET operation={expected_id} full_physical_and_proof_stable=true lookups=0 downloads=0 new_GETs=0 operations=2"
        );
    }
}

async fn ordinary_template_preserves_recorded_paths(pass_kind: PassKind) {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let page = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = |folder_structure, cycle_token| CycleOptions {
        private: true,
        pass_kind,
        folder_structure,
        baseline_successor: Some("initial-token"),
        cycle_token: Some(cycle_token),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        page.clone(),
        false,
        EditedNaming::Primary,
        options("first", "initial-token"),
    )
    .await;
    assert_eq!(
        initial.failed_count, 0,
        "{pass_kind:?}: {:?}",
        initial.stats
    );
    assert_eq!(initial.stats.downloaded, 2);
    assert!(initial.db_sync_token_advance_safe);
    let first = root.path().join("media/first");
    let original = first.join("IMG_0501_original.JPG");
    let edited = first.join("IMG_0501.JPG");
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&edited, EDIT_ONE);
    let times = [
        std::fs::metadata(&original).unwrap().modified().unwrap(),
        std::fs::metadata(&edited).unwrap().modified().unwrap(),
    ];
    let snapshot = || {
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let receipts: Vec<String> = conn.prepare("SELECT json_array(scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash,account_key,provider_key) FROM download_receipts ORDER BY version_size").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        let paths: Vec<String> = conn.prepare("SELECT json_array(library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash) FROM asset_metadata_paths ORDER BY version_size,local_path").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        (receipts, paths)
    };
    let (receipts, paths) = snapshot();
    assert_eq!(receipts.len(), 2);
    assert_eq!(paths.len(), 2);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let (account_key, provider_key) = owner.authenticated_keys().unwrap();
    let owner_row: (String, String) = rusqlite::Connection::open(root.path().join("state.db"))
        .unwrap()
        .query_row(
            "SELECT account_key,provider_key FROM account_owner WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(owner_row, (account_key.to_owned(), provider_key.to_owned()));
    // Fixed pass identities independently bind these fixtures' kinds and provider
    // query scopes (including album-501 for Album), with no exclusions.
    let expected_pass = match pass_kind {
        PassKind::Unfiled => "7de366311251c17f1e952c68059e8b40b6c3a25d8a01646bf24c4dbd61202256",
        PassKind::Album => "8f22641465c3b441df94dc2d08b5c1029c663a7da888d17799e8eb30e9caf0c2",
        PassKind::SmartFolder => "bfd1a1429a2d64c388c4d15164469212b7334f0fa06ed59ac35a6e52c9e1113a",
    };
    let parsed_paths: Vec<Value> = paths
        .iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect();
    for (row, version, bytes, path) in [
        (&parsed_paths[0], "adjusted", EDIT_ONE, &edited),
        (&parsed_paths[1], "original", ORIGINAL, &original),
    ] {
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        assert_eq!(
            (&row[0], &row[1], &row[2]),
            (&json!("PrimarySync"), &json!("asset-501"), &json!(version))
        );
        assert_eq!(row[3], path.to_str().unwrap());
        assert_eq!(row[4], provider_hash(bytes));
        assert_eq!(row[5], hash);
        assert_eq!(row[6], hash);
        assert_eq!(row[7], hash);
        assert!(row[8].as_i64().is_some_and(|time| time > 0));
        assert!(row[9].is_null());
    }
    let parsed: Vec<Value> = receipts
        .iter()
        .map(|r| serde_json::from_str(r).unwrap())
        .collect();
    for (receipt, version, bytes, path) in [
        (&parsed[0], "adjusted", EDIT_ONE, &edited),
        (&parsed[1], "original", ORIGINAL, &original),
    ] {
        let scope: Value = serde_json::from_str(receipt[0].as_str().unwrap()).unwrap();
        assert_eq!(
            scope,
            json!({"container":"com.apple.photos.cloud", "database":"private", "environment":"production", "format":1, "realm":"com", "zone":{"ownerRecordName":"_defaultOwner", "zoneName":"PrimarySync"}})
        );
        assert_eq!(
            (&receipt[1], &receipt[2], &receipt[3], &receipt[4]),
            (
                &json!("PrimarySync"),
                &json!("asset-501"),
                &json!("501"),
                &json!("asset-501")
            )
        );
        assert_eq!(receipt[5], expected_pass);
        assert_eq!(receipt[6], version);
        assert_eq!(receipt[7], provider_hash(bytes));
        assert_eq!(receipt[8], bytes.len());
        let native: Value = serde_json::from_str(receipt[9].as_str().unwrap()).unwrap();
        assert_eq!(native["path"], path.to_str().unwrap());
        let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        assert_eq!(receipt[10], hash);
        assert_eq!(receipt[11], hash);
        assert!(!receipt[12].as_str().unwrap().is_empty());
        assert_eq!(receipt[13], account_key);
        assert_eq!(receipt[14], provider_key);
    }
    assert_eq!(parsed[0][5], parsed[1][5]);
    assert_eq!(
        db_count(root.path(), "SELECT COUNT(*) FROM account_owner"),
        1
    );
    async fn metadata_work(db: &state::SqliteStateDb) -> Vec<(std::path::PathBuf, String, String)> {
        let mut work: Vec<_> = db
            .get_pending_metadata_rewrites(100)
            .await
            .unwrap()
            .into_iter()
            .map(|asset| {
                (
                    asset.local_path.unwrap(),
                    asset.checksum.into_string(),
                    asset.local_checksum.unwrap(),
                )
            })
            .collect();
        work.sort();
        work
    }
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let metadata_before = metadata_work(&db).await;
    let mut expected_work = vec![
        (
            original.clone(),
            provider_hash(ORIGINAL),
            data_encoding::HEXLOWER.encode(&Sha256::digest(ORIGINAL)),
        ),
        (
            edited.clone(),
            provider_hash(EDIT_ONE),
            data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_ONE)),
        ),
    ];
    expected_work.sort();
    assert_eq!(metadata_before, expected_work);
    drop(db);
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 2);
    let token = || {
        rusqlite::Connection::open(root.path().join("state.db"))
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    };
    assert_eq!(token(), "initial-token");
    for round in 0..3 {
        let (result, lookups) = cycle_options(
            root.path(),
            page.clone(),
            round > 0,
            EditedNaming::Primary,
            options("second", "changed-token"),
        )
        .await;
        assert_eq!(
            result.failed_count, 0,
            "{pass_kind:?} round {round}: {:?}",
            result.stats
        );
        assert!(result.db_sync_token_advance_safe);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.skipped.uncertain, 0);
        if round == 0 {
            assert_eq!(
                result.stats.full_enumeration_reason,
                Some(download::FullEnumerationReason::DownloadConfigHashDrift)
            );
            assert!(result.stats.assets_seen > 0);
            assert!(result.stats.skipped.on_disk > 0);
        } else {
            assert_eq!(lookups, 0);
        }
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&edited, EDIT_ONE);
        assert_eq!(
            [
                std::fs::metadata(&original).unwrap().modified().unwrap(),
                std::fs::metadata(&edited).unwrap().modified().unwrap()
            ],
            times
        );
        assert!(!root.path().join("media/second/IMG_0501.JPG").exists());
        assert!(
            !root
                .path()
                .join("media/second/IMG_0501_original.JPG")
                .exists()
        );
        assert!(!root.path().join("media/.kei-history").exists());
        assert_eq!(snapshot(), (receipts.clone(), paths.clone()));
        assert_eq!(token(), "changed-token");
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND status='downloaded'"
            ),
            2
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND status='pending'"
            ),
            0
        );
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let decisions: Vec<(String, String, String)> = conn.prepare("SELECT pass_key,outcome,admission FROM provider_active_decisions WHERE child='asset-501'").unwrap().query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(!decisions.is_empty());
        assert!(
            decisions
                .iter()
                .all(|(pass, outcome, admission)| pass == expected_pass
                    && outcome == "selected"
                    && admission == "admitted"),
            "{decisions:?}"
        );
        let destinations: Vec<String> = conn.prepare("SELECT json_array(pass_key,child,library,asset_id,master,version_size,path,checksum,size_bytes,admitted,verified_media,verified_metadata,source_checksum,local_checksum) FROM provider_active_destinations WHERE child='asset-501' ORDER BY version_size,path").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(destinations.len(), 2, "{destinations:?}");
        for (row, version, bytes, path) in [
            (&destinations[0], "adjusted", EDIT_ONE, &edited),
            (&destinations[1], "original", ORIGINAL, &original),
        ] {
            let row: Value = serde_json::from_str(row).unwrap();
            let native: Value = serde_json::from_str(row[6].as_str().unwrap()).unwrap();
            let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
            assert_eq!(
                (&row[0], &row[1], &row[2], &row[3], &row[4], &row[5]),
                (
                    &json!(expected_pass),
                    &json!("asset-501"),
                    &json!("PrimarySync"),
                    &json!("asset-501"),
                    &json!("501"),
                    &json!(version)
                )
            );
            assert_eq!(native["path"], path.to_str().unwrap());
            assert_eq!(row[7], provider_hash(bytes));
            assert_eq!(row[8], bytes.len());
            assert_eq!(
                (&row[9], &row[10], &row[11]),
                (&json!(1), &json!(1), &json!(1))
            );
            assert_eq!(row[12], hash);
            assert_eq!(row[13], hash);
        }
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM provider_work_retries"),
            0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_work_receipts WHERE state='deferred'"
            ),
            0
        );
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(db.get_pending().await.unwrap().is_empty());
        assert_eq!(metadata_work(&db).await, metadata_before);
        for table in [
            "primary_layout_operations",
            "primary_layout_bindings",
            "primary_layout_claims",
            "primary_layout_preserved",
            "primary_layout_superseded_paths",
        ] {
            assert_eq!(
                db_count(root.path(), &format!("SELECT COUNT(*) FROM {table}")),
                0,
                "{table}"
            );
        }
    }
}

#[tokio::test]
async fn ordinary_template_preserves_recorded_unfiled_paths() {
    ordinary_template_preserves_recorded_paths(PassKind::Unfiled).await;
}

#[tokio::test]
async fn ordinary_template_preserves_recorded_album_paths() {
    ordinary_template_preserves_recorded_paths(PassKind::Album).await;
}

#[tokio::test]
async fn ordinary_template_preserves_recorded_smart_folder_paths() {
    ordinary_template_preserves_recorded_paths(PassKind::SmartFolder).await;
}

async fn root_change_preserves_verified_paths_for_pass(
    pass_kind: PassKind,
    next_folder: &'static str,
) {
    let server = server().await;
    let mut root = tempfile::tempdir().unwrap();
    root.disable_cleanup(true);
    eprintln!(
        "ROOT_TEMPLATE_REUSE_FIXTURE pass={pass_kind:?} folder={next_folder} root={}",
        root.path().display()
    );
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = |directory, folder, token| CycleOptions {
        private: true,
        pass_kind,
        media_directory: directory,
        folder_structure: folder,
        baseline_successor: Some("initial-token"),
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options("media", "first", "initial-token"),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert_eq!(initial.stats.downloaded, 2);
    let old = root.path().join("media/first");
    let original = old.join("IMG_0501_original.JPG");
    let edit = old.join("IMG_0501.JPG");
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&edit, EDIT_ONE);
    let mtimes = [
        std::fs::metadata(&original).unwrap().modified().unwrap(),
        std::fs::metadata(&edit).unwrap().modified().unwrap(),
    ];
    let snapshot = || {
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let receipts: Vec<String> = conn.prepare("SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' ORDER BY version_size").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        let paths: Vec<String> = conn.prepare("SELECT json_array(version_size,local_path,provider_checksum,local_checksum,source_checksum,metadata_write_failed_at) FROM asset_metadata_paths WHERE id='asset-501' ORDER BY version_size").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        (receipts, paths)
    };
    let identities = || {
        [&original, &edit]
            .map(|path| crate::fs_util::file_identity(&std::fs::File::open(path).unwrap()).unwrap())
    };
    let before_identities = identities();
    let new_dir = root.path().join("new-root").join(next_folder);
    let before = snapshot();
    assert_eq!(before.0.len(), 2);
    assert_eq!(before.1.len(), 2);
    let expected_metadata_debt = if pass_kind == PassKind::Unfiled { 0 } else { 2 };
    assert!(before.1.iter().all(|row| {
        let row: Value = serde_json::from_str(row).unwrap();
        let metadata_write_failed_at = &row[5];
        if pass_kind == PassKind::Unfiled {
            metadata_write_failed_at.is_null()
        } else {
            metadata_write_failed_at.as_i64().is_some_and(|v| v > 0)
        }
    }));
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 2);
    for round in 0..3 {
        let (result, lookups) = cycle_options(
            root.path(),
            current.clone(),
            round != 0,
            EditedNaming::Primary,
            options("new-root", next_folder, "changed-token"),
        )
        .await;
        assert_eq!(result.failed_count, 0, "round {round}: {:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(result.stats.skipped.uncertain, 0);
        if round == 0 {
            assert_eq!(
                result.stats.full_enumeration_reason,
                Some(download::FullEnumerationReason::DownloadConfigHashDrift)
            );
            assert!(result.stats.assets_seen > 0);
        } else {
            assert_eq!(lookups, 0);
        }
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&edit, EDIT_ONE);
        assert_eq!(
            mtimes,
            [
                std::fs::metadata(&original).unwrap().modified().unwrap(),
                std::fs::metadata(&edit).unwrap().modified().unwrap()
            ]
        );
        assert_eq!(snapshot(), before);
        assert_eq!(identities(), before_identities);
        assert!(!new_dir.join("IMG_0501.JPG").exists());
        assert!(!new_dir.join("IMG_0501_original.JPG").exists());
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("changed-token")
        );
        assert!(db.get_pending().await.unwrap().is_empty());
        assert_eq!(
            db.get_pending_metadata_rewrites(100).await.unwrap().len(),
            expected_metadata_debt
        );
        drop(db);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_destinations WHERE child='asset-501' AND (admitted=0 OR verified_media=0)"
            ),
            0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            0
        );
        eprintln!(
            "ROOT_TEMPLATE_REUSE_ROUND pass={pass_kind:?} folder={next_folder} round={round} old_identity_proofs_stable=true GETs={requests} downloads=0 metadata_debt={expected_metadata_debt}"
        );
    }
    // This synthetic Album session reports one selected member; its root
    // preservation is covered above. The Unfiled and SmartFolder fixtures
    // below also exercise a genuinely new download at the new root.
    if pass_kind == PassKind::Album {
        root.disable_cleanup(false);
        return;
    }
    const NEW: &[u8] = b"\xff\xd8\xff\xe0new independent asset bytes";
    Mock::given(method("GET"))
        .and(path("/new-resource"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(NEW))
        .mount(&server)
        .await;
    let mut new_page = full_album_page_with_download(
        "PrimarySync",
        "502",
        "selected-token",
        &format!("{}/new-resource", server.uri()),
        NEW.len() as u64,
        &provider_hash(NEW),
    );
    new_page["records"][0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    let mut both = current;
    both.extend(new_page["records"].as_array().unwrap().iter().cloned());
    let (new, _) = cycle_options(
        root.path(),
        both,
        false,
        EditedNaming::Primary,
        options("new-root", next_folder, "new-token"),
    )
    .await;
    assert_eq!(new.failed_count, 0, "{:?}", new.stats);
    assert_eq!(new.stats.downloaded, 1, "{:?}", new.stats);
    assert_bytes(&new_dir.join("IMG_0502.JPG"), NEW);
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&edit, EDIT_ONE);
    assert_eq!(
        mtimes,
        [
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            std::fs::metadata(&edit).unwrap().modified().unwrap()
        ]
    );
    assert_eq!(snapshot(), before);
    assert_eq!(identities(), before_identities);
    let actual_requests = server.received_requests().await.unwrap();
    assert_eq!(actual_requests.len(), requests + 1);
    assert_eq!(
        actual_requests
            .iter()
            .filter(|r| r.url.path() == "/new-resource")
            .count(),
        1
    );
    eprintln!(
        "ROOT_TEMPLATE_REUSE_NEW pass={pass_kind:?} folder={next_folder} path={} GETs=1 old_proofs_unchanged=true",
        new_dir.join("IMG_0502.JPG").display()
    );
    root.disable_cleanup(false);
}

#[tokio::test]
async fn root_change_preserves_verified_paths_and_places_new_asset_in_new_root() {
    root_change_preserves_verified_paths_for_pass(PassKind::Unfiled, "first").await;
}

#[tokio::test]
async fn root_change_preserves_verified_album_paths() {
    root_change_preserves_verified_paths_for_pass(PassKind::Album, "first").await;
}

#[tokio::test]
async fn root_change_preserves_verified_smart_folder_paths() {
    root_change_preserves_verified_paths_for_pass(PassKind::SmartFolder, "first").await;
}

#[cfg(unix)]
#[tokio::test]
async fn root_change_unverifiable_recorded_path_stays_held() {
    use std::os::unix::fs::symlink;
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    for case in [
        "changed",
        "missing",
        "leaf-link",
        "ancestor-link",
        "stale",
        "transaction-fault",
        "late-owner",
        "foreign-owner",
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let current = records(&server, Some(("edit-one", EDIT_ONE)));
        let options = |directory, token| CycleOptions {
            private: true,
            media_directory: directory,
            folder_structure: "first",
            baseline_successor: Some("initial-token"),
            cycle_token: Some(token),
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Primary,
            options("media", "initial-token"),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{case}: {:?}", initial.stats);
        assert_eq!(initial.stats.downloaded, 2);
        let old_dir = root.path().join("media/first");
        let original = old_dir.join("IMG_0501_original.JPG");
        let edit = old_dir.join("IMG_0501.JPG");
        let original_mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
        let edit_mtime = std::fs::metadata(&edit).unwrap().modified().unwrap();
        let displaced = root.path().join("displaced-original");
        let moved_dir = root.path().join("displaced-dir");
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let receipts: Vec<String> = conn.prepare("SELECT json_array(id,scope,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts ORDER BY version_size").unwrap().query_map([], |r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(receipts.len(), 2);
        let metadata_before: Vec<String> = conn.prepare(
            "SELECT json_array(library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,metadata_write_failed_at,preserved_path_key) FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size,local_path"
        ).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(metadata_before.len(), 2);
        drop(conn);
        let hook = match case {
            "changed" => {
                std::fs::write(&original, b"local changed original").unwrap();
                None
            }
            "missing" => {
                std::fs::rename(&original, &displaced).unwrap();
                None
            }
            "leaf-link" => {
                std::fs::rename(&original, &displaced).unwrap();
                symlink(&displaced, &original).unwrap();
                None
            }
            "ancestor-link" => {
                std::fs::rename(&old_dir, &moved_dir).unwrap();
                symlink(&moved_dir, &old_dir).unwrap();
                None
            }
            "foreign-owner" => {
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum) VALUES ('Other','foreign','original',?1,'foreign')", [original.to_str().unwrap()]).unwrap();
                None
            }
            "transaction-fault" => {
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                conn.execute_batch("CREATE TRIGGER root_marker_fault BEFORE UPDATE OF preserved_path_key ON asset_metadata_paths WHEN NEW.preserved_path_key IS NOT NULL BEGIN SELECT RAISE(ABORT, 'root-proof-marker-fault'); END;").unwrap();
                None
            }
            "late-owner" => {
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                conn.execute_batch("CREATE TRIGGER root_marker_late_owner AFTER UPDATE OF preserved_path_key ON asset_metadata_paths WHEN NEW.library='PrimarySync' AND NEW.preserved_path_key IS NOT NULL BEGIN INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum) VALUES('Other','late-owner',NEW.version_size,NEW.local_path,'foreign'); END;").unwrap();
                None
            }
            "stale" => Some(crate::test_helpers::primary_source_mutation(
                original.clone(),
                b"changed after retained proof".to_vec(),
                false,
            )),
            _ => {
                assert!(
                    matches!(
                        case,
                        "changed"
                            | "missing"
                            | "leaf-link"
                            | "ancestor-link"
                            | "stale"
                            | "transaction-fault"
                            | "late-owner"
                            | "foreign-owner"
                    ),
                    "unknown case: {case}"
                );
                None
            }
        };
        let trace_before = capture.events().len();
        let (result, _) = cycle_options(
            root.path(),
            current,
            false,
            EditedNaming::Primary,
            options("new-root", "changed-token"),
        )
        .await;
        assert!(
            result.failed_count > 0 || !result.db_sync_token_advance_safe,
            "{case}: {result:?}"
        );
        assert!(result.stats.assets_seen > 0, "{case}: {result:?}");
        assert_eq!(result.stats.downloaded, 0, "{case}");
        if let Some(hook) = hook {
            assert!(hook.fired(), "{case}: proof boundary was not reached");
        }
        match case {
            "changed" => assert_bytes(&original, b"local changed original"),
            "stale" => assert_bytes(&original, b"changed after retained proof"),
            "foreign-owner" => {
                assert_bytes(&original, ORIGINAL);
                assert_eq!(
                    db_count(
                        root.path(),
                        "SELECT COUNT(*) FROM asset_metadata_paths WHERE library='Other' AND id='foreign'"
                    ),
                    1
                );
            }
            "transaction-fault" => {
                assert_bytes(&original, ORIGINAL);
                assert!(
                    capture.events().iter().any(|event| event
                        .fields
                        .values()
                        .any(|value| value.contains("root-proof-marker-fault"))),
                    "transaction fault did not fire"
                );
            }
            "late-owner" => {
                assert_bytes(&original, ORIGINAL);
                assert_eq!(
                    std::fs::metadata(&original).unwrap().modified().unwrap(),
                    original_mtime
                );
                assert_eq!(
                    std::fs::metadata(&edit).unwrap().modified().unwrap(),
                    edit_mtime
                );
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                let metadata_after: Vec<String> = conn.prepare(
                    "SELECT json_array(library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,metadata_write_failed_at,preserved_path_key) FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size,local_path"
                ).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
                assert_eq!(metadata_after, metadata_before);
                drop(conn);
                assert!(
                    capture.events().iter().skip(trace_before).any(|event| event
                        .fields
                        .values()
                        .any(|value| value.contains(
                            "Provider generation conflicts with unfinished projected work"
                        ))),
                    "late owner did not reach the conflict guard"
                );
                assert_eq!(
                    db_count(
                        root.path(),
                        "SELECT COUNT(*) FROM asset_metadata_paths WHERE library='Other' AND id='late-owner'"
                    ),
                    0,
                    "foreign row must roll back with the admission"
                );
            }
            "missing" | "leaf-link" => assert_bytes(&displaced, ORIGINAL),
            "ancestor-link" => {
                assert_bytes(&moved_dir.join("IMG_0501_original.JPG"), ORIGINAL);
                assert_bytes(&moved_dir.join("IMG_0501.JPG"), EDIT_ONE);
            }
            _ => assert!(
                matches!(
                    case,
                    "changed"
                        | "missing"
                        | "leaf-link"
                        | "ancestor-link"
                        | "stale"
                        | "transaction-fault"
                        | "late-owner"
                        | "foreign-owner"
                ),
                "unknown case: {case}"
            ),
        }
        if case != "ancestor-link" {
            assert_bytes(&edit, EDIT_ONE);
        }
        if case == "leaf-link" {
            assert!(
                std::fs::symlink_metadata(&original)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        if case == "ancestor-link" {
            assert!(
                std::fs::symlink_metadata(&old_dir)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert!(!root.path().join("new-root/first/IMG_0501.JPG").exists());
        assert!(
            !root
                .path()
                .join("new-root/first/IMG_0501_original.JPG")
                .exists()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let after: Vec<String> = conn.prepare("SELECT json_array(id,scope,child,master,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts ORDER BY version_size").unwrap().query_map([], |r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(after, receipts, "{case}");
        let false_work: i64 = conn.query_row("SELECT COUNT(*) FROM provider_active_destinations WHERE child='asset-501' AND path LIKE '%new-root%'", [], |r| r.get(0)).unwrap();
        assert_eq!(false_work, 0, "{case}");
        let checkpoint: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint, "initial-token", "{case}");
    }
}

#[tokio::test]
async fn root_change_receipt_lookup_requires_exact_authenticated_identity() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = CycleOptions {
        private: true,
        baseline_successor: Some("initial-token"),
        ..CycleOptions::default()
    };
    let (initial, _) =
        cycle_options(root.path(), current, false, EditedNaming::Primary, options).await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert_eq!(initial.stats.downloaded, 2);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
        .await
        .unwrap();
    let scope = json!({"container":"com.apple.photos.cloud", "database":"private", "environment":"production", "format":1, "realm":"com", "zone":{"ownerRecordName":"_defaultOwner", "zoneName":"PrimarySync"}}).to_string();
    let binding = db
        .bind_publication(
            owner,
            state::db::PublicationBinding {
                account_key: String::new(),
                provider_key: String::new(),
                scope,
                library: "PrimarySync".into(),
                child: "asset-501".into(),
                master: "501".into(),
                state_id: "asset-501".into(),
                origin_pass: "7de366311251c17f1e952c68059e8b40b6c3a25d8a01646bf24c4dbd61202256"
                    .into(),
            },
        )
        .await
        .unwrap();
    let find =
        |binding: state::db::PublicationBinding, version: &'static str, checksum: String, size| {
            db.bound_download_receipts(binding, version.into(), checksum, size)
        };
    let exact = find(
        binding.clone(),
        "original",
        provider_hash(ORIGINAL),
        ORIGINAL.len() as u64,
    )
    .await
    .unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(
        exact[0].native_path.to_path(),
        root.path().join("media/IMG_0501_original.JPG")
    );
    for field in [
        "account",
        "provider",
        "scope",
        "zone-owner",
        "library",
        "child",
        "master",
        "state",
        "pass",
    ] {
        let mut wrong = binding.clone();
        match field {
            "account" => wrong.account_key.push_str("-other"),
            "provider" => wrong.provider_key.push_str("-other"),
            "scope" => wrong.scope = wrong.scope.replace("\"private\"", "\"shared\""),
            "zone-owner" => wrong.scope = wrong.scope.replace("_defaultOwner", "other-owner"),
            "library" => wrong.library.push_str("-other"),
            "child" => wrong.child.push_str("-other"),
            "master" => wrong.master.push_str("-other"),
            "state" => wrong.state_id.push_str("-other"),
            "pass" => wrong.origin_pass.push_str("-other"),
            _ => assert!(
                matches!(
                    field,
                    "account"
                        | "provider"
                        | "scope"
                        | "zone-owner"
                        | "library"
                        | "child"
                        | "master"
                        | "state"
                        | "pass"
                ),
                "unknown field: {field}"
            ),
        }
        let result = find(
            wrong,
            "original",
            provider_hash(ORIGINAL),
            ORIGINAL.len() as u64,
        )
        .await;
        assert!(
            !result.as_ref().is_ok_and(|values| !values.is_empty()),
            "{field}: {result:?}"
        );
    }
    assert!(
        find(
            binding.clone(),
            "adjusted",
            provider_hash(ORIGINAL),
            ORIGINAL.len() as u64
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        find(
            binding.clone(),
            "original",
            provider_hash(EDIT_ONE),
            ORIGINAL.len() as u64
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        find(
            binding,
            "original",
            provider_hash(ORIGINAL),
            ORIGINAL.len() as u64 + 1
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert_bytes(&root.path().join("media/IMG_0501_original.JPG"), ORIGINAL);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_ONE);
}

#[tokio::test]
async fn root_change_does_not_settle_independent_unfinished_asset() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = |directory, token| CycleOptions {
        private: true,
        media_directory: directory,
        folder_structure: "first",
        baseline_successor: Some("initial-token"),
        cycle_token: Some(token),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options("media", "initial-token"),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert_eq!(initial.stats.downloaded, 2);
    let original = root.path().join("media/first/IMG_0501_original.JPG");
    let edited = root.path().join("media/first/IMG_0501.JPG");
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&edited, EDIT_ONE);
    let times = [
        std::fs::metadata(&original).unwrap().modified().unwrap(),
        std::fs::metadata(&edited).unwrap().modified().unwrap(),
    ];
    const OTHER: &[u8] = b"\xff\xd8\xff\xe0unfinished other asset";
    let mut other = full_album_page_with_download(
        "PrimarySync",
        "502",
        "selected-token",
        &format!("{}/unavailable", server.uri()),
        OTHER.len() as u64,
        &provider_hash(OTHER),
    );
    other["records"][0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    let mut both = current.clone();
    both.extend(other["records"].as_array().unwrap().iter().cloned());
    let (failed, _) = cycle_options(
        root.path(),
        both,
        false,
        EditedNaming::Primary,
        options("media", "pending-token"),
    )
    .await;
    assert!(
        failed.failed_count > 0 || !failed.db_sync_token_advance_safe,
        "{:?}",
        failed.stats
    );
    let snapshot = || {
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.prepare("SELECT json_array(generation,pass_key,child,version_size,path,checksum,size_bytes,admitted,verified_media,verified_metadata) FROM provider_active_destinations WHERE child='asset-502' AND verified_media=0 ORDER BY generation,version_size,path").unwrap().query_map([], |r|r.get::<_, String>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
    };
    let pending_before = snapshot();
    assert!(
        !pending_before.is_empty(),
        "a real failed transfer must leave independent selected work: {failed:?}"
    );
    let (changed, _) = cycle_options(
        root.path(),
        current,
        false,
        EditedNaming::Primary,
        options("new-root", "changed-token"),
    )
    .await;
    assert!(changed.stats.assets_seen > 0, "{:?}", changed.stats);
    assert_bytes(&original, ORIGINAL);
    assert_bytes(&edited, EDIT_ONE);
    assert_eq!(
        times,
        [
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            std::fs::metadata(&edited).unwrap().modified().unwrap()
        ]
    );
    let pending_after = snapshot();
    for prior in &pending_before {
        assert!(
            pending_after.contains(prior),
            "independent obligation disappeared: {prior}; after={pending_after:?}"
        );
    }
    assert!(
        !root
            .path()
            .join("new-root/first/IMG_0501_original.JPG")
            .exists()
    );
    assert!(!root.path().join("new-root/first/IMG_0501.JPG").exists());
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    // The active destination, not the compatibility row's particular pending
    // versus failed status, is the durable owner of this unfinished work.
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let states: Vec<String> = conn
        .prepare("SELECT status FROM assets WHERE library='PrimarySync' AND id='asset-502'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        states.iter().all(|status| status != "downloaded"),
        "unfinished destination was reported downloaded: {states:?}"
    );
    assert_eq!(
        db.get_pending_metadata_rewrites(100)
            .await
            .unwrap()
            .iter()
            .filter(|asset| asset.id.as_ref() == "asset-501")
            .count(),
        2
    );
}

#[tokio::test]
async fn root_change_refuses_pending_projection_corrupted_between_checks() {
    use crate::state::db::provider_generations::root_test_cache_reads;
    for corruption in ["missing", "non-admitted", "false-complete"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("state.db");
        let current = records(&server, Some(("edit-one", EDIT_ONE)));
        let options = |directory, token| CycleOptions {
            private: true,
            media_directory: directory,
            folder_structure: "first",
            baseline_successor: Some("initial-token"),
            cycle_token: Some(token),
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Primary,
            options("media", "initial-token"),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{corruption}: {:?}", initial.stats);
        assert_eq!(initial.stats.downloaded, 2);
        let original = root.path().join("media/first/IMG_0501_original.JPG");
        let edited = root.path().join("media/first/IMG_0501.JPG");
        let times = [
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            std::fs::metadata(&edited).unwrap().modified().unwrap(),
        ];
        const OTHER: &[u8] = b"\xff\xd8\xff\xe0unfinished independent asset";
        let mut other = full_album_page_with_download(
            "PrimarySync",
            "502",
            "selected-token",
            &format!("{}/unavailable", server.uri()),
            OTHER.len() as u64,
            &provider_hash(OTHER),
        );
        other["records"][0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
        let mut both = current.clone();
        both.extend(other["records"].as_array().unwrap().iter().cloned());
        let (failed, _) = cycle_options(
            root.path(),
            both,
            false,
            EditedNaming::Primary,
            options("media", "pending-token"),
        )
        .await;
        assert!(failed.failed_count > 0 || !failed.db_sync_token_advance_safe);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let pending: i64 = conn.query_row("SELECT COUNT(*) FROM provider_active_destinations WHERE child='asset-502' AND admitted=1 AND verified_media=0", [], |r| r.get(0)).unwrap();
        assert!(pending > 0, "{corruption}: missing real pending work");
        let corrupt = match corruption {
            "missing" => "DELETE FROM provider_active_destinations WHERE child='asset-502'",
            "non-admitted" => {
                "UPDATE provider_active_destinations SET admitted=0 WHERE child='asset-502'"
            }
            "false-complete" => {
                "UPDATE provider_active_destinations SET verified_media=1 WHERE child='asset-502'"
            }
            _ => panic!("unknown corruption"),
        };
        conn.execute_batch(&format!(
            "CREATE TRIGGER root_projection_corruption AFTER INSERT ON provider_active_decisions WHEN NEW.child='asset-501' BEGIN {corrupt}; END;"
        )).unwrap();
        drop(conn);
        let snapshot = || {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let read = |sql: &str| {
                conn.prepare(sql)
                    .unwrap()
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap()
            };
            (
                read(
                    "SELECT json_array(id,account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts ORDER BY id",
                ),
                read(
                    "SELECT json_array(library,id,version_size,local_path,provider_checksum,local_checksum,source_checksum,metadata_write_failed_at,preserved_path_key) FROM asset_metadata_paths WHERE id='asset-501' ORDER BY version_size,local_path",
                ),
                read(
                    "SELECT json_array(generation,pass_key,child,manifest_hash,outcome,admission,reason,admission_hash) FROM provider_active_decisions WHERE child='asset-502' ORDER BY generation,pass_key",
                ),
                read(
                    "SELECT json_array(generation,pass_key,child,library,asset_id,version_size,path,checksum,size_bytes,admitted,verified_media,verified_metadata,local_checksum,source_checksum,progress_hash) FROM provider_active_destinations WHERE child='asset-502' ORDER BY generation,version_size,path",
                ),
                read(
                    "SELECT json_array(page_id,ordinal,config_hash,confirmation_hash,version_size,library,asset_id,checksum,size_bytes,filename,metadata_hash) FROM provider_work_obligations WHERE asset_id='asset-502' ORDER BY page_id,ordinal,version_size",
                ),
                conn.query_row::<String, _, _>(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
            )
        };
        let before = snapshot();
        assert!(
            !before.2.is_empty(),
            "{corruption}: retained decisions missing"
        );
        let reads = root_test_cache_reads(&db_path);
        let changed = try_cycle_options(
            root.path(),
            current,
            false,
            EditedNaming::Primary,
            options("new-root", "changed-token"),
        )
        .await;
        assert!(
            root_test_cache_reads(&db_path) >= reads + 7,
            "{corruption}: the post-insert index refresh was not reached; result={changed:?}"
        );
        match changed {
            Ok((result, _)) => {
                assert!(
                    result.stats.assets_seen > 0,
                    "{corruption}: {:?}",
                    result.stats
                );
                assert!(
                    result.failed_count > 0 || !result.db_sync_token_advance_safe,
                    "{corruption}: {:?}",
                    result.stats
                );
                assert_eq!(result.stats.downloaded, 0);
            }
            Err(error) => assert!(
                error.to_string().contains("Provider selection provenance"),
                "{corruption}: {error:#}"
            ),
        }
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&edited, EDIT_ONE);
        assert_eq!(
            times,
            [
                std::fs::metadata(&original).unwrap().modified().unwrap(),
                std::fs::metadata(&edited).unwrap().modified().unwrap(),
            ]
        );
        assert!(
            !root
                .path()
                .join("new-root/first/IMG_0501_original.JPG")
                .exists()
        );
        assert!(!root.path().join("new-root/first/IMG_0501.JPG").exists());
        assert_eq!(
            snapshot(),
            before,
            "{corruption}: prior work, receipts, metadata, or checkpoint changed"
        );
    }
}

#[tokio::test]
async fn root_candidate_completed_decision_census_and_generation_refresh_scale() {
    use crate::state::db::provider_generations::{
        root_test_cache_counts, root_test_pending_path, root_test_reset_cache_counts,
    };
    for size in [8, 32] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let mut page = Vec::new();
        for index in 0..size {
            let id = format!("{}", 600 + index);
            let url = if index + 1 == size {
                format!("{}/unavailable", server.uri())
            } else {
                format!("{}/original", server.uri())
            };
            let mut records = full_album_page_with_download(
                "PrimarySync",
                &id,
                "selected-token",
                &url,
                ORIGINAL.len() as u64,
                &provider_hash(ORIGINAL),
            );
            records["records"][0]["fields"]["filenameEnc"]["value"] =
                json!(format!("IMG_{id}.JPG"));
            page.extend(records["records"].as_array().unwrap().iter().cloned());
        }
        let options = CycleOptions {
            reported_count: Some(size),
            private: true,
            media_directory: "media",
            folder_structure: "first",
            baseline_successor: Some("initial-token"),
            cycle_token: Some("workload-token"),
            ..CycleOptions::default()
        };
        let (result, _) =
            cycle_options(root.path(), page, false, EditedNaming::Primary, options).await;
        assert_eq!(
            result.stats.downloaded,
            size - 1,
            "size={size}: {:?}",
            result.stats
        );
        assert!(result.failed_count > 0 || !result.db_sync_token_advance_safe);
        let db_path = root.path().join("state.db");
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = state::SqliteStateDb::open_owned(&db_path, &owner)
            .await
            .unwrap();
        let conn = db.acquire_lock("completed decision workload").unwrap();
        let retained: i64 = conn
            .query_row("SELECT COUNT(*) FROM provider_active_decisions", [], |r| {
                r.get(0)
            })
            .unwrap();
        let pending: i64 = conn.query_row("SELECT COUNT(DISTINCT generation||':'||pass_key||':'||child) FROM provider_active_destinations WHERE admitted=1 AND verified_media=0", [], |r| r.get(0)).unwrap();
        let retained = usize::try_from(retained).unwrap();
        let pending = usize::try_from(pending).unwrap();
        assert!(retained >= size, "size={size}, retained={retained}");
        assert!(pending > 0);
        root_test_reset_cache_counts();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(
            root_test_pending_path(&conn, &root.path().join("unrelated"))
                .unwrap()
                .is_empty()
        );
        conn.execute_batch("COMMIT").unwrap();
        let after_build = root_test_cache_counts();
        assert_eq!(after_build[0], 1);
        assert!(
            after_build[2] >= retained,
            "size={size}, stats={after_build:?}"
        );
        for _ in 0..8 {
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            assert!(
                root_test_pending_path(&conn, &root.path().join("unrelated"))
                    .unwrap()
                    .is_empty()
            );
            conn.execute_batch("COMMIT").unwrap();
        }
        assert_eq!(
            root_test_cache_counts(),
            after_build,
            "no unchanged lookup may rescan decisions"
        );
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.execute(
            "UPDATE provider_active_generations SET last_replayed_at=last_replayed_at+1",
            [],
        )
        .unwrap();
        assert!(
            root_test_pending_path(&conn, &root.path().join("unrelated"))
                .unwrap()
                .is_empty()
        );
        conn.execute_batch("COMMIT").unwrap();
        let after_generation = root_test_cache_counts();
        assert_eq!(after_generation[0], 1);
        assert_eq!(
            after_generation, after_build,
            "replay-only generation update must not rescan decisions"
        );
        for _ in 0..8 {
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            conn.execute(
                "UPDATE provider_active_generations SET last_replayed_at=last_replayed_at+1",
                [],
            )
            .unwrap();
            assert!(
                root_test_pending_path(&conn, &root.path().join("unrelated"))
                    .unwrap()
                    .is_empty()
            );
            conn.execute_batch("COMMIT").unwrap();
        }
        assert_eq!(
            root_test_cache_counts(),
            after_generation,
            "repeated replay updates must not rescan decisions"
        );
        conn.execute_batch("BEGIN IMMEDIATE; SAVEPOINT bad_cursor")
            .unwrap();
        conn.execute(
            "UPDATE provider_active_generations SET replay_after='[\"missing\",\"child\"]'",
            [],
        )
        .unwrap();
        assert!(root_test_pending_path(&conn, &root.path().join("unrelated")).is_err());
        conn.execute_batch("ROLLBACK TO bad_cursor; RELEASE bad_cursor")
            .unwrap();
        assert!(
            root_test_pending_path(&conn, &root.path().join("unrelated"))
                .unwrap()
                .is_empty()
        );
        conn.execute_batch("SAVEPOINT bad_generation").unwrap();
        conn.execute(
            "UPDATE provider_active_generations SET config_hash='damaged'",
            [],
        )
        .unwrap();
        assert!(root_test_pending_path(&conn, &root.path().join("unrelated")).is_err());
        conn.execute_batch("ROLLBACK TO bad_generation; RELEASE bad_generation")
            .unwrap();
        assert!(
            root_test_pending_path(&conn, &root.path().join("unrelated"))
                .unwrap()
                .is_empty()
        );
        conn.execute_batch("COMMIT").unwrap();
        eprintln!(
            "root completed workload: size={size}, retained={retained}, pending={pending}, build={after_build:?}, generation_refresh={after_generation:?}"
        );
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn selected_metadata_fault_attempts_exact_receipt_once_per_tail() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut page = records(&server, None);
    let opts = || CycleOptions {
        private: true,
        sidecar: true,
        refresh_metadata: true,
        baseline_successor: Some("initial-token"),
        cycle_token: Some("cycle-token"),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        page.clone(),
        false,
        EditedNaming::Primary,
        opts(),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{initial:?}");
    let media = root.path().join("media/IMG_0501.JPG");
    let sidecar = root.path().join("media/IMG_0501.JPG.xmp");
    assert_bytes(&media, ORIGINAL);
    let (healthy, _) = cycle_options(
        root.path(),
        page.clone(),
        false,
        EditedNaming::Primary,
        opts(),
    )
    .await;
    assert_eq!(healthy.stats.exif_failures, 0, "{healthy:?}");
    let token = || {
        rusqlite::Connection::open(root.path().join("state.db"))
            .unwrap()
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap()
    };
    let before_token = token();
    std::fs::remove_file(&sidecar).unwrap();
    std::fs::create_dir(&sidecar).unwrap();
    page[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
    page[1]["recordChangeTag"] = json!("changed-metadata");
    let attempts = crate::download::metadata_rewrite::test_sidecar_attempts(&media);
    let mut readonly = opts();
    readonly.controls = download::DownloadControls::dry_run_hidden();
    let _ = cycle_options(
        root.path(),
        page.clone(),
        false,
        EditedNaming::Primary,
        readonly,
    )
    .await;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "read-only execution must not enter the writer"
    );
    let suppressions = crate::download::metadata_rewrite::test_metadata_suppressions(&media);
    let before_ordinary = Arc::new(AtomicUsize::new(usize::MAX));
    let after_ordinary = Arc::new(AtomicUsize::new(usize::MAX));
    let first_skips = Arc::new(AtomicUsize::new(usize::MAX));
    let seen_priority = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_tail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe_hook = attempts.clone();
    let skips_hook = suppressions.clone();
    let before_hook = before_ordinary.clone();
    let after_hook = after_ordinary.clone();
    let first_skips_hook = first_skips.clone();
    crate::download::metadata_rewrite::test_metadata_tail_hook(
        &root.path().join("media"),
        Arc::new(move |after_priority| {
            if after_priority && !seen_priority.swap(true, Ordering::SeqCst) {
                before_hook.store(probe_hook.load(Ordering::SeqCst), Ordering::SeqCst);
            } else if !after_priority && !seen_tail.swap(true, Ordering::SeqCst) {
                after_hook.store(probe_hook.load(Ordering::SeqCst), Ordering::SeqCst);
                first_skips_hook.store(skips_hook.load(Ordering::SeqCst), Ordering::SeqCst);
            }
        }),
    );
    let (fault, _) = cycle_options(
        root.path(),
        page.clone(),
        false,
        EditedNaming::Primary,
        opts(),
    )
    .await;
    assert_eq!(
        before_ordinary.load(Ordering::SeqCst),
        1,
        "priority must reach the actual sidecar destination probe"
    );
    assert_eq!(
        after_ordinary.load(Ordering::SeqCst),
        1,
        "the identical receipt must not probe twice in the same tail"
    );
    assert_eq!(
        first_skips.load(Ordering::SeqCst),
        1,
        "ordinary must skip that same attempted receipt"
    );
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "a distinct later generation must still reach the probe"
    );
    assert!(fault.stats.exif_failures >= 2, "{fault:?}");
    assert_eq!(token(), before_token);
    assert_bytes(&media, ORIGINAL);
    assert_eq!(
        db_count(root.path(), "SELECT count(*) FROM download_receipts"),
        1
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1 AND verified_metadata=0"
        ),
        2
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM asset_metadata_paths WHERE metadata_write_failed_at IS NOT NULL"
        ),
        1
    );
    std::fs::remove_dir(&sidecar).unwrap();
    let (recovery, _) = cycle_options(
        root.path(),
        page.clone(),
        false,
        EditedNaming::Primary,
        opts(),
    )
    .await;
    assert_eq!(recovery.stats.exif_failures, 0, "{recovery:?}");
    assert!(
        std::fs::read_to_string(&sidecar)
            .unwrap()
            .contains("<xmp:Rating>5</xmp:Rating>")
    );
    assert_bytes(&media, ORIGINAL);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_metadata=0"
        ),
        0
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM asset_metadata_paths WHERE metadata_write_failed_at IS NOT NULL"
        ),
        0
    );
    let before_later = attempts.load(Ordering::SeqCst);
    let (later, _) = cycle_options(root.path(), page, true, EditedNaming::Primary, opts()).await;
    assert_eq!(later.stats.exif_failures, 0, "{later:?}");
    assert!(
        attempts.load(Ordering::SeqCst) > before_later,
        "a later explicit refresh must enter the writer"
    );
    assert_bytes(&media, ORIGINAL);
}

#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn selected_metadata_same_tail_refusal_and_identity_changes_do_not_suppress() {
    for scenario in ["target", "parent", "receipt", "prewrite"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let mut page = records(&server, None);
        let opts = || CycleOptions {
            private: true,
            sidecar: true,
            refresh_metadata: true,
            baseline_successor: Some("initial-token"),
            cycle_token: Some("cycle-token"),
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            page.clone(),
            false,
            EditedNaming::Primary,
            opts(),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{scenario}: {initial:?}");
        let media_root = root.path().join("media");
        let media = media_root.join("IMG_0501.JPG");
        let sidecar = media_root.join("IMG_0501.JPG.xmp");
        std::fs::remove_file(&sidecar).unwrap();
        std::fs::create_dir(&sidecar).unwrap();
        page[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
        page[1]["recordChangeTag"] = json!("changed-metadata");
        let attempts = crate::download::metadata_rewrite::test_sidecar_attempts(&media);
        let suppressed = crate::download::metadata_rewrite::test_metadata_suppressions(&media);
        if scenario == "prewrite" {
            crate::download::metadata_rewrite::test_deny_metadata_writer_once(&media);
        }
        let before_ordinary = Arc::new(AtomicUsize::new(usize::MAX));
        let after_ordinary = Arc::new(AtomicUsize::new(usize::MAX));
        let first_tail_skips = Arc::new(AtomicUsize::new(usize::MAX));
        let first_priority = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first_tail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let attempt_hook = attempts.clone();
        let before_hook = before_ordinary.clone();
        let after_hook = after_ordinary.clone();
        let skips_hook = suppressed.clone();
        let first_tail_skips_hook = first_tail_skips.clone();
        let parent = media_root.clone();
        let new_sidecar = sidecar.clone();
        let db_path = root.path().join("state.db");
        let moved = root.path().join("moved-media");
        let moved_hook = moved.clone();
        crate::download::metadata_rewrite::test_metadata_tail_hook(
            &media_root,
            Arc::new(move |after_priority| {
                if after_priority && !first_priority.swap(true, Ordering::SeqCst) {
                    before_hook.store(attempt_hook.load(Ordering::SeqCst), Ordering::SeqCst);
                    match scenario {
                        "target" => std::fs::remove_dir(&new_sidecar).unwrap(),
                        "parent" => {
                            std::fs::rename(&parent, &moved_hook).unwrap();
                            std::fs::create_dir(&parent).unwrap();
                            std::fs::write(parent.join("IMG_0501.JPG"), ORIGINAL).unwrap();
                        }
                        "receipt" => {
                            let conn = rusqlite::Connection::open(&db_path).unwrap();
                            let changed = conn.execute("UPDATE provider_active_destinations SET admitted=0 WHERE admitted=1 AND verified_metadata=0", []).unwrap();
                            assert_eq!(changed, 1);
                        }
                        "prewrite" => {}
                        _ => panic!("unexpected scenario: {scenario}"),
                    }
                } else if !after_priority && !first_tail.swap(true, Ordering::SeqCst) {
                    after_hook.store(attempt_hook.load(Ordering::SeqCst), Ordering::SeqCst);
                    first_tail_skips_hook
                        .store(skips_hook.load(Ordering::SeqCst), Ordering::SeqCst);
                }
            }),
        );
        let result =
            try_cycle_options(root.path(), page, false, EditedNaming::Primary, opts()).await;
        assert_ne!(
            before_ordinary.load(Ordering::SeqCst),
            usize::MAX,
            "{scenario}: priority not reached"
        );
        if scenario == "receipt" {
            let error = result.expect_err("invalidated receipt must fail closed");
            assert!(
                error.to_string().contains("selection provenance"),
                "{error:#}"
            );
            assert_eq!(suppressed.load(Ordering::SeqCst), 0);
            assert_eq!(before_ordinary.load(Ordering::SeqCst), 1);
            assert_bytes(&media, ORIGINAL);
            continue;
        }
        let (result, _) = result.unwrap();
        assert_ne!(
            after_ordinary.load(Ordering::SeqCst),
            usize::MAX,
            "{scenario}: ordinary tail not reached"
        );
        assert_eq!(
            first_tail_skips.load(Ordering::SeqCst),
            0,
            "{scenario}: changed or unattempted work must not be suppressed in the intervened tail"
        );
        match scenario {
            "target" => {
                assert_eq!(before_ordinary.load(Ordering::SeqCst), 1);
                assert_eq!(
                    after_ordinary.load(Ordering::SeqCst),
                    2,
                    "same receipt must retry when its target changes"
                );
                assert!(
                    std::fs::read_to_string(&sidecar)
                        .unwrap()
                        .contains("<xmp:Rating>5</xmp:Rating>")
                );
            }
            "parent" => {
                assert_eq!(before_ordinary.load(Ordering::SeqCst), 1);
                assert_bytes(&media, ORIGINAL);
                assert_bytes(&moved.join("IMG_0501.JPG"), ORIGINAL);
                match after_ordinary.load(Ordering::SeqCst) {
                    1 => assert!(
                        result.failed_count > 0,
                        "a parent guard refusal must retain failure: {result:?}"
                    ),
                    2 => assert!(
                        sidecar.is_file(),
                        "a reached writer must publish into the changed namespace"
                    ),
                    count => panic!("unexpected parent-change attempt count: {count}"),
                }
            }
            "prewrite" => {
                assert_eq!(
                    before_ordinary.load(Ordering::SeqCst),
                    0,
                    "prewrite refusal entered publisher"
                );
                assert_eq!(
                    after_ordinary.load(Ordering::SeqCst),
                    1,
                    "ordinary lane must still attempt the receipt"
                );
                assert!(sidecar.is_dir());
            }
            _ => panic!("unexpected scenario: {scenario}"),
        }
    }
}

#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn selected_metadata_suppressed_page_advances_to_distinct_work() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut page = records(&server, None);
    let mut second = records(&server, None);
    second[0]["recordName"] = json!("502");
    second[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    second[1]["recordName"] = json!("asset-502");
    second[1]["fields"]["masterRef"]["value"]["recordName"] = json!("502");
    page.extend(second);
    let opts = || CycleOptions {
        private: true,
        sidecar: true,
        refresh_metadata: true,
        reported_count: Some(2),
        baseline_successor: Some("initial-token"),
        cycle_token: Some("cycle-token"),
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        page.clone(),
        false,
        EditedNaming::Primary,
        opts(),
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{initial:?}");
    assert_eq!(initial.stats.downloaded, 2);
    let media_root = root.path().join("media");
    let first = media_root.join("IMG_0501.JPG");
    let second = media_root.join("IMG_0502.JPG");
    for media in [&first, &second] {
        assert_bytes(media, ORIGINAL);
        let mut sidecar = media.as_os_str().to_os_string();
        sidecar.push(".xmp");
        let sidecar = std::path::PathBuf::from(sidecar);
        std::fs::remove_file(&sidecar).unwrap();
        std::fs::create_dir(&sidecar).unwrap();
    }
    for index in [1, 3] {
        page[index]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
        page[index]["recordChangeTag"] = json!("changed-metadata");
    }
    let first_attempts = crate::download::metadata_rewrite::test_sidecar_attempts(&first);
    let second_attempts = crate::download::metadata_rewrite::test_sidecar_attempts(&second);
    let first_skips = crate::download::metadata_rewrite::test_metadata_suppressions(&first);
    let second_skips = crate::download::metadata_rewrite::test_metadata_suppressions(&second);
    let first_tail_writes = Arc::new(AtomicUsize::new(usize::MAX));
    let first_tail_skips = Arc::new(AtomicUsize::new(usize::MAX));
    let first_tail_offset = Arc::new(AtomicUsize::new(usize::MAX));
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let a = first_attempts.clone();
    let b = second_attempts.clone();
    let sa = first_skips.clone();
    let sb = second_skips.clone();
    let writes = first_tail_writes.clone();
    let skips = first_tail_skips.clone();
    let offset = first_tail_offset.clone();
    let db_path = root.path().join("state.db");
    crate::download::metadata_rewrite::test_metadata_tail_hook(
        &media_root,
        Arc::new(move |after_priority| {
            if !after_priority && !done.swap(true, Ordering::SeqCst) {
                writes.store(
                    a.load(Ordering::SeqCst) + b.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
                skips.store(
                    sa.load(Ordering::SeqCst) + sb.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                let value: String = conn.query_row("SELECT value FROM metadata WHERE key='active_selection_metadata_retry_offset'", [], |r| r.get(0)).unwrap();
                offset.store(value.parse().unwrap(), Ordering::SeqCst);
            }
        }),
    );
    let (result, _) = crate::download::metadata_rewrite::test_metadata_budget(
        2,
        cycle_options(root.path(), page, false, EditedNaming::Primary, opts()),
    )
    .await;
    assert!(result.stats.exif_failures > 0, "{result:?}");
    assert_eq!(
        first_tail_writes.load(Ordering::SeqCst),
        1,
        "one priority writer reached"
    );
    assert_eq!(
        first_tail_skips.load(Ordering::SeqCst),
        1,
        "ordinary page contains the same receipt"
    );
    assert_eq!(
        first_tail_offset.load(Ordering::SeqCst),
        1,
        "suppression consumes the fetched page position"
    );
    assert!(first_attempts.load(Ordering::SeqCst) > 0);
    assert!(
        second_attempts.load(Ordering::SeqCst) > 0,
        "the other selected destination must progress on the next page"
    );
    assert_bytes(&first, ORIGINAL);
    assert_bytes(&second, ORIGINAL);
    assert!(
        db_count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1 AND verified_metadata=0"
        ) >= 2
    );
}

#[cfg(all(feature = "xmp", unix))]
#[tokio::test]
async fn selected_metadata_preprobe_and_gps_errors_do_not_suppress_ordinary_retry() {
    for scenario in [
        "preprobe",
        "gps",
        "unchanged",
        "temp_create",
        "temp_write",
        "temp_sync",
        "publication",
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let mut page = records(&server, None);
        let opts = || CycleOptions {
            private: true,
            sidecar: true,
            refresh_metadata: true,
            baseline_successor: Some("initial-token"),
            cycle_token: Some("cycle-token"),
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            page.clone(),
            false,
            EditedNaming::Primary,
            opts(),
        )
        .await;
        assert_eq!(initial.failed_count, 0, "{scenario}: {initial:?}");
        let media_root = root.path().join("media");
        let media = media_root.join("IMG_0501.JPG");
        let sidecar = media_root.join("IMG_0501.JPG.xmp");
        let original_sidecar = std::fs::read(&sidecar).unwrap();
        if scenario == "preprobe" {
            std::fs::remove_file(&sidecar).unwrap();
            std::fs::create_dir(&sidecar).unwrap();
            crate::download::metadata_rewrite::test_fail_before_sidecar_probe_once(&media);
        } else if scenario == "gps" {
            crate::download::metadata_rewrite::test_fail_source_gps_once(&media);
        } else if scenario == "publication" {
            page[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
            page[1]["recordChangeTag"] = json!("changed-metadata");
            crate::download::metadata_rewrite::test_corrupt_publication_once(&sidecar);
        } else {
            use crate::download::metadata_rewrite::TestPreparationFault;
            let fault = match scenario {
                "unchanged" => TestPreparationFault::UnchangedValidation,
                "temp_create" => TestPreparationFault::TempCreate,
                "temp_write" => TestPreparationFault::TempWrite,
                "temp_sync" => TestPreparationFault::TempSync,
                _ => panic!("unexpected scenario: {scenario}"),
            };
            if scenario != "unchanged" {
                page[1]["fields"]["isFavorite"] = json!({"value":1,"type":"INT64"});
                page[1]["recordChangeTag"] = json!("changed-metadata");
            }
            crate::download::metadata_rewrite::test_fail_sidecar_preparation_once(&media, fault);
        }
        let probes = crate::download::metadata_rewrite::test_sidecar_attempts(&media);
        let successes = crate::download::metadata_rewrite::test_sidecar_successes(&media);
        let publications = crate::download::metadata_rewrite::test_sidecar_publications(&sidecar);
        let suppressions = crate::download::metadata_rewrite::test_metadata_suppressions(&media);
        let before = Arc::new(AtomicUsize::new(usize::MAX));
        let after = Arc::new(AtomicUsize::new(usize::MAX));
        let first_debt = Arc::new(AtomicUsize::new(usize::MAX));
        let priority_successes = Arc::new(AtomicUsize::new(usize::MAX));
        let priority_publications = Arc::new(AtomicUsize::new(usize::MAX));
        let tail_publications = Arc::new(AtomicUsize::new(usize::MAX));
        let last_debt = Arc::new(AtomicUsize::new(usize::MAX));
        let first_skips = Arc::new(AtomicUsize::new(usize::MAX));
        let unchanged = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_priority = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_tail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let p = probes.clone();
        let success_hook = successes.clone();
        let priority_successes_hook = priority_successes.clone();
        let publication_hook = publications.clone();
        let priority_publications_hook = priority_publications.clone();
        let tail_publications_hook = tail_publications.clone();
        let skip = suppressions.clone();
        let before_hook = before.clone();
        let after_hook = after.clone();
        let debt_before = first_debt.clone();
        let debt_after = last_debt.clone();
        let skips = first_skips.clone();
        let unchanged_hook = unchanged.clone();
        let sidecar_hook = sidecar.clone();
        let db_path = root.path().join("state.db");
        crate::download::metadata_rewrite::test_metadata_tail_hook(
            &media_root,
            Arc::new(move |after_priority| {
                let first = if after_priority {
                    !seen_priority.swap(true, Ordering::SeqCst)
                } else {
                    !seen_tail.swap(true, Ordering::SeqCst)
                };
                if !first {
                    return;
                }
                let conn = rusqlite::Connection::open(&db_path).unwrap();
                let debt: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE admitted=1 AND verified_media=1 AND verified_metadata=0", [], |r| r.get(0)).unwrap();
                if after_priority {
                    before_hook.store(p.load(Ordering::SeqCst), Ordering::SeqCst);
                    priority_successes_hook
                        .store(success_hook.load(Ordering::SeqCst), Ordering::SeqCst);
                    priority_publications_hook
                        .store(publication_hook.load(Ordering::SeqCst), Ordering::SeqCst);
                    debt_before.store(debt as usize, Ordering::SeqCst);
                    if scenario != "preprobe" {
                        unchanged_hook.store(
                            std::fs::read(&sidecar_hook).unwrap() == original_sidecar,
                            Ordering::SeqCst,
                        );
                    }
                } else {
                    after_hook.store(p.load(Ordering::SeqCst), Ordering::SeqCst);
                    tail_publications_hook
                        .store(publication_hook.load(Ordering::SeqCst), Ordering::SeqCst);
                    debt_after.store(debt as usize, Ordering::SeqCst);
                    skips.store(skip.load(Ordering::SeqCst), Ordering::SeqCst);
                }
            }),
        );
        let (result, _) = cycle_options(
            root.path(),
            page.clone(),
            false,
            EditedNaming::Primary,
            opts(),
        )
        .await;
        assert_eq!(
            first_debt.load(Ordering::SeqCst),
            1,
            "{scenario}: priority failure must retain selected debt"
        );
        assert_eq!(
            first_skips.load(Ordering::SeqCst),
            0,
            "{scenario}: ordinary retry was suppressed"
        );
        match scenario {
            "preprobe" => {
                assert_eq!(priority_successes.load(Ordering::SeqCst), 0);
                assert_eq!(
                    before.load(Ordering::SeqCst),
                    0,
                    "failure must occur before the destination probe"
                );
                assert_eq!(
                    after.load(Ordering::SeqCst),
                    1,
                    "ordinary lane must reach the destination probe"
                );
                assert!(last_debt.load(Ordering::SeqCst) > 0);
                assert!(result.stats.exif_failures > 0);
                std::fs::remove_dir(&sidecar).unwrap();
                let (recovered, _) =
                    cycle_options(root.path(), page, false, EditedNaming::Primary, opts()).await;
                assert_eq!(recovered.stats.exif_failures, 0, "{recovered:?}");
                assert!(sidecar.is_file());
            }
            "gps" => {
                assert_eq!(
                    priority_successes.load(Ordering::SeqCst),
                    1,
                    "the sidecar owner must succeed before the independent GPS failure"
                );
                assert_eq!(before.load(Ordering::SeqCst), 1);
                assert!(
                    unchanged.load(Ordering::SeqCst),
                    "sidecar succeeded unchanged before the independent GPS failure"
                );
                assert_eq!(
                    after.load(Ordering::SeqCst),
                    2,
                    "ordinary lane must retry the independent GPS debt"
                );
                assert_eq!(
                    last_debt.load(Ordering::SeqCst),
                    0,
                    "successful ordinary retry must settle the selected receipt"
                );
            }
            "publication" => {
                assert_eq!(before.load(Ordering::SeqCst), 1);
                assert_eq!(
                    priority_publications.load(Ordering::SeqCst),
                    1,
                    "the publisher must reject the corrupted owned temporary"
                );
                assert_eq!(priority_successes.load(Ordering::SeqCst), 0);
                assert!(
                    unchanged.load(Ordering::SeqCst),
                    "the target must remain unchanged after the publisher refusal"
                );
                assert_eq!(after.load(Ordering::SeqCst), 2);
                assert_eq!(
                    tail_publications.load(Ordering::SeqCst),
                    2,
                    "ordinary must enter the publisher with a fresh temporary"
                );
                assert_eq!(last_debt.load(Ordering::SeqCst), 0);
                assert!(
                    std::fs::read_to_string(&sidecar)
                        .unwrap()
                        .contains("<xmp:Rating>5</xmp:Rating>")
                );
            }
            "unchanged" | "temp_create" | "temp_write" | "temp_sync" => {
                assert_eq!(
                    priority_successes.load(Ordering::SeqCst),
                    0,
                    "{scenario}: injected operation did not fail"
                );
                assert_eq!(
                    before.load(Ordering::SeqCst),
                    1,
                    "{scenario}: the real destination probe must have succeeded first"
                );
                assert!(
                    unchanged.load(Ordering::SeqCst),
                    "{scenario}: failed preparation changed the sidecar"
                );
                assert_eq!(
                    after.load(Ordering::SeqCst),
                    2,
                    "{scenario}: ordinary must retry after the post-probe failure"
                );
                assert_eq!(
                    last_debt.load(Ordering::SeqCst),
                    0,
                    "{scenario}: ordinary retry must settle the receipt"
                );
                if scenario != "unchanged" {
                    assert!(
                        std::fs::read_to_string(&sidecar)
                            .unwrap()
                            .contains("<xmp:Rating>5</xmp:Rating>")
                    );
                }
            }
            _ => panic!("unexpected scenario: {scenario}"),
        }
        assert_bytes(&media, ORIGINAL);
    }
}

#[cfg(all(target_os = "linux", feature = "xmp"))]
#[tokio::test]
async fn review501_unreceipted_refresh_stage_remains_safely_held() {
    let modes = [false, true];
    for sidecar in modes {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let root = tempfile::tempdir().unwrap();
        eprintln!(
            "REVIEW501_TEMP root={} sidecar={sidecar}",
            root.path().display()
        );
        let media = root.path().join("media");
        let options = |refresh_metadata| CycleOptions {
            refresh_metadata,
            private: true,
            #[cfg(feature = "xmp")]
            sidecar,
            ..CycleOptions::default()
        };
        #[cfg(not(feature = "xmp"))]
        let _ = sidecar;
        let current =
            records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
        let (initial, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Primary,
            options(false),
        )
        .await;
        assert_eq!(initial.failed_count, 0);
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let db = Arc::new(
            state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
                .await
                .unwrap(),
        );
        let source = current.clone();
        let provider = Session {
            reported_count: None,
            records: source,
            quiet: true,
            lookups: Arc::new(AtomicUsize::new(0)),
            current: None,
            lookup_change: None,
            indexing: None,
            baseline_successor: None,
            cycle_token: None,
            trace: None,
        };
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), owner, "com"),
            Arc::from("private"),
        );
        let pass = AlbumPass {
            kind: PassKind::Unfiled,
            album,
            exclude_ids: Arc::default(),
        };
        let expected_scope = json!({
            "zone": {"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
            "name":"TestAlbum",
            "list_type":"CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
            "obj_type":"CPLAssetByAssetDateWithoutHiddenOrDeleted",
            "query_filter":null, "container_id":null, "cross_zone_sources":[]
        });
        assert_eq!(pass.album.selection_scope(), expected_scope);
        let expected_pass = format!(
            "{:x}",
            Sha256::digest(
                json!({"kind":"Unfiled","scope":expected_scope,"excludes":[]})
                    .to_string()
                    .as_bytes()
            )
        );
        let store: Arc<dyn download::DownloadStore> = db.clone();
        let build = make_run_cycle_download_config_builder(&media, store);
        let mut config = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        )
        .as_ref()
        .clone();
        config.folder_structure = String::new();
        config.folder_structure_albums = Arc::from("");
        config.folder_structure_smart_folders = Arc::from("");
        config.edited = true;
        config.edited_naming = EditedNaming::Primary;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = sidecar;
        }
        let mut op = download::seed_prepared_edited_compat(&config, &pass).await;
        assert_eq!(op.phase, "prepared");
        assert_eq!(op.source_library, "PrimarySync");
        assert_eq!(op.child, "asset-501");
        assert_eq!(op.pass, expected_pass);
        assert!(op.members.iter().any(|m| {
            m.record.as_ref().is_some_and(|r| {
                r.version_size.as_str() == "adjusted"
                    && r.checksum.as_ref() == provider_hash(VALID_EDIT)
            })
        }));
        assert!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_claims WHERE operation IS NOT NULL"
            ) > 0
        );
        download::finish_prepared_edited_compat(&config, &mut op).await;
        assert!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .is_empty()
        );

        let wrong_owner = state::db::account::AccountOwner::authenticated(
            "other@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"other-provider"}})).unwrap(),
        )
        .unwrap();
        assert!(
            state::SqliteStateDb::open_owned(&root.path().join("state.db"), &wrong_owner)
                .await
                .is_err()
        );
        let xmp = sidecar.then(|| std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap());
        let requests = server.received_requests().await.unwrap().len();
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let (_generation, hash): (i64, String) = conn.query_row("SELECT b.generation,a.metadata_hash FROM primary_layout_bindings b JOIN assets a ON a.library=b.library AND a.id=b.child WHERE a.version_size='adjusted'", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        // Simulate stale catalogue capture without changing any owned media.
        conn.execute("UPDATE assets SET metadata_hash='stale-catalogue-capture' WHERE version_size='adjusted'", []).unwrap();
        drop(conn);
        // Produce the historical refresh through the layout owner, not ordinary
        // sync: current policy deliberately does not plan new layout work.
        config.refresh_metadata = true;
        let refresh_operation = download::seed_unprepared_refresh(&config, &pass).await;
        assert!(refresh_operation.refresh_metadata);
        assert_eq!(refresh_operation.child, "asset-501");
        assert_eq!(refresh_operation.pass, expected_pass);
        assert!(refresh_operation.members.iter().any(|m| m.old.is_some()));
        let recorded_stages: Vec<_> = refresh_operation
            .members
            .iter()
            .filter_map(|member| member.stage.as_ref().map(|path| path.to_path()))
            .collect();
        assert!(!recorded_stages.is_empty());
        assert!(recorded_stages.iter().all(|path| !path.exists()));
        assert_eq!(
            db.primary_layout_operations("PrimarySync".into())
                .await
                .unwrap()
                .len(),
            1
        );
        let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();
        drop(config);
        drop(build);
        drop(pass);
        drop(db);
        eprintln!("REVIEW501_REACHED recorded_authenticated_refresh sidecar={sidecar}");
        // The deliberately stale synthetic catalogue must fail closed before
        // preparation. Restore only the exact value captured above.
        let stale = try_cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(false),
        )
        .await
        .expect_err("stale catalogue cannot authorize recorded refresh");
        assert!(
            format!("{stale:#}")
                .contains("recorded layout differs from current provider observation")
        );
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
            ),
            1
        );
        eprintln!("REVIEW501_REACHED stale_catalogue_refused sidecar={sidecar}");
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        assert_eq!(conn.execute("UPDATE assets SET metadata_hash=?1 WHERE version_size='adjusted' AND metadata_hash='stale-catalogue-capture'", [&hash]).unwrap(), 1);
        conn.execute_batch("CREATE TRIGGER fail_refresh_prepare BEFORE UPDATE OF phase ON primary_layout_operations WHEN NEW.phase='prepared' BEGIN SELECT RAISE(FAIL,'injected forced-refresh preparation failure'); END").unwrap();
        drop(conn);
        let held = try_cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(false),
        )
        .await
        .expect_err("injected prepare failure must retain the recorded refresh");
        assert!(format!("{held:#}").contains("injected forced-refresh preparation failure"));
        let held_state = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            held_state
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap(),
            checkpoint
        );
        drop(held_state);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let refresh: bool = conn.query_row("SELECT json_extract(CAST(header AS TEXT),'$.refresh_metadata') FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')", [], |row| row.get(0)).unwrap();
        assert!(refresh, "forced refresh intent was not durable");
        eprintln!("REVIEW501_REACHED held_refresh sidecar={sidecar}");
        eprintln!(
            "REVIEW501_STAGE_AFTER_HOLD {:?}",
            recorded_stages
                .iter()
                .map(|p| (p, p.exists(), std::fs::metadata(p).ok().map(|m| m.len())))
                .collect::<Vec<_>>()
        );
        conn.execute_batch("DROP TRIGGER fail_refresh_prepare")
            .unwrap();
        drop(conn);
        use std::os::unix::fs::MetadataExt as _;
        fn tree(
            root: &std::path::Path,
        ) -> Vec<(std::path::PathBuf, Vec<u8>, u64, u64, std::time::SystemTime)> {
            fn walk(
                root: &std::path::Path,
                dir: &std::path::Path,
                out: &mut Vec<(std::path::PathBuf, Vec<u8>, u64, u64, std::time::SystemTime)>,
            ) {
                for entry in std::fs::read_dir(dir).unwrap() {
                    let entry = entry.unwrap();
                    let path = entry.path();
                    let meta = std::fs::symlink_metadata(&path).unwrap();
                    assert!(!meta.file_type().is_symlink());
                    if meta.is_dir() {
                        walk(root, &path, out);
                    } else {
                        out.push((
                            path.strip_prefix(root).unwrap().to_path_buf(),
                            std::fs::read(&path).unwrap(),
                            meta.dev(),
                            meta.ino(),
                            meta.modified().unwrap(),
                        ));
                    }
                }
            }
            let mut files = Vec::new();
            walk(root, root, &mut files);
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        }
        fn rows(root: &std::path::Path) -> Vec<(String, Vec<String>)> {
            let conn = rusqlite::Connection::open_with_flags(
                root.join("state.db"),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap();
            [
                "account_owner",
                "assets",
                "asset_metadata_paths",
                "primary_layout_operations",
                "primary_layout_members",
                "primary_layout_claims",
                "primary_layout_bindings",
                "primary_layout_preserved",
                "download_receipts",
                "provider_active_generations",
                "provider_active_decisions",
                "provider_active_sources",
                "provider_active_destinations",
                "metadata",
            ]
            .iter()
            .map(|table| {
                let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
                let count = stmt.column_count();
                let mut values = stmt
                    .query_map([], |row| {
                        let mut fields = Vec::new();
                        for i in 0..count {
                            fields.push(format!("{:?}", row.get::<_, rusqlite::types::Value>(i)?));
                        }
                        Ok(fields.join("|"))
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                values.sort();
                ((*table).to_owned(), values)
            })
            .collect()
        }
        if let Some(expected_xmp) = &xmp {
            assert_eq!(
                std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap(),
                *expected_xmp
            );
        }
        let files_before = tree(&media);
        let rows_before = rows(root.path());
        assert!(recorded_stages.iter().all(|path| path.exists()));
        let failure = try_cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(false),
        )
        .await
        .expect_err("unreceipted occupied stage cannot be silently completed");
        let rendered = format!("{failure:#}");
        assert!(
            rendered.contains("File exists"),
            "the caller must receive the actual error: {rendered}"
        );
        assert_eq!(
            tree(&media),
            files_before,
            "all media, sidecars and staged bytes and identities must remain unchanged"
        );
        assert_eq!(
            rows(root.path()),
            rows_before,
            "owner, receipts, journal and checkpoint must remain unchanged"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        eprintln!("REVIEW501_REACHED safe_hold_verified sidecar={sidecar}");
    }
}

#[tokio::test]
async fn ordinary_suffix_album_new_adjusted_resource_preserves_prior_publications() {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Physical {
        File(Vec<u8>, crate::fs_util::FileIdentity, std::time::SystemTime),
        Directory(std::time::SystemTime),
    }
    fn physical(path: &Path) -> Physical {
        let metadata = std::fs::symlink_metadata(path).unwrap();
        if metadata.is_dir() {
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
            return Physical::Directory(metadata.modified().unwrap());
        }
        assert!(metadata.is_file(), "unexpected physical entry: {path:?}");
        let file = std::fs::File::open(path).unwrap();
        Physical::File(
            std::fs::read(path).unwrap(),
            crate::fs_util::file_identity(&file).unwrap(),
            file.metadata().unwrap().modified().unwrap(),
        )
    }
    fn signature(bytes: &[u8]) -> String {
        use sha1::{Digest as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        hash.update(bytes);
        let signature = data_encoding::BASE64.encode(&[&[1][..], &hash.finalize()[..]].concat());
        assert_eq!(signature.len(), 28);
        assert_eq!(
            data_encoding::BASE64
                .decode(signature.as_bytes())
                .unwrap()
                .len(),
            21
        );
        signature
    }
    fn positive_records(server: &MockServer, name: &str, bytes: &[u8]) -> Vec<Value> {
        let mut values = records(server, Some((name, bytes)));
        values[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!(signature(ORIGINAL));
        values[1]["fields"]["resJPEGFullRes"]["value"]["fileChecksum"] = json!(signature(bytes));
        eprintln!(
            "ORDINARY_VALID_PROVIDER resource={name} original={} adjusted={} protocol=MMCS_v1 local_proof=independent_SHA256",
            signature(ORIGINAL),
            signature(bytes)
        );
        values
    }
    fn rows(
        conn: &rusqlite::Connection,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Vec<Vec<rusqlite::types::Value>> {
        let mut statement = conn.prepare(sql).unwrap();
        let columns = statement.column_count();
        statement
            .query_map(params, |row| {
                (0..columns).map(|index| row.get(index)).collect()
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }
    fn census(media: &Path) -> BTreeMap<PathBuf, Physical> {
        std::fs::read_dir(media)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (entry.path(), physical(&entry.path()))
            })
            .collect()
    }
    fn ordinary_only(conn: &rusqlite::Connection) {
        for table in ["primary_layout_bindings", "primary_layout_operations"] {
            assert_eq!(
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0,
                "ordinary authenticated publication must not manufacture {table}"
            );
        }
    }
    fn assert_scoped_publication(
        conn: &rusqlite::Connection,
        path: &Path,
        version: &str,
        bytes: &[u8],
    ) {
        let owner = state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
        )
        .unwrap();
        let (account, provider) = owner.authenticated_keys().unwrap();
        let pass = "8f22641465c3b441df94dc2d08b5c1029c663a7da888d17799e8eb30e9caf0c2";
        let native = state::db::provider_selection::SelectionPath::from_path(path)
            .key()
            .unwrap();
        let checksum = signature(bytes);
        let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        let proof: String = conn.query_row("SELECT json_array(account_key,provider_key,scope,library,child,master,state_id,origin_pass,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,trust_receipt,selected_metadata_hash,generation_id) FROM download_receipts WHERE child='asset-501' AND version_size=?1 AND provider_checksum=?2 AND native_path=?3", rusqlite::params![version,checksum,native], |row|row.get(0)).unwrap();
        let proof: Value = serde_json::from_str(&proof).unwrap();
        let scope = proof[2].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(scope).unwrap(),
            json!({"container":"com.apple.photos.cloud","database":"private","environment":"production","format":1,"realm":"com","zone":{"ownerRecordName":"_defaultOwner","zoneName":"PrimarySync"}})
        );
        assert_eq!(
            proof,
            json!([
                account,
                provider,
                scope,
                "PrimarySync",
                "asset-501",
                "501",
                "asset-501",
                pass,
                version,
                checksum,
                bytes.len(),
                native,
                sha,
                sha,
                null,
                null,
                null
            ])
        );
        let verified: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.library='PrimarySync' AND d.child='asset-501' AND d.asset_id='asset-501' AND d.master='501' AND d.version_size=?1 AND d.checksum=?2 AND d.size_bytes=?3 AND d.path=?4 AND d.pass_key=?5 AND d.admitted=1 AND d.verified_media=1 AND d.source_checksum=?6 AND d.local_checksum=?6 AND g.account_key=?7 AND g.provider_key=?8 AND g.scope=?9 AND g.sealed=1 AND g.checkpoint_ready=1 AND json_extract(CAST(g.specification AS TEXT),'$.format')=2", rusqlite::params![version,checksum,bytes.len() as i64,native,pass,sha,account,provider,scope], |row|row.get(0)).unwrap();
        assert!(
            verified > 0,
            "completed scoped resource needs verified sealed destination: {path:?}"
        );
        eprintln!(
            "ORDINARY_AUTHENTICATED_PROOF version={version} path={} provider_size={} receipt=source_backed scoped_owner=matched verified_destinations={verified}",
            path.display(),
            bytes.len()
        );
    }
    fn assert_retained_paths(
        conn: &rusqlite::Connection,
        sql: &str,
        params: &[&dyn rusqlite::ToSql],
        before: &[Vec<rusqlite::types::Value>],
    ) {
        let actual = rows(conn, sql, params);
        assert_eq!(actual.len(), before.len());
        let statement = conn.prepare(sql).unwrap();
        let marker = statement.column_index("preserved_path_key").unwrap();
        let path = statement.column_index("local_path").unwrap();
        let mut expected = before.to_vec();
        for (old, current) in expected.iter_mut().zip(&actual) {
            // Verified reuse may add protection, never alter any byte proof.
            if old[marker] == rusqlite::types::Value::Null && current[marker] != old[marker] {
                let rusqlite::types::Value::Text(native) = &old[path] else {
                    panic!("path proof must name its native file");
                };
                old[marker] = rusqlite::types::Value::Text(
                    state::db::primary_layout::path_key(Path::new(native)).unwrap(),
                );
            }
        }
        assert_eq!(
            actual, expected,
            "old path evidence changed beyond exact read-only protection"
        );
    }
    fn assert_publication(conn: &rusqlite::Connection, path: &Path, version: &str, bytes: &[u8]) {
        let sha = data_encoding::HEXLOWER.encode(&Sha256::digest(bytes));
        let proof: String = conn.query_row("SELECT json_array(a.library,a.id,a.version_size,a.checksum,a.size_bytes,a.status,a.local_path,a.local_checksum,a.download_checksum,a.is_deleted,p.provider_checksum,p.source_checksum,p.local_checksum,p.download_checksum) FROM assets a JOIN asset_metadata_paths p ON p.library=a.library AND p.id=a.id AND p.version_size=a.version_size AND p.local_path=a.local_path WHERE a.library='PrimarySync' AND a.id='asset-501' AND a.version_size=?1", [version], |row| row.get(0)).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&proof).unwrap(),
            json!([
                "PrimarySync",
                "asset-501",
                version,
                signature(bytes),
                bytes.len(),
                "downloaded",
                path.to_str().unwrap(),
                sha,
                sha,
                0,
                signature(bytes),
                sha,
                sha,
                sha
            ])
        );
    }
    async fn media_requests(server: &MockServer, phase: &str) -> [usize; 3] {
        let requests = server.received_requests().await.unwrap();
        for request in &requests {
            assert_eq!(request.method.as_str(), "GET");
            eprintln!(
                "ORDINARY_COLLECTING_REQUEST phase={phase} method={} path={}",
                request.method,
                request.url.path()
            );
        }
        let counts = ["/original", "/edit-one", "/edit-two"].map(|path| {
            requests
                .iter()
                .filter(|request| request.url.path() == path)
                .count()
        });
        assert_eq!(counts.iter().sum::<usize>(), requests.len());
        counts
    }
    for case in [
        "unchanged",
        "changed",
        "directory",
        "missing_original_receipt",
    ] {
        let server = server().await;
        let mut root = tempfile::tempdir().unwrap();
        root.disable_cleanup(true);
        eprintln!(
            "ORDINARY_COLLECTING_FIXTURE case={case} retained={}",
            root.path().display()
        );
        let media = root.path().join("media");
        let database = root.path().join("state.db");
        let p = media.join("IMG_0501.JPG");
        let e = media.join("IMG_0501_edited.JPG");
        let options = |token, trace| CycleOptions {
            pass_kind: PassKind::Album,
            // Production owns the DB and propagates capture to selected albums too.
            // Album selection, not capture absence, chooses incremental collecting.
            private: true,
            resolution: crate::types::PhotoResolution::Original,
            refresh_metadata: false,
            #[cfg(feature = "xmp")]
            sidecar: false,
            controls: download::DownloadControls::download_hidden(),
            cycle_token: Some(token),
            trace: Some(trace),
            ..CycleOptions::default()
        };
        let first = positive_records(&server, "edit-one", EDIT_ONE);
        let initial_trace: RequestTrace = Arc::default();
        let initial = try_cycle_options(
            root.path(),
            first,
            false,
            EditedNaming::Suffix,
            options("ordinary-initial", initial_trace.clone()),
        )
        .await;
        eprintln!(
            "ORDINARY_COLLECTING_INITIAL result={initial:?} requests={:?}",
            *initial_trace.lock().unwrap()
        );
        let (initial, _) = initial.unwrap();
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert!(initial.can_advance_database_checkpoint());
        assert_eq!(initial.stats.downloaded, 2);
        assert_bytes(&p, ORIGINAL);
        assert_bytes(&e, EDIT_ONE);
        let old_physical = census(&media);
        assert_eq!(
            old_physical.keys().cloned().collect::<Vec<_>>(),
            vec![p.clone(), e.clone()]
        );
        assert_eq!(media_requests(&server, "initial").await, [1, 1, 0]);
        let connect = || {
            rusqlite::Connection::open_with_flags(
                &database,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .unwrap()
        };
        let path_sql = "SELECT * FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND local_path IN (?1,?2) ORDER BY version_size,local_path";
        let catalogue_sql = "SELECT library,id,version_size,checksum,size_bytes,status,local_path,local_checksum,download_checksum,is_deleted FROM assets WHERE library='PrimarySync' AND id='asset-501' ORDER BY version_size";
        let claims_sql =
            "SELECT * FROM legacy_master_state_owners ORDER BY library,master_record_name";
        let receipts_sql = "SELECT * FROM download_receipts ORDER BY id";
        let (old_paths, old_catalogue, old_claims, old_receipts, membership, cursor) = {
            let conn = connect();
            ordinary_only(&conn);
            assert_publication(&conn, &p, "original", ORIGINAL);
            assert_publication(&conn, &e, "adjusted", EDIT_ONE);
            assert_scoped_publication(&conn, &p, "original", ORIGINAL);
            assert_scoped_publication(&conn, &e, "adjusted", EDIT_ONE);
            let receipts = rows(&conn, receipts_sql, &[]);
            assert_eq!(receipts.len(), 2);
            let paths = rows(
                &conn,
                path_sql,
                &[&p.to_str().unwrap(), &e.to_str().unwrap()],
            );
            assert_eq!(paths.len(), 2);
            let catalogue = rows(&conn, catalogue_sql, &[]);
            assert_eq!(catalogue.len(), 2);
            let claims = rows(&conn, claims_sql, &[]);
            let membership = rows(
                &conn,
                "SELECT * FROM asset_album_memberships WHERE library='PrimarySync' AND asset_record_name='asset-501' AND container_id='album-501' AND is_deleted=0",
                &[],
            );
            assert_eq!(membership.len(), 1);
            assert_eq!(conn.query_row("SELECT count(*) FROM album_membership_snapshots s JOIN asset_album_memberships m ON m.library=s.library AND m.container_id=s.container_id AND m.generation=s.generation WHERE s.library='PrimarySync' AND s.container_id='album-501' AND s.status='complete' AND s.completed_at IS NOT NULL AND m.asset_record_name='asset-501' AND m.is_deleted=0", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
            let cursor: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(cursor, "ordinary-initial");
            eprintln!(
                "ORDINARY_COLLECTING_BEFORE physical={old_physical:?} full_path_rows={paths:?} catalogue={:?} legacy_claims={claims:?} scoped_receipts={receipts:?} membership={membership:?} cursor={cursor}",
                rows(
                    &conn,
                    "SELECT * FROM assets ORDER BY library,id,version_size",
                    &[]
                )
            );
            (paths, catalogue, claims, receipts, membership, cursor)
        };
        let backup = root.path().join("original-before-controlled-change.JPG");
        if matches!(case, "changed" | "directory") {
            std::fs::rename(&p, &backup).unwrap();
            assert_eq!(physical(&backup), old_physical[&p]);
            if case == "changed" {
                std::fs::write(&p, b"changed Original witness, not the selected resource").unwrap();
            } else {
                std::fs::create_dir(&p).unwrap();
            }
        }
        let mut expected_receipts = old_receipts.clone();
        let mut old_selection = None;
        let mut deleted_receipt = None;
        if case == "missing_original_receipt" {
            // Simulate corruption of exactly the genuine Original compact receipt.
            // The original row is retained in old_receipts and the causal log.
            let conn = rusqlite::Connection::open(&database).unwrap();
            old_selection = Some([
                rows(
                    &conn,
                    "SELECT * FROM provider_active_generations ORDER BY id",
                    &[],
                ),
                rows(
                    &conn,
                    "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child",
                    &[],
                ),
                rows(
                    &conn,
                    "SELECT * FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path",
                    &[],
                ),
            ]);
            let native = state::db::provider_selection::SelectionPath::from_path(&p)
                .key()
                .unwrap();
            let id: String = conn.query_row("SELECT id FROM download_receipts WHERE child='asset-501' AND version_size='original' AND native_path=?1 AND provider_checksum=?2",rusqlite::params![native,signature(ORIGINAL)],|row|row.get(0)).unwrap();
            eprintln!(
                "DISCOVERED_ORIGINAL_PROOF_FAULT case={case} id={id} retained_original_rows={old_receipts:?}"
            );
            assert_eq!(conn.execute("DELETE FROM download_receipts WHERE id=?1 AND child='asset-501' AND version_size='original' AND native_path=?2",rusqlite::params![id,native]).unwrap(),1);
            expected_receipts.retain(|row| row[0] != rusqlite::types::Value::Text(id.clone()));
            assert_eq!(expected_receipts.len(), 1);
            deleted_receipt = Some((
                id.clone(),
                old_receipts
                    .iter()
                    .find(|row| row[0] == rusqlite::types::Value::Text(id.clone()))
                    .unwrap()
                    .clone(),
            ));
        }
        let controlled_physical = census(&media);
        let current = positive_records(&server, "edit-two", EDIT_TWO);
        let delta_trace: RequestTrace = Arc::default();
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let current_result = try_cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Suffix,
            options("ordinary-current", delta_trace.clone()),
        )
        .await;
        let requests = delta_trace.lock().unwrap().clone();
        eprintln!(
            "ORDINARY_COLLECTING_CURRENT result={current_result:?} requests={requests:?} events={:?}",
            capture.events()
        );
        let current_counts = media_requests(&server, "current").await;
        {
            let conn = connect();
            eprintln!(
                "ORDINARY_COLLECTING_ACTUAL catalogue={:?} paths={:?} choices={:?} physical={:?} scoped_receipts={:?}",
                rows(
                    &conn,
                    "SELECT * FROM assets ORDER BY library,id,version_size",
                    &[]
                ),
                rows(
                    &conn,
                    "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
                    &[]
                ),
                rows(
                    &conn,
                    "SELECT * FROM reconciliation_paths ORDER BY library,id,version_size",
                    &[]
                ),
                census(&media),
                rows(&conn, receipts_sql, &[])
            );
        }
        assert!(
            capture.contains_event(|event| event.message() == Some("Starting sync cycle")
                && event.field("sync_mode") == Some("incremental")
                && event.field("zone") == Some("PrimarySync")),
            "replacement did not start incremental"
        );
        assert!(
            capture.contains_event(
                |event| event.message() == Some("Incremental changes read complete")
            ),
            "replacement did not execute collecting change reader"
        );
        let changes: Vec<_> = requests
            .iter()
            .filter(|(kind, _)| kind == "changes")
            .collect();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].1["zones"][0]["syncToken"], cursor);
        assert_eq!(
            changes[0].1["zones"][0]["zoneID"]["zoneName"],
            "PrimarySync"
        );
        if case == "missing_original_receipt" {
            let error = current_result
                .as_ref()
                .expect_err("corrupt completed proof must fail provenance before selection");
            assert!(
                error.chain().any(|cause| matches!(
                    cause.downcast_ref::<state::error::StateError>(),
                    Some(state::error::StateError::ProviderSelectionInvalid)
                )),
                "wrong corruption boundary: {error:#}"
            );
            assert!(capture.contains_event(|event| {
                event.message()
                    == Some("Failed to record asset/master mapping from incremental delta")
                    && event.field("asset_record_name") == Some("asset-501")
                    && event.field("asset_id") == Some("501")
                    && event.field("library") == Some("PrimarySync")
                    && event.field("error")
                        == Some(
                            state::error::StateError::ProviderSelectionInvalid
                                .to_string()
                                .as_str(),
                        )
            }));
            assert!(!capture.contains_event(|event| event.message() == Some("Started sync run")));
            assert_eq!(current_counts, [1, 1, 0]);
            assert_eq!(census(&media), old_physical);
            assert!(!backup.exists());
            let conn = connect();
            ordinary_only(&conn);
            assert_eq!(rows(&conn, receipts_sql, &[]), expected_receipts);
            let (deleted_id, deleted_row) = deleted_receipt.as_ref().unwrap();
            assert!(old_receipts.contains(deleted_row));
            assert_eq!(
                conn.query_row(
                    "SELECT count(*) FROM download_receipts WHERE id=?1",
                    [deleted_id],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                0
            );
            assert_eq!(
                rows(
                    &conn,
                    path_sql,
                    &[&p.to_str().unwrap(), &e.to_str().unwrap()]
                ),
                old_paths
            );
            assert_eq!(rows(&conn, catalogue_sql, &[]), old_catalogue);
            assert_eq!(rows(&conn, claims_sql, &[]), old_claims);
            assert_eq!(
                [
                    rows(
                        &conn,
                        "SELECT * FROM provider_active_generations ORDER BY id",
                        &[]
                    ),
                    rows(
                        &conn,
                        "SELECT * FROM provider_active_decisions ORDER BY generation,pass_key,child",
                        &[]
                    ),
                    rows(
                        &conn,
                        "SELECT * FROM provider_active_destinations ORDER BY generation,pass_key,child,version_size,path",
                        &[]
                    ),
                ],
                *old_selection.as_ref().unwrap(),
                "corruption must not admit a current root or destination"
            );
            for sql in [
                "SELECT count(*) FROM provider_active_destinations WHERE checksum=?1",
                "SELECT count(*) FROM reconciliation_paths WHERE provider_checksum=?1",
                "SELECT count(*) FROM download_receipts WHERE provider_checksum=?1",
            ] {
                assert_eq!(
                    conn.query_row(sql, [signature(EDIT_TWO)], |row| row.get::<_, i64>(0))
                        .unwrap(),
                    0
                );
            }
            let token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(token, "ordinary-initial");
            eprintln!(
                "DISCOVERED_ORIGINAL_PROVENANCE_COMPLETE case={case} error=ProviderSelectionInvalid boundary=asset_master_mapping producer_started=false current_root_admission_intent_receipt=none GETs={current_counts:?} physical_and_surviving_proofs=unchanged deleted_original_backup={deleted_row:?}"
            );
            continue;
        }
        // Authenticated collecting returns through generation::after_delta before
        // the later, non-selection "Incremental task planning" event is emitted.
        assert!(
            capture.contains_event(|event| event.message() == Some("Started sync run")),
            "authenticated delta did not enter the selection producer"
        );
        let (published, _) = current_result.unwrap();
        if case != "unchanged" {
            assert_eq!(
                (
                    published.stats.downloaded,
                    published.stats.failed,
                    published.stats.state_write_failures,
                    published.stats.enumeration_errors
                ),
                (0, 0, 0, 0),
                "negative candidate must reach uncertainty, not fail elsewhere: {case} {:?}",
                published.stats
            );
            assert!(
                published.stats.skipped.uncertain > 0,
                "negative did not execute uncertain matching: {case}"
            );
            assert_eq!(current_counts, [1, 1, 0]);
            assert_eq!(census(&media), controlled_physical);
            assert_eq!(physical(&e), old_physical[&e]);
            if backup.exists() {
                assert_eq!(physical(&backup), old_physical[&p]);
            }
            let conn = connect();
            ordinary_only(&conn);
            assert_eq!(
                rows(&conn, receipts_sql, &[]),
                expected_receipts,
                "negative changed genuine receipt evidence: {case}"
            );
            assert_eq!(rows(&conn, claims_sql, &[]), old_claims);
            assert_retained_paths(
                &conn,
                path_sql,
                &[&p.to_str().unwrap(), &e.to_str().unwrap()],
                &old_paths,
            );
            let diagnostic:String=conn.query_row("SELECT last_error FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND checksum=?1 AND status='pending'",[signature(EDIT_TWO)],|row|row.get(0)).unwrap();
            let diagnostic: Value =
                serde_json::from_str(diagnostic.strip_prefix("existing_match_v1:").unwrap())
                    .unwrap();
            let attempt = diagnostic["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|attempt| attempt["checksum"] == signature(EDIT_TWO))
                .unwrap();
            let refusal = attempt["explanation"]["candidates"]
                .as_array()
                .unwrap()
                .iter()
                .find(|candidate| candidate["path"]["path"] == p.to_str().unwrap())
                .unwrap();
            assert_eq!(
                refusal["reason"],
                if case == "directory" {
                    "candidate_unreadable_unsafe_or_changed"
                } else {
                    "candidate_bytes_not_verified_for_current_resource"
                }
            );
            let destination: state::db::provider_selection::SelectionPath =
                serde_json::from_value(attempt["destination"].clone()).unwrap();
            let s = destination.to_path();
            assert_ne!(s, p);
            assert_ne!(s, e);
            assert!(!s.exists());
            let pending:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child='asset-501' AND version_size='adjusted' AND path=?3 AND checksum=?4 AND admitted=1 AND verified_media=0 AND source_checksum IS NULL AND local_checksum IS NULL",rusqlite::params![attempt["generation"].as_str().unwrap(),attempt["pass"].as_str().unwrap(),destination.key().unwrap(),signature(EDIT_TWO)],|row|row.get(0)).unwrap();
            assert_eq!(pending, 1);
            assert_eq!(conn.query_row("SELECT count(*) FROM download_receipts WHERE version_size='adjusted' AND provider_checksum=?1",[signature(EDIT_TWO)],|row|row.get::<_,i64>(0)).unwrap(),0);
            let mapping:String=conn.query_row("SELECT json_array(requested_path_key,destination_path_key,destination_path,provider_checksum,provider_size) FROM reconciliation_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND provider_checksum=?1",[signature(EDIT_TWO)],|row|row.get(0)).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&mapping).unwrap(),
                json!([
                    crate::fs_util::confined_path_key(&e).unwrap(),
                    crate::fs_util::confined_path_key(&s).unwrap(),
                    s.to_str().unwrap(),
                    signature(EDIT_TWO),
                    EDIT_TWO.len()
                ])
            );
            eprintln!(
                "DISCOVERED_ORIGINAL_NEGATIVE_COMPLETE case={case} refusal={refusal:?} destination={} pending={pending} GETs={current_counts:?} physical=unchanged full_proofs=retained",
                s.display()
            );
            continue;
        }
        assert!(
            capture.contains_event(|event| event.message() == Some("Completed sync run")
                && event.field("downloaded") == Some("1")
                && event.field("failed") == Some("0")),
            "authenticated selection must publish the new resource once"
        );
        assert_eq!(published.failed_count, 0, "{:?}", published.stats);
        assert!(published.can_advance_database_checkpoint());
        assert_eq!(
            (
                published.stats.downloaded,
                published.stats.state_write_failures,
                published.stats.enumeration_errors
            ),
            (1, 0, 0)
        );
        assert_eq!(current_counts, [1, 1, 1]);
        let (s, current_paths, current_catalogue, current_receipts) = {
            let conn = connect();
            ordinary_only(&conn);
            let s: String = conn.query_row("SELECT local_path FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND checksum=?1 AND status='downloaded'", [signature(EDIT_TWO)], |row| row.get(0)).unwrap();
            let s = PathBuf::from(s);
            assert_ne!(s, p);
            assert_ne!(s, e);
            assert_eq!(s.parent(), Some(media.as_path()));
            assert_bytes(&s, EDIT_TWO);
            assert_publication(&conn, &p, "original", ORIGINAL);
            assert_publication(&conn, &s, "adjusted", EDIT_TWO);
            assert_scoped_publication(&conn, &s, "adjusted", EDIT_TWO);
            let receipts = rows(&conn, receipts_sql, &[]);
            assert_eq!(receipts.len(), 3);
            assert!(
                old_receipts.iter().all(|proof| receipts.contains(proof)),
                "old full scoped receipts must remain unchanged"
            );
            assert_retained_paths(
                &conn,
                path_sql,
                &[&p.to_str().unwrap(), &e.to_str().unwrap()],
                &old_paths,
            );
            assert_eq!(rows(&conn, claims_sql, &[]), old_claims);
            assert_eq!(
                rows(
                    &conn,
                    "SELECT * FROM asset_album_memberships WHERE library='PrimarySync' AND asset_record_name='asset-501' AND container_id='album-501' AND is_deleted=0",
                    &[]
                ),
                membership
            );
            let catalogue = rows(&conn, catalogue_sql, &[]);
            assert_eq!(
                catalogue
                    .iter()
                    .find(|row| row[2] == rusqlite::types::Value::Text("original".into())),
                old_catalogue
                    .iter()
                    .find(|row| row[2] == rusqlite::types::Value::Text("original".into()))
            );
            let token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(token, "ordinary-current");
            let paths = rows(
                &conn,
                "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
                &[],
            );
            assert_eq!(paths.len(), 3);
            (s, paths, catalogue, receipts)
        };
        let current_physical = census(&media);
        assert_eq!(current_physical.len(), 3);
        for (path, proof) in &old_physical {
            assert_eq!(current_physical.get(path), Some(proof));
        }
        assert!(current_physical.contains_key(&s));
        eprintln!(
            "ORDINARY_COLLECTING_PUBLISHED S={} physical={current_physical:?} path_proofs={current_paths:?} catalogue={current_catalogue:?} scoped_receipts={current_receipts:?} GETs={current_counts:?}",
            s.display()
        );
        for round in 0..2 {
            let trace: RequestTrace = Arc::default();
            let (quiet, _) = cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Suffix,
                options("ordinary-current", trace.clone()),
            )
            .await;
            eprintln!(
                "ORDINARY_COLLECTING_QUIET round={round} result={quiet:?} requests={:?}",
                *trace.lock().unwrap()
            );
            assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
            assert!(quiet.can_advance_database_checkpoint());
            assert_eq!(
                (
                    quiet.stats.downloaded,
                    quiet.stats.state_write_failures,
                    quiet.stats.enumeration_errors
                ),
                (0, 0, 0)
            );
            assert_eq!(media_requests(&server, "quiet").await, current_counts);
            assert_eq!(census(&media), current_physical);
            let conn = connect();
            ordinary_only(&conn);
            assert_eq!(
                rows(
                    &conn,
                    "SELECT * FROM asset_metadata_paths ORDER BY library,id,version_size,local_path",
                    &[]
                ),
                current_paths
            );
            assert_eq!(rows(&conn, catalogue_sql, &[]), current_catalogue);
            assert_eq!(rows(&conn, claims_sql, &[]), old_claims);
            assert_eq!(rows(&conn, receipts_sql, &[]), current_receipts);
            assert_publication(&conn, &p, "original", ORIGINAL);
            assert_publication(&conn, &s, "adjusted", EDIT_TWO);
            assert_scoped_publication(&conn, &s, "adjusted", EDIT_TWO);
            let token: String = conn
                .query_row(
                    "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(token, "ordinary-current");
            eprintln!(
                "ORDINARY_COLLECTING_QUIET_COMPLETE round={round} GETs=unchanged full_physical_and_path_proofs=unchanged old_claims=unchanged all_scoped_receipts=unchanged"
            );
        }
    }
}
