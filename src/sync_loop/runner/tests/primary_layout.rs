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
                json!({"batch":body["batch"].as_array().unwrap().iter().map(|_|json!({"records":[{"fields":{"itemCount":{"value":if self.indexing.is_some() {self.records.len()/2} else {1}}}}]})).collect::<Vec<_>>()}),
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
        assert_eq!(changes[0].1["zones"][0]["syncToken"], "query-token");
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
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("query-token"),
            "{variant}"
        );
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
    // A newly edited occupied primary stays pending while unrelated new media
    // continues. The current duplicate failure count is tracked in #1050.
    {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let (initial, _) = cycle(
            root.path(),
            records(&server, None),
            false,
            EditedNaming::Primary,
        )
        .await;
        assert!(initial.db_sync_token_advance_safe);
        let mut changed = records(&server, Some(("edit-one", EDIT_ONE)));
        let mut healthy = records(&server, None);
        for record in &mut healthy {
            record["recordName"] =
                json!(record["recordName"].as_str().unwrap().replace("501", "502"));
            if let Some(reference) = record["fields"].get_mut("masterRef") {
                reference["value"]["recordName"] = json!("502");
            }
        }
        healthy[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
        healthy[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL": format!("{}/edit-two", server.uri()), "size": EDIT_TWO.len(), "fileChecksum": provider_hash(EDIT_TWO)});
        changed.extend(healthy);
        let (result, _) = cycle(root.path(), changed, false, EditedNaming::Primary).await;
        assert_eq!(result.stats.skipped.uncertain, 1);
        assert_eq!(result.stats.downloaded, 1);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&root.path().join("media/IMG_0502.JPG"), EDIT_TWO);
        assert!(!root.path().join("media/IMG_0501_original.JPG").exists());
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1);
        let pending = &pending[0];
        assert_eq!(pending.library.as_ref(), "PrimarySync");
        assert_eq!(pending.id.as_ref(), "asset-501");
        assert_eq!(pending.version_size, state::VersionSizeKey::Adjusted);
        assert_eq!(pending.status, state::AssetStatus::Pending);
        let evidence = pending.last_error.clone().unwrap();
        assert!(evidence.starts_with("existing_match_v1:"));
        let checksum = pending.checksum.to_string();
        let assert_excluded_evidence = || {
            let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
            let (status, retained, retained_checksum, deleted): (String, Option<String>, String, bool) = conn.query_row(
                "SELECT status, last_error, checksum, is_deleted FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).unwrap();
            assert_eq!(status, "policy_excluded");
            assert_eq!(retained.as_deref(), Some(evidence.as_str()));
            assert_eq!(retained_checksum, checksum);
            assert!(!deleted);
        };
        assert_eq!(result.stats.failed, 1);
        // The same unresolved item is counted in both initial and retry work.
        // Keep this explicit until issue #1050 fixes the accounting.
        assert_eq!(result.failed_count, 2, "{:?}", result.stats);
        drop(db);
        let reverted = records(&server, None);
        let (result, _) = cycle(root.path(), reverted.clone(), false, EditedNaming::Primary).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_eq!(result.stats.failed, 0);
        assert_eq!(result.stats.skipped.uncertain, 0);
        assert_eq!(result.stats.downloaded, 0);
        assert!(result.db_sync_token_advance_safe);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(db.get_pending().await.unwrap().is_empty());
        assert_excluded_evidence();
        drop(db);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='original' AND status='downloaded' AND local_path LIKE '%/media/IMG_0501.JPG'"
            ),
            1
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
        let requests = server.received_requests().await.unwrap().len();
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&root.path().join("media/IMG_0502.JPG"), EDIT_TWO);
        assert!(!root.path().join("media/IMG_0501_original.JPG").exists());
        for _ in 0..2 {
            let (quiet, _) =
                cycle(root.path(), reverted.clone(), true, EditedNaming::Primary).await;
            assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
            assert_eq!(quiet.stats.failed, 0);
            assert_eq!(quiet.stats.skipped.uncertain, 0);
            assert_eq!(quiet.stats.downloaded, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
            assert_bytes(&root.path().join("media/IMG_0502.JPG"), EDIT_TWO);
            assert!(!root.path().join("media/IMG_0501_original.JPG").exists());
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
                .await
                .unwrap();
            assert!(db.get_pending().await.unwrap().is_empty());
            assert_excluded_evidence();
        }
    }
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    std::fs::write(media.join("foreign.jpg"), b"unrelated user bytes").unwrap();
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let (initial, _) = cycle(root.path(), current.clone(), true, EditedNaming::Suffix).await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
    let requests = server.received_requests().await.unwrap().len();
    for naming in [
        EditedNaming::Primary,
        EditedNaming::Primary,
        EditedNaming::Suffix,
    ] {
        let (result, lookups) = cycle(root.path(), current.clone(), true, naming).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
        assert_bytes(&media.join("foreign.jpg"), b"unrelated user bytes");
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
async fn primary_layout_heic_jpeg_extension_changes_retire_only_preserved_owned_aliases() {
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
        let (changed, _) = cycle(root.path(), current.clone(), false, EditedNaming::Primary).await;
        assert_eq!(
            changed.failed_count, 0,
            "transition {index}: {:?}",
            changed.stats
        );
        assert!(changed.db_sync_token_advance_safe);
        assert_bytes(&media.join(current_name), current_bytes);
        let obsolete = if current_name.ends_with(".HEIC") {
            "IMG_0501.JPG"
        } else {
            "IMG_0501.HEIC"
        };
        assert!(
            !media.join(obsolete).exists(),
            "obsolete visible alias at transition {index}"
        );
        if index == 0 {
            assert!(!media.join("IMG_0501_original.HEIC").exists());
        } else {
            assert_bytes(&media.join("IMG_0501_original.HEIC"), original);
        }
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        let requests = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            let (quiet, lookups) =
                cycle(root.path(), current.clone(), true, EditedNaming::Primary).await;
            assert_eq!(quiet.failed_count, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(lookups, 0, "transition {index}: {:?}", quiet.stats);
            assert_bytes(&media.join(current_name), current_bytes);
            assert!(!media.join(obsolete).exists());
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
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let manifest = db.get_manifest_assets().await.unwrap();
    assert!(
        manifest
            .iter()
            .flat_map(|row| &row.preserved_files)
            .any(|receipt| std::fs::read(receipt.native_path.to_path()).unwrap() == EDIT_ONE)
    );
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
async fn scoped_bad_admitted_adjusted_recovers_after_reopen() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let original = media.join("IMG_0501.JPG");
    let options = |resolution, cycle_token| CycleOptions {
        private: true,
        resolution,
        cycle_token: Some(cycle_token),
        ..CycleOptions::default()
    };
    let (first, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::Original, "original-token"),
    )
    .await;
    assert_eq!(first.failed_count, 0);
    {
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let conn = db.acquire_lock("initial checkpoint").unwrap();
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "original-token");
    }
    assert_bytes(&original, ORIGINAL);
    std::fs::write(&original, b"changed original").unwrap();
    let mut edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let mut healthy = records(&server, None);
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
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "debt-token"),
    )
    .await;
    assert!(held.failed_count > 0, "{:?}", held.stats);
    assert!(held.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0502.JPG"), EDIT_TWO);
    let (held_again, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "debt-token-2"),
    )
    .await;
    assert!(held_again.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0502.JPG"), EDIT_TWO);
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
        let conn = db.acquire_lock("recorded bad destination").unwrap();
        let count:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation WHERE d.child='asset-501' AND d.version_size='adjusted' AND d.admitted=1 AND d.verified_media=0 AND g.sealed=1",[],|r|r.get(0)).unwrap();
        assert!(
            count >= 2,
            "must start from two sealed admitted generations: {count}"
        );
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "debt-token-2");
    }
    std::fs::write(&original, ORIGINAL).unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let (recovered, _) = cycle_options(
        root.path(),
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "recovered-token"),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert_bytes(&original, ORIGINAL);
    let sibling = std::fs::read_dir(&media)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p != &original && std::fs::read(p).is_ok_and(|b| b == EDIT_ONE))
        .expect("edited sibling");
    {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let conn = db.acquire_lock("recovered adjusted ownership").unwrap();
        let incomplete: i64 = conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0", [], |r|r.get(0)).unwrap();
        assert_eq!(incomplete, 0);
        let receipts: i64 = conn.query_row("SELECT count(*) FROM download_receipts WHERE child='asset-501' AND version_size IN ('original','adjusted') AND source_checksum IS NOT NULL", [], |r|r.get(0)).unwrap();
        assert_eq!(receipts, 2);
    }
    for _ in 0..2 {
        let (quiet, _) = cycle_options(
            root.path(),
            edited.clone(),
            true,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None, "recovered-token"),
        )
        .await;
        assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_bytes(&original, ORIGINAL);
        assert_bytes(&sibling, EDIT_ONE);
    }
}

#[tokio::test]
async fn scoped_distinct_admitted_edits_publish_current_and_preserve_old_debt() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let original = media.join("IMG_0501.JPG");
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
        options(crate::types::PhotoResolution::Original, "initial-token"),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    assert_bytes(&original, ORIGINAL);
    let original_receipt: String = {
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let conn = db.acquire_lock("original source-backed receipt").unwrap();
        conn.query_row("SELECT json_array(id,scope,child,master,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original' AND source_checksum IS NOT NULL",[],|r|r.get(0)).unwrap()
    };
    std::fs::write(&original, b"changed original").unwrap();
    let first = records(&server, Some(("edit-one", EDIT_ONE)));
    let (old, _) = cycle_options(
        root.path(),
        first,
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "old-edit-token"),
    )
    .await;
    assert!(old.failed_count > 0);
    assert!(old.db_sync_token_advance_safe);
    let current = records(&server, Some(("edit-two", EDIT_TWO)));
    let (new, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "current-edit-token"),
    )
    .await;
    assert!(new.failed_count > 0);
    assert!(new.db_sync_token_advance_safe);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    std::fs::write(&original, ORIGINAL).unwrap();
    let original_mtime = std::fs::metadata(&original).unwrap().modified().unwrap();
    let old_snapshot: Vec<String> = {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
        let conn = db.acquire_lock("capture distinct historical debt").unwrap();
        let rows:Vec<String>=conn.prepare("SELECT json_array(d.generation,d.path,d.checksum,d.verified_media,d.verified_metadata,g.sealed,g.seal_hash,p.admission,p.reason,p.attempts,p.next_retry_at,p.manifest_hash) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation JOIN provider_active_decisions p ON p.generation=d.generation AND p.pass_key=d.pass_key AND p.child=d.child WHERE d.child='asset-501' AND d.version_size='adjusted' AND d.checksum=?1 ORDER BY d.generation").unwrap().query_map([provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(!rows.is_empty(), "old edit must be admitted debt");
        let current_count:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1 AND admitted=1 AND verified_media=0",[provider_hash(EDIT_TWO)],|r|r.get(0)).unwrap();
        assert!(
            current_count > 0,
            "current edit must have an admitted bad destination"
        );
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "current-edit-token");
        rows
    };
    let (recovered, _) = cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None, "recovered-token"),
    )
    .await;
    assert_eq!(recovered.stats.downloaded, 1, "{:?}", recovered.stats);
    assert!(recovered.db_sync_token_advance_safe);
    assert_bytes(&original, ORIGINAL);
    let sibling = std::fs::read_dir(&media)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p != &original && std::fs::read(p).is_ok_and(|b| b == EDIT_TWO))
        .expect("current edit sibling");
    let old_immutable: Vec<String> = {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let conn = db
            .acquire_lock("verify historical debt survives current publication")
            .unwrap();
        let rows:Vec<String>=conn.prepare("SELECT json_array(d.generation,d.path,d.checksum,d.verified_media,d.verified_metadata,g.sealed,g.seal_hash,p.admission,p.reason,p.attempts,p.next_retry_at,p.manifest_hash) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation JOIN provider_active_decisions p ON p.generation=d.generation AND p.pass_key=d.pass_key AND p.child=d.child WHERE d.child='asset-501' AND d.version_size='adjusted' AND d.checksum=?1 ORDER BY d.generation").unwrap().query_map([provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(
            rows, old_snapshot,
            "historical debt and seal must not be rewritten"
        );
        let fabricated:i64=conn.query_row("SELECT count(*) FROM download_receipts WHERE child='asset-501' AND version_size='adjusted' AND provider_checksum=?1",[provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap();
        assert_eq!(fabricated, 0);
        let completed:i64=conn.query_row("SELECT count(*) FROM download_receipts WHERE child='asset-501' AND version_size='adjusted' AND provider_checksum=?1 AND source_checksum IS NOT NULL",[provider_hash(EDIT_TWO)],|r|r.get(0)).unwrap();
        assert_eq!(completed, 1);
        let retained:String=conn.query_row("SELECT json_array(id,scope,child,master,version_size,provider_checksum,provider_size,native_path,source_checksum,local_checksum,proof_hash) FROM download_receipts WHERE child='asset-501' AND version_size='original' AND source_checksum IS NOT NULL",[],|r|r.get(0)).unwrap();
        assert_eq!(retained, original_receipt);
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            original_mtime
        );
        let token: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(token, "recovered-token");
        let immutable:Vec<String>=conn.prepare("SELECT json_array(d.generation,d.path,d.checksum,d.verified_media,d.verified_metadata,g.sealed,g.seal_hash,p.admission,p.manifest_hash) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation JOIN provider_active_decisions p ON p.generation=d.generation AND p.pass_key=d.pass_key AND p.child=d.child WHERE d.child='asset-501' AND d.version_size='adjusted' AND d.checksum=?1 ORDER BY d.generation").unwrap().query_map([provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        immutable
    };
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/edit-one")
            .count(),
        0
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/edit-two")
            .count(),
        1
    );
    drop(requests);
    for _ in 0..2 {
        let (quiet, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None, "recovered-token"),
        )
        .await;
        assert_eq!(quiet.stats.downloaded, 0);
        assert_bytes(&original, ORIGINAL);
        assert_eq!(
            std::fs::metadata(&original).unwrap().modified().unwrap(),
            original_mtime
        );
        assert_bytes(&sibling, EDIT_TWO);
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let conn = db.acquire_lock("reopened historical debt").unwrap();
        let old_pending:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1 AND admitted=1 AND verified_media=0",[provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap();
        assert!(old_pending > 0);
        let immutable:Vec<String>=conn.prepare("SELECT json_array(d.generation,d.path,d.checksum,d.verified_media,d.verified_metadata,g.sealed,g.seal_hash,p.admission,p.manifest_hash) FROM provider_active_destinations d JOIN provider_active_generations g ON g.id=d.generation JOIN provider_active_decisions p ON p.generation=d.generation AND p.pass_key=d.pass_key AND p.child=d.child WHERE d.child='asset-501' AND d.version_size='adjusted' AND d.checksum=?1 ORDER BY d.generation").unwrap().query_map([provider_hash(EDIT_ONE)],|r|r.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
        assert_eq!(immutable, old_immutable);
    }
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.url.path() == "/edit-two")
            .count(),
        1
    );
}

#[tokio::test]
async fn scoped_missing_original_keeps_admitted_adjusted_pending() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("media/IMG_0501.JPG");
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
    assert_eq!(first.failed_count, 0);
    std::fs::write(&original, b"changed original").unwrap();
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (held, _) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None),
    )
    .await;
    assert!(held.failed_count > 0);
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(&original).unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner.clone(), "asset-501".into())
            .await
            .unwrap();
    }
    let requests = server.received_requests().await.unwrap().len();
    let (_held, _) = cycle_options(
        root.path(),
        edited.clone(),
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None),
    )
    .await;
    assert!(!original.exists());
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        let conn = db
            .acquire_lock("missing original retains adjusted debt")
            .unwrap();
        let pending:i64=conn.query_row("SELECT count(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0",[],|r|r.get(0)).unwrap();
        assert!(pending > 0);
    }
    std::fs::write(&original, ORIGINAL).unwrap();
    {
        let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
            .await
            .unwrap();
        db.make_selection_retry_due(owner, "asset-501".into())
            .await
            .unwrap();
    }
    let (recovered, _) = cycle_options(
        root.path(),
        edited,
        true,
        EditedNaming::Primary,
        options(crate::types::PhotoResolution::None),
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert_bytes(&original, ORIGINAL);
    assert!(
        std::fs::read_dir(root.path().join("media"))
            .unwrap()
            .any(|entry| {
                let path = entry.unwrap().path();
                path != original && std::fs::read(path).is_ok_and(|bytes| bytes == EDIT_ONE)
            })
    );
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
async fn scoped_adjusted_only_holds_changed_or_foreign_original() {
    for changed in [true, false] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let options = |resolution| CycleOptions {
            private: true,
            resolution,
            ..CycleOptions::default()
        };
        if changed {
            let (first, _) = cycle_options(
                root.path(),
                records(&server, None),
                false,
                EditedNaming::Primary,
                options(crate::types::PhotoResolution::Original),
            )
            .await;
            assert_eq!(first.failed_count, 0);
        } else {
            std::fs::create_dir_all(&media).unwrap();
        }
        let original = media.join("IMG_0501.JPG");
        let foreign = b"foreign or locally changed original";
        std::fs::write(&original, foreign).unwrap();
        let requests = server.received_requests().await.unwrap().len();
        let (held, _) = cycle_options(
            root.path(),
            records(&server, Some(("edit-one", EDIT_ONE))),
            false,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None),
        )
        .await;
        assert!(held.failed_count > 0, "{changed}: {:?}", held.stats);
        assert_bytes(&original, foreign);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert!(!std::fs::read_dir(&media).unwrap().any(|entry| {
            std::fs::read(entry.unwrap().path()).is_ok_and(|bytes| bytes == EDIT_ONE)
        }));
    }
}

#[tokio::test]
async fn primary_layout_retains_original_when_only_adjusted_selected() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let (initial, _) = cycle(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (result, _) = cycle_resolution(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        crate::types::PhotoResolution::None,
    )
    .await;
    assert_eq!(result.failed_count, 0, "{:?}", result.stats);
    assert!(result.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/original")
            .count(),
        1,
        "unselected original was fetched"
    );
    let (quiet, lookups) = cycle_resolution(
        root.path(),
        edited,
        true,
        EditedNaming::Primary,
        crate::types::PhotoResolution::None,
    )
    .await;
    assert_eq!(quiet.failed_count, 0);
    assert!(quiet.db_sync_token_advance_safe);
    assert_eq!(lookups, 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "dedicated process-death fixture, spawned by its parent test"]
async fn primary_layout_death_worker() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    let uri = std::env::var("KEI_TEST_PROCESS_DEATH_URL").unwrap();
    cycle(
        &root,
        records_at(&uri, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Primary,
    )
    .await;
    panic!("process-death fixture passed its selected rendezvous");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn contract_source_checkpoint_requires_durable_recovery_and_temp_file_delete_requires_durable_ownership_primary_layout_process_death()
 {
    use std::os::unix::process::ExitStatusExt;
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
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        cycle(
            root.path(),
            records(&server, None),
            false,
            EditedNaming::Primary,
        )
        .await;
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        let log = std::fs::File::create(root.path().join("worker.log")).unwrap();
        let mut worker = Worker(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sync_loop::runner::tests::primary_layout::primary_layout_death_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("KEI_TEST_PROCESS_DEATH_ROOT", root.path())
                .env("KEI_TEST_PROCESS_DEATH_POINT", point)
                .env("KEI_TEST_PROCESS_DEATH_URL", server.uri())
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !root.path().join("ready").exists() {
            assert!(
                worker.0.try_wait().unwrap().is_none(),
                "worker exited at {point}: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            assert!(
                std::time::Instant::now() < deadline,
                "worker timeout at {point}: {}",
                std::fs::read_to_string(root.path().join("worker.log")).unwrap()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        worker.0.kill().unwrap();
        assert_eq!(worker.0.wait().unwrap().signal(), Some(9));
        let requests = server.received_requests().await.unwrap().len();
        let (recovered, _) = cycle(
            root.path(),
            records(&server, Some(("edit-one", EDIT_ONE))),
            true,
            EditedNaming::Primary,
        )
        .await;
        assert_eq!(recovered.failed_count, 0, "{point}: {:?}", recovered.stats);
        assert!(recovered.db_sync_token_advance_safe, "{point}");
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        if point != "layout-planned" {
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                requests,
                "{point}: redownloaded prepared media"
            );
        }
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
        for _ in 0..2 {
            let (quiet, lookups) = cycle(
                root.path(),
                records(&server, Some(("edit-one", EDIT_ONE))),
                true,
                EditedNaming::Primary,
            )
            .await;
            assert_eq!(quiet.failed_count, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(lookups, 0, "{point}: quiet hydration");
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                operations
            );
        }
    }
}

#[tokio::test]
async fn primary_layout_private_edit_retains_occupied_original_and_processes_new_work() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = || CycleOptions {
        private: true,
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    let mut current = records(&server, Some(("edit-one", EDIT_ONE)));
    let mut new = records(&server, None);
    for record in &mut new {
        record["recordName"] = json!(record["recordName"].as_str().unwrap().replace("501", "502"));
        if let Some(reference) = record["fields"].get_mut("masterRef") {
            reference["value"]["recordName"] = json!("502");
        }
    }
    new[0]["fields"]["filenameEnc"]["value"] = json!("NEW_0502.JPG");
    new[0]["fields"]["resOriginalRes"]["value"] = json!({"downloadURL":format!("{}/edit-two",server.uri()),"size":EDIT_TWO.len(),"fileChecksum":provider_hash(EDIT_TWO)});
    current.extend(new);
    let mut quiet_snapshot = None;
    for (round, (edit, quiet, uncertain, downloads, generations, debt)) in [
        (Some(("edit-one", EDIT_ONE)), false, 1, 1, 1, 1),
        (Some(("edit-two", EDIT_TWO)), false, 2, 0, 2, 2),
        (None, false, 1, 0, 3, 2),
        (None, true, 0, 0, 3, 2),
        (None, true, 0, 0, 3, 2),
    ]
    .into_iter()
    .enumerate()
    {
        let mut page = records(&server, edit);
        page.extend(current[2..].to_vec());
        let (result, lookups) =
            cycle_options(root.path(), page, quiet, EditedNaming::Primary, options()).await;
        assert_eq!(
            result.stats.skipped.uncertain, uncertain,
            "round {round}: {result:?}"
        );
        assert_eq!(
            result.stats.downloaded, downloads,
            "round {round}: {result:?}"
        );
        assert!(
            result.db_sync_token_advance_safe,
            "round {round}: {result:?}"
        );
        if quiet {
            assert_eq!(result.failed_count, 0);
            assert_eq!(lookups, 0);
        }
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("NEW_0502.JPG"), EDIT_TWO);
        assert!(!media.join("IMG_0501_original.JPG").exists());
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("cycle-token")
        );
        let pending = db.get_pending().await.unwrap();
        assert_eq!(pending.len(), 1, "round {round}: {pending:?}");
        assert_eq!(pending[0].id.as_ref(), "asset-501");
        assert_eq!(pending[0].version_size.as_str(), "adjusted");
        assert!(
            pending[0]
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("unsupported_provider_proof_without_valid_receipt"))
        );
        drop(db);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let receipt: (String, String, String, String) = conn.query_row("SELECT local_checksum, source_checksum, provider_checksum, local_path FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='original'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_eq!(receipt.0, format!("{:x}", Sha256::digest(ORIGINAL)));
        assert_eq!(receipt.0, receipt.1);
        assert_eq!(receipt.2, provider_hash(ORIGINAL));
        assert_eq!(std::path::Path::new(&receipt.3), media.join("IMG_0501.JPG"));
        let current_receipt: (String, String, String) = conn.query_row("SELECT local_checksum, source_checksum, provider_checksum FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-502' AND version_size='original'", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(current_receipt.0, format!("{:x}", Sha256::digest(EDIT_TWO)));
        assert_eq!(current_receipt.0, current_receipt.1);
        assert_eq!(current_receipt.2, provider_hash(EDIT_TWO));
        let pending_checksum: String = conn.query_row("SELECT checksum FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted' AND status='pending' AND local_path IS NULL", [], |r| r.get(0)).unwrap();
        assert_eq!(
            pending_checksum,
            provider_hash(if round == 0 { EDIT_ONE } else { EDIT_TWO })
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM asset_metadata_paths WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'"
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
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id='asset-502' AND version_size='original' AND status='downloaded'"
            ),
            1
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_generations WHERE sealed=1 AND checkpoint_ready=1"
            ),
            generations
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_generations WHERE sealed=0 OR checkpoint_ready=0"
            ),
            0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_destinations WHERE admitted=1 AND (verified_media=0 OR verified_metadata=0)"
            ),
            debt
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND admitted=1 AND verified_media=0 AND verified_metadata=0"
            ),
            debt
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='original' AND admitted=1 AND verified_media=1 AND verified_metadata=1"
            ),
            generations
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_destinations WHERE child='asset-502' AND version_size='original' AND admitted=1 AND verified_media=1 AND verified_metadata=1"
            ),
            1
        );
        for bytes in [EDIT_ONE, EDIT_TWO].into_iter().take(debt as usize) {
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM provider_active_destinations WHERE child='asset-501' AND version_size='adjusted' AND checksum=?1 AND verified_media=0 AND verified_metadata=0", [provider_hash(bytes)], |r| r.get(0)).unwrap();
            assert_eq!(count, 1);
        }
        if quiet {
            let snapshot: Vec<String> = ["SELECT json_array(id,sealed,checkpoint_ready) FROM provider_active_generations ORDER BY id", "SELECT json_array(generation,child,version_size,checksum,verified_media,verified_metadata) FROM provider_active_destinations ORDER BY generation,child,version_size", "SELECT json_array(id,version_size,status,checksum,last_error) FROM assets ORDER BY id,version_size"].into_iter().flat_map(|sql| conn.prepare(sql).unwrap().query_map([], |r| r.get::<_,String>(0)).unwrap().collect::<Result<Vec<_>,_>>().unwrap()).collect();
            if let Some(previous) = &quiet_snapshot {
                assert_eq!(&snapshot, previous);
            }
            quiet_snapshot = Some(snapshot);
        }
    }
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/original")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/edit-two")
            .count(),
        1
    );
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn primary_layout_preserves_sidecar_bytes_and_fences_old_metadata_queue() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = || CycleOptions {
        sidecar: true,
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options(),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let old_sidecar = std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap();
    assert!(!old_sidecar.is_empty());
    let (edited, _) = cycle_options(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Primary,
        options(),
    )
    .await;
    assert_eq!(edited.failed_count, 0, "{:?}", edited.stats);
    assert!(edited.db_sync_token_advance_safe);
    assert_eq!(
        std::fs::read(media.join("IMG_0501_original.JPG.xmp")).unwrap(),
        old_sidecar
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute("UPDATE asset_metadata_paths SET metadata_write_failed_at=1 WHERE version_size='original' AND local_path=?1",[media.join("IMG_0501.JPG").to_str().unwrap()]).unwrap();
    drop(conn);
    let current = std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap();
    let downloads = server.received_requests().await.unwrap().len();
    let (quiet, _) = cycle_options(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        true,
        EditedNaming::Primary,
        options(),
    )
    .await;
    assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
    assert!(quiet.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
    assert_eq!(
        std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap(),
        current
    );
    assert_eq!(server.received_requests().await.unwrap().len(), downloads);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM asset_metadata_paths WHERE version_size='original' AND metadata_write_failed_at=1"
        ),
        1,
        "historical marker was discarded"
    );
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
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let (current_motion, archive_motion) =
            if policy == crate::types::LivePhotoMovFilenamePolicy::Suffix {
                ("IMG_0501_HEVC.MOV", "IMG_0501_HEVC_original.MOV")
            } else {
                ("IMG_0501.MOV", "IMG_0501_original.MOV")
            };
        let options = || CycleOptions {
            mov_policy: policy,
            ..CycleOptions::default()
        };
        let original = live_records(&server.uri(), original_still, original_motion, None);
        let db_path = root.path().join("state.db");
        drop(state::SqliteStateDb::open(&db_path).await.unwrap());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_float_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected float journal interruption'); END").unwrap();
        let (blocked, _) = cycle_options(
            root.path(),
            original.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert!(!blocked.db_sync_token_advance_safe);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='publishing'"
            ),
            1
        );
        assert_bytes(&media.join("IMG_0501.HEIC"), original_still);
        assert_bytes(&media.join(current_motion), original_motion);
        conn.execute_batch("DROP TRIGGER fail_float_commit")
            .unwrap();
        drop(conn);
        let requests = server.received_requests().await.unwrap().len();
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
        assert_bytes(&media.join("IMG_0501.HEIC"), &edited_still);
        assert_bytes(&media.join(current_motion), &edited_motion);
        assert_bytes(&media.join("IMG_0501_original.HEIC"), original_still);
        assert_bytes(&media.join(archive_motion), original_motion);
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
        let requests = server.received_requests().await.unwrap().len();
        let ops = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
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
            assert_bytes(&media.join("IMG_0501_original.HEIC"), original_still);
            assert_bytes(&media.join(archive_motion), original_motion);
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
        assert!(preserved.iter().any(|bytes| bytes == &edited_still));
        assert!(preserved.iter().any(|bytes| bytes == &edited_motion));
    }
}

#[tokio::test]
async fn primary_layout_source_change_during_preparation_retains_work_then_replans() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    cycle(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
    )
    .await;
    let first = records(&server, Some(("edit-changing", EDIT_ONE)));
    let second = records(&server, Some(("edit-two", EDIT_TWO)));
    let current = Arc::new(std::sync::Mutex::new(first.clone()));
    let update = current.clone();
    let next = second.clone();
    Mock::given(method("GET"))
        .and(path("/edit-changing"))
        .respond_with(move |_: &wiremock::Request| {
            *update.lock().unwrap() = next.clone();
            ResponseTemplate::new(200).set_body_bytes(EDIT_ONE)
        })
        .mount(&server)
        .await;
    let (held, _) = cycle_options(
        root.path(),
        first,
        false,
        EditedNaming::Primary,
        CycleOptions {
            current: Some(current),
            ..CycleOptions::default()
        },
    )
    .await;
    assert!(
        !held.db_sync_token_advance_safe,
        "changed source promoted checkpoint"
    );
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert!(!media.join("IMG_0501_original.JPG").exists());
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='prepared' AND conflict='verified_recovery_required'"
        ),
        1
    );
    let (replanned, _) = cycle(root.path(), second.clone(), true, EditedNaming::Primary).await;
    assert_eq!(replanned.failed_count, 0, "{:?}", replanned.stats);
    assert!(replanned.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_TWO);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let members:Vec<Vec<u8>>=conn.prepare("SELECT m.evidence FROM primary_layout_members m JOIN primary_layout_operations o ON o.operation=m.operation WHERE o.phase='cancelled'").unwrap().query_map([],|row|row.get(0)).unwrap().collect::<Result<_,_>>().unwrap();
    assert!(
        members.iter().any(|evidence| {
            let member: state::db::primary_layout::LayoutMember =
                serde_json::from_slice(evidence).unwrap();
            member
                .prepared
                .is_some_and(|prepared| std::fs::read(prepared.path.to_path()).unwrap() == EDIT_ONE)
        }),
        "cancelled preparation lost its downloaded bytes"
    );
    drop(conn);
    let requests = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        let (quiet, lookups) =
            cycle(root.path(), second.clone(), true, EditedNaming::Primary).await;
        assert_eq!(quiet.failed_count, 0);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(lookups, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
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
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let server = server().await;
    for pass_kind in [PassKind::Unfiled, PassKind::Album, PassKind::SmartFolder] {
        let root = tempfile::tempdir().unwrap();
        let edited = records(&server, Some(("edit-one", EDIT_ONE)));
        let options = |media_directory, folder_structure| CycleOptions {
            media_directory,
            folder_structure,
            pass_kind,
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            options("media", "first"),
        )
        .await;
        assert_eq!(
            initial.failed_count, 0,
            "{pass_kind:?}: {:?}",
            initial.stats
        );
        assert!(initial.db_sync_token_advance_safe);
        let first = root.path().join("media/first");
        assert_bytes(&first.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&first.join("IMG_0501_original.JPG"), ORIGINAL);
        let requests = server.received_requests().await.unwrap().len();
        let (changed, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            options("new-root", "second"),
        )
        .await;
        assert_eq!(
            changed.failed_count,
            0,
            "{pass_kind:?}: {:?}; errors {:?}",
            changed.stats,
            capture
                .events()
                .into_iter()
                .filter(|event| event.level == tracing::Level::ERROR
                    || event.level == tracing::Level::WARN)
                .collect::<Vec<_>>()
        );
        assert!(changed.db_sync_token_advance_safe);
        let second = root.path().join("new-root/second");
        assert_bytes(&first.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&first.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_bytes(&second.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&second.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            requests,
            "root drift redownloaded owned bytes"
        );
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        for _ in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                edited.clone(),
                true,
                EditedNaming::Primary,
                options("new-root", "second"),
            )
            .await;
            assert_eq!(quiet.failed_count, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(lookups, 0, "{pass_kind:?}: unexpected quiet lookup");
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations"
                ),
                operations
            );
        }
    }
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
        let current = root.path().join("media/IMG_0501.JPG");
        let archive = root.path().join("media/IMG_0501_original.JPG");
        let sidecar = current.with_file_name("IMG_0501.JPG.xmp");
        let packet = std::fs::read(&sidecar).ok();
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
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }
        // Remove only this synthetic journal, then prove recovery and steady state.
        std::fs::remove_dir_all(&journal).unwrap();
        for quiet in [false, true] {
            let (resumed, _) = cycle_options(
                root.path(),
                edited.clone(),
                quiet,
                EditedNaming::Primary,
                options("new-root"),
            )
            .await;
            assert_eq!(resumed.failed_count, 0, "{:?}", resumed.stats);
            assert!(resumed.db_sync_token_advance_safe);
            assert_bytes(&root.path().join("new-root/IMG_0501.JPG"), EDIT_ONE);
            assert_bytes(
                &root.path().join("new-root/IMG_0501_original.JPG"),
                ORIGINAL,
            );
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }
    }
}

#[tokio::test]
async fn primary_layout_root_drift_after_old_parent_moves_redownloads_then_stays_quiet() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = |media_directory| CycleOptions {
        media_directory,
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
    let old_family: String = rusqlite::Connection::open(root.path().join("state.db"))
        .unwrap()
        .query_row("SELECT family FROM primary_layout_bindings", [], |row| {
            row.get(0)
        })
        .unwrap();
    let old_claims = || {
        rusqlite::Connection::open(root.path().join("state.db")).unwrap()
            .query_row("SELECT group_concat(path_key || ':' || COALESCE(local_checksum,''), '|') FROM (SELECT path_key,local_checksum FROM primary_layout_claims WHERE family=?1 ORDER BY path_key)", [&old_family], |row| row.get::<_, String>(0)).unwrap()
    };
    let original_claims = old_claims();
    let moved = root.path().join("moved-media");
    let sidecar_name = "IMG_0501.JPG.xmp";
    let packet = std::fs::read(root.path().join("media").join(sidecar_name)).ok();
    std::fs::rename(root.path().join("media"), &moved).unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let (changed, lookups) = cycle_options(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        options("new-root"),
    )
    .await;
    assert_eq!(changed.failed_count, 0, "{:?}", changed.stats);
    assert!(changed.db_sync_token_advance_safe);
    assert!(lookups > 0, "missing sources must reach their layout owner");
    assert!(changed.stats.downloaded > 0);
    let resumed_requests = server.received_requests().await.unwrap().len();
    assert!(
        resumed_requests > requests,
        "missing owned sources must be downloaded"
    );
    let operations = db_count(
        root.path(),
        "SELECT COUNT(*) FROM primary_layout_operations",
    );
    let (quiet, lookups) = cycle_options(
        root.path(),
        edited,
        true,
        EditedNaming::Primary,
        options("new-root"),
    )
    .await;
    assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
    assert!(quiet.db_sync_token_advance_safe);
    assert_eq!(quiet.stats.downloaded, 0);
    assert_eq!(lookups, 0);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        resumed_requests
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations"
        ),
        operations
    );
    assert_eq!(old_claims(), original_claims);
    assert!(!root.path().join("media").exists());
    for directory in [moved, root.path().join("new-root")] {
        assert_bytes(&directory.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&directory.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(std::fs::read(directory.join(sidecar_name)).ok(), packet);
    }
}

#[tokio::test]
async fn primary_layout_stale_writers_and_downloaded_receipts_cannot_finalize_current_slots() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    cycle(root.path(), edited, false, EditedNaming::Primary).await;
    let db = state::SqliteStateDb::open(&root.path().join("state.db"))
        .await
        .unwrap();
    let current = root.path().join("media/IMG_0501.JPG");
    use crate::state::db::MetadataRewriteStore as _;
    assert!(db.guard_primary_writer(&current).await.is_err());
    assert!(
        db.mark_downloaded(
            "PrimarySync",
            "asset-501",
            "original",
            &current,
            "forged-local-sha",
            None
        )
        .await
        .is_err()
    );
    assert!(
        db.mark_downloaded(
            "PrimarySync",
            "asset-501",
            "adjusted",
            &current,
            "forged-local-sha",
            None
        )
        .await
        .is_err()
    );
    assert_bytes(&current, EDIT_ONE);
    let manifest = db.get_manifest_assets().await.unwrap();
    let adjusted = manifest
        .iter()
        .find(|row| row.version == "adjusted")
        .unwrap();
    assert_eq!(
        adjusted.local_checksum.as_deref(),
        Some(
            data_encoding::HEXLOWER
                .encode(&Sha256::digest(EDIT_ONE))
                .as_str()
        )
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
async fn review501_legacy_original_survives_adjusted_only_migration_as_archive() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    #[cfg(feature = "xmp")]
    let sidecar_modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let sidecar_modes = [false];
    for sidecar in sidecar_modes {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let options = |resolution| CycleOptions {
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
        assert_eq!(initial.failed_count, 0);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_ORIGINAL);
        let original_sidecar =
            sidecar.then(|| std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap());
        let edited = records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
        let (migrated, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Primary,
            options(crate::types::PhotoResolution::None),
        )
        .await;
        assert_eq!(
            migrated.failed_count,
            0,
            "sidecar={sidecar}: {:?}; events {:?}",
            migrated.stats,
            capture
                .events()
                .into_iter()
                .filter(|event| event.level == tracing::Level::WARN
                    || event.level == tracing::Level::ERROR)
                .collect::<Vec<_>>()
        );
        assert!(migrated.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
        assert_bytes(&media.join("IMG_0501_original.JPG"), VALID_ORIGINAL);
        if let Some(expected) = &original_sidecar {
            assert_bytes(&media.join("IMG_0501_original.JPG.xmp"), expected);
        }
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let (checksum, local_path): (String, String) = conn.query_row(
            "SELECT checksum,local_path FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='original'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(checksum, provider_hash(VALID_ORIGINAL));
        assert_eq!(
            std::path::PathBuf::from(local_path),
            media.join("IMG_0501_original.JPG")
        );
        drop(conn);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/original")
                .count(),
            1,
            "migration invented an unselected original download"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.url.path() == "/edit-one")
                .count(),
            1
        );
        for _ in 0..2 {
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
            assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
            assert_bytes(&media.join("IMG_0501_original.JPG"), VALID_ORIGINAL);
            if let Some(expected) = &original_sidecar {
                assert_bytes(&media.join("IMG_0501_original.JPG.xmp"), expected);
            }
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                requests.len()
            );
        }
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        assert!(
            db.get_pending_metadata_rewrites(100)
                .await
                .unwrap()
                .is_empty(),
            "an archive must not re-enter the ordinary writer queue"
        );
        // Prepared repair evidence must remain visible to source-deletion
        // guards even when the original is archived and unselected.
        let archive = media.join("IMG_0501_original.JPG");
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let checksum = data_encoding::HEXLOWER.encode(&Sha256::digest(VALID_ORIGINAL));
        let size = i64::try_from(VALID_ORIGINAL.len()).unwrap();
        conn.execute(
            "UPDATE assets SET capture_repair_metadata_hash='synthetic-prepared-repair',
                capture_repair_output_checksum=?1,capture_repair_output_size=?2
                WHERE library='PrimarySync' AND id='asset-501' AND version_size='original'",
            rusqlite::params![checksum, size],
        )
        .unwrap();
        conn.execute(
            "UPDATE asset_metadata_paths SET capture_repair_metadata_hash='synthetic-prepared-repair',
                capture_repair_output_checksum=?1,capture_repair_output_size=?2
                WHERE library='PrimarySync' AND id='asset-501' AND version_size='original' AND local_path=?3",
            rusqlite::params![checksum, size, archive.to_str().unwrap()],
        ).unwrap();
        drop(conn);
        assert!(
            db.mark_master_family_soft_deleted("PrimarySync", "501", None)
                .await
                .is_err(),
            "archival hid prepared capture-repair debt from source deletion"
        );
        assert_bytes(&archive, VALID_ORIGINAL);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
    }
}

#[tokio::test]
async fn review501_legacy_reverted_edit_is_preserved_outside_visible_library() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    #[cfg(feature = "xmp")]
    let sidecar_modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let sidecar_modes = [false];
    for sidecar in sidecar_modes {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let options = || CycleOptions {
            #[cfg(feature = "xmp")]
            sidecar,
            ..CycleOptions::default()
        };
        let (initial, _) = cycle_options(
            root.path(),
            records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT))),
            false,
            EditedNaming::Suffix,
            options(),
        )
        .await;
        assert_eq!(initial.failed_count, 0);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), VALID_EDIT);
        let edited_sidecar =
            sidecar.then(|| std::fs::read(media.join("IMG_0501_edited.JPG.xmp")).unwrap());
        let requests = server.received_requests().await.unwrap().len();
        let current = records_with_original(&server, VALID_ORIGINAL, None);
        let (migrated, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(
            migrated.failed_count,
            0,
            "sidecar={sidecar}: {:?}; events {:?}",
            migrated.stats,
            capture
                .events()
                .into_iter()
                .filter(|event| event.level == tracing::Level::WARN
                    || event.level == tracing::Level::ERROR)
                .collect::<Vec<_>>()
        );
        assert!(migrated.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_ORIGINAL);
        assert!(!media.join("IMG_0501_edited.JPG").exists());
        assert!(!media.join("IMG_0501_edited.JPG.xmp").exists());
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let manifest = db.get_manifest_assets().await.unwrap();
        let history = manifest
            .iter()
            .flat_map(|row| &row.preserved_files)
            .find(|receipt| std::fs::read(receipt.native_path.to_path()).unwrap() == VALID_EDIT)
            .expect("reverted edit has no independently preserved history receipt");
        let path = history.native_path.to_path();
        assert!(
            path.strip_prefix(&media)
                .unwrap()
                .components()
                .any(|part| part.as_os_str() == ".kei-history")
        );
        assert_eq!(history.provider_checksum, provider_hash(VALID_EDIT));
        assert_eq!(
            history.local_checksum,
            data_encoding::HEXLOWER.encode(&Sha256::digest(VALID_EDIT))
        );
        if let Some(expected) = &edited_sidecar {
            let mut xmp = path.as_os_str().to_os_string();
            xmp.push(".xmp");
            assert_bytes(&std::path::PathBuf::from(xmp), expected);
            assert_eq!(
                history.sidecar_checksum.as_deref(),
                Some(
                    data_encoding::HEXLOWER
                        .encode(&Sha256::digest(expected))
                        .as_str()
                )
            );
        }
        let summary = db.get_summary().await.unwrap().primary_layout.unwrap();
        let inventory = (
            summary.bound_families,
            summary.preserved_files,
            summary.pending_operations,
            summary.held_operations,
        );
        drop(db);
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
            assert_bytes(&media.join("IMG_0501.JPG"), VALID_ORIGINAL);
            assert_bytes(&path, VALID_EDIT);
            if let Some(expected) = &edited_sidecar {
                let mut xmp = path.as_os_str().to_os_string();
                xmp.push(".xmp");
                assert_bytes(&std::path::PathBuf::from(xmp), expected);
            }
            assert!(!media.join("IMG_0501_edited.JPG").exists());
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
                .await
                .unwrap();
            let summary = db.get_summary().await.unwrap().primary_layout.unwrap();
            assert_eq!(
                (
                    summary.bound_families,
                    summary.preserved_files,
                    summary.pending_operations,
                    summary.held_operations
                ),
                inventory
            );
        }
    }
}

#[tokio::test]
async fn review501_started_old_root_finishes_before_new_root_commits() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    for (directory, template) in [("new-root", ""), ("media", "new-template")] {
        for naming in [EditedNaming::Primary, EditedNaming::Suffix] {
            let server = server().await;
            let root = tempfile::tempdir().unwrap();
            let old_media = root.path().join("media");
            let (initial, _) = cycle(
                root.path(),
                records(&server, None),
                false,
                EditedNaming::Primary,
            )
            .await;
            assert_eq!(initial.failed_count, 0);
            let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
            conn.execute_batch("CREATE TRIGGER fail_layout_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'independent review commit failure'); END").unwrap();
            drop(conn);
            let (failed, _) = cycle(
                root.path(),
                records(&server, Some(("edit-one", EDIT_ONE))),
                false,
                EditedNaming::Primary,
            )
            .await;
            assert!(!failed.db_sync_token_advance_safe);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='publishing'"
                ),
                1
            );
            assert_bytes(&old_media.join("IMG_0501.JPG"), EDIT_ONE);
            rusqlite::Connection::open(root.path().join("state.db"))
                .unwrap()
                .execute_batch("DROP TRIGGER fail_layout_commit")
                .unwrap();
            let current = records(&server, Some(("edit-two", EDIT_TWO)));
            let parent = root.path().join(directory).join(template);
            let adjusted_path = parent.join(if naming == EditedNaming::Primary {
                "IMG_0501.JPG"
            } else {
                "IMG_0501_edited.JPG"
            });
            let original_path = parent.join(if naming == EditedNaming::Primary {
                "IMG_0501_original.JPG"
            } else {
                "IMG_0501.JPG"
            });
            for quiet in [false, true, true] {
                let before_requests = server.received_requests().await.unwrap().len();
                let (result, lookups) = cycle_options(
                    root.path(),
                    current.clone(),
                    quiet,
                    naming,
                    CycleOptions {
                        media_directory: directory,
                        folder_structure: template,
                        ..CycleOptions::default()
                    },
                )
                .await;
                assert_eq!(
                    result.failed_count,
                    0,
                    "{directory}/{template}, {naming:?}: {:?}, events {:?}",
                    result.stats,
                    capture
                        .events()
                        .into_iter()
                        .filter(
                            |e| e.level == tracing::Level::WARN || e.level == tracing::Level::ERROR
                        )
                        .collect::<Vec<_>>()
                );
                assert!(result.db_sync_token_advance_safe);
                assert_bytes(&adjusted_path, EDIT_TWO);
                assert_bytes(&original_path, ORIGINAL);
                assert_bytes(&old_media.join("IMG_0501.JPG"), EDIT_ONE);
                assert_bytes(&old_media.join("IMG_0501_original.JPG"), ORIGINAL);
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                let (checksum, local_path): (String, String) = conn.query_row(
                    "SELECT checksum,local_path FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'",
                    [], |row| Ok((row.get(0)?, row.get(1)?)),
                ).unwrap();
                assert_eq!(checksum, provider_hash(EDIT_TWO));
                assert_eq!(std::path::PathBuf::from(local_path), adjusted_path);
                assert_eq!(
                    db_count(
                        root.path(),
                        "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
                    ),
                    0
                );
                if quiet {
                    assert_eq!(result.stats.downloaded, 0);
                    assert_eq!(lookups, 0);
                    assert_eq!(
                        server.received_requests().await.unwrap().len(),
                        before_requests
                    );
                }
            }
            let requests = server.received_requests().await.unwrap();
            for endpoint in ["/original", "/edit-one", "/edit-two"] {
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request.url.path() == endpoint)
                        .count(),
                    1,
                    "unexpected GET count: {endpoint}, {directory}/{template}, {naming:?}; events {:?}",
                    capture
                        .events()
                        .into_iter()
                        .filter(|event| event.level == tracing::Level::WARN
                            || event.level == tracing::Level::ERROR)
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}

#[tokio::test]
async fn review501_changed_unselected_legacy_original_holds_migration() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let (initial, _) = cycle(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Suffix,
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let modified = b"locally modified original bytes";
    std::fs::write(media.join("IMG_0501.JPG"), modified).unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (held, _) = cycle_resolution(
        root.path(),
        edited.clone(),
        false,
        EditedNaming::Primary,
        crate::types::PhotoResolution::None,
    )
    .await;
    assert!(held.failed_count > 0);
    assert!(!held.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), modified);
    assert!(!media.join("IMG_0501_original.JPG").exists());
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    // Restore only this synthetic fixture's exact owned bytes, then recover.
    std::fs::write(media.join("IMG_0501.JPG"), ORIGINAL).unwrap();
    let (recovered, _) = cycle_resolution(
        root.path(),
        edited.clone(),
        true,
        EditedNaming::Primary,
        crate::types::PhotoResolution::None,
    )
    .await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    for _ in 0..2 {
        let (quiet, lookups) = cycle_resolution(
            root.path(),
            edited.clone(),
            true,
            EditedNaming::Primary,
            crate::types::PhotoResolution::None,
        )
        .await;
        assert_eq!(quiet.failed_count, 0);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
    }
}

#[tokio::test]
async fn review501_legacy_changed_extensions_and_aliases_retire_after_revert() {
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
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let options = || CycleOptions {
            #[cfg(feature = "xmp")]
            sidecar,
            ..CycleOptions::default()
        };
        let mut first = records(&server, Some(("edit-heic", heic.as_slice())));
        first[1]["fields"]["resJPEGFullFileType"] = json!({"value":"public.heic"});
        let (initial, _) =
            cycle_options(root.path(), first, false, EditedNaming::Suffix, options()).await;
        assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
        assert_bytes(&media.join("IMG_0501_edited.HEIC"), heic);
        let second = records(&server, Some(("edit-two", EDIT_TWO)));
        let (updated, _) =
            cycle_options(root.path(), second, false, EditedNaming::Suffix, options()).await;
        assert_eq!(updated.failed_count, 0, "{:?}", updated.stats);
        assert_bytes(&media.join("IMG_0501_edited.HEIC"), heic);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_TWO);
        // Two durable aliases can have identical content and sidecars but
        // distinct inodes. They need distinct journaled retirement slots.
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
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
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
        let old_files: Vec<_> = [
            ("IMG_0501_edited.HEIC", heic.as_slice()),
            ("IMG_0501_edited.JPG", EDIT_TWO),
            (alias_name.as_str(), EDIT_TWO),
        ]
        .into_iter()
        .map(|(name, bytes)| {
            let xmp = sidecar.then(|| std::fs::read(media.join(format!("{name}.xmp"))).unwrap());
            (name.to_owned(), bytes, xmp)
        })
        .collect();
        let requests = server.received_requests().await.unwrap().len();
        let current = records(&server, None);
        let (migrated, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(
            migrated.failed_count, 0,
            "sidecar={sidecar}: {:?}",
            migrated.stats
        );
        assert!(migrated.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let manifest = db.get_manifest_assets().await.unwrap();
        let mut histories = Vec::new();
        for (name, bytes, expected_xmp) in &old_files {
            assert!(!media.join(name).exists(), "left old visible alias {name}");
            assert!(!media.join(format!("{name}.xmp")).exists());
            let receipt = manifest
                .iter()
                .flat_map(|row| &row.preserved_files)
                .find(|receipt| {
                    receipt
                        .native_path
                        .to_path()
                        .file_name()
                        .and_then(|name| name.to_str())
                        == Some(name.as_str())
                })
                .expect("missing per-path historical receipt");
            let path = receipt.native_path.to_path();
            assert!(
                path.strip_prefix(&media)
                    .unwrap()
                    .components()
                    .any(|part| part.as_os_str() == ".kei-history")
            );
            assert_bytes(&path, bytes);
            assert_eq!(receipt.provider_checksum, provider_hash(bytes));
            assert_eq!(
                receipt.local_checksum,
                data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
            );
            if let Some(expected) = expected_xmp {
                let mut xmp = path.as_os_str().to_os_string();
                xmp.push(".xmp");
                assert_bytes(&std::path::PathBuf::from(xmp), expected);
                assert_eq!(
                    receipt.sidecar_checksum.as_deref(),
                    Some(
                        data_encoding::HEXLOWER
                            .encode(&Sha256::digest(expected))
                            .as_str()
                    )
                );
            }
            histories.push(path);
        }
        let receipts =
            crate::state::db::DownloadContextStateStore::get_primary_layout_receipts(&db)
                .await
                .unwrap();
        for history in &histories {
            assert!(
                receipts.iter().any(|receipt| {
                    receipt.local_path.as_ref() == Some(history) && receipt.is_preserved_history
                }),
                "preserved history must remain a reuse source, not a visible legacy alias"
            );
        }
        let summary = db.get_summary().await.unwrap().primary_layout.unwrap();
        let inventory = (
            summary.bound_families,
            summary.preserved_files,
            summary.pending_operations,
            summary.held_operations,
        );
        drop(db);
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
            assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
            for ((name, bytes, expected_xmp), history) in old_files.iter().zip(&histories) {
                assert!(!media.join(name).exists());
                assert_bytes(history, bytes);
                if let Some(expected) = expected_xmp {
                    let mut xmp = history.as_os_str().to_os_string();
                    xmp.push(".xmp");
                    assert_bytes(&std::path::PathBuf::from(xmp), expected);
                }
            }
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
                .await
                .unwrap();
            let summary = db.get_summary().await.unwrap().primary_layout.unwrap();
            assert_eq!(
                (
                    summary.bound_families,
                    summary.preserved_files,
                    summary.pending_operations,
                    summary.held_operations
                ),
                inventory
            );
        }
    }
}

#[tokio::test]
async fn review501_retained_source_mutation_between_snapshots_holds_publication() {
    for replace_inode in [false, true] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let current = root.path().join("media/IMG_0501.JPG");
        for edit in [("edit-one", EDIT_ONE), ("edit-two", EDIT_TWO)] {
            let (result, _) = cycle(
                root.path(),
                records(&server, Some(edit)),
                false,
                EditedNaming::Primary,
            )
            .await;
            assert_eq!(result.failed_count, 0, "{:?}", result.stats);
            assert!(result.db_sync_token_advance_safe);
        }
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
        let manifest = db.get_manifest_assets().await.unwrap();
        let history = manifest
            .iter()
            .flat_map(|row| &row.preserved_files)
            .find(|file| file.provider_checksum == provider_hash(EDIT_ONE))
            .expect("edit-one history missing")
            .native_path
            .to_path();
        drop(db);
        assert_bytes(&history, EDIT_ONE);
        let changed = b"unrelated local bytes introduced between source snapshots";
        let hook = crate::test_helpers::primary_source_mutation(
            history.clone(),
            changed.to_vec(),
            replace_inode,
        );
        let requests = server.received_requests().await.unwrap().len();
        let (held, _) = cycle(
            root.path(),
            records(&server, Some(("edit-one", EDIT_ONE))),
            false,
            EditedNaming::Primary,
        )
        .await;
        assert!(
            hook.fired(),
            "the deterministic proof boundary was not exercised"
        );
        assert!(!held.db_sync_token_advance_safe);
        assert!(held.failed_count > 0, "{:?}", held.stats);
        assert_bytes(&history, changed);
        assert_bytes(&current, EDIT_TWO);
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let local: String = conn.query_row("SELECT local_checksum FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'", [], |row| row.get(0)).unwrap();
        assert_ne!(
            local,
            data_encoding::HEXLOWER.encode(&Sha256::digest(changed))
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
                "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
            ),
            1
        );
        drop(conn);
        // Restore only the synthetic fixture's known history bytes. The held
        // journal must then converge without changing provider identity or GETs.
        std::fs::write(&history, EDIT_ONE).unwrap();
        for quiet in [false, true, true] {
            let (result, _) = cycle(
                root.path(),
                records(&server, Some(("edit-one", EDIT_ONE))),
                quiet,
                EditedNaming::Primary,
            )
            .await;
            assert_eq!(result.failed_count, 0, "{:?}", result.stats);
            assert!(result.db_sync_token_advance_safe);
            assert_bytes(&current, EDIT_ONE);
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
            let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
            let (checksum, local, path): (String, String, String) = conn.query_row("SELECT checksum,local_checksum,local_path FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'", [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
            assert_eq!(checksum, provider_hash(EDIT_ONE));
            assert_eq!(
                local,
                data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_ONE))
            );
            assert_eq!(std::path::PathBuf::from(path), current);
        }
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn review501_suffix_album_collecting_delta_uses_managed_metadata_owner() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
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
    let old_xmp = std::fs::read(media.join("IMG_0501_edited.JPG.xmp")).unwrap();
    let archive_xmp = std::fs::read(media.join("IMG_0501_original.JPG.xmp")).unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let mut changed = current;
    changed[1]["recordChangeTag"] = json!("ct-asset-caption");
    changed[1]["fields"]["captionEnc"] =
        json!({"type":"STRING","value":"caption changed after suffix reversal"});
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
    assert_bytes(&media.join("IMG_0501.JPG"), VALID_ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), VALID_EDIT);
    let xmp = std::fs::read_to_string(media.join("IMG_0501_edited.JPG.xmp")).unwrap();
    assert!(
        xmp.contains("caption changed after suffix reversal"),
        "stats {:?}, lookups/events {:?}: {xmp}",
        result.stats,
        capture.events()
    );
    assert_ne!(xmp.as_bytes(), old_xmp);
    assert_bytes(&media.join("IMG_0501_original.JPG.xmp"), &archive_xmp);
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.get_pending_metadata_rewrites(100)
            .await
            .unwrap()
            .is_empty(),
        "whole-asset refresh left competing ordinary markers"
    );
    let manifest = db.get_manifest_assets().await.unwrap();
    let retained = manifest
        .iter()
        .flat_map(|row| &row.preserved_files)
        .any(|file| {
            let mut path = file.native_path.to_path().as_os_str().to_os_string();
            path.push(".xmp");
            std::fs::read(std::path::PathBuf::from(path)).is_ok_and(|bytes| bytes == old_xmp)
        });
    assert!(
        retained,
        "prior adjusted sidecar was not independently preserved"
    );
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
        assert_bytes(&media.join("IMG_0501_edited.JPG.xmp"), xmp.as_bytes());
        assert_bytes(&media.join("IMG_0501_original.JPG.xmp"), &archive_xmp);
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
}

#[tokio::test]
async fn review501_explicit_metadata_refresh_uses_preserving_layout_owner() {
    #[cfg(feature = "xmp")]
    let modes = [false, true];
    #[cfg(not(feature = "xmp"))]
    let modes = [false];
    for sidecar in modes {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, EDIT_TWO).await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let options = |refresh_metadata| CycleOptions {
            refresh_metadata,
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
        let xmp = sidecar.then(|| std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap());
        let requests = server.received_requests().await.unwrap().len();
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let (generation, hash): (i64, String) = conn.query_row("SELECT b.generation,a.metadata_hash FROM primary_layout_bindings b JOIN assets a ON a.library=b.library AND a.id=b.child WHERE a.version_size='adjusted'", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        // Simulate stale catalogue capture without changing any owned media.
        conn.execute("UPDATE assets SET metadata_hash='stale-catalogue-capture' WHERE version_size='adjusted'", []).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_refresh_prepare BEFORE UPDATE OF phase ON primary_layout_operations WHEN NEW.phase='prepared' BEGIN SELECT RAISE(FAIL,'injected forced-refresh preparation failure'); END").unwrap();
        drop(conn);
        let (held, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(true),
        )
        .await;
        assert!(held.failed_count > 0);
        assert!(!held.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let refresh: bool = conn.query_row("SELECT json_extract(CAST(header AS TEXT),'$.refresh_metadata') FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')", [], |row| row.get(0)).unwrap();
        assert!(refresh, "forced refresh intent was not durable");
        conn.execute_batch("DROP TRIGGER fail_refresh_prepare")
            .unwrap();
        drop(conn);
        // Restart without the one-shot operator flag. Recorded intent and work
        // must still complete before the unchanged-binding shortcut can run.
        let (refreshed, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            options(false),
        )
        .await;
        assert_eq!(refreshed.failed_count, 0, "{:?}", refreshed.stats);
        assert!(refreshed.db_sync_token_advance_safe);
        assert_eq!(refreshed.stats.downloaded, 0);
        assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
        assert_bytes(&media.join("IMG_0501_original.JPG"), VALID_ORIGINAL);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let (new_generation, new_hash): (i64,String) = conn.query_row("SELECT b.generation,a.metadata_hash FROM primary_layout_bindings b JOIN assets a ON a.library=b.library AND a.id=b.child WHERE a.version_size='adjusted'", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        assert!(
            new_generation > generation,
            "explicit refresh silently took the unchanged-binding shortcut"
        );
        assert_eq!(new_hash, hash);
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
        if let Some(xmp) = &xmp {
            #[cfg(feature = "xmp")]
            let fresh_xmp = std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap();
            #[cfg(feature = "xmp")]
            {
                let parsed: xmp_toolkit::XmpMeta =
                    std::str::from_utf8(&fresh_xmp).unwrap().parse().unwrap();
                let old: xmp_toolkit::XmpMeta = std::str::from_utf8(xmp).unwrap().parse().unwrap();
                for (namespace, key) in [
                    (xmp_toolkit::xmp_ns::XMP, "CreateDate"),
                    (xmp_toolkit::xmp_ns::EXIF, "DateTimeOriginal"),
                ] {
                    assert_eq!(
                        parsed.property(namespace, key).unwrap().value,
                        old.property(namespace, key).unwrap().value
                    );
                }
                assert!(String::from_utf8_lossy(&fresh_xmp).contains("TestAlbum"));
            }
            assert!(
                db.get_manifest_assets()
                    .await
                    .unwrap()
                    .iter()
                    .flat_map(|row| &row.preserved_files)
                    .any(|file| {
                        let mut path = file.native_path.to_path().as_os_str().to_os_string();
                        path.push(".xmp");
                        std::fs::read(std::path::PathBuf::from(path))
                            .is_ok_and(|bytes| bytes == *xmp)
                    }),
                "forced refresh did not retain prior metadata bytes"
            );
        }
        drop(db);
        for _ in 0..2 {
            let (quiet, lookups) = cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Primary,
                options(false),
            )
            .await;
            assert_eq!(quiet.failed_count, 0, "{:?}", quiet.stats);
            assert!(quiet.db_sync_token_advance_safe);
            assert_eq!(quiet.stats.downloaded, 0);
            assert_eq!(lookups, 0);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
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
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let mut current = records_with_original(&server, original_bytes, None);
        current[0]["fields"]["filenameEnc"]["value"] = json!(filename);
        if is_heif {
            current[0]["fields"]["itemType"] = json!({"value":"public.heic"});
            current[0]["fields"]["resOriginalFileType"] = json!({"value":"public.heic"});
        }
        current[1]["fields"]["isFavorite"] = json!({"value":1});
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
        let provider = Session {
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
        assert_eq!(
            recovered.failed_count, 0,
            "{filename}: {:?}",
            recovered.stats
        );
        assert!(recovered.db_sync_token_advance_safe);
        assert_eq!(recovered.stats.downloaded, 0);
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
async fn review501_suffix_album_collecting_edit_recovers_without_redownload() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = || CycleOptions {
        pass_kind: PassKind::Album,
        private: false,
        ..CycleOptions::default()
    };
    let first = records(&server, Some(("edit-one", EDIT_ONE)));
    for (naming, quiet) in [(EditedNaming::Primary, false), (EditedNaming::Suffix, true)] {
        let (result, _) = cycle_options(root.path(), first.clone(), quiet, naming, options()).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
    }
    rusqlite::Connection::open(root.path().join("state.db")).unwrap().execute_batch(
        "CREATE TRIGGER fail_collecting_layout_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected collecting layout commit failure'); END"
    ).unwrap();
    let second = records(&server, Some(("edit-two", EDIT_TWO)));
    let (held, _) = cycle_options(
        root.path(),
        second.clone(),
        false,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert!(held.failed_count > 0);
    assert!(!held.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_TWO);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='publishing'"
        ),
        1
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.path() == "/edit-two")
            .count(),
        1
    );
    rusqlite::Connection::open(root.path().join("state.db"))
        .unwrap()
        .execute_batch("DROP TRIGGER fail_collecting_layout_commit")
        .unwrap();
    for index in 0..3 {
        let (result, lookups) = cycle_options(
            root.path(),
            second.clone(),
            true,
            EditedNaming::Suffix,
            options(),
        )
        .await;
        if index > 0 {
            assert_eq!(lookups, 0);
        }
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_TWO);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            requests.len()
        );
    }
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.get_manifest_assets()
            .await
            .unwrap()
            .iter()
            .flat_map(|row| &row.preserved_files)
            .any(|file| {
                file.provider_checksum == provider_hash(EDIT_ONE)
                    && std::fs::read(file.native_path.to_path())
                        .is_ok_and(|bytes| bytes == EDIT_ONE)
            }),
        "collecting edit lost its prior managed revision"
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let (provider,local,path): (String,String,String) = conn.query_row("SELECT checksum,local_checksum,local_path FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    assert_eq!(provider, provider_hash(EDIT_TWO));
    assert_eq!(
        local,
        data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_TWO))
    );
    assert_eq!(
        std::path::PathBuf::from(path),
        media.join("IMG_0501_edited.JPG")
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
        ),
        0
    );
}
