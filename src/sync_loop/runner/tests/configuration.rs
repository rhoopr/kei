use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::sync_cycle::{
    DownloadConfigHashOutcome, ENUM_CONFIG_HASH_KEY, EnumConfigHashOutcome,
    PENDING_DOWNLOAD_CONFIG_HASH_KEY, PENDING_ENUM_CONFIG_HASH_KEY, SYNC_TOKEN_PREFIX,
    check_and_persist_enum_config_hash, check_download_config_hash_for_cycle,
    pending_zone_token_key, run_cycle,
};
use crate::sync_loop::test_support::{
    FailingMetadataSetDb, MetadataSetFailure, RunCycleDownloadConfigOptions, album_count_response,
    full_album_page_with_download, make_empty_full_album, make_full_album_with_boxed_session,
    make_incremental_album, make_named_empty_full_album,
    make_recording_run_cycle_download_config_builder, make_run_cycle_config,
    make_run_cycle_download_config_builder, make_run_cycle_download_config_builder_with_options,
    make_run_cycle_library_state_with_album, make_run_cycle_library_state_with_passes,
    make_shared_session_for_run_cycle, make_state_db,
};
use crate::{config, download, state};

#[derive(Clone)]
struct ConfigBridgeSession {
    zone: Arc<str>,
    inventory_token: Arc<str>,
    bridge_token: Arc<str>,
}

impl ConfigBridgeSession {
    fn new(zone: &str, inventory_token: &str, bridge_token: &str) -> Self {
        Self {
            zone: Arc::from(zone),
            inventory_token: Arc::from(inventory_token),
            bridge_token: Arc::from(bridge_token),
        }
    }
}

#[async_trait::async_trait]
impl crate::icloud::photos::PhotosSession for ConfigBridgeSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<serde_json::Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(album_count_response(0));
        }
        if url.contains("/records/query?") {
            return Ok(serde_json::json!({
                "records": [],
                "syncToken": self.inventory_token.as_ref()
            }));
        }
        if url.contains("/changes/zone?") {
            return Ok(serde_json::json!({
                "zones": [{
                    "zoneID": {"zoneName": self.zone.as_ref(), "ownerRecordName": "_defaultOwner"},
                    "syncToken": self.bridge_token.as_ref(),
                    "moreComing": false,
                    "records": []
                }]
            }));
        }
        Ok(serde_json::json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn run_cycle_config_hash_stage_failure_forces_full_without_replacing_checkpoint() {
    let config = make_run_cycle_config();
    let current_hash = download::compute_config_hash(&config);
    assert_ne!(current_hash, "old-hash");

    let inner = make_state_db();
    inner
        .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
        .await
        .expect("seed enum hash");
    inner
        .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        Arc::clone(&inner),
        MetadataSetFailure::Exact(PENDING_ENUM_CONFIG_HASH_KEY),
        "simulated pending hash write failure",
    ));
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_empty_full_album("zone-tok-new"),
    );
    let states = vec![&lib_state];
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let build_download_config = make_recording_run_cycle_download_config_builder(
        download_dir.path(),
        Arc::clone(&db),
        Arc::clone(&observed_modes),
    );

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(
        result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::EnumConfigHashDrift)
    );
    let observed_modes = observed_modes.lock().expect("recorded modes lock").clone();
    assert!(
        observed_modes
            .iter()
            .all(|mode| matches!(mode, download::SyncMode::Full)),
        "config drift must not trust a surviving old incremental token in this cycle: {observed_modes:?}"
    );
    assert!(
        !result.db_sync_token_advance_safe,
        "database precheck token must not advance until config-hash invalidation can persist safely"
    );
    assert_eq!(
        inner
            .get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read enum hash")
            .as_deref(),
        Some("old-hash"),
        "new hash must not become active when reconciliation could not be staged"
    );
    assert_eq!(
        inner
            .get_metadata("sync_token:PrimarySync")
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-prev"),
        "failed reconciliation staging must preserve the last safe zone token"
    );
}

#[tokio::test]
async fn run_cycle_enum_config_drift_atomically_promotes_bridged_checkpoint() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .expect("seed active enum hash");
    db.set_metadata("sync_token:PrimarySync", "prior-token")
        .await
        .expect("seed prior token");
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(ConfigBridgeSession::new(
            "PrimarySync",
            "inventory-token",
            "bridge-token",
        )),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("inventory plus delta bridge should complete");

    assert!(result.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("bridge-token")
    );
    let expected_hash = download::compute_config_hash(&config);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn run_cycle_multi_zone_reconciliation_preserves_all_active_tokens_on_partial_failure() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-enum-hash")
        .await
        .unwrap();
    db.set_metadata("sync_token:PrimarySync", "primary-prior")
        .await
        .unwrap();
    db.set_metadata("sync_token:SharedSync-TEST", "shared-prior")
        .await
        .unwrap();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let primary = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "primary-inventory",
                "primary-bridge",
            )),
        ),
    );
    let shared = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        make_full_album_with_boxed_session(
            "SharedSync-TEST",
            Box::new(ConfigBridgeSession::new("SharedSync-TEST", "", "unused")),
        ),
    );
    let build_download_config =
        make_run_cycle_download_config_builder(download_dir.path(), Arc::clone(&db));

    let result = run_cycle(
        &[&primary, &shared],
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("partial multi-zone reconciliation should preserve active state");

    assert!(!result.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("primary-prior")
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .unwrap()
            .as_deref(),
        Some("shared-prior")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("old-enum-hash")
    );
    let expected_hash = download::compute_config_hash(&config);
    assert_eq!(
        db.get_metadata(&pending_zone_token_key(&expected_hash, "PrimarySync",))
            .await
            .unwrap()
            .as_deref(),
        Some("primary-bridge"),
        "the completed zone must retain its reconciliation checkpoint"
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key(&expected_hash, "SharedSync-TEST",))
            .await
            .unwrap(),
        None
    );

    let primary_resume = make_run_cycle_library_state_with_album(
        "PrimarySync",
        "sync_token:PrimarySync",
        make_full_album_with_boxed_session(
            "PrimarySync",
            Box::new(ConfigBridgeSession::new(
                "PrimarySync",
                "unused-inventory",
                "primary-after-resume",
            )),
        ),
    );
    let shared_retry = make_run_cycle_library_state_with_album(
        "SharedSync-TEST",
        "sync_token:SharedSync-TEST",
        make_full_album_with_boxed_session(
            "SharedSync-TEST",
            Box::new(ConfigBridgeSession::new(
                "SharedSync-TEST",
                "shared-inventory",
                "shared-bridge",
            )),
        ),
    );

    let resumed = run_cycle(
        &[&primary_resume, &shared_retry],
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("unfinished zone reconciliation should resume");

    assert!(resumed.db_sync_token_advance_safe);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("primary-after-resume")
    );
    assert_eq!(
        db.get_metadata("sync_token:SharedSync-TEST")
            .await
            .unwrap()
            .as_deref(),
        Some("shared-bridge")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some(expected_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key(&expected_hash, "PrimarySync",))
            .await
            .unwrap(),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn run_cycle_reconciliation_rejection_preserves_source_dispatch_and_state() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Clone, Debug)]
    struct ReconciliationSession {
        records: serde_json::Value,
        source_calls: Arc<AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for ReconciliationSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/records/lookup?") {
                return Ok(self.records.clone());
            }
            self.source_calls.fetch_add(1, Ordering::SeqCst);
            if url.contains("/changes/zone?") {
                return Ok(
                    serde_json::json!({"zones": [{"zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}, "syncToken": "zone-after", "moreComing": false, "records": []}]}),
                );
            }
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response(0));
            }
            Ok(serde_json::json!({"records": [], "syncToken": "zone-after"}))
        }
        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }
    let config = make_run_cycle_config();
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let db_path = old_dir.path().join("state.db");
    let db = Arc::new(state::SqliteStateDb::open(&db_path).await.unwrap());
    let old_path = old_dir.path().join("photo.jpg");
    let external = outside.path().join("photo.jpg");
    std::fs::write(&old_path, vec![0u8; 1024]).unwrap();
    std::fs::write(&external, vec![0u8; 1024]).unwrap();
    let checksum = download::file::compute_sha256(&old_path).await.unwrap();
    let provider_checksum = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let record = crate::test_helpers::TestAssetRecord::new("BLOCKED")
        .filename("photo.jpg")
        .size(1024)
        .checksum(provider_checksum)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "BLOCKED",
        "original",
        &old_path,
        &checksum,
        None,
    )
    .await
    .unwrap();
    db.upsert_asset_master_mapping("PrimarySync", "asset-BLOCKED", "BLOCKED")
        .await
        .unwrap();
    db.set_metadata(
        ENUM_CONFIG_HASH_KEY,
        &download::compute_config_hash(&config),
    )
    .await
    .unwrap();
    db.set_metadata("sync_token:PrimarySync", "zone-before")
        .await
        .unwrap();
    let old_builder = make_run_cycle_download_config_builder(old_dir.path(), db.clone());
    let new_builder = make_run_cycle_download_config_builder(new_dir.path(), db.clone());
    let old_config = old_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let new_config = new_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let old_hash = download::hash_download_config(&old_config);
    let new_hash = download::hash_download_config(&new_config);
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, &old_hash)
        .await
        .unwrap();
    let records = full_album_page_with_download(
        "PrimarySync",
        "BLOCKED",
        "unused",
        "https://p01.icloud-content.com/photo.jpg",
        1024,
        provider_checksum,
    );
    let asset = crate::icloud::photos::PhotoAsset::new(
        records["records"][0].clone(),
        records["records"][1].clone(),
    );
    let expected = download::filter::expected_paths_for(&asset, new_config.as_ref())
        .remove(0)
        .path;
    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&external, &expected).unwrap();
    let source_calls = Arc::new(AtomicUsize::new(0));
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(ReconciliationSession {
            records,
            source_calls: Arc::clone(&source_calls),
        }),
    );
    let lib =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let (_session_dir, session) = make_shared_session_for_run_cycle().await;
    for _ in 0..2 {
        let result = run_cycle(
            &[&lib],
            &config,
            Some(db.as_ref()),
            false,
            &new_builder,
            download::DownloadControls::download_hidden(),
            &session,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result.failed_count, 1);
        assert!(!result.db_sync_token_advance_safe);
        assert_eq!(source_calls.load(Ordering::SeqCst), 0);
        let reopened = state::SqliteStateDb::open(&db_path).await.unwrap();
        let rows = reopened.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows[0].local_path.as_deref(), Some(old_path.as_path()));
        assert_eq!(rows[0].local_checksum.as_deref(), Some(checksum.as_str()));
        assert_eq!(
            reopened
                .get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("zone-before")
        );
        assert_eq!(
            reopened
                .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(old_hash.as_str())
        );
        assert_eq!(
            reopened
                .get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
                .await
                .unwrap()
                .as_deref(),
            Some(new_hash.as_str())
        );
        assert_eq!(std::fs::read_link(&expected).unwrap(), external);
        assert_eq!(std::fs::read(&external).unwrap(), vec![0u8; 1024]);
        assert_eq!(std::fs::read(&old_path).unwrap(), vec![0u8; 1024]);
    }
}

#[tokio::test]
async fn run_cycle_download_config_hash_drift_keeps_source_incremental() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    let old_download_dir = tempfile::tempdir().expect("old download tempdir");
    let new_download_dir = tempfile::tempdir().expect("new download tempdir");

    let old_build_download_config =
        make_run_cycle_download_config_builder(old_download_dir.path(), Arc::clone(&db));
    let old_download_config = old_build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let old_hash = download::hash_download_config(&old_download_config);
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, &old_hash)
        .await
        .expect("seed old download hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "zone-tok-prev")
        .await
        .expect("seed zone token");

    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let lib_state = make_run_cycle_library_state_with_album(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        make_incremental_album("zone-tok-new"),
    );
    let states = vec![&lib_state];
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let build_download_config = make_recording_run_cycle_download_config_builder(
        new_download_dir.path(),
        Arc::clone(&db),
        Arc::clone(&observed_modes),
    );
    let new_hash = download::hash_download_config(&build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    ));

    let result = run_cycle(
        &states,
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run cycle");

    assert_eq!(result.failed_count, 0);
    assert_eq!(result.stats.full_enumeration_reason, None);
    let observed_modes = observed_modes.lock().expect("recorded modes lock").clone();
    assert!(
        matches!(
            observed_modes.last(),
            Some(download::SyncMode::Incremental { zone_sync_token })
                if zone_sync_token == "zone-tok-prev"
        ),
        "path-only drift must preserve incremental source tracking: {observed_modes:?}"
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read zone token")
            .as_deref(),
        Some("zone-tok-new"),
        "the incremental source pass should refresh the selected zone token after success"
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read active download hash")
            .as_deref(),
        Some(new_hash.as_str()),
        "an empty catalog completes local path reconciliation immediately"
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read pending download hash"),
        None
    );
}

#[tokio::test]
async fn run_cycle_date_bound_expansion_preserves_existing_media_and_reaches_steady_state() {
    use base64::Engine as _;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    #[derive(Clone, Debug)]
    struct DateBoundSession {
        records: Arc<Vec<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl crate::icloud::photos::PhotosSession for DateBoundSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<serde_json::Value> {
            if url.contains("/internal/records/query/batch") {
                return Ok(album_count_response((self.records.len() / 2) as u64));
            }
            if url.contains("/records/lookup?") {
                return Ok(serde_json::json!({"records": self.records.as_ref()}));
            }
            if url.contains("/records/query?") {
                let request: serde_json::Value = serde_json::from_str(&body)?;
                let offset = request["query"]["filterBy"]
                    .as_array()
                    .and_then(|filters| {
                        filters.iter().find_map(|filter| {
                            (filter["fieldName"] == "startRank")
                                .then(|| filter["fieldValue"]["value"].as_u64())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                let limit = request["resultsLimit"].as_u64().unwrap_or(0) / 2;
                let records: Vec<_> = self
                    .records
                    .chunks_exact(2)
                    .skip(offset as usize)
                    .take(limit as usize)
                    .flatten()
                    .cloned()
                    .collect();
                return Ok(serde_json::json!({
                    "records": records,
                    "syncToken": "zone-token-inventory"
                }));
            }
            if url.contains("/changes/zone?") {
                return Ok(serde_json::json!({
                    "zones": [{
                        "zoneID": {
                            "zoneName": "PrimarySync",
                            "ownerRecordName": "_defaultOwner"
                        },
                        "syncToken": "zone-token-incremental",
                        "moreComing": false,
                        "records": []
                    }]
                }));
            }
            Ok(serde_json::json!({"records": []}))
        }

        fn clone_box(&self) -> Box<dyn crate::icloud::photos::PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn asset_records(
        record_name: &str,
        filename: &str,
        created: chrono::DateTime<chrono::Utc>,
        download_url: &str,
        body: &[u8],
    ) -> Vec<serde_json::Value> {
        let checksum = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body));
        let mut page = full_album_page_with_download(
            "PrimarySync",
            record_name,
            "unused",
            download_url,
            body.len() as u64,
            &checksum,
        );
        page["records"][0]["fields"]["filenameEnc"]["value"] =
            serde_json::json!(base64::engine::general_purpose::STANDARD.encode(filename));
        page["records"][1]["fields"]["assetDate"]["value"] =
            serde_json::json!(created.timestamp_millis());
        page["records"][1]["fields"]["addedDate"]["value"] =
            serde_json::json!(created.timestamp_millis());
        page["records"].as_array().expect("asset records").clone()
    }

    fn snapshot_files(
        root: &std::path::Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn visit(
            root: &std::path::Path,
            dir: &std::path::Path,
            files: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(dir).expect("read media directory") {
                let entry = entry.expect("read media entry");
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root)
                            .expect("relative media path")
                            .to_path_buf(),
                        std::fs::read(path).expect("read media file"),
                    );
                }
            }
        }

        let mut files = std::collections::BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    let server = crate::start_wiremock_or_skip!();
    let january_body = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    let april_body = &[
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00,
        0x02, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
    ];
    Mock::given(method("GET"))
        .and(path("/january.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(january_body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/april.jpg"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(april_body)
                .insert_header("content-type", "image/jpeg"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let january_created = chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc();
    let april_created = chrono::NaiveDate::from_ymd_opt(2026, 4, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc();
    let mut records = asset_records(
        "APRIL",
        "april.jpg",
        april_created,
        &format!("{}/april.jpg", server.uri()),
        april_body,
    );
    records.extend(asset_records(
        "JANUARY",
        "january.jpg",
        january_created,
        &format!("{}/january.jpg", server.uri()),
        january_body,
    ));
    let album = make_full_album_with_boxed_session(
        "PrimarySync",
        Box::new(DateBoundSession {
            records: Arc::new(records),
        }),
    );
    let lib_state =
        make_run_cycle_library_state_with_album("PrimarySync", "sync_token:PrimarySync", album);
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let old_lower_bound = config::CreatedDateFilter::CaptureDate(
        chrono::NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(),
    );
    let new_lower_bound = config::CreatedDateFilter::CaptureDate(
        chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
    );
    let upper_bound = config::CreatedDateFilter::CaptureDate(
        chrono::NaiveDate::from_ymd_opt(2026, 5, 31).unwrap(),
    );
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;

    let mut config = make_run_cycle_config();
    config.filters.skip_created_before = Some(old_lower_bound);
    config.filters.skip_created_after = Some(upper_bound);
    let old_enum_hash = download::compute_config_hash(&config);
    let old_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            skip_created_before: Some(old_lower_bound),
            skip_created_after: Some(upper_bound),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let first_result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &old_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run initial date-bound cycle");
    assert_eq!(
        first_result.stats.downloaded, 1,
        "{:#?}",
        first_result.stats
    );

    let first_snapshot = snapshot_files(download_dir.path());
    assert_eq!(first_snapshot.len(), 1);
    let april_record = db
        .get_downloaded_page(0, 10)
        .await
        .expect("read initial downloaded rows")
        .into_iter()
        .find(|record| record.id.as_ref() == "asset-APRIL")
        .expect("April downloaded row");
    let april_path = april_record.local_path.expect("April local path");
    assert_eq!(tokio::fs::read(&april_path).await.unwrap(), april_body);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read initial enumeration hash")
            .as_deref(),
        Some(old_enum_hash.as_str())
    );
    db.set_metadata(
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        "zone-token-old-bound",
    )
    .await
    .expect("seed prior provider checkpoint");
    let checkpoint_before_expansion = db
        .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
        .await
        .expect("read initial provider checkpoint");
    assert_eq!(
        checkpoint_before_expansion.as_deref(),
        Some("zone-token-old-bound")
    );

    let old_download_config = old_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    db.set_metadata(
        download::DOWNLOAD_CONFIG_HASH_KEY,
        &download::hash_legacy_download_config(&old_download_config),
    )
    .await
    .expect("seed pre-fix path hash");
    drop(old_builder);

    config.filters.skip_created_before = Some(new_lower_bound);
    let current_enum_hash = download::compute_config_hash(&config);
    let current_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        RunCycleDownloadConfigOptions {
            skip_created_before: Some(new_lower_bound),
            skip_created_after: Some(upper_bound),
            ..RunCycleDownloadConfigOptions::default()
        },
    );
    let current_download_config = current_builder(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let current_download_hash = download::hash_download_config(&current_download_config);
    assert_ne!(
        download::hash_legacy_download_config(&old_download_config),
        download::hash_legacy_download_config(&current_download_config)
    );

    let second_result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &current_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run expanded date-bound cycle");
    assert_eq!(second_result.stats.downloaded, 1);
    assert_ne!(
        second_result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::DownloadConfigHashDrift)
    );

    let second_snapshot = snapshot_files(download_dir.path());
    assert_eq!(second_snapshot.len(), 2);
    assert!(
        first_snapshot
            .iter()
            .all(|entry| second_snapshot.get(entry.0) == Some(entry.1))
    );
    assert_eq!(tokio::fs::read(&april_path).await.unwrap(), april_body);
    let downloaded = db
        .get_downloaded_page(0, 10)
        .await
        .expect("read expanded downloaded rows");
    assert_eq!(downloaded.len(), 2);
    assert_eq!(
        downloaded
            .iter()
            .find(|record| record.id.as_ref() == "asset-APRIL")
            .and_then(|record| record.local_path.as_deref()),
        Some(april_path.as_path())
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read current path hash")
            .as_deref(),
        Some(current_download_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read pending path hash"),
        None
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read provider checkpoint")
            .as_deref(),
        Some("zone-token-incremental")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read enumeration hash")
            .as_deref(),
        Some(current_enum_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read pending enumeration hash"),
        None
    );

    let third_result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &current_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("run steady-state date-bound cycle");
    assert_eq!(third_result.stats.downloaded, 0);
    assert_eq!(snapshot_files(download_dir.path()), second_snapshot);
    assert_ne!(
        third_result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::DownloadConfigHashDrift)
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read steady path hash")
            .as_deref(),
        Some(current_download_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .expect("read steady pending path hash"),
        None
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read steady provider checkpoint")
            .as_deref(),
        Some("zone-token-incremental")
    );
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read steady enumeration hash")
            .as_deref(),
        Some(current_enum_hash.as_str())
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read steady pending enumeration hash"),
        None
    );
}

#[tokio::test]
async fn run_cycle_multi_pass_persists_base_download_config_hash() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let passes = vec![
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: make_named_empty_full_album("PrimarySync", "Vacation", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Album,
            album: make_named_empty_full_album("PrimarySync", "Family", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
        crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album: make_named_empty_full_album("PrimarySync", "", "zone-tok"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        },
    ];
    let lib_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        passes.clone(),
    );
    let options = RunCycleDownloadConfigOptions {
        per_pass_paths: true,
        ..RunCycleDownloadConfigOptions::default()
    };
    let build_download_config = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        options,
    );
    let base_config = build_download_config(
        download::SyncMode::Full,
        Arc::new(rustc_hash::FxHashSet::default()),
        Arc::new(download::AssetGroupings::default()),
        Arc::from("PrimarySync"),
    );
    let base_hash = download::hash_download_config(&base_config);
    let pass_hashes: Vec<String> = passes
        .iter()
        .map(|pass| download::hash_download_config(&base_config.with_pass(pass)))
        .collect();

    let result = run_cycle(
        &[&lib_state],
        &config,
        Some(db.as_ref()),
        false,
        &build_download_config,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("multi-pass cycle should complete");

    assert_eq!(result.failed_count, 0);
    let stored_hash = db
        .get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
        .await
        .expect("read stored download hash")
        .expect("download hash should be persisted");
    assert_eq!(
        stored_hash, base_hash,
        "global download config hash must be the base run-level hash"
    );
    assert!(
        pass_hashes.iter().take(2).all(|hash| hash != &stored_hash),
        "album-expanded folder templates must not overwrite the global hash: {pass_hashes:?}"
    );
}

#[tokio::test]
async fn unchanged_multi_pass_second_cycle_is_not_download_config_hash_drift() {
    let config = make_run_cycle_config();
    let db = make_state_db();
    let download_dir = tempfile::tempdir().expect("download tempdir");
    let (_session_dir, shared_session) = make_shared_session_for_run_cycle().await;
    let options = RunCycleDownloadConfigOptions {
        per_pass_paths: true,
        ..RunCycleDownloadConfigOptions::default()
    };
    let first_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        vec![
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Album,
                album: make_named_empty_full_album("PrimarySync", "Vacation", "zone-tok-1"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
            crate::commands::AlbumPass {
                kind: crate::commands::PassKind::Unfiled,
                album: make_named_empty_full_album("PrimarySync", "", "zone-tok-1"),
                exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
            },
        ],
    );
    let first_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        options,
    );
    let first_result = run_cycle(
        &[&first_state],
        &config,
        Some(db.as_ref()),
        false,
        &first_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("first multi-pass cycle should complete");
    assert_eq!(first_result.failed_count, 0);

    let second_state = make_run_cycle_library_state_with_passes(
        "PrimarySync",
        &format!("{SYNC_TOKEN_PREFIX}PrimarySync"),
        vec![crate::commands::AlbumPass {
            kind: crate::commands::PassKind::Unfiled,
            album: make_empty_full_album("zone-tok-2"),
            exclude_ids: Arc::new(rustc_hash::FxHashSet::default()),
        }],
    );
    let observed_modes = Arc::new(std::sync::Mutex::new(Vec::<download::SyncMode>::new()));
    let base_builder = make_run_cycle_download_config_builder_with_options(
        download_dir.path(),
        Arc::clone(&db),
        options,
    );
    let second_builder = {
        let observed_modes = Arc::clone(&observed_modes);
        move |sync_mode: download::SyncMode,
              exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
              asset_groupings: Arc<download::AssetGroupings>,
              library: Arc<str>| {
            observed_modes
                .lock()
                .expect("recorded modes lock")
                .push(sync_mode.clone());
            base_builder(sync_mode, exclude_asset_ids, asset_groupings, library)
        }
    };
    let second_result = run_cycle(
        &[&second_state],
        &config,
        Some(db.as_ref()),
        false,
        &second_builder,
        download::DownloadControls::download_hidden(),
        &shared_session,
        &CancellationToken::new(),
    )
    .await
    .expect("second cycle should complete");

    assert_ne!(
        second_result.stats.full_enumeration_reason,
        Some(download::FullEnumerationReason::DownloadConfigHashDrift),
        "pass-expanded hash from first run must not force path-drift reconciliation"
    );
    assert!(
        observed_modes
            .lock()
            .expect("recorded modes lock")
            .iter()
            .any(|mode| matches!(mode, download::SyncMode::Incremental { .. })),
        "unchanged config with a stored token should reach the incremental decision path"
    );
}

// ── check_and_persist_enum_config_hash ─────────────────────────────

#[tokio::test]
async fn enum_config_hash_initial_persists_only() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-abc")
        .await
        .expect("set token");

    let outcome = check_and_persist_enum_config_hash(&db, "hash-1").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Initial);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("hash-1"),
    );
    // First run must NOT clear pre-existing sync tokens.
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-abc"),
    );
}

#[tokio::test]
async fn enum_config_hash_drift_stages_reconciliation_and_preserves_tokens() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
        .await
        .expect("seed old hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-primary")
        .await
        .expect("seed primary token");
    db.set_metadata(
        &format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"),
        "tok-shared",
    )
    .await
    .expect("seed shared token");

    let outcome = check_and_persist_enum_config_hash(&db, "new-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Changed);
    assert_eq!(
        db.get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("old-hash"),
    );
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("new-hash")
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-primary")
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}SharedSync-AAAA1111"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-shared")
    );
}

#[tokio::test]
async fn enum_config_hash_stage_failure_keeps_old_hash_and_tokens() {
    let inner = make_state_db();
    inner
        .set_metadata(ENUM_CONFIG_HASH_KEY, "old-hash")
        .await
        .expect("seed enum hash");
    inner
        .set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "old-zone-token")
        .await
        .expect("seed zone token");
    let db: Arc<dyn download::DownloadStore> = Arc::new(FailingMetadataSetDb::new(
        Arc::clone(&inner),
        MetadataSetFailure::Exact(PENDING_ENUM_CONFIG_HASH_KEY),
        "simulated pending hash write failure",
    ));

    let outcome = check_and_persist_enum_config_hash(db.as_ref(), "new-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::ChangedTokenPurgeFailed);
    assert_eq!(
        inner
            .get_metadata(ENUM_CONFIG_HASH_KEY)
            .await
            .expect("read enum hash")
            .as_deref(),
        Some("old-hash"),
        "new hash must not become active before reconciliation"
    );
    assert_eq!(
        inner
            .get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .expect("read zone token")
            .as_deref(),
        Some("old-zone-token"),
        "the last safe token must survive a failed reconciliation-stage write"
    );
}

#[tokio::test]
async fn enum_config_hash_unchanged_is_noop() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "stable-hash")
        .await
        .expect("seed stable hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .expect("seed token");

    let outcome = check_and_persist_enum_config_hash(&db, "stable-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-keep"),
    );
}

#[tokio::test]
async fn enum_config_revert_clears_pending_reconciliation() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "active-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_ENUM_CONFIG_HASH_KEY, "abandoned-hash")
        .await
        .unwrap();
    db.set_metadata(
        &pending_zone_token_key("abandoned-hash", "PrimarySync"),
        "candidate-token",
    )
    .await
    .unwrap();

    let outcome = check_and_persist_enum_config_hash(&db, "active-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY).await.unwrap(),
        None
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key("abandoned-hash", "PrimarySync"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn enum_config_new_drift_discards_superseded_zone_candidates() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(ENUM_CONFIG_HASH_KEY, "active-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_ENUM_CONFIG_HASH_KEY, "superseded-hash")
        .await
        .unwrap();
    db.set_metadata(
        &pending_zone_token_key("superseded-hash", "PrimarySync"),
        "candidate-token",
    )
    .await
    .unwrap();

    let outcome = check_and_persist_enum_config_hash(&db, "replacement-hash").await;

    assert_eq!(outcome, EnumConfigHashOutcome::Changed);
    assert_eq!(
        db.get_metadata(PENDING_ENUM_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("replacement-hash")
    );
    assert_eq!(
        db.get_metadata(&pending_zone_token_key("superseded-hash", "PrimarySync"))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn download_config_hash_drift_stages_reconciliation_without_clearing_token() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "old-download-hash")
        .await
        .expect("seed old path hash");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .expect("seed token");

    let outcome = check_download_config_hash_for_cycle(
        &db,
        "current-download-hash",
        "legacy-current-download-hash",
    )
    .await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Changed);
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap(),
        Some("tok-keep".to_string()),
        "path drift must preserve the active provider cursor"
    );
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        Some("old-download-hash".to_string()),
        "the active path hash changes only after reconciliation"
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        Some("current-download-hash".to_string()),
        "the candidate path hash must be durable while reconciliation runs"
    );
}

#[tokio::test]
async fn download_config_hash_initial_persists_current_hash() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .expect("seed token");

    let outcome = check_download_config_hash_for_cycle(
        &db,
        "current-download-hash",
        "legacy-current-download-hash",
    )
    .await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("current-download-hash"),
        "first observation should persist the stable run-level path hash"
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-keep"),
        "initial hash persistence must not purge existing tokens"
    );
}

#[tokio::test]
async fn download_config_revert_clears_pending_reconciliation() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "active-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, "abandoned-hash")
        .await
        .unwrap();

    let outcome =
        check_download_config_hash_for_cycle(&db, "active-hash", "legacy-active-hash").await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn download_config_legacy_hash_migrates_without_reconciliation() {
    let db = state::SqliteStateDb::open_in_memory().expect("open in-memory state DB");
    db.set_metadata(download::DOWNLOAD_CONFIG_HASH_KEY, "legacy-current-hash")
        .await
        .unwrap();
    db.set_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY, "abandoned-hash")
        .await
        .unwrap();
    db.set_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"), "tok-keep")
        .await
        .unwrap();

    let outcome =
        check_download_config_hash_for_cycle(&db, "current-path-hash", "legacy-current-hash").await;

    assert_eq!(outcome, DownloadConfigHashOutcome::Unchanged);
    assert_eq!(
        db.get_metadata(download::DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap()
            .as_deref(),
        Some("current-path-hash")
    );
    assert_eq!(
        db.get_metadata(PENDING_DOWNLOAD_CONFIG_HASH_KEY)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db.get_metadata(&format!("{SYNC_TOKEN_PREFIX}PrimarySync"))
            .await
            .unwrap()
            .as_deref(),
        Some("tok-keep")
    );
}
