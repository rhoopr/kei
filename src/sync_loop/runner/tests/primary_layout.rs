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
mod directory_authority;
mod indexing;
mod membership;

#[derive(Clone)]
struct IndexingOptions {
    readiness: Arc<std::sync::Mutex<&'static str>>,
    regress: bool,
    cancel: CancellationToken,
    interrupt: bool,
    stale_plan: bool,
    date_bound: bool,
    token: Option<&'static str>,
    queries: Arc<AtomicUsize>,
    cursors: Arc<std::sync::Mutex<Vec<String>>>,
}
fn provider_hash(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
}
fn fixture_owner() -> state::db::account::AccountOwner {
    state::db::account::AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap()
}
fn records(server: &MockServer, edit: Option<(&str, &[u8])>) -> Vec<Value> {
    records_at(&server.uri(), edit)
}

fn second_date_records(server: &MockServer) -> Vec<Value> {
    let mut second = records(server, None);
    second[0]["recordName"] = json!("502");
    second[0]["fields"]["filenameEnc"]["value"] = json!("IMG_0502.JPG");
    second[1]["recordName"] = json!("asset-502");
    second[1]["fields"]["masterRef"]["value"]["recordName"] = json!("502");
    second[1]["fields"]["assetDate"]["value"] = json!(1731628800000_i64);
    second
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
    page["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
        json!("_defaultOwner");
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
    indexing: Option<IndexingOptions>,
}
fn versioned_records(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .cloned()
        .map(|mut record| {
            // Provider revisions identify the actual emitted fields, including edits
            // applied by scenario builders after the base fixture was constructed.
            record["recordChangeTag"] = json!(provider_hash(
                &serde_json::to_vec(&record["fields"]).unwrap()
            ));
            record
        })
        .collect()
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
        let source = versioned_records(&self.records);
        if request["query"]["recordType"] == "CheckIndexingState" {
            return Ok(
                json!({"records":[{"fields":{"state":{"value":*self.indexing.as_ref().unwrap().readiness.lock().unwrap()}}}]}),
            );
        }
        if url.contains("/changes/zone?") {
            if let Some(indexing) = &self.indexing {
                indexing.cursors.lock().unwrap().push(
                    request["zones"][0]["syncToken"]
                        .as_str()
                        .unwrap()
                        .to_owned(),
                );
            }
            let token = if self.indexing.is_some() {
                "cycle-token".to_owned()
            } else {
                format!(
                    "cycle-token-{}",
                    provider_hash(&serde_json::to_vec(&source)?)
                )
            };
            return Ok(
                json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":token,"moreComing":false,"records":if self.quiet {vec![]} else {source}}]}),
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
            let current = versioned_records(&current);
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
                json!({"batch":body["batch"].as_array().unwrap().iter().map(|_|json!({"records":[{"fields":{"itemCount":{"value":self.records.len()/2}}}]})).collect::<Vec<_>>()}),
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
                json!({"records":if offset==0 {source} else {vec![]},"syncToken":self.indexing.as_ref().map_or(Some("query-token"), |indexing| indexing.token)}),
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
    exif_description: bool,
    second_album: bool,
    album_name: &'static str,
    expected_due_selection: Option<bool>,
    indexing: Option<IndexingOptions>,
    refresh_metadata: bool,
    media_directory: &'static str,
    folder_structure: &'static str,
    shared_folder_structure: Option<&'static str>,
    pass_kind: PassKind,
    resolution: crate::types::PhotoResolution,
    #[cfg(feature = "xmp")]
    sidecar: bool,
    #[cfg(feature = "xmp")]
    embed_xmp: bool,
    controls: download::DownloadControls,
    mov_policy: crate::types::LivePhotoMovFilenamePolicy,
    live_mode: crate::types::LivePhotoMode,
    current: Option<Arc<std::sync::Mutex<Vec<Value>>>>,
}
impl Default for CycleOptions {
    fn default() -> Self {
        Self {
            exif_description: false,
            second_album: false,
            album_name: "TestAlbum",
            expected_due_selection: None,
            indexing: None,
            refresh_metadata: false,
            media_directory: "media",
            folder_structure: "",
            shared_folder_structure: None,
            pass_kind: PassKind::Unfiled,
            resolution: crate::types::PhotoResolution::Original,
            #[cfg(feature = "xmp")]
            sidecar: false,
            #[cfg(feature = "xmp")]
            embed_xmp: false,
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
        indexing: options.indexing.clone(),
    };
    let owner = fixture_owner();
    let inner = Arc::new(
        state::SqliteStateDb::open_owned(&root.join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let (name, list_type, obj_type, query_filter) = match options.pass_kind {
        PassKind::Album => (
            options.album_name,
            "CPLContainerRelationLiveByAssetDate",
            "CPLContainerRelationNotDeletedByAssetDate:album-501",
            Some(Arc::new(
                json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":"album-501"}}]),
            )),
        ),
        PassKind::SmartFolder => (
            "Hidden",
            "CPLAssetAndMasterHiddenByAssetDate",
            "CPLAssetHiddenByAssetDate",
            None,
        ),
        PassKind::Unfiled => (
            "TestAlbum",
            "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted",
            "CPLAssetByAssetDateWithoutHiddenOrDeleted",
            None,
        ),
    };
    let mut album = crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from(list_type),
            obj_type: Arc::from(obj_type),
            query_filter,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: (options.pass_kind == PassKind::Album).then(|| Arc::from("album-501")),
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider.clone()),
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(inner.clone(), owner, "com"),
        Arc::from("private"),
    );
    let pass = AlbumPass {
        kind: options.pass_kind,
        album,
        exclude_ids: Arc::default(),
    };
    let mut passes = vec![pass];
    if options.second_album {
        let mut album = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("SecondAlbum"),
                list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
                obj_type: Arc::from("CPLContainerRelationNotDeletedByAssetDate:album-502"),
                query_filter: Some(Arc::new(
                    json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":"album-502"}}]),
                )),
                page_size: 100,
                zone_id: Arc::new(
                    json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                ),
                retry_config: crate::retry::RetryConfig::default(),
                container_id: Some(Arc::from("album-502")),
                cross_zone_sources: Vec::new(),
            },
            Box::new(provider.clone()),
        );
        album.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(inner.clone(), fixture_owner(), "com"),
            Arc::from("private"),
        );
        passes.push(AlbumPass {
            kind: PassKind::Album,
            album,
            exclude_ids: Arc::default(),
        });
    }
    if options.shared_folder_structure.is_some() {
        let mut shared = crate::icloud::photos::PhotoAlbum::new(
            crate::icloud::photos::PhotoAlbumConfig {
                params: Arc::default(),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("Hidden"),
                list_type: Arc::from("CPLAssetAndMasterHiddenByAssetDate"),
                obj_type: Arc::from("CPLAssetHiddenByAssetDate"),
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
        shared.set_shadow_capture(
            crate::icloud::photos::inbox::ShadowCapture::new(inner.clone(), fixture_owner(), "com"),
            Arc::from("private"),
        );
        passes.push(AlbumPass {
            kind: PassKind::SmartFolder,
            album: shared,
            exclude_ids: Arc::default(),
        });
    }
    let mut library =
        make_run_cycle_library_state_with_passes("PrimarySync", "sync_token:PrimarySync", passes);
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
        config.metadata.set_exif_description = options.exif_description;
        config.edited = true;
        config.edited_naming = naming;
        config.skip_created_after = options
            .indexing
            .as_ref()
            .is_some_and(|indexing| indexing.date_bound)
            .then_some(crate::config::CreatedDateFilter::CaptureDate(
                chrono::NaiveDate::from_ymd_opt(2100, 1, 1).unwrap(),
            ));
        config.resolution = resolution;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = options.sidecar;
            config.metadata.embed_xmp = options.embed_xmp;
        }
        config.live_photo_mov_filename_policy = options.mov_policy;
        config.live_photo_mode = options.live_mode;
        config.folder_structure = options
            .shared_folder_structure
            .unwrap_or(options.folder_structure)
            .to_owned();
        config.folder_structure_albums = Arc::from(options.folder_structure);
        config.folder_structure_smart_folders = Arc::from(
            options
                .shared_folder_structure
                .unwrap_or(options.folder_structure),
        );
        Arc::new(config.clone())
    };
    let mut config = make_run_cycle_config();
    config.download.directory = media.clone();
    config.download.folder_structure = options
        .shared_folder_structure
        .unwrap_or(options.folder_structure)
        .to_owned();
    config.download.folder_structure_albums = options.folder_structure.to_owned();
    config.download.folder_structure_smart_folders = options
        .shared_folder_structure
        .unwrap_or(options.folder_structure)
        .to_owned();
    config.runtime.refresh_metadata = options.refresh_metadata;
    config.metadata.set_exif_description = options.exif_description;
    config.photos.edited = true;
    config.photos.edited_naming = naming;
    config.photos.resolution = resolution;
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = options.sidecar;
        config.metadata.embed_xmp = options.embed_xmp;
    }
    config.photos.live_photo_mov_filename_policy = options.mov_policy;
    config.photos.live_photo_mode = options.live_mode;
    let (_session_root, session) = make_shared_session_for_run_cycle().await;
    if let Some(expected) = options.expected_due_selection {
        let check = build(
            download::SyncMode::Full,
            Arc::default(),
            Arc::default(),
            Arc::from("PrimarySync"),
        );
        assert_eq!(
            download::has_due_selection_work(&library.plan.passes, &check, options.controls)
                .await
                .unwrap(),
            Some(expected)
        );
    }
    let shutdown = options
        .indexing
        .as_ref()
        .map_or_else(CancellationToken::new, |indexing| indexing.cancel.clone());
    let result = Box::pin(crate::sync_cycle::run_cycle(
        &[&library],
        &config,
        Some(db.as_ref()),
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
    assert_eq!(
        std::fs::read(path).unwrap_or_else(|error| panic!("{}: {error}", path.display())),
        bytes,
        "{}",
        path.display()
    );
}

#[tokio::test]
async fn local_policy_two_copies_update_and_preserve_across_reopen() {
    for naming in [EditedNaming::Suffix, EditedNaming::Primary] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let options = || CycleOptions {
            second_album: true,
            pass_kind: PassKind::Album,
            folder_structure: "{album}",
            ..CycleOptions::default()
        };
        let initial = records(&server, Some(("edit-one", EDIT_ONE)));
        let (result, _) = cycle_options(root.path(), initial, false, naming, options()).await;
        assert_eq!(result.failed_count, 0, "{naming:?}: {:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        let filename = if naming == EditedNaming::Suffix {
            "IMG_0501_edited.JPG"
        } else {
            "IMG_0501.JPG"
        };
        let first = root.path().join("media/TestAlbum").join(filename);
        let second = root.path().join("media/SecondAlbum").join(filename);
        assert_bytes(&first, EDIT_ONE);
        assert_bytes(&second, EDIT_ONE);
        let local = b"\xff\xd8\xff\xe0independent local edit";
        std::fs::write(&second, local).unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_750_000_000);
        std::fs::File::options()
            .write(true)
            .open(&second)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let changed = records(&server, Some(("edit-two", EDIT_TWO)));
        let current_naming = EditedNaming::Suffix;
        let (result, _) = cycle_options(
            root.path(),
            changed.clone(),
            false,
            current_naming,
            options(),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{naming:?}: {:?}", result.stats);
        assert!(
            result.db_sync_token_advance_safe,
            "{naming:?}: {:?}",
            result.stats
        );
        assert_bytes(&first, EDIT_TWO);
        assert_bytes(&second, local);
        assert_eq!(
            std::fs::metadata(&second).unwrap().modified().unwrap(),
            modified
        );
        assert!(
            db_count(
                root.path(),
                "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL"
            ) > 0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL AND (verified_media<>0 OR source_checksum IS NOT NULL)"
            ),
            0
        );
        let requests = server.received_requests().await.unwrap().len();
        for _ in 0..2 {
            let (quiet, _) = cycle_options(
                root.path(),
                changed.clone(),
                true,
                current_naming,
                options(),
            )
            .await;
            assert_eq!(quiet.failed_count, 0);
            assert!(quiet.db_sync_token_advance_safe);
            assert_bytes(&first, EDIT_TWO);
            assert_bytes(&second, local);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        std::fs::remove_file(&first).unwrap();
        let (missing_verified, _) = cycle_options(
            root.path(),
            changed.clone(),
            true,
            current_naming,
            options(),
        )
        .await;
        assert_eq!(missing_verified.failed_count, 0);
        assert!(missing_verified.db_sync_token_advance_safe);
        assert_bytes(&first, EDIT_TWO);
        assert_bytes(&second, local);
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            requests + 1,
            "preserved bytes cannot supply the missing current-provider copy"
        );
        std::fs::remove_file(&second).unwrap();
        let (recovered, _) = cycle_options(
            root.path(),
            changed.clone(),
            true,
            current_naming,
            options(),
        )
        .await;
        assert_eq!(
            recovered.failed_count, 0,
            "{naming:?}: {:?}",
            recovered.stats
        );
        assert!(recovered.db_sync_token_advance_safe);
        assert_bytes(&second, EDIT_TWO);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let proof: (bool, bool, bool) = conn.query_row(
            "SELECT verified_media,preserved_local IS NULL,source_checksum IS NOT NULL FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE compat_path=?1 ORDER BY g.created_at DESC LIMIT 1",
            [second.to_str().unwrap()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        ).unwrap();
        assert_eq!(proof, (true, true, true));
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn local_policy_metadata_keeps_preserved_disposition_and_mtime() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    for (naming, shared_path) in [
        (EditedNaming::Suffix, false),
        (EditedNaming::Primary, false),
        (EditedNaming::Suffix, true),
    ] {
        let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, VALID_EDIT).await;
        let root = tempfile::tempdir().unwrap();
        let options = |metadata| CycleOptions {
            second_album: true,
            pass_kind: PassKind::Album,
            folder_structure: if shared_path { "" } else { "{album}" },
            sidecar: metadata,
            embed_xmp: metadata,
            ..CycleOptions::default()
        };
        let initial =
            records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
        let (first, _) =
            cycle_options(root.path(), initial.clone(), false, naming, options(false)).await;
        assert_eq!(first.failed_count, 0);
        let filename = if naming == EditedNaming::Suffix {
            "IMG_0501_edited.JPG"
        } else {
            "IMG_0501.JPG"
        };
        let preserved = root
            .path()
            .join(if shared_path {
                "media"
            } else {
                "media/SecondAlbum"
            })
            .join(filename);
        std::fs::write(&preserved, VALID_ORIGINAL).unwrap();
        let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_750_000_000);
        std::fs::File::options()
            .write(true)
            .open(&preserved)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let mut current = initial;
        current[1]["fields"]["captionEnc"] =
            json!({"type":"STRING","value":"local policy updated"});
        let requests = server.received_requests().await.unwrap().len();
        let (updated, _) = cycle_options(
            root.path(),
            current.clone(),
            false,
            EditedNaming::Suffix,
            options(true),
        )
        .await;
        assert_eq!(
            updated.failed_count,
            0,
            "{naming:?}/{shared_path}: {:?}; {:?}",
            updated.stats,
            capture
                .events()
                .into_iter()
                .filter(|event| event.level == tracing::Level::WARN
                    || event.level == tracing::Level::ERROR)
                .collect::<Vec<_>>()
        );
        assert!(
            updated.db_sync_token_advance_safe,
            "{naming:?}: {:?}",
            updated.stats
        );
        assert_eq!(
            std::fs::metadata(&preserved).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        let bytes = std::fs::read(&preserved).unwrap();
        assert_ne!(bytes, VALID_EDIT);
        let sidecar =
            std::fs::read_to_string(preserved.with_file_name(format!("{filename}.xmp"))).unwrap();
        assert!(sidecar.contains("local policy updated"));
        assert!(
            db_count(
                root.path(),
                "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL AND verified_metadata=1"
            ) > 0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL AND (verified_media<>0 OR source_checksum IS NOT NULL)"
            ),
            0
        );
        for _ in 0..2 {
            let (quiet, _) = cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Suffix,
                options(true),
            )
            .await;
            assert_eq!(
                quiet.failed_count,
                0,
                "{naming:?}: {:?}; {:?}",
                quiet.stats,
                capture
                    .events()
                    .into_iter()
                    .filter(|event| event.level == tracing::Level::WARN
                        || event.level == tracing::Level::ERROR)
                    .collect::<Vec<_>>()
            );
            assert!(quiet.db_sync_token_advance_safe);
            assert_bytes(&preserved, &bytes);
            assert_eq!(
                std::fs::metadata(&preserved).unwrap().modified().unwrap(),
                modified
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
}
fn db_count(root: &std::path::Path, sql: &str) -> i64 {
    rusqlite::Connection::open(root.join("state.db"))
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn local_policy_restored_historical_publication_is_a_local_edit() {
    restored_historical_publication(std::ffi::OsStr::new("utf8")).await;
}

#[cfg(unix)]
#[tokio::test]
async fn local_policy_native_current_publication_updates_but_history_does_not() {
    use std::os::unix::ffi::OsStringExt;
    restored_historical_publication(&std::ffi::OsString::from_vec(b"native-\xff".to_vec())).await;
}

#[cfg(unix)]
#[tokio::test]
async fn local_policy_native_authority_rollback_recovers_landed_publication() {
    use std::os::unix::ffi::OsStringExt;
    for (timing, condition) in [
        (
            "BEFORE",
            "OLD.current_publication=1 AND NEW.current_publication=0",
        ),
        ("AFTER", "NEW.current_publication=1"),
    ] {
        let root = tempfile::Builder::new()
            .prefix(&std::ffi::OsString::from_vec(b"native-fault-\xff".to_vec()))
            .tempdir()
            .unwrap();
        let server = server().await;
        let (first, _) = cycle(
            root.path(),
            records(&server, Some(("edit-one", EDIT_ONE))),
            false,
            EditedNaming::Suffix,
        )
        .await;
        assert_eq!(first.failed_count, 0);
        let target = root.path().join("media/IMG_0501_edited.JPG");
        let old = data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_ONE));
        let new = data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_TWO));
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        conn.execute_batch(&format!("CREATE TRIGGER fail_native_authority {timing} UPDATE OF current_publication ON provider_active_destinations WHEN NEW.version_size='adjusted' AND ({condition}) BEGIN SELECT RAISE(ABORT,'authority fault'); END;")).unwrap();
        let current = records(&server, Some(("edit-two", EDIT_TWO)));
        let (failed, _) = cycle(root.path(), current.clone(), false, EditedNaming::Suffix).await;
        assert!(
            failed.failed_count > 0 || !failed.db_sync_token_advance_safe,
            "{timing}: {:?}",
            failed.stats
        );
        assert_bytes(&target, EDIT_TWO);
        let authority: String = conn.query_row("SELECT local_checksum FROM provider_active_destinations WHERE version_size='adjusted' AND current_publication=1", [], |row|row.get(0)).unwrap();
        assert_eq!(
            authority, old,
            "{timing}: authority must roll back with finalization"
        );
        conn.execute_batch("DROP TRIGGER fail_native_authority")
            .unwrap();
        let requests = server.received_requests().await.unwrap().len();
        for quiet in [false, true, true] {
            let (recovered, _) =
                cycle(root.path(), current.clone(), quiet, EditedNaming::Suffix).await;
            assert_eq!(recovered.failed_count, 0, "{timing}: {:?}", recovered.stats);
            assert!(recovered.db_sync_token_advance_safe);
            assert_bytes(&target, EDIT_TWO);
            let authority: String = conn.query_row("SELECT local_checksum FROM provider_active_destinations WHERE version_size='adjusted' AND current_publication=1", [], |row|row.get(0)).unwrap();
            assert_eq!(authority, new);
        }
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
        assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM provider_active_destinations WHERE version_size='adjusted' AND current_publication=0 AND verified_media=1 AND local_checksum=?1)", [&old], |row|row.get::<_,bool>(0)).unwrap());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn local_policy_native_original_authority_survives_aliases_and_config_drift() {
    use state::db::provider_selection::SelectionPath;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let root = tempfile::Builder::new()
        .prefix(&std::ffi::OsString::from_vec(
            b"native-original-\xff".to_vec(),
        ))
        .tempdir()
        .unwrap();
    let server = server().await;
    let options = |live_mode| CycleOptions {
        second_album: true,
        pass_kind: PassKind::Album,
        live_mode,
        ..CycleOptions::default()
    };
    let original = |name: &str, bytes: &[u8]| {
        let mut current = records(&server, None);
        current[0]["fields"]["resOriginalRes"]["value"] = json!({"size":bytes.len(),"downloadURL":format!("{}/{name}",server.uri()),"fileChecksum":provider_hash(bytes)});
        current
    };
    let target = root.path().join("media/IMG_0501.JPG");
    for (name, bytes, mode) in [
        ("edit-one", EDIT_ONE, crate::types::LivePhotoMode::Both),
        ("edit-two", EDIT_TWO, crate::types::LivePhotoMode::ImageOnly),
    ] {
        let (result, _) = cycle_options(
            root.path(),
            original(name, bytes),
            false,
            EditedNaming::Suffix,
            options(mode),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&target, bytes);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT count(*) FROM provider_active_destinations WHERE version_size='original' AND current_publication=1"
            ),
            2
        );
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let paths = conn.prepare("SELECT DISTINCT path FROM provider_active_destinations WHERE version_size='original' AND current_publication=1").unwrap()
            .query_map([], |row| row.get::<_, String>(0)).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(paths.len(), 2);
        for path in paths {
            assert_bytes(
                &serde_json::from_str::<SelectionPath>(&path)
                    .unwrap()
                    .to_path(),
                bytes,
            );
        }
    }
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    assert!(
        db_count(
            root.path(),
            "SELECT count(DISTINCT pass_key) FROM provider_active_destinations WHERE version_size='original' AND verified_media=1"
        ) >= 2
    );
    assert!(
        db_count(
            root.path(),
            "SELECT count(DISTINCT config_hash) FROM provider_active_generations"
        ) >= 2
    );
    let (scope,key,child,library):(String,String,String,String) = conn.query_row("SELECT g.scope,w.pass_key,w.child,w.library FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation WHERE w.version_size='original' AND w.current_publication=1", [], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
    let owner = fixture_owner();
    let db = state::SqliteStateDb::open_owned(&root.path().join("state.db"), &owner)
        .await
        .unwrap();
    let fingerprint = (
        data_encoding::HEXLOWER.encode(&Sha256::digest(EDIT_TWO)),
        EDIT_TWO.len() as u64,
    );
    assert!(
        db.selection_path_matches_local(
            owner.clone(),
            scope.clone(),
            key.clone(),
            (child.clone(), library.clone()),
            SelectionPath::from_path(&target),
            fingerprint.clone()
        )
        .await
        .unwrap()
    );
    assert!(
        !db.selection_path_matches_local(
            owner.clone(),
            scope.clone(),
            key.clone(),
            (child.clone(), "OtherSync".into()),
            SelectionPath::from_path(&target),
            fingerprint.clone()
        )
        .await
        .unwrap()
    );
    let mut collision = target.as_os_str().as_bytes().to_vec();
    *collision.iter_mut().find(|byte| **byte == 0xff).unwrap() = 0xfe;
    let collision = std::path::PathBuf::from(std::ffi::OsString::from_vec(collision));
    assert_eq!(collision.to_string_lossy(), target.to_string_lossy());
    assert!(
        !db.selection_path_matches_local(
            owner,
            scope.clone(),
            key.clone(),
            (child.clone(), library.clone()),
            SelectionPath::from_path(&collision),
            fingerprint.clone()
        )
        .await
        .unwrap()
    );
    let other = state::db::account::AccountOwner::authenticated(
        "other@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"other-provider"}})).unwrap(),
    )
    .unwrap();
    assert!(
        db.selection_path_matches_local(
            other,
            scope,
            key,
            (child, library),
            SelectionPath::from_path(&target),
            fingerprint
        )
        .await
        .is_err()
    );
    drop(db);
    // Model an ambiguous migrated store: observing/reusing historical receipts
    // in another configuration must not manufacture publication authority.
    conn.execute(
        "UPDATE provider_active_destinations SET current_publication=0",
        [],
    )
    .unwrap();
    let requests_before_reuse = server.received_requests().await.unwrap().len();
    let (reused, _) = cycle_options(
        root.path(),
        original("edit-two", EDIT_TWO),
        false,
        EditedNaming::Suffix,
        options(crate::types::LivePhotoMode::Both),
    )
    .await;
    assert_eq!(reused.failed_count, 0, "{:?}", reused.stats);
    assert!(reused.db_sync_token_advance_safe);
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests_before_reuse
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations WHERE current_publication=1"
        ),
        0
    );
    std::fs::write(&target, EDIT_ONE).unwrap();
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_750_000_000);
    std::fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let requests = server.received_requests().await.unwrap().len();
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            original("original", ORIGINAL),
            quiet,
            EditedNaming::Suffix,
            options(crate::types::LivePhotoMode::ImageOnly),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&target, EDIT_ONE);
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            modified
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations WHERE current_publication=1"
        ),
        0
    );
}

async fn restored_historical_publication(prefix: &std::ffi::OsStr) {
    const EDIT_THREE: &[u8] = b"\xff\xd8\xff\xe0third provider publication";
    let local_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for naming in [EditedNaming::Suffix, EditedNaming::Primary] {
        for offset in [None, Some(0_i64), Some(-60)] {
            let server = server().await;
            Mock::given(method("GET"))
                .and(path("/edit-three"))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(EDIT_THREE))
                .mount(&server)
                .await;
            let root = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
            let options = || CycleOptions {
                second_album: true,
                pass_kind: PassKind::Album,
                folder_structure: "{album}",
                #[cfg(feature = "xmp")]
                sidecar: prefix.to_str().is_none(),
                ..CycleOptions::default()
            };
            let filename = if naming == EditedNaming::Suffix {
                "IMG_0501_edited.JPG"
            } else {
                "IMG_0501.JPG"
            };
            let peer = root.path().join("media/TestAlbum").join(filename);
            let restored = root.path().join("media/SecondAlbum").join(filename);
            for (name, bytes) in [("edit-one", EDIT_ONE), ("edit-two", EDIT_TWO)] {
                let (result, _) = cycle_options(
                    root.path(),
                    records(&server, Some((name, bytes))),
                    false,
                    naming,
                    options(),
                )
                .await;
                assert_eq!(
                    result.failed_count, 0,
                    "{naming:?}/{offset:?}: {:?}",
                    result.stats
                );
                assert!(result.db_sync_token_advance_safe);
                assert_bytes(&peer, bytes);
                assert_bytes(&restored, bytes);
            }
            std::fs::write(&restored, EDIT_ONE).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&restored)
                .unwrap()
                .set_modified(local_time)
                .unwrap();
            let mut current = records(&server, Some(("edit-three", EDIT_THREE)));
            if let Some(offset) = offset {
                let time = chrono::DateTime::from_timestamp(1_700_000_000 + offset, 0).unwrap();
                let mut bytes = Vec::new();
                plist::Value::Date(std::time::SystemTime::from(time).into())
                    .to_writer_binary(&mut bytes)
                    .unwrap();
                current[1]["fields"]["adjustmentTimestampEnc"] = json!({"type":"ENCRYPTED_BYTES","value":base64::engine::general_purpose::STANDARD.encode(bytes)});
            }
            let (updated, _) =
                cycle_options(root.path(), current.clone(), false, naming, options()).await;
            assert_eq!(
                updated.failed_count, 0,
                "{naming:?}/{offset:?}: {:?}",
                updated.stats
            );
            assert!(updated.db_sync_token_advance_safe);
            assert_bytes(&peer, EDIT_THREE);
            assert_bytes(&restored, EDIT_ONE);
            assert_eq!(
                std::fs::metadata(&restored).unwrap().modified().unwrap(),
                local_time
            );
            assert!(
                db_count(
                    root.path(),
                    "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL"
                ) > 0
            );
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL AND (verified_media<>0 OR source_checksum IS NOT NULL)"
                ),
                0
            );
            let requests = server.received_requests().await.unwrap().len();
            for _ in 0..2 {
                let (quiet, _) =
                    cycle_options(root.path(), current.clone(), true, naming, options()).await;
                assert_eq!(quiet.failed_count, 0);
                assert!(quiet.db_sync_token_advance_safe);
                assert_bytes(&peer, EDIT_THREE);
                assert_bytes(&restored, EDIT_ONE);
                assert_eq!(
                    std::fs::metadata(&restored).unwrap().modified().unwrap(),
                    local_time
                );
            }
            assert_eq!(server.received_requests().await.unwrap().len(), requests);
        }
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn local_policy_restored_old_metadata_output_is_not_current_publication() {
    restored_metadata_publication(std::ffi::OsStr::new("utf8")).await;
}

#[cfg(all(unix, not(feature = "xmp")))]
#[tokio::test]
async fn local_policy_native_metadata_authority_retains_latest_output() {
    use std::os::unix::ffi::OsStringExt;
    restored_metadata_publication(&std::ffi::OsString::from_vec(b"native-\xff".to_vec())).await;
}

#[cfg(any(feature = "xmp", unix))]
async fn restored_metadata_publication(prefix: &std::ffi::OsStr) {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let server = server_with_media(VALID_ORIGINAL, VALID_EDIT, VALID_ORIGINAL).await;
    let root = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
    let options = |embed| CycleOptions {
        #[cfg(feature = "xmp")]
        embed_xmp: embed,
        #[cfg(not(feature = "xmp"))]
        exif_description: embed,
        ..CycleOptions::default()
    };
    let target = root.path().join("media/IMG_0501_edited.JPG");
    let mut current =
        records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
    current[1]["fields"]["captionEnc"] = json!({"type":"STRING","value":"first metadata output"});
    let (first, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Suffix,
        options(true),
    )
    .await;
    assert_eq!(
        first.failed_count,
        0,
        "{:?}",
        capture
            .events()
            .into_iter()
            .filter(
                |event| event.level == tracing::Level::WARN || event.level == tracing::Level::ERROR
            )
            .collect::<Vec<_>>()
    );
    let historical = std::fs::read(&target).unwrap();
    current[1]["fields"]["captionEnc"] = json!({"type":"STRING","value":"second metadata output"});
    let (second, _) = cycle_options(
        root.path(),
        current,
        false,
        EditedNaming::Suffix,
        options(true),
    )
    .await;
    assert_eq!(
        second.failed_count,
        0,
        "{:?}",
        capture
            .events()
            .into_iter()
            .filter(
                |event| event.level == tracing::Level::WARN || event.level == tracing::Level::ERROR
            )
            .collect::<Vec<_>>()
    );
    let latest = std::fs::read(&target).unwrap();
    assert_ne!(latest, historical);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let authority: String = conn.query_row("SELECT local_checksum FROM provider_active_destinations WHERE version_size='adjusted' AND current_publication=1", [], |row| row.get(0)).unwrap();
    assert_eq!(
        authority,
        data_encoding::HEXLOWER.encode(&Sha256::digest(&latest)),
        "checked metadata output must advance native publication authority"
    );
    std::fs::write(&target, &historical).unwrap();
    let modified = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_750_000_000);
    std::fs::File::options()
        .write(true)
        .open(&target)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let changed =
        records_with_original(&server, VALID_ORIGINAL, Some(("edit-two", VALID_ORIGINAL)));
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            changed.clone(),
            quiet,
            EditedNaming::Suffix,
            options(false),
        )
        .await;
        assert_eq!(result.failed_count, 0);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&target, &historical);
        assert_eq!(
            std::fs::metadata(&target).unwrap().modified().unwrap(),
            modified
        );
    }
    assert!(
        db_count(
            root.path(),
            "SELECT count(*) FROM provider_active_destinations WHERE preserved_local IS NOT NULL AND verified_media=0 AND source_checksum IS NULL"
        ) > 0
    );
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/edit-two")
            .count(),
        0
    );
}

#[tokio::test]
async fn local_policy_adjusted_still_requires_strictly_newer_cloud_time() {
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let local_time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for naming in [EditedNaming::Suffix, EditedNaming::Primary] {
        for offset in [-60_i64, 0, 60] {
            let server = server().await;
            let root = tempfile::tempdir().unwrap();
            let (first, _) = cycle(
                root.path(),
                records(&server, Some(("edit-one", EDIT_ONE))),
                false,
                naming,
            )
            .await;
            assert_eq!(first.failed_count, 0);
            let filename = if naming == EditedNaming::Suffix {
                "IMG_0501_edited.JPG"
            } else {
                "IMG_0501.JPG"
            };
            let target = root.path().join("media").join(filename);
            let local = b"\xff\xd8\xff\xe0user media edit";
            std::fs::write(&target, local).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&target)
                .unwrap()
                .set_modified(local_time)
                .unwrap();
            let time = chrono::DateTime::from_timestamp(1_700_000_000 + offset, 0).unwrap();
            let mut encoded = Vec::new();
            plist::Value::Date(std::time::SystemTime::from(time).into())
                .to_writer_binary(&mut encoded)
                .unwrap();
            let mut current = records(&server, Some(("edit-two", EDIT_TWO)));
            current[1]["fields"]["adjustmentTimestampEnc"] = json!({"type":"ENCRYPTED_BYTES","value":base64::engine::general_purpose::STANDARD.encode(encoded)});
            let (result, _) = cycle(root.path(), current, false, EditedNaming::Suffix).await;
            assert_eq!(
                result.failed_count,
                0,
                "{naming:?}/{offset}: {:?}; {:?}",
                result.stats,
                capture
                    .events()
                    .into_iter()
                    .filter(|event| event.level == tracing::Level::WARN
                        || event.level == tracing::Level::ERROR)
                    .collect::<Vec<_>>()
            );
            assert!(
                result.db_sync_token_advance_safe,
                "{naming:?}/{offset}: {:?}",
                result.stats
            );
            assert_bytes(&target, if offset > 0 { EDIT_TWO } else { local });
            if offset <= 0 {
                assert_eq!(
                    std::fs::metadata(&target).unwrap().modified().unwrap(),
                    local_time
                );
            }
        }
    }
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

fn album_directory_options(name: &'static str) -> CycleOptions {
    CycleOptions {
        album_name: name,
        pass_kind: PassKind::Album,
        folder_structure: "{album}",
        ..CycleOptions::default()
    }
}

fn retained_album_manifests(root: &std::path::Path) -> Vec<Vec<u8>> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    conn.prepare(
        "SELECT manifest FROM provider_active_decisions ORDER BY generation,pass_key,child",
    )
    .unwrap()
    .query_map([], |row| row.get(0))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn album_path_receipts(root: &std::path::Path, path: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let prefix = path.to_string_lossy().into_owned();
    conn.prepare("SELECT json_array(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,metadata_write_failed_at,capture_repair_metadata_hash,capture_repair_output_checksum,capture_repair_output_size,source_checksum) FROM asset_metadata_paths WHERE instr(local_path,?1)=1 ORDER BY library,id,version_size,local_path")
        .unwrap().query_map([prefix], |row| row.get(0)).unwrap().collect::<Result<_,_>>().unwrap()
}

fn old_album_receipts(root: &std::path::Path) -> Vec<String> {
    album_path_receipts(root, &root.join("media").join("Old"))
}

fn album_decision_rows(root: &std::path::Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    conn.prepare("SELECT json_array(generation,pass_key,child,hex(manifest),admission,reason) FROM provider_active_decisions ORDER BY generation,pass_key,child")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn album_admitted_rows(root: &std::path::Path) -> Vec<(String, String, String, bool)> {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    conn.prepare("SELECT generation,pass_key,child,admitted FROM provider_active_destinations WHERE admitted=1 ORDER BY generation,pass_key,child")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn album_directory_conflicting_historical_receipt_does_not_veto_owned_rename() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    let old = root.path().join("media/Old");
    let next = root.path().join("media/New");
    let (first, _) = cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    assert_eq!(first.failed_count, 0, "{:?}", first.stats);
    assert!(first.db_sync_token_advance_safe);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute("INSERT INTO asset_metadata_paths(library,id,version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum) SELECT library,id || '-conflicting',version_size,local_path,provider_checksum,local_checksum,download_checksum,source_checksum FROM asset_metadata_paths LIMIT 1", []).unwrap();
    drop(conn);
    let receipts = album_path_receipts(root.path(), &old);
    assert_eq!(
        receipts
            .iter()
            .filter(|row| row.contains("-conflicting"))
            .count(),
        1,
        "the extra obsolete foreign receipt is historical evidence only"
    );
    let before_requests = server.received_requests().await.unwrap().len();
    for quiet in [false, true, true] {
        let mut options = album_directory_options("New");
        options.expected_due_selection = quiet.then_some(false);
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert!(!old.exists());
        assert_bytes(&next.join("IMG_0501.JPG"), ORIGINAL);
        assert_eq!(
            album_path_receipts(root.path(), &old),
            receipts,
            "obsolete historical receipts must remain exactly"
        );
        assert_eq!(
            album_path_receipts(root.path(), &next).len(),
            1,
            "the current owned copy has its own exact receipt"
        );
    }
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        before_requests,
        "rename must reuse owned media"
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='committed'"
        ),
        1
    );
}

#[tokio::test]
async fn exclusive_album_directory_rename_carries_user_tree_and_preserves_old_receipts() {
    for naming in [EditedNaming::Suffix, EditedNaming::Primary] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let media = root.path().join("media");
        let provider = records(&server, None);
        let (first, _) = cycle_options(
            root.path(),
            provider.clone(),
            false,
            naming,
            album_directory_options("Old"),
        )
        .await;
        assert_eq!(first.failed_count, 0, "{:?}", first.stats);
        assert_bytes(&media.join("Old/IMG_0501.JPG"), ORIGINAL);
        let manifests = retained_album_manifests(root.path());
        let receipts = old_album_receipts(root.path());
        assert!(!receipts.is_empty());
        let decisions = album_decision_rows(root.path());
        let admissions = album_admitted_rows(root.path());
        std::fs::create_dir_all(media.join("Old/user/nested")).unwrap();
        std::fs::write(media.join("Old/user/nested/notes.txt"), b"user notes").unwrap();
        std::fs::write(media.join("Old/user.xmp"), b"user sidecar").unwrap();
        std::fs::create_dir(media.join("New")).unwrap();
        std::fs::write(media.join("New/foreign.txt"), b"foreign contents").unwrap();
        let before_requests = server.received_requests().await.unwrap().len();
        for quiet in [false, true, true] {
            let mut options = album_directory_options("New");
            options.expected_due_selection = quiet.then_some(false);
            let (result, _) =
                cycle_options(root.path(), provider.clone(), quiet, naming, options).await;
            assert_eq!(result.failed_count, 0, "{:?}", result.stats);
            assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
            assert!(!media.join("Old").exists());
            assert_bytes(&media.join("New/foreign.txt"), b"foreign contents");
            assert_bytes(&media.join("New (1)/IMG_0501.JPG"), ORIGINAL);
            assert_bytes(&media.join("New (1)/user/nested/notes.txt"), b"user notes");
            assert_bytes(&media.join("New (1)/user.xmp"), b"user sidecar");
            let retained = retained_album_manifests(root.path());
            assert!(manifests.iter().all(|manifest| retained.contains(manifest)));
            assert_eq!(old_album_receipts(root.path()), receipts);
            let retained_decisions = album_decision_rows(root.path());
            assert!(decisions.iter().all(|row| retained_decisions.contains(row)));
            let retained_admissions = album_admitted_rows(root.path());
            assert!(
                admissions
                    .iter()
                    .all(|row| retained_admissions.contains(row))
            );
        }
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            before_requests,
            "rename must reuse owned media"
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='committed'"
            ),
            1
        );
        assert!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM asset_metadata_paths WHERE instr(local_path,'Old')>0"
            ) > 0
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_decisions WHERE reason LIKE 'album_directory_successor:%'"
            ),
            0,
            "historical selection evidence already preserves the previous receipts"
        );
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let header: Vec<u8> = conn.query_row("SELECT header FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='committed'", [], |row| row.get(0)).unwrap();
        let op: crate::state::db::primary_layout::directory::AlbumDirectoryOperation =
            serde_json::from_slice(&header).unwrap();
        let binding: Vec<u8> = conn.query_row("SELECT directory_binding FROM album_containers WHERE directory_binding IS NOT NULL", [], |row| row.get(0)).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&binding).unwrap(),
            serde_json::to_value(&op.next).unwrap()
        );
        assert_eq!(op.old.generation, 1);
        assert_eq!(op.next.generation, 2);
        assert_eq!(op.old.container, op.next.container);
        assert_ne!(op.old.pass, op.next.pass);
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM provider_active_destinations WHERE pass_key=?1 AND admitted=1",
                [&op.old.pass],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "historical admission remains retained"
        );
        drop(conn);

        // Missing-file repair must reopen only the current slot, even though
        // historical manifests and their exact old path receipts still exist.
        std::fs::remove_file(media.join("New (1)/IMG_0501.JPG")).unwrap();
        for quiet in [false, true, true] {
            let mut options = album_directory_options("New");
            options.expected_due_selection = quiet.then_some(false);
            let (result, _) =
                cycle_options(root.path(), provider.clone(), quiet, naming, options).await;
            assert_eq!(result.failed_count, 0, "{:?}", result.stats);
            assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
            assert_bytes(&media.join("New (1)/IMG_0501.JPG"), ORIGINAL);
            assert!(!media.join("Old").exists());
            assert_bytes(&media.join("New (1)/user/nested/notes.txt"), b"user notes");
            assert_eq!(old_album_receipts(root.path()), receipts);
            let retained_decisions = album_decision_rows(root.path());
            assert!(decisions.iter().all(|row| retained_decisions.contains(row)));
            let retained_admissions = album_admitted_rows(root.path());
            assert!(
                admissions
                    .iter()
                    .all(|row| retained_admissions.contains(row))
            );
        }
    }
}

#[tokio::test]
async fn album_directory_replaced_source_identity_refuses_before_movement() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    let media = root.path().join("media");
    std::fs::rename(media.join("Old"), media.join("retained-original")).unwrap();
    std::fs::create_dir(media.join("Old")).unwrap();
    std::fs::write(media.join("Old/IMG_0501.JPG"), ORIGINAL).unwrap();
    std::fs::write(media.join("Old/user.txt"), b"replacement directory").unwrap();
    let receipts = old_album_receipts(root.path());
    let result = try_cycle_options(
        root.path(),
        provider,
        false,
        EditedNaming::Suffix,
        album_directory_options("New"),
    )
    .await;
    assert!(result.is_err());
    assert!(!media.join("New").exists());
    assert_bytes(&media.join("retained-original/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(&media.join("Old/user.txt"), b"replacement directory");
    assert_eq!(old_album_receipts(root.path()), receipts);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='committed'"
        ),
        0
    );
}

#[tokio::test]
async fn album_directory_first_binding_never_adopts_existing_directory() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    let media = root.path().join("media");
    std::fs::create_dir_all(media.join("Old")).unwrap();
    std::fs::write(media.join("Old/user.txt"), b"unowned user directory").unwrap();
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    assert_bytes(&media.join("Old (1)/IMG_0501.JPG"), ORIGINAL);
    assert!(!media.join("Old/IMG_0501.JPG").exists());
    std::fs::write(
        media.join("Old (1)/user.txt"),
        b"owned directory user contents",
    )
    .unwrap();
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            album_directory_options("New"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert!(!media.join("Old (1)").exists());
        assert_bytes(&media.join("Old/user.txt"), b"unowned user directory");
        assert_bytes(&media.join("New/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(
            &media.join("New/user.txt"),
            b"owned directory user contents",
        );
    }
}

#[tokio::test]
async fn album_directory_modified_managed_media_stays_in_place() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("Old"),
    )
    .await;
    let media = root.path().join("media");
    std::fs::write(
        media.join("Old/IMG_0501.JPG"),
        b"user modified managed media",
    )
    .unwrap();
    std::fs::write(media.join("Old/user.txt"), b"user added file").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("user.txt", media.join("Old/user-link")).unwrap();
    let receipts = old_album_receipts(root.path());
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            album_directory_options("New"),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(
            &media.join("Old/IMG_0501.JPG"),
            b"user modified managed media",
        );
        assert_bytes(&media.join("New/user.txt"), b"user added file");
        #[cfg(unix)]
        assert_eq!(
            std::fs::read_link(media.join("New/user-link")).unwrap(),
            std::path::Path::new("user.txt")
        );
        assert_bytes(&media.join("New/IMG_0501.JPG"), ORIGINAL);
        assert!(!media.join("Old/user.txt").exists());
        assert!(!media.join("New (1)").exists());
        assert_eq!(old_album_receipts(root.path()), receipts);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1'"
            ),
            1
        );
    }
}

#[tokio::test]
async fn album_directory_legacy_paths_progress_without_claiming_user_tree() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    let mut legacy = album_directory_options("Old");
    legacy.folder_structure = "Old";
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        legacy,
    )
    .await;
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM album_containers WHERE directory_binding IS NOT NULL"
        ),
        0
    );
    std::fs::write(
        root.path().join("media/Old/user.txt"),
        b"uncertain user owner",
    )
    .unwrap();
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        album_directory_options("Old"),
    )
    .await;
    assert_bytes(&root.path().join("media/Old/IMG_0501.JPG"), ORIGINAL);
    assert!(!root.path().join("media/Old (1)").exists());
    let receipts = old_album_receipts(root.path());
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            album_directory_options("New"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_eq!(result.failed_count, 0);
        assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(
            &root.path().join("media/Old/user.txt"),
            b"uncertain user owner",
        );
        assert_bytes(&root.path().join("media/Old/IMG_0501.JPG"), ORIGINAL);
        assert!(!root.path().join("media/New/user.txt").exists());
        assert_eq!(old_album_receipts(root.path()), receipts);
    }
}

#[tokio::test]
async fn album_directory_fallback_failed_new_publication_keeps_local_copy_and_checkpoint_debt() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("Old"),
    )
    .await;
    std::fs::write(
        root.path().join("media/Old/IMG_0501.JPG"),
        b"only local edited copy",
    )
    .unwrap();
    let receipts = old_album_receipts(root.path());
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_new_copy BEFORE INSERT ON asset_metadata_paths WHEN instr(NEW.local_path,'New')>0 BEGIN SELECT RAISE(FAIL,'injected new copy finalization'); END;").unwrap();
    drop(conn);
    let (failed, _) = cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("New"),
    )
    .await;
    assert!(!failed.db_sync_token_advance_safe);
    assert_bytes(
        &root.path().join("media/Old/IMG_0501.JPG"),
        b"only local edited copy",
    );
    assert_eq!(old_album_receipts(root.path()), receipts);
    assert!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase='publishing'"
        ) > 0
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("DROP TRIGGER fail_new_copy").unwrap();
    drop(conn);
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            album_directory_options("New"),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(
            &root.path().join("media/Old/IMG_0501.JPG"),
            b"only local edited copy",
        );
        assert_eq!(old_album_receipts(root.path()), receipts);
    }
}

#[cfg(feature = "xmp")]
#[tokio::test]
async fn album_directory_missing_primary_preserves_owned_orphan_sidecar() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    for name in ["Old", "New"] {
        let mut options = album_directory_options(name);
        options.sidecar = true;
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Primary,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
    }
    let receipts = old_album_receipts(root.path());
    let xmp = std::fs::read(root.path().join("media/New/IMG_0501.JPG.xmp")).unwrap();
    std::fs::remove_file(root.path().join("media/New/IMG_0501.JPG")).unwrap();
    for quiet in [false, true, true] {
        let mut options = album_directory_options("New");
        options.sidecar = true;
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_eq!(result.failed_count, 0);
        assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), ORIGINAL);
        assert_eq!(
            std::fs::read(root.path().join("media/New/IMG_0501.JPG.xmp")).unwrap(),
            xmp
        );
        assert!(!root.path().join("media/Old").exists());
        assert_eq!(old_album_receipts(root.path()), receipts);
    }
}

#[tokio::test]
async fn album_directory_first_seen_legacy_rename_preserves_uncertain_paths() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    let mut options = album_directory_options("Old");
    options.folder_structure = "legacy";
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        options,
    )
    .await;
    let old = root.path().join("media/legacy/IMG_0501.JPG");
    std::fs::write(&old, b"unowned legacy edit").unwrap();
    let target = root.path().join("media/2023/photos-New");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("IMG_0501.JPG"), b"foreign selected name").unwrap();
    let mut selected = None;
    for quiet in [false, true, true] {
        let mut options = album_directory_options("New");
        options.folder_structure = "%Y/photos-{album}";
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_bytes(&old, b"unowned legacy edit");
        assert_bytes(&target.join("IMG_0501.JPG"), b"foreign selected name");
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let path: String = conn
            .query_row(
                "SELECT local_path FROM assets WHERE version_size='original'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_bytes(std::path::Path::new(&path), ORIGINAL);
        if let Some(selected) = &selected {
            assert_eq!(&path, selected);
        } else {
            selected = Some(path);
        }
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn album_directory_projection_preserves_independent_selected_owner() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    let mut options = album_directory_options("Old");
    options.folder_structure = "%Y/{album}";
    options.shared_folder_structure = Some("%Y/Old");
    let (result, _) = cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        options,
    )
    .await;
    assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
    let old = root.path().join("media/2023/Old/IMG_0501.JPG");
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let album_path: String = conn.query_row("SELECT w.compat_path FROM provider_active_destinations w JOIN provider_active_generations g ON g.id=w.generation JOIN json_each(CAST(g.specification AS TEXT),'$.profile.passes') p ON json_extract(p.value,'$.key')=w.pass_key WHERE w.current_publication=1 AND json_extract(p.value,'$.kind')='Album'", [], |row| row.get(0)).unwrap();
    drop(conn);
    let album_path = std::path::PathBuf::from(album_path);
    assert_ne!(
        album_path, old,
        "album and smart folder own independent copies"
    );
    let next = root
        .path()
        .join("media/2023/New")
        .join(album_path.file_name().unwrap());
    std::fs::write(old.parent().unwrap().join("user.txt"), b"shared user entry").unwrap();
    for quiet in [false, true, true] {
        let mut options = album_directory_options("New");
        options.folder_structure = "%Y/{album}";
        options.shared_folder_structure = Some("%Y/Old");
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_eq!(result.failed_count, 0);
        assert_bytes(&old, ORIGINAL);
        assert_bytes(&next, ORIGINAL);
        assert_bytes(
            &old.parent().unwrap().join("user.txt"),
            b"shared user entry",
        );
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let rows: Vec<(String,String,i64,i64)> = conn.prepare("SELECT pass_key,compat_path,verified_media,admitted FROM provider_active_destinations").unwrap().query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
        assert!(conn.query_row("SELECT COUNT(*) FROM provider_active_destinations WHERE compat_path=?1 AND admitted=1", [old.to_str().unwrap()], |row| row.get::<_,i64>(0)).unwrap() > 0, "{rows:?}");
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM primary_layout_superseded_paths WHERE compat_path=?1",
                [old.to_str().unwrap()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn album_directory_projection_keeps_primary_rendition_policy_in_its_owner() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut options = album_directory_options("Old");
    options.folder_structure = "%Y/photos-{album}";
    cycle_options(
        root.path(),
        records(&server, None),
        false,
        EditedNaming::Primary,
        options,
    )
    .await;
    for (name, quiet) in [("Old", false), ("New", false), ("New", true), ("New", true)] {
        let mut options = album_directory_options(name);
        options.folder_structure = "%Y/photos-{album}";
        options.expected_due_selection = quiet.then_some(false);
        let (result, _) = cycle_options(
            root.path(),
            records(&server, Some(("edit-one", EDIT_ONE))),
            quiet,
            EditedNaming::Primary,
            options,
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_eq!(result.failed_count, 0);
        assert_bytes(
            &root
                .path()
                .join(format!("media/2023/photos-{name}/IMG_0501.JPG")),
            EDIT_ONE,
        );
        assert_bytes(
            &root
                .path()
                .join(format!("media/2023/photos-{name}/IMG_0501_original.JPG")),
            ORIGINAL,
        );
    }
}

#[tokio::test]
async fn album_directory_projection_template_changes_apply_only_to_future_files() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let mut provider = records(&server, None);
    let mut options = album_directory_options("Old");
    options.folder_structure = "%Y/{album}";
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        options,
    )
    .await;
    provider.extend(second_date_records(&server));
    let mut options = album_directory_options("Old");
    options.folder_structure = "future/%Y/photos-{album}";
    let (result, _) = cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Suffix,
        options,
    )
    .await;
    assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
    assert_bytes(&root.path().join("media/2023/Old/IMG_0501.JPG"), ORIGINAL);
    assert_bytes(
        &root
            .path()
            .join("media/future/2024/photos-Old/IMG_0502.JPG"),
        ORIGINAL,
    );
    assert!(
        !root
            .path()
            .join("media/future/2023/photos-Old/IMG_0501.JPG")
            .exists()
    );
    for name in ["New", "Again"] {
        for quiet in [false, true, true] {
            let mut options = album_directory_options(name);
            options.folder_structure = "later/%Y/{album}";
            let (result, _) = cycle_options(
                root.path(),
                provider.clone(),
                quiet,
                EditedNaming::Suffix,
                options,
            )
            .await;
            assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
            assert_eq!(result.failed_count, 0);
            assert_bytes(
                &root.path().join(format!("media/2023/{name}/IMG_0501.JPG")),
                ORIGINAL,
            );
            assert_bytes(
                &root
                    .path()
                    .join(format!("media/future/2024/photos-{name}/IMG_0502.JPG")),
                ORIGINAL,
            );
            assert!(!root.path().join("media/later").exists());
        }
    }
}

#[tokio::test]
async fn album_directory_per_file_projection_handles_dates_and_embedded_tokens() {
    for template in ["%Y/{album}/%m", "%Y/photos-{album}-selected/%m"] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let mut provider = records(&server, None);
        provider.extend(second_date_records(&server));
        let mut options = album_directory_options("Old");
        options.folder_structure = template;
        let (initial, _) = cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Suffix,
            options,
        )
        .await;
        assert!(initial.db_sync_token_advance_safe, "{:?}", initial.stats);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let old: Vec<String> = conn
            .prepare("SELECT local_path FROM assets WHERE version_size='original' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        drop(conn);
        assert_eq!(old.len(), 2);
        assert_ne!(
            std::path::Path::new(&old[0]).parent(),
            std::path::Path::new(&old[1]).parent()
        );
        let mut destinations = Vec::new();
        for path in &old {
            let old = std::path::PathBuf::from(path);
            let parent = old.parent().unwrap();
            std::fs::write(parent.join("unrelated.txt"), b"unclaimed local entry").unwrap();
            let next = std::path::PathBuf::from(path.replace("Old", "New"));
            let occupied = next.parent().unwrap();
            std::fs::create_dir_all(occupied).unwrap();
            std::fs::write(occupied.join("foreign.txt"), b"foreign target contents").unwrap();
            let sibling = occupied.with_file_name(format!(
                "{} (1)",
                occupied.file_name().unwrap().to_str().unwrap()
            ));
            destinations.push(sibling.join(next.file_name().unwrap()));
        }
        std::fs::write(&old[0], b"modified local media").unwrap();
        for quiet in [false, true, true] {
            let mut options = album_directory_options("New");
            options.folder_structure = template;
            let (result, _) = cycle_options(
                root.path(),
                provider.clone(),
                quiet,
                EditedNaming::Suffix,
                options,
            )
            .await;
            assert_eq!(result.failed_count, 0, "{:?}", result.stats);
            assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
            assert_bytes(std::path::Path::new(&old[0]), b"modified local media");
            for (old, current) in old.iter().zip(&destinations) {
                assert_bytes(current, ORIGINAL);
                assert_bytes(
                    &std::path::Path::new(old)
                        .parent()
                        .unwrap()
                        .join("unrelated.txt"),
                    b"unclaimed local entry",
                );
                let occupied = std::path::PathBuf::from(old.replace("Old", "New"));
                assert_bytes(
                    &occupied.parent().unwrap().join("foreign.txt"),
                    b"foreign target contents",
                );
                assert!(
                    !occupied
                        .parent()
                        .unwrap()
                        .with_file_name(format!(
                            "{} (2)",
                            occupied
                                .parent()
                                .unwrap()
                                .file_name()
                                .unwrap()
                                .to_str()
                                .unwrap()
                        ))
                        .exists()
                );
            }
        }
    }
}

#[tokio::test]
async fn album_directory_date_templates_use_exclusive_or_per_file_transition() {
    for (template, prefix, modified) in [
        ("{album}/%Y", "", false),
        ("{library}/{album}/%Y", "PrimarySync", true),
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let provider = records(&server, None);
        let mut options = album_directory_options("Old");
        options.folder_structure = template;
        cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Primary,
            options,
        )
        .await;
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let old: String = conn
            .query_row(
                "SELECT local_path FROM assets WHERE version_size='original' LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        let old = std::path::PathBuf::from(old);
        let parent = root.path().join("media").join(prefix);
        let relative = old.strip_prefix(parent.join("Old")).unwrap().to_owned();
        if modified {
            std::fs::write(&old, b"dated local edit").unwrap();
        }
        std::fs::create_dir_all(parent.join("Old/user/nested")).unwrap();
        std::fs::write(parent.join("Old/user/nested/file"), b"dated user contents").unwrap();
        std::fs::create_dir(parent.join("New")).unwrap();
        std::fs::write(parent.join("New/foreign"), b"foreign").unwrap();
        for quiet in [false, true, true] {
            let mut options = album_directory_options("New");
            options.folder_structure = template;
            let (result, _) = cycle_options(
                root.path(),
                provider.clone(),
                quiet,
                EditedNaming::Primary,
                options,
            )
            .await;
            assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
            assert_eq!(result.failed_count, 0);
            assert_bytes(&parent.join("New (1)").join(&relative), ORIGINAL);
            assert_bytes(
                &parent.join("New (1)/user/nested/file"),
                b"dated user contents",
            );
            assert_bytes(&parent.join("New/foreign"), b"foreign");
            assert!(!parent.join("New (2)").exists());
            if modified {
                assert_bytes(&old, b"dated local edit");
            } else {
                assert!(!parent.join("Old").exists());
            }
        }
    }
}

#[tokio::test]
async fn album_directory_retained_edit_history_keeps_its_original_paths() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    for edit in [Some(("edit-one", EDIT_ONE)), Some(("edit-two", EDIT_TWO))] {
        let (result, _) = cycle_options(
            root.path(),
            records(&server, edit),
            false,
            EditedNaming::Primary,
            album_directory_options("Old"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
    }
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let paths: Vec<Vec<u8>> = conn
        .prepare("SELECT native_path FROM primary_layout_preserved")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    drop(conn);
    assert!(!paths.is_empty());
    let paths: Vec<_> = paths
        .into_iter()
        .map(|bytes| {
            serde_json::from_slice::<state::db::provider_selection::SelectionPath>(&bytes)
                .unwrap()
                .to_path()
        })
        .collect();
    let bytes: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            records(&server, Some(("edit-two", EDIT_TWO))),
            quiet,
            EditedNaming::Primary,
            album_directory_options("New"),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), EDIT_TWO);
        for (path, expected) in paths.iter().zip(&bytes) {
            assert_bytes(path, expected);
        }
    }
}

#[tokio::test]
async fn album_directory_hierarchy_ancestor_change_uses_per_file_transition() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("Before/Album"),
    )
    .await;
    std::fs::write(
        root.path().join("media/Before/Album/user.txt"),
        b"leaf-owned contents",
    )
    .unwrap();
    std::fs::write(
        root.path().join("media/Before/ancestor.txt"),
        b"ancestor not owned",
    )
    .unwrap();
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            album_directory_options("After/Album"),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(
            &root.path().join("media/After/Album/IMG_0501.JPG"),
            ORIGINAL,
        );
        assert_bytes(
            &root.path().join("media/After/Album/user.txt"),
            b"leaf-owned contents",
        );
        assert_bytes(
            &root.path().join("media/Before/ancestor.txt"),
            b"ancestor not owned",
        );
        assert!(!root.path().join("media/After/ancestor.txt").exists());
    }
}

#[tokio::test]
async fn album_directory_state_failure_after_move_recovers_without_rewriting_old_journals() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("Old"),
    )
    .await;
    let media = root.path().join("media");
    std::fs::write(media.join("Old/user.txt"), b"carried user contents").unwrap();
    let manifests = retained_album_manifests(root.path());
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_album_directory_commit BEFORE UPDATE OF directory_binding ON album_containers WHEN OLD.directory_binding IS NOT NULL BEGIN SELECT RAISE(FAIL,'injected album directory commit failure'); END;").unwrap();
    drop(conn);
    let _failed = try_cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("New"),
    )
    .await;
    assert!(!media.join("Old").exists());
    assert_bytes(&media.join("New/user.txt"), b"carried user contents");
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='publishing'"
        ),
        1
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("DROP TRIGGER fail_album_directory_commit;")
        .unwrap();
    drop(conn);
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            album_directory_options("New"),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert_bytes(&media.join("New/IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("New/user.txt"), b"carried user contents");
    }
    let retained = retained_album_manifests(root.path());
    assert!(manifests.iter().all(|manifest| retained.contains(manifest)));
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "parent-owned SIGKILL fixture"]
async fn album_directory_process_death_child() {
    let root = std::path::PathBuf::from(std::env::var_os("KEI_TEST_PROCESS_DEATH_ROOT").unwrap());
    assert_eq!(
        std::fs::read(root.join("fixture-owner")).unwrap(),
        b"kei-synthetic-process-death"
    );
    let server = server().await;
    let projection = root.join("projection-fixture").exists();
    let additional = root.join("additional-projection-fixture").exists();
    let mut options = album_directory_options("New");
    if projection {
        options.folder_structure = "%Y/photos-{album}";
    }
    let mut provider = records(&server, None);
    if additional {
        options.folder_structure = "later/{album}";
        provider.extend(second_date_records(&server));
    }
    cycle_options(
        &root,
        provider,
        false,
        if projection || additional {
            EditedNaming::Suffix
        } else {
            EditedNaming::Primary
        },
        options,
    )
    .await;
    panic!("album directory child did not reach its process-death point");
}

#[cfg(target_os = "linux")]
async fn kill_album_child(root: &std::path::Path, point: &str) {
    use std::os::unix::process::ExitStatusExt;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sync_loop::runner::tests::primary_layout::album_directory_process_death_child",
            "--ignored",
            "--test-threads=1",
        ])
        .env("KEI_TEST_PROCESS_DEATH_ROOT", root)
        .env("KEI_TEST_PROCESS_DEATH_POINT", point)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if root.join("ready").exists() {
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("album child exited before {point}: {status}");
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            let _status = child.wait().unwrap();
            panic!("album child did not reach {point}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(std::fs::read_to_string(root.join("ready")).unwrap(), point);
    child.kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(9));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn album_directory_projection_reopens_partial_routing_and_late_collision() {
    for point in [
        "album-projection-planned",
        "album-projection-directory-published",
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let mut provider = records(&server, None);
        provider.extend(second_date_records(&server));
        let mut options = album_directory_options("Old");
        options.folder_structure = "%Y/photos-{album}";
        cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Suffix,
            options,
        )
        .await;
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        std::fs::write(root.path().join("projection-fixture"), b"dated album").unwrap();
        std::fs::write(
            root.path().join("media/2023/photos-Old/IMG_0501.JPG"),
            b"local edit",
        )
        .unwrap();
        kill_album_child(root.path(), point).await;
        let collision = point == "album-projection-planned";
        if collision {
            std::fs::create_dir(root.path().join("media/2024/photos-New")).unwrap();
            std::fs::write(
                root.path().join("media/2024/photos-New/foreign"),
                b"racing user directory",
            )
            .unwrap();
            let mut options = album_directory_options("New");
            options.folder_structure = "%Y/photos-{album}";
            assert!(
                try_cycle_options(
                    root.path(),
                    provider.clone(),
                    false,
                    EditedNaming::Suffix,
                    options
                )
                .await
                .is_err()
            );
            assert!(!root.path().join("media/2023/photos-New").exists());
        }
        for quiet in [false, true, true] {
            let mut options = album_directory_options("New");
            options.folder_structure = "%Y/photos-{album}";
            let (result, _) = cycle_options(
                root.path(),
                provider.clone(),
                quiet,
                EditedNaming::Suffix,
                options,
            )
            .await;
            assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
            assert_eq!(result.failed_count, 0);
            assert_bytes(
                &root.path().join("media/2023/photos-Old/IMG_0501.JPG"),
                b"local edit",
            );
            assert_bytes(
                &root.path().join("media/2023/photos-New/IMG_0501.JPG"),
                ORIGINAL,
            );
            let second = if collision {
                "photos-New (1)"
            } else {
                "photos-New"
            };
            assert_bytes(
                &root
                    .path()
                    .join("media/2024")
                    .join(second)
                    .join("IMG_0502.JPG"),
                ORIGINAL,
            );
            assert!(!root.path().join("media/2023/photos-New (1)").exists());
            assert!(!root.path().join("media/2024/photos-New (2)").exists());
            if collision {
                assert_bytes(
                    &root.path().join("media/2024/photos-New/foreign"),
                    b"racing user directory",
                );
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn album_directory_planned_move_reclassifies_a_later_local_edit() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("Old"),
    )
    .await;
    std::fs::write(
        root.path().join("fixture-owner"),
        b"kei-synthetic-process-death",
    )
    .unwrap();
    kill_album_child(root.path(), "album-directory-planned").await;
    std::fs::write(
        root.path().join("media/Old/IMG_0501.JPG"),
        b"edited after planning",
    )
    .unwrap();
    let result = try_cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("New"),
    )
    .await;
    assert!(result.is_err());
    assert_bytes(
        &root.path().join("media/Old/IMG_0501.JPG"),
        b"edited after planning",
    );
    assert!(!root.path().join("media/New").exists());
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            album_directory_options("New"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(
            &root.path().join("media/Old/IMG_0501.JPG"),
            b"edited after planning",
        );
        assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), ORIGINAL);
        assert!(!root.path().join("media/New (1)").exists());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn album_directory_staged_fallback_reselects_a_racing_collision_once() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let provider = records(&server, None);
    cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("Old"),
    )
    .await;
    std::fs::write(root.path().join("media/Old/IMG_0501.JPG"), b"local edit").unwrap();
    std::fs::write(
        root.path().join("fixture-owner"),
        b"kei-synthetic-process-death",
    )
    .unwrap();
    kill_album_child(root.path(), "album-directory-planned").await;
    std::fs::create_dir(root.path().join("media/New")).unwrap();
    std::fs::write(
        root.path().join("media/New/user.txt"),
        b"racing foreign directory",
    )
    .unwrap();
    let result = try_cycle_options(
        root.path(),
        provider.clone(),
        false,
        EditedNaming::Primary,
        album_directory_options("New"),
    )
    .await;
    assert!(result.is_err());
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            provider.clone(),
            quiet,
            EditedNaming::Primary,
            album_directory_options("New"),
        )
        .await;
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&root.path().join("media/Old/IMG_0501.JPG"), b"local edit");
        assert_bytes(
            &root.path().join("media/New/user.txt"),
            b"racing foreign directory",
        );
        assert_bytes(&root.path().join("media/New (1)/IMG_0501.JPG"), ORIGINAL);
        assert!(!root.path().join("media/New (2)").exists());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn album_directory_followon_sigkill_recovers_file_routing_and_missing_primary() {
    for point in [
        "album-directory-before-journal",
        "album-directory-planned",
        "album-directory-moved",
        "album-files-copied",
        "layout-planned",
        "layout-member-published",
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let provider = records(&server, None);
        cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Primary,
            album_directory_options("Old"),
        )
        .await;
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        std::fs::write(root.path().join("media/Old/user.txt"), b"user contents").unwrap();
        let receipts = old_album_receipts(root.path());
        let fallback = point.starts_with("album-");
        if fallback {
            std::fs::write(root.path().join("media/Old/IMG_0501.JPG"), b"local edit").unwrap();
        } else {
            cycle_options(
                root.path(),
                provider.clone(),
                false,
                EditedNaming::Primary,
                album_directory_options("New"),
            )
            .await;
            std::fs::remove_file(root.path().join("media/New/IMG_0501.JPG")).unwrap();
        }
        kill_album_child(root.path(), point).await;
        for quiet in [false, true, true] {
            let (result, _) = cycle_options(
                root.path(),
                provider.clone(),
                quiet,
                EditedNaming::Primary,
                album_directory_options("New"),
            )
            .await;
            assert_eq!(result.failed_count, 0, "{point}: {:?}", result.stats);
            assert!(
                result.db_sync_token_advance_safe,
                "{point}: {:?}",
                result.stats
            );
            assert_bytes(&root.path().join("media/New/IMG_0501.JPG"), ORIGINAL);
            assert_bytes(&root.path().join("media/New/user.txt"), b"user contents");
            if fallback {
                assert_bytes(&root.path().join("media/Old/IMG_0501.JPG"), b"local edit");
            } else {
                assert!(!root.path().join("media/Old").exists());
            }
            assert!(!root.path().join("media/New (1)").exists());
            assert_eq!(old_album_receipts(root.path()), receipts);
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn album_directory_sigkill_reopens_planned_moved_and_durable_transitions() {
    for point in [
        "album-directory-planned",
        "album-directory-moved",
        "album-directory-durable",
    ] {
        let server = server().await;
        let root = tempfile::tempdir().unwrap();
        let provider = records(&server, None);
        cycle_options(
            root.path(),
            provider.clone(),
            false,
            EditedNaming::Primary,
            album_directory_options("Old"),
        )
        .await;
        std::fs::write(
            root.path().join("fixture-owner"),
            b"kei-synthetic-process-death",
        )
        .unwrap();
        std::fs::write(root.path().join("media/Old/user.txt"), b"survives SIGKILL").unwrap();
        let manifests = retained_album_manifests(root.path());
        kill_album_child(root.path(), point).await;
        let expected = if point == "album-directory-planned" {
            "media/Old/user.txt"
        } else {
            "media/New/user.txt"
        };
        assert_bytes(&root.path().join(expected), b"survives SIGKILL");
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='publishing'"
            ),
            1
        );
        let destination = if point == "album-directory-planned" {
            std::fs::create_dir(root.path().join("media/New")).unwrap();
            std::fs::write(
                root.path().join("media/New/foreign.txt"),
                b"racing user contents",
            )
            .unwrap();
            let _failed = try_cycle_options(
                root.path(),
                provider.clone(),
                false,
                EditedNaming::Primary,
                album_directory_options("New"),
            )
            .await;
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM primary_layout_operations WHERE policy='album-directory-v1' AND phase='cancelled'"
                ),
                1
            );
            assert_bytes(&root.path().join("media/Old/user.txt"), b"survives SIGKILL");
            "New (1)"
        } else {
            "New"
        };
        for quiet in [false, true, true] {
            let (result, _) = cycle_options(
                root.path(),
                provider.clone(),
                quiet,
                EditedNaming::Primary,
                album_directory_options("New"),
            )
            .await;
            assert_eq!(result.failed_count, 0, "{point}: {:?}", result.stats);
            assert!(
                result.db_sync_token_advance_safe,
                "{point}: {:?}",
                result.stats
            );
            assert!(!root.path().join("media/Old").exists());
            assert_bytes(
                &root.path().join("media").join(destination).join("user.txt"),
                b"survives SIGKILL",
            );
            assert_bytes(
                &root
                    .path()
                    .join("media")
                    .join(destination)
                    .join("IMG_0501.JPG"),
                ORIGINAL,
            );
            if point == "album-directory-planned" {
                assert_bytes(
                    &root.path().join("media/New/foreign.txt"),
                    b"racing user contents",
                );
            }
        }
        let retained = retained_album_manifests(root.path());
        assert!(manifests.iter().all(|manifest| retained.contains(manifest)));
    }
}
#[tokio::test]
async fn startup_archive_adoption_flows_into_managed_sync_without_downloads() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    std::fs::create_dir(&media).unwrap();
    std::fs::write(media.join("IMG_0501.JPG"), ORIGINAL).unwrap();
    std::fs::write(media.join("IMG_0501.xmp"), b"retained user sidecar").unwrap();
    let unedited = records(&server, None);
    let provider = Session {
        records: unedited.clone(),
        quiet: false,
        lookups: Arc::new(AtomicUsize::new(0)),
        current: None,
        indexing: None,
    };
    let passes = [AlbumPass {
        kind: PassKind::Unfiled,
        album: make_full_album_with_boxed_session("PrimarySync", Box::new(provider)),
        exclude_ids: Arc::default(),
    }];
    let db = Arc::new(
        state::SqliteStateDb::open_owned(&root.path().join("state.db"), &fixture_owner())
            .await
            .unwrap(),
    );
    let mut config = download::DownloadConfig::test_default();
    config.directory = Arc::from(media.as_path());
    config.folder_structure = String::new();
    let stats = crate::commands::adopt_startup_archive(
        &db,
        &[(Arc::new(config), passes.as_slice())],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(stats.matched, 1);
    drop(db);
    for quiet in [false, true, true] {
        let (result, _) = cycle(root.path(), unedited.clone(), quiet, EditedNaming::Suffix).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501.xmp"), b"retained user sidecar");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM assets WHERE status='downloaded'"
        ),
        1
    );
}

#[tokio::test]
async fn recorded_generation_destinations_survive_template_changes() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let old = records(&server, Some(("edit-one", EDIT_ONE)));
    let (initial, _) = cycle_options(
        root.path(),
        old.clone(),
        false,
        EditedNaming::Suffix,
        CycleOptions {
            folder_structure: "before",
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    assert!(initial.db_sync_token_advance_safe);
    let media = root.path().join("media");
    let recorded: Vec<_> = std::fs::read_dir(media.join("before"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    assert_eq!(recorded.len(), 2);
    let initial_requests = server.received_requests().await.unwrap().len();
    assert_eq!(initial_requests, 2);
    let mut added = full_album_page_with_download(
        "PrimarySync",
        "502",
        "new-token",
        &format!("{}/original", server.uri()),
        ORIGINAL.len() as u64,
        &provider_hash(ORIGINAL),
    );
    added["records"][0]["fields"]["filenameEnc"]["value"] = json!("NEW.JPG");
    let mut current = old;
    current.extend(added["records"].as_array().unwrap().iter().cloned());
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            current.clone(),
            quiet,
            EditedNaming::Suffix,
            CycleOptions {
                folder_structure: "after",
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        for (path, bytes) in &recorded {
            assert_bytes(path, bytes);
        }
        assert_bytes(&media.join("after/NEW.JPG"), ORIGINAL);
        assert!(!media.join("after/IMG_0501.JPG").exists());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            initial_requests + 1
        );
    }
    std::fs::remove_file(media.join("before/IMG_0501.JPG")).unwrap();
    for cycle_index in 0..3 {
        let (result, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Suffix,
            CycleOptions {
                expected_due_selection: Some(cycle_index == 0),
                folder_structure: "after",
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe, "{:?}", result.stats);
        assert_bytes(&media.join("before/IMG_0501.JPG"), ORIGINAL);
        assert!(!media.join("after/IMG_0501.JPG").exists());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            initial_requests + 2
        );
    }
}

#[tokio::test]
async fn established_primary_layout_keeps_its_directory_after_template_change() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let (initial, _) = cycle_options(
        root.path(),
        records(&server, Some(("edit-one", EDIT_ONE))),
        false,
        EditedNaming::Primary,
        CycleOptions {
            folder_structure: "before",
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(initial.failed_count, 0, "{:?}", initial.stats);
    let requests = server.received_requests().await.unwrap().len();
    let bindings = db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings");
    let renamed = |edit| {
        let mut current = records(&server, edit);
        current[0]["fields"]["filenameEnc"]["value"] = json!("RENAMED.JPG");
        current
    };
    {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        let (preview, _) = cycle_options(
            root.path(),
            renamed(Some(("edit-one", EDIT_ONE))),
            false,
            EditedNaming::Suffix,
            CycleOptions {
                folder_structure: "after",
                controls: download::DownloadControls::dry_run_hidden(),
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(preview.failed_count, 0);
        assert!(capture.contains_event(|event| {
            event.message() == Some("[PLAN] Current selected rendition destination")
                && event.field("version") == Some("adjusted")
                && event.field("path") == root.path().join("media/before/IMG_0501.JPG").to_str()
        }));
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            operations
        );
    }
    for quiet in [false, true, true] {
        let (result, _) = cycle_options(
            root.path(),
            renamed(Some(("edit-one", EDIT_ONE))),
            quiet,
            EditedNaming::Suffix,
            CycleOptions {
                folder_structure: "after",
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&root.path().join("media/before/IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(
            &root.path().join("media/before/IMG_0501_original.JPG"),
            ORIGINAL,
        );
        assert!(!root.path().join("media/after/IMG_0501.JPG").exists());
        assert_eq!(
            db_count(root.path(), "SELECT COUNT(*) FROM primary_layout_bindings"),
            bindings
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_template_commit BEFORE INSERT ON primary_layout_bindings BEGIN SELECT RAISE(FAIL,'injected layout commit failure'); END;").unwrap();
    drop(conn);
    let interrupted = try_cycle_options(
        root.path(),
        renamed(Some(("edit-two", EDIT_TWO))),
        false,
        EditedNaming::Suffix,
        CycleOptions {
            folder_structure: "after",
            ..CycleOptions::default()
        },
    )
    .await;
    if let Ok((result, _)) = interrupted {
        assert!(result.failed_count > 0);
    }
    assert!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
        ) > 0
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("DROP TRIGGER fail_template_commit")
        .unwrap();
    drop(conn);
    let (updated, _) = cycle_options(
        root.path(),
        renamed(Some(("edit-two", EDIT_TWO))),
        false,
        EditedNaming::Suffix,
        CycleOptions {
            folder_structure: "final",
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(updated.failed_count, 0, "{:?}", updated.stats);
    assert_bytes(&root.path().join("media/before/IMG_0501.JPG"), EDIT_TWO);
    assert_bytes(
        &root.path().join("media/before/IMG_0501_original.JPG"),
        ORIGINAL,
    );
    assert!(!root.path().join("media/after/IMG_0501.JPG").exists());
    assert!(!root.path().join("media/final/IMG_0501.JPG").exists());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        requests + 1
    );
}

#[tokio::test]
async fn quiet_missing_file_requeue_failure_rolls_back_and_recovers_after_reopen() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let current = records(&server, Some(("edit-one", EDIT_ONE)));
    let options = || CycleOptions {
        ..CycleOptions::default()
    };
    let (initial, _) = cycle_options(
        root.path(),
        current.clone(),
        false,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert_eq!(initial.failed_count, 0);
    let media = root.path().join("media");
    std::fs::remove_file(media.join("IMG_0501.JPG")).unwrap();
    {
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        db.acquire_lock("inject missing-file requeue failure").unwrap().execute_batch(
            "CREATE TRIGGER fail_missing_requeue BEFORE UPDATE OF verified_media ON provider_active_destinations WHEN OLD.verified_media=1 AND NEW.verified_media=0 BEGIN SELECT RAISE(ABORT, 'injected missing requeue failure'); END;"
        ).unwrap();
    }
    let failed = try_cycle_options(
        root.path(),
        current.clone(),
        true,
        EditedNaming::Suffix,
        options(),
    )
    .await;
    assert!(failed.is_err());
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_destinations WHERE verified_media=0"
        ),
        0
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    {
        let db = state::SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap();
        db.acquire_lock("restore missing-file requeue writes")
            .unwrap()
            .execute_batch("DROP TRIGGER fail_missing_requeue")
            .unwrap();
    }
    for _ in 0..3 {
        let (result, _) = cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Suffix,
            options(),
        )
        .await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
    // Existence discovery must not interpret an existing local edit as absence.
    std::fs::write(media.join("IMG_0501.JPG"), b"local user edit").unwrap();
    let (quiet, _) =
        cycle_options(root.path(), current, true, EditedNaming::Suffix, options()).await;
    assert_eq!(quiet.failed_count, 0);
    assert_bytes(&media.join("IMG_0501.JPG"), b"local user edit");
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn primary_layout_cloud_filename_change_keeps_bound_rendition_paths() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let initial = records(&server, Some(("edit-one", EDIT_ONE)));
    let (first, _) = cycle(root.path(), initial.clone(), false, EditedNaming::Suffix).await;
    assert_eq!(first.failed_count, 0, "{:?}", first.stats);
    let media = root.path().join("media");
    let before: Vec<_> = std::fs::read_dir(&media)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    assert_eq!(before.len(), 2);
    let requests = server.received_requests().await.unwrap().len();
    assert_eq!(requests, 2);
    let mut renamed = initial;
    renamed[0]["fields"]["filenameEnc"]["value"] = json!("RENAMED.JPG");
    let (preview, _) = cycle_options(
        root.path(),
        renamed.clone(),
        false,
        EditedNaming::Suffix,
        CycleOptions {
            controls: download::DownloadControls::dry_run_hidden(),
            ..CycleOptions::default()
        },
    )
    .await;
    assert_eq!(preview.failed_count, 0, "{:?}", preview.stats);
    for quiet in [false, true, true] {
        let (result, _) = cycle(root.path(), renamed.clone(), quiet, EditedNaming::Suffix).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), ORIGINAL);
        for (path, bytes) in &before {
            assert_bytes(path, bytes);
        }
        assert!(!media.join("RENAMED.JPG").exists());
        assert_eq!(
            std::fs::read_dir(&media)
                .unwrap()
                .filter(|entry| entry.as_ref().unwrap().path().is_file())
                .count(),
            2
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM assets WHERE status='downloaded'"
        ),
        2
    );
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
    // Reusing a previously observed edit after a confirmed revert must retain
    // its stable provider rendition and independent historical receipts.
    let edited_again = records(&server, Some(("edit-one", EDIT_ONE)));
    let (again, _) = cycle(
        root.path(),
        edited_again.clone(),
        false,
        EditedNaming::Primary,
    )
    .await;
    assert_eq!(again.failed_count, 0, "{:?}", again.stats);
    assert!(again.db_sync_token_advance_safe);
    assert_eq!(
        again.stats.downloaded, 0,
        "re-observed edit ignored its verified history source"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), requests);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    let requests = server.received_requests().await.unwrap().len();
    let operations = db_count(
        root.path(),
        "SELECT COUNT(*) FROM primary_layout_operations",
    );
    for _ in 0..2 {
        let (quiet, lookups) = cycle(
            root.path(),
            edited_again.clone(),
            true,
            EditedNaming::Primary,
        )
        .await;
        assert_eq!(quiet.failed_count, 0);
        assert!(quiet.db_sync_token_advance_safe);
        assert_eq!(quiet.stats.downloaded, 0);
        assert_eq!(lookups, 0);
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            operations
        );
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
    }
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    let manifest = db.get_manifest_assets().await.unwrap();
    let files: std::collections::HashSet<_> = manifest
        .iter()
        .flat_map(|row| &row.preserved_files)
        .map(|receipt| receipt.native_path.to_path())
        .collect();
    let inventory = db.get_summary().await.unwrap().primary_layout.unwrap();
    assert_eq!(
        inventory.preserved_files,
        u64::try_from(files.len()).unwrap(),
        "inventory counted multiple receipts for one physical preserved file"
    );
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
async fn primary_layout_retains_recorded_paths_after_public_naming_retirement() {
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
    let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
        .await
        .unwrap();
    assert!(
        db.get_all_asset_albums("PrimarySync")
            .await
            .unwrap()
            .is_empty(),
        "unfiled path label became real membership"
    );
    drop(db);
    for mode in [
        download::DownloadRunMode::DryRun,
        download::DownloadRunMode::PrintFilenames,
    ] {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let operations = db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations",
        );
        let (preview, _) = cycle_options(
            root.path(),
            edited.clone(),
            false,
            EditedNaming::Suffix,
            CycleOptions {
                controls: download::DownloadControls::new(
                    mode,
                    download::DownloadReporting::hidden(),
                ),
                ..CycleOptions::default()
            },
        )
        .await;
        assert_eq!(preview.failed_count, 0);
        assert!(
            capture.contains_event(|event| {
                event.message() == Some("[PLAN] Current selected rendition destination")
                    && event.field("version") == Some("adjusted")
                    && event.field("path") == media.join("IMG_0501.JPG").to_str()
            }),
            "{:?}",
            capture.events()
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM primary_layout_operations"
            ),
            operations
        );
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    }
    for _ in 0..2 {
        let (retained, _) = cycle(root.path(), edited.clone(), true, EditedNaming::Suffix).await;
        assert_eq!(retained.failed_count, 0, "{:?}", retained.stats);
        assert!(retained.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
        assert!(!media.join("IMG_0501_edited.JPG").exists());
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_eq!(server.received_requests().await.unwrap().len(), downloads);
    }
    let changed = records(&server, Some(("edit-two", EDIT_TWO)));
    for cycle_number in 0..3 {
        let (retained, _) = cycle(
            root.path(),
            changed.clone(),
            cycle_number > 0,
            EditedNaming::Suffix,
        )
        .await;
        assert_eq!(
            retained.failed_count, 0,
            "cycle {cycle_number}: {:?}",
            retained.stats
        );
        assert!(retained.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_TWO);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert!(!media.join("IMG_0501_edited.JPG").exists());
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            downloads + 1
        );
    }
}

#[tokio::test]
async fn primary_layout_foreign_collision_and_local_modification_are_preserved() {
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
    assert!(conflict.db_sync_token_advance_safe);
    assert_eq!(conflict.failed_count, 0);
    assert_bytes(current, b"user changed owned bytes");
    assert_bytes(archive, ORIGINAL);
    assert_bytes(&media.join("IMG_0501.JPG"), b"foreign primary bytes");
}

#[tokio::test]
async fn contract_source_checkpoint_requires_durable_recovery_primary_layout_after_database_failure()
 {
    primary_layout_after_database_failure(false).await;
}

#[tokio::test]
async fn fresh_start_preserves_interrupted_primary_publication() {
    primary_layout_after_database_failure(true).await;
}

async fn primary_layout_after_database_failure(upgrade: bool) {
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
    // A real interrupted publication must wake its selected zone even when
    // the provider is quiet and no retained-checkpoint marker can mask it.
    {
        let db = state::SqliteStateDb::open_read_only(&root.path().join("state.db"))
            .await
            .unwrap();
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
        let selected = crate::sync_loop::test_support::make_run_cycle_library_state(
            "PrimarySync",
            "sync_token",
            "zone_token",
        );
        let mut precheck = crate::sync_loop::precheck::WatchPrecheck::SkipAll;
        crate::sync_loop::precheck::include_pending_local_work(
            &mut precheck,
            &db,
            &crate::config::MetadataConfig::default(),
            &[selected],
        )
        .await;
        assert!(precheck.should_sync_zone("PrimarySync"));
        assert!(!precheck.should_sync_zone("SharedSync-unselected"));
        assert!(precheck.db_sync_token_after_success().is_none());
    }
    let requests = server.received_requests().await.unwrap().len();
    rusqlite::Connection::open(root.path().join("state.db"))
        .unwrap()
        .execute_batch("DROP TRIGGER fail_layout_commit")
        .unwrap();
    if upgrade {
        rusqlite::Connection::open(root.path().join("state.db"))
            .unwrap()
            .pragma_update(None, "user_version", 37)
            .unwrap();
    }
    let (recovered, _) = cycle(root.path(), edited.clone(), true, EditedNaming::Suffix).await;
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
    for _ in 0..2 {
        let (quiet, _) = cycle(root.path(), edited.clone(), true, EditedNaming::Suffix).await;
        assert_eq!(quiet.failed_count, 0);
        assert!(quiet.db_sync_token_advance_safe);
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_ONE);
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert!(!media.join("IMG_0501_edited.JPG").exists());
        assert_eq!(server.received_requests().await.unwrap().len(), requests);
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
async fn primary_layout_private_generations_remain_complete_across_edits() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = || CycleOptions {
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
        drop(
            state::SqliteStateDb::open_owned(&db_path, &fixture_owner())
                .await
                .unwrap(),
        );
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

type RetainedSelectionEvidence = Vec<(String, String, String, Vec<u8>, String)>;

fn destination_evidence(
    conn: &rusqlite::Connection,
    generation: &str,
    pass: &str,
    child: &str,
) -> String {
    conn.query_row("SELECT json_group_array(json_array(version_size,path,checksum,size_bytes,metadata_hash,created_at,added_at,verified_media,verified_metadata,local_checksum,source_checksum,grouping_hash,intent_hash,progress_hash,prepared_checksum,prepared_size,prepared_hash)) FROM (SELECT * FROM provider_active_destinations WHERE generation=?1 AND pass_key=?2 AND child=?3 ORDER BY version_size,path)", rusqlite::params![generation,pass,child], |row| row.get(0)).unwrap()
}

fn retained_selection_evidence(root: &std::path::Path) -> RetainedSelectionEvidence {
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    let rows: Vec<(String,String,String,Vec<u8>)> = conn.prepare("SELECT generation,pass_key,child,manifest FROM provider_active_decisions d WHERE admission='admitted' AND EXISTS(SELECT 1 FROM provider_active_destinations w WHERE w.generation=d.generation AND w.pass_key=d.pass_key AND w.child=d.child AND w.verified_media=0) ORDER BY generation,pass_key,child").unwrap().query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap().collect::<Result<_,_>>().unwrap();
    rows.into_iter()
        .map(|(generation, pass, child, manifest)| {
            let evidence = destination_evidence(&conn, &generation, &pass, &child);
            (generation, pass, child, manifest, evidence)
        })
        .collect()
}

fn assert_retained_selection_evidence(root: &std::path::Path, before: &RetainedSelectionEvidence) {
    assert!(!before.is_empty());
    let conn = rusqlite::Connection::open(root.join("state.db")).unwrap();
    for (generation, pass, child, manifest, evidence) in before {
        let after: Vec<u8> = conn.query_row("SELECT manifest FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3", rusqlite::params![generation,pass,child], |row| row.get(0)).unwrap();
        assert_eq!(
            &after, manifest,
            "successor changed historical manifest bytes"
        );
        assert_eq!(
            &destination_evidence(&conn, generation, pass, child),
            evidence,
            "successor changed historical paths or receipts"
        );
    }
}

async fn interrupted_managed_source_change() -> (tempfile::TempDir, MockServer, Vec<Value>) {
    interrupted_managed_source_change_with_peer(false).await
}

async fn interrupted_managed_source_change_with_peer(
    include_peer: bool,
) -> (tempfile::TempDir, MockServer, Vec<Value>) {
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
    let mut first = records(&server, Some(("edit-changing", EDIT_ONE)));
    let mut second = records(&server, Some(("edit-two", EDIT_TWO)));
    if include_peer {
        let mut peer = full_album_page_with_download(
            "PrimarySync",
            "502",
            "peer-token",
            &format!("{}/missing-peer", server.uri()),
            ORIGINAL.len() as u64,
            &provider_hash(ORIGINAL),
        );
        peer["records"][1]["fields"]["masterRef"]["value"]["zoneID"]["ownerRecordName"] =
            json!("_defaultOwner");
        let records = peer["records"].as_array().unwrap();
        first.extend(records.iter().cloned());
        second.extend(records.iter().cloned());
    }
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
    (root, server, second)
}

#[tokio::test]
async fn primary_layout_source_change_during_preparation_retains_work_then_replans() {
    let (root, server, second) = interrupted_managed_source_change().await;
    let media = root.path().join("media");
    let before = retained_selection_evidence(root.path());
    let (replanned, _) = cycle(root.path(), second.clone(), true, EditedNaming::Primary).await;
    assert_eq!(replanned.failed_count, 0, "{:?}", replanned.stats);
    assert!(replanned.db_sync_token_advance_safe);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_TWO);
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    assert_retained_selection_evidence(root.path(), &before);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_decisions WHERE admission='excluded' AND reason LIKE 'managed_successor:%'"
        ),
        1
    );
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_destinations w JOIN provider_active_decisions d USING(generation,pass_key,child) WHERE d.reason LIKE 'managed_successor:%' AND (w.admitted<>0 OR w.verified_media<>0 OR w.verified_metadata<>0)"
        ),
        0,
        "successor invented old publication progress"
    );
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
async fn managed_successor_admission_rollback_preserves_old_authority_and_recovers() {
    let (root, server, second) = interrupted_managed_source_change().await;
    let before = retained_selection_evidence(root.path());
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_managed_successor_admission BEFORE INSERT ON assets WHEN EXISTS(SELECT 1 FROM provider_active_decisions WHERE reason LIKE 'managed_successor:%') BEGIN SELECT RAISE(FAIL,'injected successor admission failure'); END").unwrap();
    drop(conn);
    let held = try_cycle_options(
        root.path(),
        second.clone(),
        true,
        EditedNaming::Primary,
        CycleOptions::default(),
    )
    .await;
    assert!(
        held.is_err()
            || held
                .as_ref()
                .is_ok_and(|(result, _)| !result.db_sync_token_advance_safe)
    );
    assert_retained_selection_evidence(root.path(), &before);
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM provider_active_decisions WHERE reason LIKE 'managed_successor:%'"
        ),
        0
    );
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    for (generation, pass, child, _, _) in &before {
        let admission: String = conn.query_row("SELECT admission FROM provider_active_decisions WHERE generation=?1 AND pass_key=?2 AND child=?3", rusqlite::params![generation,pass,child], |row| row.get(0)).unwrap();
        assert_eq!(admission, "admitted");
    }
    conn.execute_batch("DROP TRIGGER fail_managed_successor_admission")
        .unwrap();
    drop(conn);
    assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
    for _ in 0..3 {
        let (result, _) = cycle(root.path(), second.clone(), true, EditedNaming::Primary).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_TWO);
        assert_retained_selection_evidence(root.path(), &before);
    }
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/edit-two")
            .count(),
        1
    );
}

#[tokio::test]
async fn managed_successor_preserves_unrelated_unfinished_identity() {
    let (root, _server, current) = interrupted_managed_source_change_with_peer(true).await;
    let before = retained_selection_evidence(root.path());
    assert_eq!(before.len(), 2);
    for _ in 0..2 {
        let held = try_cycle_options(
            root.path(),
            current.clone(),
            true,
            EditedNaming::Primary,
            CycleOptions::default(),
        )
        .await
        .unwrap()
        .0;
        assert!(
            !held.db_sync_token_advance_safe,
            "unfinished peer released checkpoint"
        );
        assert_bytes(&root.path().join("media/IMG_0501.JPG"), EDIT_TWO);
        assert_retained_selection_evidence(root.path(), &before);
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let remaining: i64 = conn.query_row("SELECT COUNT(*) FROM provider_active_destinations w JOIN provider_active_decisions d USING(generation,pass_key,child) WHERE w.generation=?1 AND d.admission='admitted' AND w.admitted=1 AND w.verified_media=0", [&before[0].0], |row| row.get(0)).unwrap();
        assert!(
            remaining > 0,
            "unrelated unfinished destination lost authority"
        );
        assert_eq!(
            db_count(
                root.path(),
                "SELECT COUNT(*) FROM provider_active_decisions WHERE reason LIKE 'managed_successor:%'"
            ),
            1
        );
    }
}

#[tokio::test]
async fn managed_successor_refuses_unknown_mismatched_publishing_and_damaged_evidence() {
    for fault in [
        "unknown_identity",
        "mismatched_master",
        "publishing",
        "damaged_prepared",
        "unknown_source",
    ] {
        let (root, _server, mut current) = interrupted_managed_source_change().await;
        let before = retained_selection_evidence(root.path());
        let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
        let members: Vec<(String,i64,Vec<u8>)> = conn.prepare("SELECT m.operation,m.member,m.evidence FROM primary_layout_members m JOIN primary_layout_operations o USING(operation) WHERE o.phase='prepared'").unwrap().query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().collect::<Result<_,_>>().unwrap();
        let (operation, index, bytes) = members
            .into_iter()
            .find(|(_, _, bytes)| {
                let member: state::db::primary_layout::LayoutMember =
                    serde_json::from_slice(bytes).unwrap();
                member
                    .record
                    .is_some_and(|record| record.version_size.as_str() == "adjusted")
            })
            .unwrap();
        let mut member: state::db::primary_layout::LayoutMember =
            serde_json::from_slice(&bytes).unwrap();
        let prepared = member.prepared.as_ref().unwrap().path.to_path();
        match fault {
            "unknown_identity" => {
                current[1]["fields"]
                    .as_object_mut()
                    .unwrap()
                    .remove("masterRef");
            }
            "mismatched_master" => {
                let old_master = current[0].clone();
                current[0]["recordName"] = json!("different-master-501");
                current[1]["fields"]["masterRef"]["value"]["recordName"] =
                    json!("different-master-501");
                current.push(old_master);
            }
            "publishing" => {
                conn.execute("UPDATE primary_layout_operations SET phase='publishing',header=CAST(json_set(CAST(header AS TEXT),'$.phase','publishing') AS BLOB) WHERE operation=?1", [&operation]).unwrap();
                conn.execute_batch("CREATE TRIGGER hold_publishing_journal BEFORE UPDATE ON primary_layout_operations BEGIN SELECT RAISE(FAIL,'publishing journal must recover first'); END").unwrap();
            }
            "damaged_prepared" => std::fs::write(&prepared, b"changed prepared evidence").unwrap(),
            "unknown_source" => {
                member.prepared.as_mut().unwrap().source_checksum = None;
                conn.execute("UPDATE primary_layout_members SET evidence=?3 WHERE operation=?1 AND member=?2", rusqlite::params![operation,index,serde_json::to_vec(&member).unwrap()]).unwrap();
            }
            other => panic!("unknown managed successor fault: {other}"),
        }
        drop(conn);
        for _ in 0..2 {
            let held = try_cycle_options(
                root.path(),
                current.clone(),
                true,
                EditedNaming::Primary,
                CycleOptions::default(),
            )
            .await;
            assert!(
                held.is_err()
                    || held
                        .as_ref()
                        .is_ok_and(|(result, _)| !result.db_sync_token_advance_safe),
                "{fault}: unexpectedly released checkpoint"
            );
            assert_bytes(&root.path().join("media/IMG_0501.JPG"), ORIGINAL);
            assert_retained_selection_evidence(root.path(), &before);
            assert_eq!(
                db_count(
                    root.path(),
                    "SELECT COUNT(*) FROM provider_active_decisions WHERE reason LIKE 'managed_successor:%'"
                ),
                0,
                "{fault}: withdrew old authority"
            );
            assert_bytes(
                &prepared,
                if fault == "damaged_prepared" {
                    b"changed prepared evidence"
                } else {
                    EDIT_ONE
                },
            );
        }
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
            // The zone delta cannot establish smart-folder membership. Current
            // selection refreshes and confirms it even when media is unchanged.
            let expected_lookups = if pass_kind == PassKind::SmartFolder {
                2
            } else {
                0
            };
            assert_eq!(
                lookups, expected_lookups,
                "{pass_kind:?}: unexpected quiet lookup"
            );
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
        let mut edited =
            records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
        // Current policy always includes available originals. Exercise a
        // genuinely edit-only provider response while retaining the historical
        // original and its sidecar as preservation evidence.
        edited[0]["fields"]
            .as_object_mut()
            .unwrap()
            .remove("resOriginalRes");
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
            let retained_template = directory == "media";
            let parent = if retained_template {
                old_media.clone()
            } else {
                root.path().join(directory).join(template)
            };
            let adjusted_path =
                parent.join(if retained_template || naming == EditedNaming::Primary {
                    "IMG_0501.JPG"
                } else {
                    "IMG_0501_edited.JPG"
                });
            let original_path =
                parent.join(if retained_template || naming == EditedNaming::Primary {
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
                assert_bytes(
                    &old_media.join("IMG_0501.JPG"),
                    if retained_template {
                        EDIT_TWO
                    } else {
                        EDIT_ONE
                    },
                );
                assert_bytes(&old_media.join("IMG_0501_original.JPG"), ORIGINAL);
                let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
                if retained_template {
                    assert!(
                        !root
                            .path()
                            .join(directory)
                            .join(template)
                            .join("IMG_0501.JPG")
                            .exists()
                    );
                    let preserved: Vec<Vec<u8>> = conn
                        .prepare("SELECT native_path FROM primary_layout_preserved")
                        .unwrap()
                        .query_map([], |row| row.get(0))
                        .unwrap()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    assert!(preserved.iter().any(|path| {
                        let path = serde_json::from_slice::<
                            state::db::provider_selection::SelectionPath,
                        >(path)
                        .unwrap()
                        .to_path();
                        std::fs::read(path).unwrap() == EDIT_ONE
                    }));
                }
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
        ..CycleOptions::default()
    };
    let current = records_with_original(&server, VALID_ORIGINAL, Some(("edit-one", VALID_EDIT)));
    for (naming, quiet) in [(EditedNaming::Primary, false), (EditedNaming::Suffix, true)] {
        let (result, _) =
            cycle_options(root.path(), current.clone(), quiet, naming, options()).await;
        assert_eq!(result.failed_count, 0, "{:?}", result.stats);
        assert!(result.db_sync_token_advance_safe);
    }
    let old_xmp = std::fs::read(media.join("IMG_0501.JPG.xmp")).unwrap();
    let archive_xmp = std::fs::read(media.join("IMG_0501_original.JPG.xmp")).unwrap();
    let requests = server.received_requests().await.unwrap().len();
    let mut changed = current;
    changed[1]["recordChangeTag"] = json!("ct-asset-caption");
    changed[1]["fields"]["captionEnc"] =
        json!({"type":"STRING","value":"caption changed after naming retirement"});
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
    assert_bytes(&media.join("IMG_0501_original.JPG"), VALID_ORIGINAL);
    assert_bytes(&media.join("IMG_0501.JPG"), VALID_EDIT);
    assert!(!media.join("IMG_0501_edited.JPG").exists());
    let xmp = std::fs::read_to_string(media.join("IMG_0501.JPG.xmp")).unwrap();
    assert!(
        xmp.contains("caption changed after naming retirement"),
        "stats {:?}, lookups/events {:?}: {xmp}",
        result.stats,
        capture.events()
    );
    assert_ne!(xmp.as_bytes(), old_xmp);
    // The original remains selected in the retained primary layout, so its
    // configured sidecar receives current metadata too. Its previous bytes
    // must remain independently preserved, unlike an unselected archive.
    let current_original_xmp = std::fs::read(media.join("IMG_0501_original.JPG.xmp")).unwrap();
    assert!(
        String::from_utf8_lossy(&current_original_xmp)
            .contains("caption changed after naming retirement")
    );
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
    assert!(
        manifest
            .iter()
            .flat_map(|row| &row.preserved_files)
            .any(|file| {
                let mut path = file.native_path.to_path().as_os_str().to_os_string();
                path.push(".xmp");
                std::fs::read(std::path::PathBuf::from(path))
                    .is_ok_and(|bytes| bytes == archive_xmp)
            }),
        "prior selected original sidecar was not independently preserved"
    );
    drop(db);
    let conn = rusqlite::Connection::open(root.path().join("state.db")).unwrap();
    let title: String = conn.query_row("SELECT title FROM assets WHERE library='PrimarySync' AND id='asset-501' AND version_size='adjusted'", [], |row| row.get(0)).unwrap();
    assert_eq!(title, "caption changed after naming retirement");
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
        assert_bytes(&media.join("IMG_0501.JPG.xmp"), xmp.as_bytes());
        assert_bytes(
            &media.join("IMG_0501_original.JPG.xmp"),
            &current_original_xmp,
        );
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

#[tokio::test]
async fn review501_suffix_album_collecting_edit_recovers_without_redownload() {
    let server = server().await;
    let root = tempfile::tempdir().unwrap();
    let media = root.path().join("media");
    let options = || CycleOptions {
        pass_kind: PassKind::Album,
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
    assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
    assert_bytes(&media.join("IMG_0501.JPG"), EDIT_TWO);
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
        assert_bytes(&media.join("IMG_0501_original.JPG"), ORIGINAL);
        assert_bytes(&media.join("IMG_0501.JPG"), EDIT_TWO);
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
    assert_eq!(std::path::PathBuf::from(path), media.join("IMG_0501.JPG"));
    assert_eq!(
        db_count(
            root.path(),
            "SELECT COUNT(*) FROM primary_layout_operations WHERE phase NOT IN ('committed','cancelled')"
        ),
        0
    );
}
