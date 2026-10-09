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
#[derive(Clone)]
struct Session {
    records: Vec<Value>,
    quiet: bool,
    lookups: Arc<AtomicUsize>,
    current: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
}
#[async_trait::async_trait]
impl PhotosSession for Session {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"cycle-token","moreComing":false,"records":if self.quiet {vec![]} else {self.records.clone()}}]}),
            );
        }
        if url.contains("/records/lookup?") {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let request: Value = serde_json::from_str(&body)?;
            let current = self
                .current
                .as_ref()
                .map(|current| current.lock().unwrap().clone())
                .unwrap_or_else(|| self.records.clone());
            let records: Vec<_> = request["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|wanted| {
                    current
                        .iter()
                        .find(|record| record["recordName"] == wanted["recordName"])
                        .unwrap()
                        .clone()
                })
                .collect();
            return Ok(json!({"records":records}));
        }
        if url.contains("/records/query/batch?") || url.contains("/internal/records/query/batch") {
            let body: Value = serde_json::from_str(&body)?;
            return Ok(
                json!({"batch":body["batch"].as_array().unwrap().iter().map(|_|json!({"records":[{"fields":{"itemCount":{"value":1}}}]})).collect::<Vec<_>>()}),
            );
        }
        if url.contains("/records/query?") {
            let body: Value = serde_json::from_str(&body)?;
            let offset = body["query"]["filterBy"]
                .as_array()
                .and_then(|items| items.iter().find(|i| i["fieldName"] == "startRank"))
                .and_then(|i| i["fieldValue"]["value"].as_u64())
                .unwrap_or(0);
            return Ok(
                json!({"records":if offset==0 {self.records.clone()} else {vec![]},"syncToken":"query-token"}),
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
    media_directory: &'static str,
    folder_structure: &'static str,
    pass_kind: PassKind,
    resolution: crate::types::PhotoResolution,
    private: bool,
    sidecar: bool,
    controls: download::DownloadControls,
    mov_policy: crate::types::LivePhotoMovFilenamePolicy,
    live_mode: crate::types::LivePhotoMode,
    current: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
}
impl Default for CycleOptions {
    fn default() -> Self {
        Self {
            media_directory: "media",
            folder_structure: "",
            pass_kind: PassKind::Unfiled,
            resolution: crate::types::PhotoResolution::Original,
            private: false,
            sidecar: false,
            controls: download::DownloadControls::download_hidden(),
            mov_policy: crate::types::LivePhotoMovFilenamePolicy::Suffix,
            live_mode: crate::types::LivePhotoMode::Both,
            current: None,
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
    let resolution = options.resolution;
    let lookups = Arc::new(AtomicUsize::new(0));
    let provider = Session {
        records,
        quiet,
        lookups: lookups.clone(),
        current: options.current.clone(),
    };
    let owner = state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap();
    let inner = Arc::new(if options.private {
        state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
            .await
            .unwrap()
    } else {
        state::SqliteStateDb::open(&root.join("state.db"))
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
        if options.private {
            album.set_shadow_capture(
                crate::icloud::photos::inbox::ShadowCapture::new(inner.clone(), owner, "com"),
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
    library.library =
        crate::icloud::photos::PhotoLibrary::new_stub_with_zone(Box::new(provider), "PrimarySync");
    let db: Arc<dyn download::DownloadStore> = inner;
    let media = root.join(options.media_directory);
    // run_loop creates the configured root before entering run_cycle. Reproduce
    // that caller contract when a fixture changes roots and reuses local bytes.
    std::fs::create_dir_all(&media).unwrap();
    let base = make_run_cycle_download_config_builder(&media, db.clone());
    let build = |mode, exclude, groups, zone| {
        let mut config = base(mode, exclude, groups, zone);
        let config = Arc::make_mut(&mut config);
        config.edited = true;
        config.edited_naming = naming;
        config.resolution = resolution;
        config.metadata.xmp_sidecar = options.sidecar;
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
    config.photos.edited = true;
    config.photos.edited_naming = naming;
    config.photos.resolution = resolution;
    config.metadata.xmp_sidecar = options.sidecar;
    config.photos.live_photo_mov_filename_policy = options.mov_policy;
    config.photos.live_photo_mode = options.live_mode;
    let (_session_root, session) = make_shared_session_for_run_cycle().await;
    let result = Box::pin(crate::sync_cycle::run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
        false,
        &build,
        options.controls,
        &session,
        &CancellationToken::new(),
    ))
    .await
    .unwrap();
    drop(base);
    drop(db);
    (result, lookups.load(Ordering::SeqCst))
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
#[tokio::test]
async fn contract_file_publish_no_overwrite_primary_layout_edits_revert_and_quiet_reopen() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    std::fs::write(media.join("foreign.jpg"), b"unrelated user bytes").unwrap();
    let unedited = records(&server, None);
    let (initial, _) = cycle(root.path(), unedited.clone(), false, EditedNaming::Primary).await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert!(!media.join("IMG_0501_original.JPG").exists());
    for (name, bytes) in [("edit-one", EDIT_ONE), ("edit-two", EDIT_TWO)] {
        let (result, _) = cycle(
            root.path(),
            records(&server, Some((name, bytes))),
            false,
            EditedNaming::Primary,
        )
        .await;
        assert_eq!(result.failed_count, 0, "{name}: {:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{name}");
        assert_bytes(&media.join("IMG_0501.JPG"), bytes);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM assets WHERE library='PrimarySync' AND id='asset-501' AND status='downloaded' AND version_size IN ('original','adjusted')"
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
    }
    let (reverted, _) = cycle(root.path(), unedited.clone(), false, EditedNaming::Primary).await;
    assert_eq!(reverted.failed_count, 0);
    assert!(reverted.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let paths: Vec<Vec<u8>> = conn
        .prepare("SELECT native_path FROM primary_layout_preserved")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let preserved: Vec<Vec<u8>> = paths
        .iter()
        .map(|path| {
            std::fs::read(
                serde_json::from_slice::<state::db::provider_selection::SelectionPath>(path)
                    .unwrap()
                    .to_path(),
            )
            .unwrap()
        })
        .collect();
    assert!(preserved.iter().any(|bytes| bytes == EDIT_ONE));
    assert!(preserved.iter().any(|bytes| bytes == EDIT_TWO));
    drop(conn);
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let manifest = db.get_manifest_assets().await.unwrap();
    let old_edit = manifest
        .iter()
        .find(|row| row.version == "adjusted")
        .unwrap();
    assert!(
        old_edit.local_path.is_none(),
        "manifest attributed reused primary to a superseded edit"
    );
    assert!(
        old_edit
            .preserved_files
            .iter()
            .any(|receipt| std::fs::read(receipt.native_path.to_path()).unwrap() == EDIT_TWO)
    );
    for receipt in manifest.iter().flat_map(|row| &row.preserved_files) {
        let bytes = std::fs::read(receipt.native_path.to_path()).unwrap();
        use sha2::Digest;
        assert_eq!(
            receipt.local_checksum,
            data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&bytes))
        );
        assert_eq!(receipt.phase, "committed");
    }
    let summary = db.get_summary().await.unwrap().primary_layout.unwrap();
    assert_eq!(summary.bound_families, 1);
    assert_eq!(summary.pending_operations, 0);
    assert!(summary.preserved_files >= 3);
    drop(db);
    let requests = server.received_requests().await.unwrap().len();
    let operations = db_count(
        root.path(),
        "SELECT COUNT(*) FROM primary_layout_operations",
    );
    for _ in 0..2 {
        let (result, lookups) =
            cycle(root.path(), unedited.clone(), true, EditedNaming::Primary).await;
        assert_eq!(result.failed_count, 0);
        assert!(result.db_sync_token_advance_safe);
        assert_eq!(result.stats.downloaded, 0);
        assert_eq!(lookups, 0, "quiet cycle hydrated completed layout");
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_bytes(&media.join("foreign.jpg"), b"unrelated user bytes");
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

#[tokio::test]
async fn primary_layout_migrates_suffix_and_disables_without_redownload() {
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
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    assert!(!media.join("IMG_0501_edited.JPG").exists());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        downloads,
        "migration fetched verified local media"
    );
    let (disabled, _) = cycle(root.path(), edited.clone(), true, EditedNaming::Suffix).await;
    assert_eq!(disabled.failed_count, 0, "{:?}", disabled.stats);
    assert!(disabled.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501_edited.JPG"), EDIT_ONE);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    assert_eq!(server.received_requests().await.unwrap().len(), downloads);
}

#[tokio::test]
async fn primary_layout_foreign_collision_and_local_modification_hold() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    std::fs::write(media.join("IMG_0501.JPG"), b"foreign primary bytes").unwrap();
    let (first, _) = cycle(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Primary,
    )
    .await;
    assert_eq!(first.failed_count, 0);
    assert!(first.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), b"foreign primary bytes");
    let names: Vec<_> = std::fs::read_dir(&media)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "JPG"))
        .collect();
    let current = names
        .iter()
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("IMG_0501-")
                && !path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .ends_with("_original.JPG")
        })
        .unwrap();
    let archive = names
        .iter()
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("_original.JPG")
        })
        .unwrap();
    assert_bytes(current, EDIT_ONE);
    assert_bytes(archive, ORIGINAL);
    let (second, _) = cycle(
        root.path(),
        records(&server, Some(("edit-two", EDIT_TWO))),
        false,
        EditedNaming::Primary,
    )
    .await;
    assert_eq!(second.failed_count, 0);
    assert_bytes(current, EDIT_TWO);
    assert_bytes(archive, ORIGINAL);
    std::fs::write(current, b"user changed owned bytes").unwrap();
    let (conflict, _) = cycle(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Primary,
    )
    .await;
    assert!(!conflict.db_sync_token_advance_safe);
    assert_bytes(current, b"user changed owned bytes");
    assert_bytes(archive, ORIGINAL);
    assert_bytes(&media.join("IMG_0501.JPG"), b"foreign primary bytes");
}

#[tokio::test]
async fn contract_source_checkpoint_requires_durable_recovery_primary_layout_after_database_failure()
 {
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
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_layout_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected layout commit failure'); END").unwrap();
    drop(conn);
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (failed, _) = cycle(root.path(), edited.clone(), false, EditedNaming::Primary).await;
    assert!(!failed.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='publishing'"
        ),
        1
    );
    let requests = server.received_requests().await.unwrap().len();
    rusqlite::Connection::open(root.path().join("state.db"))
        .unwrap()
        .execute_batch("DROP TRIGGER fail_layout_commit")
        .unwrap();
    let (recovered, _) = cycle(root.path(), edited, true, EditedNaming::Primary).await;
    assert_eq!(recovered.failed_count, 0, "{:?}", recovered.stats);
    assert!(recovered.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests,
        "recovery downloaded already published bytes"
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
        ),
        0
    );
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
async fn primary_layout_private_generations_remain_complete_across_edits() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = || CycleOptions {
        private: true,
        ..CycleOptions::default()
    };
    for (edit, bytes) in [
        (None, ORIGINAL),
        (Some(("edit-one", EDIT_ONE)), EDIT_ONE),
        (Some(("edit-two", EDIT_TWO)), EDIT_TWO),
        (None, ORIGINAL),
    ] {
        let (result, _) = cycle_options(
            root.path(),
            records(&server, edit),
            false,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&media.join("IMG_0501.JPG"), bytes);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_destinations WHERE admitted=1 AND (verified_media=0 OR verified_metadata=0)"
            ),
            0
        );
    }
    assert!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_generations"
        ) > 0,
        "fixture never activated private selection"
    );
    let downloads = server.received_requests().await.unwrap().len();
    for _ in 0..2 {
        let (quiet, lookups) = cycle_options(
            root.path(),
            records(&server, None),
            true,
            EditedNaming::Primary,
            options(),
        )
        .await;
        assert_eq!(quiet.failed_count, 0);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_eq!(server.received_requests().await.unwrap().len(), downloads);
    }
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
    let edited = records(&server, Some(("edit-one", EDIT_ONE)));
    let (first, _) = cycle(root.path(), edited.clone(), false, EditedNaming::Primary).await;
    assert_eq!(first.failed_count, 0);
    std::fs::write(media.join("IMG_0501.JPG"), b"local user changes").unwrap();
    let operations = db_count(
        root.path(),
        "SELECT COUNT(*) FROM primary_layout_operations",
    );
    let claims = db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_claims");
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
        assert!(!result.db_sync_token_advance_safe);
        assert!(
            result.stats.enumeration_errors > 0,
            "conflict was silently skipped"
        );
        assert_bytes(&media.join("IMG_0501.JPG"), b"local user changes");
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            operations
        );
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_claims"),
            claims
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
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
