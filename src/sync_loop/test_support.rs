//! Shared fixtures for sync-loop production-path tests.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::commands::PassScope;
use crate::sync_cycle::{CycleResult, LibraryState, run_cycle};
use crate::{auth, cli, config, download, retry, state};

pub(super) fn make_incremental_album(zone_sync_token: &str) -> crate::icloud::photos::PhotoAlbum {
    use serde_json::json;
    crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            retry_config: retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(crate::test_helpers::MockPhotosSession::new().ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": zone_sync_token,
                "moreComing": false,
                "records": []
            }]
        }))),
    )
}

pub(super) fn album_count_response(count: u64) -> serde_json::Value {
    serde_json::json!({
        "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
    })
}

pub(super) fn full_album_page(
    zone: &str,
    record_name: &str,
    sync_token: &str,
) -> serde_json::Value {
    full_album_page_with_download(
        zone,
        record_name,
        sync_token,
        "https://p01.icloud-content.com/photo.jpg",
        1024,
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
    )
}

/// The `assetDate`/`addedDate` embedded by [`full_album_page_with_download`].
pub(super) const RUN_CYCLE_ASSET_DATE_MS: i64 = 1_700_000_000_000;

/// The host-local `%Y/%m/%d` folder for an asset without an offset.
pub(super) fn run_cycle_expected_date_dir() -> String {
    chrono::DateTime::from_timestamp_millis(RUN_CYCLE_ASSET_DATE_MS)
        .expect("valid asset timestamp")
        .with_timezone(&chrono::Local)
        .format("%Y/%m/%d")
        .to_string()
}

pub(super) fn full_album_page_with_download(
    zone: &str,
    record_name: &str,
    sync_token: &str,
    download_url: &str,
    size: u64,
    checksum: &str,
) -> serde_json::Value {
    serde_json::json!({
        "records": [
            {
                "recordName": record_name,
                "recordType": "CPLMaster",
                "fields": {
                    "filenameEnc": {"value": "cGhvdG8uanBn", "type": "STRING"},
                    "resOriginalRes": {
                        "value": {
                            "downloadURL": download_url,
                            "size": size,
                            "fileChecksum": checksum
                        }
                    },
                    "resOriginalWidth": {"value": 100, "type": "INT64"},
                    "resOriginalHeight": {"value": 100, "type": "INT64"},
                    "resOriginalFileType": {"value": "public.jpeg"},
                    "itemType": {"value": "public.jpeg"},
                    "adjustmentRenderType": {"value": 0, "type": "INT64"}
                },
                "recordChangeTag": "ct-master"
            },
            {
                "recordName": format!("asset-{record_name}"),
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {
                        "value": {"recordName": record_name, "zoneID": {"zoneName": zone}},
                        "type": "REFERENCE"
                    },
                    "assetDate": {"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"},
                    "addedDate": {"value": RUN_CYCLE_ASSET_DATE_MS, "type": "TIMESTAMP"}
                },
                "recordChangeTag": "ct-asset"
            }
        ],
        "syncToken": sync_token
    })
}

pub(super) fn make_full_album_with_session(
    zone: &str,
    session: crate::test_helpers::MockPhotosSession,
) -> crate::icloud::photos::PhotoAlbum {
    make_named_full_album_with_boxed_session(zone, "TestAlbum", Box::new(session))
}

pub(super) fn make_full_album_with_boxed_session(
    zone: &str,
    session: Box<dyn crate::icloud::photos::PhotosSession>,
) -> crate::icloud::photos::PhotoAlbum {
    make_named_full_album_with_boxed_session(zone, "TestAlbum", session)
}

pub(super) fn make_named_full_album_with_boxed_session(
    zone: &str,
    name: &str,
    session: Box<dyn crate::icloud::photos::PhotosSession>,
) -> crate::icloud::photos::PhotoAlbum {
    use serde_json::json;
    crate::icloud::photos::PhotoAlbum::new(
        crate::icloud::photos::PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": zone})),
            retry_config: retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}

pub(super) fn make_named_empty_full_album(
    zone: &str,
    name: &str,
    zone_sync_token: &str,
) -> crate::icloud::photos::PhotoAlbum {
    make_named_full_album_with_boxed_session(
        zone,
        name,
        Box::new(
            crate::test_helpers::MockPhotosSession::new()
                .ok(album_count_response(0))
                .ok(serde_json::json!({"records": [], "syncToken": zone_sync_token})),
        ),
    )
}

pub(super) fn make_empty_full_album(zone_sync_token: &str) -> crate::icloud::photos::PhotoAlbum {
    make_empty_full_album_for_zone("PrimarySync", zone_sync_token)
}

pub(super) fn make_empty_full_album_for_zone(
    zone: &str,
    zone_sync_token: &str,
) -> crate::icloud::photos::PhotoAlbum {
    make_named_empty_full_album(zone, "TestAlbum", zone_sync_token)
}

pub(super) fn make_run_cycle_library_state(
    zone: &str,
    sync_token_key: &str,
    zone_sync_token: &str,
) -> LibraryState {
    make_run_cycle_library_state_with_album(
        zone,
        sync_token_key,
        make_incremental_album(zone_sync_token),
    )
}

pub(super) fn make_run_cycle_library_state_with_album(
    zone: &str,
    sync_token_key: &str,
    album: crate::icloud::photos::PhotoAlbum,
) -> LibraryState {
    make_run_cycle_library_state_with_passes(
        zone,
        sync_token_key,
        vec![crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album,
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    )
}

pub(super) fn make_run_cycle_library_state_with_passes(
    zone: &str,
    sync_token_key: &str,
    passes: Vec<crate::commands::AlbumPass>,
) -> LibraryState {
    LibraryState {
        library: crate::icloud::photos::PhotoLibrary::new_stub_with_zone(
            Box::new(crate::test_helpers::MockPhotosSession::new()),
            zone,
        ),
        pass_scope: PassScope {
            include_albums: passes
                .iter()
                .any(|pass| pass.kind == crate::commands::PassKind::Album),
            include_smart_folders: passes
                .iter()
                .any(|pass| pass.kind == crate::commands::PassKind::SmartFolder),
            include_unfiled: passes
                .iter()
                .any(|pass| pass.kind == crate::commands::PassKind::Unfiled),
        },
        zone_name: zone.to_string(),
        sync_token_key: sync_token_key.to_string(),
        plan: crate::commands::AlbumPlan { passes },
        plan_is_stale: false,
        plan_needs_refresh: false,
        cross_zone_libraries: Vec::new(),
    }
}

pub(super) async fn make_shared_session_for_run_cycle() -> (tempfile::TempDir, auth::SharedSession)
{
    let dir = tempfile::tempdir().expect("session tempdir");
    let session =
        auth::session::Session::new(dir.path(), "test@example.com", "https://example.com", None)
            .await
            .expect("test session");
    (dir, Arc::new(tokio::sync::RwLock::new(session)))
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RunCycleDownloadConfigOptions {
    pub(super) media: config::MediaSelection,
    pub(super) per_pass_paths: bool,
    pub(super) recent: Option<u32>,
    pub(super) skip_created_before: Option<config::CreatedDateFilter>,
    pub(super) skip_created_after: Option<config::CreatedDateFilter>,
    pub(super) file_match_policy: Option<crate::types::FileMatchPolicy>,
    #[cfg(feature = "xmp")]
    pub(super) xmp_sidecar: bool,
    /// Passes run through `buffer_unordered(pass_parallelism)`, which is
    /// capped by this, so it must exceed one for passes to overlap.
    pub(super) concurrent_downloads: Option<usize>,
}

pub(super) fn media_without_photo_downloads() -> config::MediaSelection {
    config::MediaSelection {
        photos: false,
        videos: true,
        live_photos: true,
    }
}

pub(super) fn make_run_cycle_download_config_builder(
    download_dir: &std::path::Path,
    db: Arc<dyn download::DownloadStore>,
) -> impl Fn(
    download::SyncMode,
    Arc<rustc_hash::FxHashSet<String>>,
    Arc<download::AssetGroupings>,
    Arc<str>,
) -> Arc<download::DownloadConfig>
+ '_ {
    make_run_cycle_download_config_builder_with_options(
        download_dir,
        db,
        RunCycleDownloadConfigOptions::default(),
    )
}

pub(super) fn make_run_cycle_download_config_builder_with_options(
    download_dir: &std::path::Path,
    db: Arc<dyn download::DownloadStore>,
    options: RunCycleDownloadConfigOptions,
) -> impl Fn(
    download::SyncMode,
    Arc<rustc_hash::FxHashSet<String>>,
    Arc<download::AssetGroupings>,
    Arc<str>,
) -> Arc<download::DownloadConfig>
+ '_ {
    move |sync_mode, exclude_asset_ids, asset_groupings, library| {
        let mut config = download::DownloadConfig::test_default();
        config.directory = Arc::from(download_dir);
        config.folder_structure = "%Y/%m/%d".to_string();
        config.folder_structure_albums = Arc::from("%Y/%m/%d");
        config.folder_structure_smart_folders = Arc::from("%Y/%m/%d");
        if options.per_pass_paths {
            config.folder_structure_albums = Arc::from("{album}");
        }
        if let Some(file_match_policy) = options.file_match_policy {
            config.file_match_policy = file_match_policy;
        }
        config.media = options.media;
        #[cfg(feature = "xmp")]
        {
            config.metadata.xmp_sidecar = options.xmp_sidecar;
        }
        config.recent = options.recent;
        config.skip_created_before = options.skip_created_before;
        config.skip_created_after = options.skip_created_after;
        if let Some(concurrent_downloads) = options.concurrent_downloads {
            config.concurrent_downloads = concurrent_downloads;
        }
        config.state_db = Some(Arc::clone(&db));
        config.sync_mode = sync_mode;
        config.exclude_asset_ids = exclude_asset_ids;
        config.asset_groupings = asset_groupings;
        config.library = library;
        Arc::new(config)
    }
}

pub(super) fn make_recording_run_cycle_download_config_builder(
    download_dir: &std::path::Path,
    db: Arc<dyn download::DownloadStore>,
    observed_modes: Arc<std::sync::Mutex<Vec<download::SyncMode>>>,
) -> impl Fn(
    download::SyncMode,
    Arc<rustc_hash::FxHashSet<String>>,
    Arc<download::AssetGroupings>,
    Arc<str>,
) -> Arc<download::DownloadConfig>
+ '_ {
    let build_download_config = make_run_cycle_download_config_builder(download_dir, db);
    move |sync_mode, exclude_asset_ids, asset_groupings, library| {
        observed_modes
            .lock()
            .expect("recorded modes lock")
            .push(sync_mode.clone());
        build_download_config(sync_mode, exclude_asset_ids, asset_groupings, library)
    }
}

pub(super) fn make_run_cycle_config() -> config::Config {
    let data_dir = tempfile::tempdir().expect("config data dir");
    let globals = config::GlobalArgs {
        username: Some("test@example.com".to_string()),
        domain: None,
        data_dir: Some(data_dir.path().to_string_lossy().into_owned()),
    };
    config::Config::build(
        &globals,
        &cli::PasswordArgs::default(),
        cli::SyncArgs::default(),
        None,
    )
    .expect("test config")
}

pub(super) async fn run_full_cycle_with_album(
    album: crate::icloud::photos::PhotoAlbum,
    is_retry_failed: bool,
    controls: download::DownloadControls,
) -> CycleResult {
    let config = make_run_cycle_config();
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let states = vec![&lib_state];
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        is_retry_failed,
        &build_download_config,
        controls,
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle")
}

// ── determine_sync_mode ──────────────────────────────────────────
//
// Sync-mode decision is the gatekeeper for the kei "user data is sacred"
// invariant: pick Full vs Incremental wrong and either (a) re-download
// the world (waste) or (b) skip previously-failed assets (silent loss).
// None of the four critical branches had a direct unit test before.

pub(super) fn make_state_db() -> Arc<dyn download::DownloadStore> {
    Arc::new(state::SqliteStateDb::open_in_memory().expect("open in-memory state DB"))
}

pub(super) const SCOPED_DB_SYNC_TOKEN_FAILURE_KEY: &str = "scoped_db_sync_token";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MetadataSetFailure {
    Exact(&'static str),
    Prefix(&'static str),
}

impl MetadataSetFailure {
    pub(super) fn matches(self, key: &str) -> bool {
        match self {
            Self::Exact(expected) => key == expected,
            Self::Prefix(prefix) => key.starts_with(prefix),
        }
    }
}

pub(super) struct FailingMetadataSetDb {
    pub(super) inner: Arc<dyn download::DownloadStore>,
    pub(super) failure: MetadataSetFailure,
    pub(super) get_failure: Option<MetadataSetFailure>,
    pub(super) delete_prefix_failure: Option<&'static str>,
    pub(super) message: &'static str,
    pub(super) cancel_on_upsert: Option<CancellationToken>,
    pub(super) replace_download_dir_on_upsert: Option<std::path::PathBuf>,
    pub(super) fail_upsert_seen: bool,
    pub(super) fail_mark_downloaded: bool,
    pub(super) fail_refresh_downloaded_metadata: bool,
    /// Stands in for a concurrent pass: refreshes the row to this snapshot
    /// and marks it for rewrite in the window between the downloader
    /// writing the file and the finaliser deciding the marker's fate.
    pub(super) refresh_on_mark_downloaded: Option<state::AssetMetadata>,
    /// Counts drains: `run_pending` starts each one with an offset-zero
    /// page fetch.
    pub(super) drains: Arc<std::sync::atomic::AtomicUsize>,
    /// Fails the rewritten-media checksum write so a drain can be driven
    /// into the window where the file changed but the row has not caught up.
    pub(super) fail_metadata_checksum_write: bool,
}

impl std::fmt::Debug for FailingMetadataSetDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailingMetadataSetDb")
            .field("failure", &self.failure)
            .field("get_failure", &self.get_failure)
            .field("delete_prefix_failure", &self.delete_prefix_failure)
            .field("message", &self.message)
            .finish_non_exhaustive()
    }
}

impl FailingMetadataSetDb {
    pub(super) fn new(
        inner: Arc<dyn download::DownloadStore>,
        failure: MetadataSetFailure,
        message: &'static str,
    ) -> Self {
        Self {
            inner,
            failure,
            get_failure: None,
            delete_prefix_failure: None,
            message,
            cancel_on_upsert: None,
            replace_download_dir_on_upsert: None,
            fail_upsert_seen: false,
            fail_mark_downloaded: false,
            fail_refresh_downloaded_metadata: false,
            refresh_on_mark_downloaded: None,
            drains: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            fail_metadata_checksum_write: false,
        }
    }

    #[cfg(feature = "xmp")]
    pub(super) fn drain_counter(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::clone(&self.drains)
    }

    pub(super) fn without_set_failure(
        inner: Arc<dyn download::DownloadStore>,
        message: &'static str,
    ) -> Self {
        Self::new(
            inner,
            MetadataSetFailure::Exact("__unused_metadata_key__"),
            message,
        )
    }

    pub(super) fn with_refresh_downloaded_metadata_failure(mut self) -> Self {
        self.fail_refresh_downloaded_metadata = true;
        self
    }

    pub(super) fn with_get_failure(mut self, failure: MetadataSetFailure) -> Self {
        self.get_failure = Some(failure);
        self
    }

    pub(super) fn with_cancel_on_upsert(mut self, token: CancellationToken) -> Self {
        self.cancel_on_upsert = Some(token);
        self
    }

    pub(super) fn with_download_dir_replaced_on_upsert(mut self, path: std::path::PathBuf) -> Self {
        self.replace_download_dir_on_upsert = Some(path);
        self
    }

    pub(super) fn with_mark_downloaded_failure(mut self) -> Self {
        self.fail_mark_downloaded = true;
        self
    }

    #[cfg(feature = "xmp")]
    pub(super) fn with_refresh_on_mark_downloaded(
        mut self,
        metadata: state::AssetMetadata,
    ) -> Self {
        self.refresh_on_mark_downloaded = Some(metadata);
        self
    }

    pub(super) fn with_upsert_seen_failure(mut self) -> Self {
        self.fail_upsert_seen = true;
        self
    }
}

#[async_trait::async_trait]
impl state::DownloadStateStore for FailingMetadataSetDb {
    #[cfg(test)]
    async fn should_download(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        checksum: &str,
        local_path: &std::path::Path,
    ) -> Result<bool, state::error::StateError> {
        self.inner
            .should_download(library, id, version_size, checksum, local_path)
            .await
    }

    async fn upsert_seen(
        &self,
        record: &state::types::AssetRecord,
    ) -> Result<(), state::error::StateError> {
        if self.fail_upsert_seen {
            return Err(state::error::StateError::LockPoisoned(self.message.into()));
        }
        let result = self.inner.upsert_seen(record).await;
        if result.is_ok() {
            if let Some(path) = &self.replace_download_dir_on_upsert {
                let _ = std::fs::remove_dir_all(path);
                std::fs::write(path, b"destination replaced by fault injection")
                    .expect("replace download dir with file");
            }
            if let Some(token) = &self.cancel_on_upsert {
                token.cancel();
            }
        }
        result
    }

    async fn mark_downloaded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        local_path: &std::path::Path,
        local_checksum: &str,
        download_checksum: Option<&str>,
    ) -> Result<(), state::error::StateError> {
        if self.fail_mark_downloaded {
            return Err(state::error::StateError::LockPoisoned(self.message.into()));
        }
        if let Some(newer) = &self.refresh_on_mark_downloaded {
            let record = self
                .inner
                .get_downloaded_page(0, u32::MAX)
                .await?
                .into_iter()
                .find(|row| {
                    row.library.as_ref() == library
                        && row.id.as_ref() == id
                        && row.version_size.as_str() == version_size
                })
                .expect("refresh target");
            self.inner
                .refresh_downloaded_asset_metadata(
                    library,
                    id,
                    (
                        &state::MetadataCapture {
                            shared: Arc::new(newer.clone()),
                            renditions: Arc::from([]),
                        },
                        record.created_at,
                        record.added_at,
                    ),
                    true,
                    false,
                    state::METADATA_CAPTURE_REVISION,
                )
                .await?;
        }
        self.inner
            .mark_downloaded(
                library,
                id,
                version_size,
                local_path,
                local_checksum,
                download_checksum,
            )
            .await
    }

    async fn mark_failed(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
        error: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .mark_failed(library, id, version_size, error)
            .await
    }

    async fn get_pending(
        &self,
    ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
        self.inner.get_pending().await
    }

    async fn get_policy_excluded_ids_for_revalidation(
        &self,
        library: &str,
    ) -> Result<Vec<String>, state::error::StateError> {
        self.inner
            .get_policy_excluded_ids_for_revalidation(library)
            .await
    }

    async fn reset_failed(&self) -> Result<u64, state::error::StateError> {
        self.inner.reset_failed().await
    }

    async fn prepare_for_retry(
        &self,
        library: Option<&str>,
        error_retention: state::RetryErrorRetention,
    ) -> Result<(u64, u64, u64), state::error::StateError> {
        self.inner.prepare_for_retry(library, error_retention).await
    }

    async fn promote_pending_to_failed(
        &self,
        seen_since: i64,
    ) -> Result<u64, state::error::StateError> {
        self.inner.promote_pending_to_failed(seen_since).await
    }

    async fn get_downloaded_ids(
        &self,
    ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError> {
        self.inner.get_downloaded_ids().await
    }

    async fn get_soft_deleted_downloaded_ids(
        &self,
    ) -> Result<std::collections::HashSet<(String, String)>, state::error::StateError> {
        self.inner.get_soft_deleted_downloaded_ids().await
    }

    async fn get_all_known_ids(
        &self,
    ) -> Result<std::collections::HashSet<(String, String)>, state::error::StateError> {
        self.inner.get_all_known_ids().await
    }

    async fn get_downloaded_checksums(
        &self,
    ) -> Result<std::collections::HashMap<(String, String, String), String>, state::error::StateError>
    {
        self.inner.get_downloaded_checksums().await
    }

    async fn get_downloaded_local_paths(
        &self,
    ) -> Result<
        std::collections::HashMap<(String, String, String), std::path::PathBuf>,
        state::error::StateError,
    > {
        self.inner.get_downloaded_local_paths().await
    }

    async fn get_attempt_counts(
        &self,
    ) -> Result<std::collections::HashMap<(String, String), u32>, state::error::StateError> {
        self.inner.get_attempt_counts().await
    }

    async fn touch_last_seen_many(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<(), state::error::StateError> {
        self.inner.touch_last_seen_many(library, asset_ids).await
    }

    async fn upsert_asset_master_mapping(
        &self,
        library: &str,
        asset_record_name: &str,
        master_record_name: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .upsert_asset_master_mapping(library, asset_record_name, master_record_name)
            .await
    }

    async fn get_master_record_name_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
    ) -> Result<Option<String>, state::error::StateError> {
        self.inner
            .get_master_record_name_for_asset(library, asset_record_name)
            .await
    }

    async fn get_asset_record_names_for_master(
        &self,
        library: &str,
        master_record_name: &str,
    ) -> Result<Vec<String>, state::error::StateError> {
        self.inner
            .get_asset_record_names_for_master(library, master_record_name)
            .await
    }

    async fn get_asset_master_mappings(
        &self,
    ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError> {
        self.inner.get_asset_master_mappings().await
    }

    async fn get_legacy_master_state_owners(
        &self,
    ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError> {
        self.inner.get_legacy_master_state_owners().await
    }

    async fn claim_legacy_master_state_owner(
        &self,
        library: &str,
        master_record_name: &str,
        asset_record_name: &str,
    ) -> Result<bool, state::error::StateError> {
        self.inner
            .claim_legacy_master_state_owner(library, master_record_name, asset_record_name)
            .await
    }

    async fn backfill_asset_master_mappings_from_album_memberships(
        &self,
    ) -> Result<u64, state::error::StateError> {
        self.inner
            .backfill_asset_master_mappings_from_album_memberships()
            .await
    }

    async fn mark_policy_excluded(
        &self,
        library: &str,
        id: &str,
        version_size: &str,
    ) -> Result<bool, state::error::StateError> {
        self.inner
            .mark_policy_excluded(library, id, version_size)
            .await
    }

    async fn mark_soft_deleted(
        &self,
        library: &str,
        asset_id: &str,
        deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .mark_soft_deleted(library, asset_id, deleted_at)
            .await
    }

    async fn mark_hidden_at_source(
        &self,
        library: &str,
        asset_id: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner.mark_hidden_at_source(library, asset_id).await
    }
}

#[async_trait::async_trait]
impl crate::state::ReconciliationStateStore for FailingMetadataSetDb {
    async fn get_reconciliation_catalog_paths(
        &self,
    ) -> Result<Vec<crate::state::ReconciliationCatalogPath>, crate::state::error::StateError> {
        self.inner.get_reconciliation_catalog_paths().await
    }

    async fn get_reconciliation_reservations(
        &self,
    ) -> Result<Vec<crate::state::ReconciliationReservation>, crate::state::error::StateError> {
        self.inner.get_reconciliation_reservations().await
    }

    async fn reserve_reconciliation_paths(
        &self,
        reservations: &[crate::state::ReconciliationReservation],
    ) -> Result<(), crate::state::error::StateError> {
        self.inner.reserve_reconciliation_paths(reservations).await
    }
}

#[async_trait::async_trait]
impl state::DownloadContextStateStore for FailingMetadataSetDb {
    async fn get_downloaded_file_records(
        &self,
    ) -> Result<Vec<state::DownloadedFileRecord>, state::error::StateError> {
        self.inner.get_downloaded_file_records().await
    }
}

#[async_trait::async_trait]
impl state::TempFileOwnershipStore for FailingMetadataSetDb {
    async fn claim_temp_file(
        &self,
        path: &std::path::Path,
    ) -> Result<(), state::error::StateError> {
        self.inner.claim_temp_file(path).await
    }

    async fn get_owned_temp_files_before(
        &self,
        claimed_before: i64,
    ) -> Result<Vec<state::OwnedTempFile>, state::error::StateError> {
        self.inner.get_owned_temp_files_before(claimed_before).await
    }

    async fn retire_temp_files(
        &self,
        paths: &[std::path::PathBuf],
    ) -> Result<u64, state::error::StateError> {
        self.inner.retire_temp_files(paths).await
    }
}

#[async_trait::async_trait]
impl state::ReportStateStore for FailingMetadataSetDb {
    async fn get_failed(&self) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
        self.inner.get_failed().await
    }

    async fn get_failed_sample(
        &self,
        limit: u32,
    ) -> Result<(Vec<state::types::AssetRecord>, u64), state::error::StateError> {
        self.inner.get_failed_sample(limit).await
    }

    async fn get_failed_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
        self.inner.get_failed_page(offset, limit).await
    }

    async fn get_pending_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
        self.inner.get_pending_page(offset, limit).await
    }

    async fn get_summary(&self) -> Result<state::types::SyncSummary, state::error::StateError> {
        self.inner.get_summary().await
    }

    async fn get_downloaded_page(
        &self,
        offset: u64,
        limit: u32,
    ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
        self.inner.get_downloaded_page(offset, limit).await
    }

    async fn start_sync_run_at(
        &self,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<i64, state::error::StateError> {
        self.inner.start_sync_run_at(started_at).await
    }

    async fn start_sync_run(&self) -> Result<i64, state::error::StateError> {
        self.inner.start_sync_run().await
    }

    async fn complete_sync_run(
        &self,
        run_id: i64,
        stats: &state::types::SyncRunStats,
    ) -> Result<(), state::error::StateError> {
        self.inner.complete_sync_run(run_id, stats).await
    }

    async fn promote_orphaned_sync_runs(&self) -> Result<u64, state::error::StateError> {
        self.inner.promote_orphaned_sync_runs().await
    }
}

#[async_trait::async_trait]
impl state::SyncTokenStore for FailingMetadataSetDb {
    async fn get_metadata(&self, key: &str) -> Result<Option<String>, state::error::StateError> {
        if self.get_failure.is_some_and(|failure| failure.matches(key)) {
            Err(state::error::StateError::LockPoisoned(self.message.into()))
        } else {
            self.inner.get_metadata(key).await
        }
    }

    async fn set_metadata(&self, key: &str, value: &str) -> Result<(), state::error::StateError> {
        if self.failure.matches(key) {
            Err(state::error::StateError::LockPoisoned(self.message.into()))
        } else {
            self.inner.set_metadata(key, value).await
        }
    }

    async fn delete_metadata_by_prefix(
        &self,
        prefix: &str,
    ) -> Result<u64, state::error::StateError> {
        if self.delete_prefix_failure == Some(prefix) {
            Err(state::error::StateError::LockPoisoned(self.message.into()))
        } else {
            self.inner.delete_metadata_by_prefix(prefix).await
        }
    }

    async fn commit_checkpoint_transition(
        &self,
        transition: state::CheckpointTransition,
    ) -> Result<(), state::error::StateError> {
        if transition
            .metadata_updates
            .iter()
            .any(|(key, _)| self.failure.matches(key))
            || transition
                .metadata_deletes
                .iter()
                .any(|key| self.delete_prefix_failure == Some(key.as_str()))
        {
            Err(state::error::StateError::LockPoisoned(self.message.into()))
        } else {
            self.inner.commit_checkpoint_transition(transition).await
        }
    }

    async fn get_scoped_db_sync_token(
        &self,
        provider: &str,
        account: &str,
        shape_version: i64,
        scope_hash: &str,
    ) -> Result<Option<state::ScopedDbSyncToken>, state::error::StateError> {
        if self
            .get_failure
            .is_some_and(|failure| failure.matches(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY))
        {
            Err(state::error::StateError::LockPoisoned(self.message.into()))
        } else {
            self.inner
                .get_scoped_db_sync_token(provider, account, shape_version, scope_hash)
                .await
        }
    }

    async fn upsert_scoped_db_sync_token(
        &self,
        token: state::ScopedDbSyncToken,
    ) -> Result<(), state::error::StateError> {
        if self.failure.matches(SCOPED_DB_SYNC_TOKEN_FAILURE_KEY) {
            Err(state::error::StateError::LockPoisoned(self.message.into()))
        } else {
            self.inner.upsert_scoped_db_sync_token(token).await
        }
    }

    async fn delete_scoped_db_sync_tokens(&self) -> Result<u64, state::error::StateError> {
        self.inner.delete_scoped_db_sync_tokens().await
    }

    async fn begin_enum_progress(&self, zone: &str) -> Result<(), state::error::StateError> {
        self.inner.begin_enum_progress(zone).await
    }

    async fn end_enum_progress(&self, zone: &str) -> Result<(), state::error::StateError> {
        self.inner.end_enum_progress(zone).await
    }

    async fn list_interrupted_enumerations(&self) -> Result<Vec<String>, state::error::StateError> {
        self.inner.list_interrupted_enumerations().await
    }
}

#[async_trait::async_trait]
impl state::MembershipStore for FailingMetadataSetDb {
    async fn add_asset_album(
        &self,
        library: &str,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .add_asset_album(library, asset_id, album_name, source)
            .await
    }

    async fn get_all_asset_albums(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, state::error::StateError> {
        self.inner.get_all_asset_albums(library).await
    }

    async fn get_all_asset_people(
        &self,
        library: &str,
    ) -> Result<Vec<(String, String)>, state::error::StateError> {
        self.inner.get_all_asset_people(library).await
    }

    async fn get_asset_groupings(
        &self,
        library: &str,
        asset_ids: &[&str],
    ) -> Result<state::db::AssetGroupingRows, state::error::StateError> {
        self.inner.get_asset_groupings(library, asset_ids).await
    }

    async fn upsert_album_container(
        &self,
        library: &str,
        container_id: &str,
        album_name: &str,
        pass_kind: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .upsert_album_container(library, container_id, album_name, pass_kind)
            .await
    }

    async fn mark_album_container_deleted(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .mark_album_container_deleted(library, container_id)
            .await
    }

    async fn start_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        enum_config_hash: Option<&str>,
    ) -> Result<i64, state::error::StateError> {
        self.inner
            .start_album_membership_snapshot(library, container_id, enum_config_hash)
            .await
    }

    async fn add_album_membership_to_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .add_album_membership_to_snapshot(
                library,
                container_id,
                generation,
                asset_record_name,
                master_record_name,
                source,
            )
            .await
    }

    async fn upsert_album_membership_delta(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
        master_record_name: Option<&str>,
        source: &str,
    ) -> Result<bool, state::error::StateError> {
        self.inner
            .upsert_album_membership_delta(
                library,
                container_id,
                asset_record_name,
                master_record_name,
                source,
            )
            .await
    }

    async fn mark_album_membership_deleted(
        &self,
        library: &str,
        container_id: &str,
        asset_record_name: &str,
    ) -> Result<bool, state::error::StateError> {
        self.inner
            .mark_album_membership_deleted(library, container_id, asset_record_name)
            .await
    }

    async fn complete_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
        generation: i64,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .complete_album_membership_snapshot(library, container_id, generation)
            .await
    }

    async fn invalidate_album_membership_snapshot(
        &self,
        library: &str,
        container_id: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .invalidate_album_membership_snapshot(library, container_id)
            .await
    }

    async fn selected_album_containers_have_complete_snapshots(
        &self,
        library: &str,
        container_ids: &[&str],
    ) -> Result<bool, state::error::StateError> {
        self.inner
            .selected_album_containers_have_complete_snapshots(library, container_ids)
            .await
    }

    async fn get_live_selected_album_memberships_for_asset(
        &self,
        library: &str,
        asset_record_name: &str,
        selected_container_ids: &[&str],
    ) -> Result<Vec<state::db::AlbumMembershipRecord>, state::error::StateError> {
        self.inner
            .get_live_selected_album_memberships_for_asset(
                library,
                asset_record_name,
                selected_container_ids,
            )
            .await
    }
}

#[async_trait::async_trait]
impl state::MetadataRewriteStore for FailingMetadataSetDb {
    async fn record_metadata_write_failure(
        &self,
        library: &str,
        asset_id: &str,
        version_size: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .record_metadata_write_failure(library, asset_id, version_size)
            .await
    }

    async fn refresh_downloaded_asset_metadata(
        &self,
        library: &str,
        asset_id: &str,
        metadata: (
            &state::MetadataCapture,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
        ),
        mark_for_rewrite: bool,
        mark_capture_repair: bool,
        capture_revision: i64,
    ) -> Result<usize, state::error::StateError> {
        if self.fail_refresh_downloaded_metadata {
            return Err(state::error::StateError::LockPoisoned(
                self.message.to_string(),
            ));
        }
        self.inner
            .refresh_downloaded_asset_metadata(
                library,
                asset_id,
                metadata,
                mark_for_rewrite,
                mark_capture_repair,
                capture_revision,
            )
            .await
    }

    async fn get_downloaded_metadata_hashes(
        &self,
    ) -> Result<std::collections::HashMap<(String, String, String), String>, state::error::StateError>
    {
        self.inner.get_downloaded_metadata_hashes().await
    }

    async fn get_metadata_retry_markers(
        &self,
    ) -> Result<std::collections::HashSet<(String, String, String)>, state::error::StateError> {
        self.inner.get_metadata_retry_markers().await
    }

    async fn get_pending_metadata_rewrites_page(
        &self,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
        if offset == 0 {
            self.drains
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner
            .get_pending_metadata_rewrites_page(library_scope, offset, limit)
            .await
    }

    async fn get_pending_metadata_rewrites_page_for_queue(
        &self,
        queue: state::db::MetadataRewriteQueue,
        library_scope: Option<&[&str]>,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<state::db::PendingMetadataRewrite>, state::error::StateError> {
        if offset == 0 {
            self.drains
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.inner
            .get_pending_metadata_rewrites_page_for_queue(queue, library_scope, offset, limit)
            .await
    }

    async fn record_capture_repair_prepared(
        &self,
        pending: &state::db::PendingMetadataRewrite,
        output_checksum: &str,
        output_size: u64,
    ) -> Result<Option<state::db::CaptureRepairReceipt>, state::error::StateError> {
        self.inner
            .record_capture_repair_prepared(pending, output_checksum, output_size)
            .await
    }

    async fn finish_metadata_rewrite(
        &self,
        pending: &state::db::PendingMetadataRewrite,
        selected_queue: state::db::MetadataRewriteQueue,
        local_checksum: Option<&str>,
        pre_rewrite_checksum: Option<&str>,
        completion: state::db::MetadataRewriteCompletion,
    ) -> Result<bool, state::error::StateError> {
        if self.fail_metadata_checksum_write {
            return Err(state::error::StateError::LockPoisoned(self.message.into()));
        }
        self.inner
            .finish_metadata_rewrite(
                pending,
                selected_queue,
                local_checksum,
                pre_rewrite_checksum,
                completion,
            )
            .await
    }

    async fn has_downloaded_without_metadata_hash(&self) -> Result<bool, state::error::StateError> {
        self.inner.has_downloaded_without_metadata_hash().await
    }

    async fn begin_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<state::MetadataCaptureStatus, state::error::StateError> {
        self.inner
            .begin_metadata_capture_revision(library, target_revision)
            .await
    }

    async fn get_metadata_capture_candidates(
        &self,
        library: &str,
        target_revision: i64,
        limit: usize,
    ) -> Result<Vec<state::MetadataCaptureCandidate>, state::error::StateError> {
        self.inner
            .get_metadata_capture_candidates(library, target_revision, limit)
            .await
    }

    async fn record_metadata_capture_failure(
        &self,
        library: &str,
        target_revision: i64,
        error: &str,
    ) -> Result<(), state::error::StateError> {
        self.inner
            .record_metadata_capture_failure(library, target_revision, error)
            .await
    }

    async fn complete_metadata_capture_revision(
        &self,
        library: &str,
        target_revision: i64,
    ) -> Result<state::MetadataCaptureStatus, state::error::StateError> {
        self.inner
            .complete_metadata_capture_revision(library, target_revision)
            .await
    }

    async fn has_metadata_capture_work(
        &self,
        libraries: &[&str],
        target_revision: i64,
    ) -> Result<bool, state::error::StateError> {
        self.inner
            .has_metadata_capture_work(libraries, target_revision)
            .await
    }
}

// ── check_changes_database ───────────────────────────────────────
//
// Watch-mode wakes the sync loop on a fixed interval. The first thing
// each cycle does is hit the `changes/database` endpoint to ask Apple
// "anything actually changed?" If we mis-classify the response we
// either hammer Apple uselessly (no changes but proceeded) or silently
// skip a real delta (changes pending but skipped). Pin every branch.

/// Build a `LibraryState` that's just enough for `check_changes_database`.
/// The `plan` and `library` fields are unused by that function, so an
/// empty plan + a stub library is safe.
pub(super) fn make_library_state(zone: &str, sync_token_key: &str) -> LibraryState {
    let stub_session = Box::new(
        crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({"records": []})),
    );
    LibraryState {
        library: crate::icloud::photos::PhotoLibrary::new_stub(stub_session),
        pass_scope: PassScope {
            include_albums: false,
            include_smart_folders: false,
            include_unfiled: false,
        },
        zone_name: zone.to_string(),
        sync_token_key: sync_token_key.to_string(),
        plan: crate::commands::AlbumPlan { passes: Vec::new() },
        plan_is_stale: false,
        plan_needs_refresh: false,
        cross_zone_libraries: Vec::new(),
    }
}
