//! Shared fixtures for orchestration production-path tests.

use crate::state::DownloadStateStore;
use crate::state::MembershipStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::commands::{AlbumPass, PassKind};
use crate::download::filter;
use crate::download::filter::DownloadTask;
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoAsset, PhotosSession};
use crate::retry::RetryConfig;
use crate::state::{SqliteStateDb, VersionSizeKey};
use crate::test_helpers::{TestAssetRecord, mock_photo_records_for_zone_with_filename};

use super::config::DownloadConfig;
use super::models::SyncMode;

pub(super) fn test_config() -> DownloadConfig {
    DownloadConfig::test_default()
}

pub(super) fn fixture_owner() -> crate::state::db::account::AccountOwner {
    crate::state::db::account::AccountOwner::authenticated(
        "bundled-media@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"bundled-media"}})).unwrap(),
    )
    .unwrap()
}

#[derive(Clone)]
pub(super) struct SelectedMediaSession(pub Arc<Vec<Value>>);

#[async_trait::async_trait]
impl PhotosSession for SelectedMediaSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        if url.contains("/internal/records/query/batch?") {
            let count = self
                .0
                .iter()
                .filter(|record| record["recordType"] == "CPLMaster")
                .count();
            return Ok(
                json!({"batch":request["batch"].as_array().unwrap().iter().map(|_|json!({"records":[{"fields":{"itemCount":{"value":count}}}]})).collect::<Vec<_>>()}),
            );
        }
        if url.contains("/changes/zone?") {
            assert_eq!(request["zones"][0]["zoneID"]["zoneName"], "PrimarySync");
            assert_eq!(
                request["zones"][0]["zoneID"]["ownerRecordName"],
                "_defaultOwner"
            );
            return Ok(changes_zone_response(
                self.0.as_ref().clone(),
                "fixture-token",
            ));
        }
        assert_eq!(request["zoneID"]["zoneName"], "PrimarySync");
        assert_eq!(request["zoneID"]["ownerRecordName"], "_defaultOwner");
        if url.contains("/records/lookup?") {
            let records: Vec<_> = request["records"].as_array().unwrap().iter().map(|wanted| {
                self.0.iter().find(|record| record["recordName"] == wanted["recordName"])
                    .cloned().unwrap_or_else(|| json!({"recordName":wanted["recordName"],"serverErrorCode":"NOT_FOUND"}))
            }).collect();
            return Ok(json!({"records":records}));
        }
        anyhow::ensure!(
            url.contains("/records/query?"),
            "Unexpected fixture request"
        );
        let start = request["query"]["filterBy"]
            .as_array()
            .unwrap()
            .iter()
            .find(|filter| filter["fieldName"] == "startRank")
            .unwrap()["fieldValue"]["value"]
            .as_u64()
            .unwrap();
        let records = if start == 0 {
            self.0.as_ref().clone()
        } else {
            Vec::new()
        };
        Ok(json!({"records":records,"syncToken":"fixture-token"}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

pub(super) async fn open_fixture_db(path: &Path) -> Arc<SqliteStateDb> {
    Arc::new(
        SqliteStateDb::open_owned(path, &fixture_owner())
            .await
            .unwrap(),
    )
}

/// Establish real current-selection coverage before exercising a later delta.
pub(super) async fn initialize_empty_selection(db: &Arc<SqliteStateDb>, config: &DownloadConfig) {
    #[derive(Clone)]
    struct EmptySelectionSession;
    #[async_trait::async_trait]
    impl PhotosSession for EmptySelectionSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/internal/records/query/batch?") {
                return Ok(json!({"batch":[{"records":[{"fields":{"itemCount":{"value":0}}}]}]}));
            }
            anyhow::ensure!(
                url.contains("/records/query?"),
                "unexpected initial selection route"
            );
            Ok(json!({"records":[],"syncToken":"fixture-initial-cursor"}))
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }
    let pass = selected_pass_with_session(db, "", Box::new(EmptySelectionSession));
    let mut initial = config.clone();
    initial.sync_mode = SyncMode::Full;
    let result = super::dispatch::download_photos_with_sync(
        &reqwest::Client::new(),
        &[pass],
        Arc::new(initial),
        super::models::DownloadControls::download_hidden(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, super::models::DownloadOutcome::Success),
        "{result:?}"
    );
    assert_eq!(result.sync_token.as_deref(), Some("fixture-initial-cursor"));
}

pub(super) fn selected_pass_with_session(
    db: &Arc<SqliteStateDb>,
    name: &str,
    session: Box<dyn PhotosSession>,
) -> AlbumPass {
    selected_pass_with_retry_config(db, name, RetryConfig::default(), session)
}

pub(super) fn selected_smart_pass_with_session(
    db: &Arc<SqliteStateDb>,
    name: &str,
    session: Box<dyn PhotosSession>,
) -> AlbumPass {
    let (_, definition) = crate::icloud::photos::smart_folders::smart_folders()
        .into_iter()
        .find(|(candidate, _)| *candidate == name)
        .unwrap();
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://fixture.invalid"),
            name: Arc::from(name),
            list_type: Arc::from(definition.list_type),
            obj_type: Arc::from(definition.obj_type),
            query_filter: definition.query_filter,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        session,
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), fixture_owner(), "com"),
        Arc::from("private"),
    );
    AlbumPass {
        kind: PassKind::SmartFolder,
        album,
        exclude_ids: Arc::new(FxHashSet::default()),
    }
}

pub(super) fn selected_pass_with_retry_config(
    db: &Arc<SqliteStateDb>,
    name: &str,
    retry_config: RetryConfig,
    session: Box<dyn PhotosSession>,
) -> AlbumPass {
    let mut album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://fixture.invalid"), name: Arc::from(name),
            list_type: Arc::from(if name.is_empty() { "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted" } else { "CPLContainerRelationLiveByAssetDate" }),
            obj_type: if name.is_empty() { Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted") } else { Arc::from(format!("CPLContainerRelationNotDeletedByAssetDate:{name}")) },
            query_filter: (!name.is_empty()).then(|| Arc::new(json!([{"fieldName":"parentId","comparator":"EQUALS","fieldValue":{"type":"STRING","value":name}}]))),
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config, container_id: (!name.is_empty()).then(|| Arc::from(name)), cross_zone_sources: Vec::new(),
        }, session,
    );
    album.set_shadow_capture(
        crate::icloud::photos::inbox::ShadowCapture::new(db.clone(), fixture_owner(), "com"),
        Arc::from("private"),
    );
    AlbumPass {
        kind: if name.is_empty() {
            PassKind::Unfiled
        } else {
            PassKind::Album
        },
        album,
        exclude_ids: Arc::new(FxHashSet::default()),
    }
}

pub(super) fn retry_test_task(
    asset_id: &str,
    version_size: VersionSizeKey,
    path: &str,
) -> DownloadTask {
    DownloadTask {
        url: format!("https://p01.icloud-content.com/{asset_id}").into(),
        download_path: Path::new("/tmp/codex/kei/retry-tests").join(path),
        replacement_fingerprint: None,
        pending_cross_parent_root: None,
        checksum: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
        asset_id: Arc::from(asset_id),
        asset_record_name: Arc::from(asset_id),
        library: Arc::from("PrimarySync"),
        metadata: Arc::new(filter::MetadataPayload::default()),
        size: 1024,
        created_local: chrono::Local::now().fixed_offset(),
        version_size,
        media_type: crate::state::MediaType::Photo,
    }
}

pub(super) fn changes_album(name: &str, session: impl PhotosSession + 'static) -> PhotoAlbum {
    changes_album_with_container(name, None, session)
}

pub(super) fn changes_album_with_container(
    name: &str,
    container_id: Option<&str>,
    session: impl PhotosSession + 'static,
) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            retry_config: RetryConfig::default(),
            container_id: container_id.map(Arc::from),
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    )
}

pub(super) fn unused_unfiled_changes_pass() -> AlbumPass {
    AlbumPass {
        kind: PassKind::Unfiled,
        album: changes_album(
            "",
            changes_zone_session(Arc::new(AtomicUsize::new(0)), Vec::new()),
        ),
        exclude_ids: Arc::new(FxHashSet::default()),
    }
}

#[derive(Clone)]
pub(super) struct CountingChangesZoneSession {
    pub(super) changes_zone_calls: Arc<AtomicUsize>,
    pub(super) count_query_calls: Arc<AtomicUsize>,
    pub(super) records_query_calls: Arc<AtomicUsize>,
    pub(super) records: Vec<Value>,
    pub(super) query_page: Value,
    pub(super) asset_count: u64,
}

pub(super) fn changes_zone_session(
    changes_zone_calls: Arc<AtomicUsize>,
    records: Vec<Value>,
) -> CountingChangesZoneSession {
    // These fixtures model their paired live delta records as the current
    // inventory too. Selection queries use a distinct rank token; tombstones
    // and relationship/unknown records remain delta-only evidence.
    let current: Vec<_> = records
        .iter()
        .filter(|record| {
            matches!(
                record["recordType"].as_str(),
                Some("CPLMaster" | "CPLAsset")
            ) && record["deleted"] != true
        })
        .cloned()
        .collect();
    let count = current
        .iter()
        .filter(|record| record["recordType"] == "CPLAsset")
        .count() as u64;
    changes_zone_session_with_query_page(
        changes_zone_calls,
        records,
        json!({"records": current, "syncToken": "rank-selection-token"}),
        count,
    )
}

pub(super) fn changes_zone_session_with_query_page(
    changes_zone_calls: Arc<AtomicUsize>,
    records: Vec<Value>,
    query_page: Value,
    asset_count: u64,
) -> CountingChangesZoneSession {
    CountingChangesZoneSession {
        changes_zone_calls,
        count_query_calls: Arc::new(AtomicUsize::new(0)),
        records_query_calls: Arc::new(AtomicUsize::new(0)),
        records,
        query_page,
        asset_count,
    }
}

impl CountingChangesZoneSession {
    pub(super) fn count_query_count(&self) -> usize {
        self.count_query_calls.load(Ordering::SeqCst)
    }

    pub(super) fn records_query_count(&self) -> usize {
        self.records_query_calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl PhotosSession for CountingChangesZoneSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            self.changes_zone_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "zone-token-next",
                    "moreComing": false,
                    "records": self.records.clone(),
                }]
            }));
        }

        if url.contains("/internal/records/query/batch") {
            self.count_query_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": self.asset_count}}}]}]
            }));
        }

        if url.contains("/records/lookup?") || url.contains("/records/query?") {
            self.records_query_calls.fetch_add(1, Ordering::SeqCst);
            if url.contains("/records/query?") {
                let request: Value = serde_json::from_str(&body)?;
                let offset = request["query"]["filterBy"]
                    .as_array()
                    .and_then(|filters| {
                        filters
                            .iter()
                            .find(|filter| filter["fieldName"] == "startRank")
                    })
                    .and_then(|filter| filter["fieldValue"]["value"].as_u64())
                    .unwrap_or(0);
                if offset > 0 {
                    let mut page = self.query_page.clone();
                    page["records"] = json!([]);
                    return Ok(page);
                }
            }
            return Ok(self.query_page.clone());
        }

        Ok(json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

pub(super) fn incremental_test_config(dir: &TempDir) -> DownloadConfig {
    let mut config = test_config();
    config.directory = Arc::from(dir.path());
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: "zone-token-prev".to_string(),
    };
    config
}

pub(super) fn incremental_photo_records(record_name: &str) -> Vec<Value> {
    vec![
        json!({
            "recordName": record_name,
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": "changed.jpg", "type": "STRING"},
                "resOriginalRes": {
                    "value": {
                        "downloadURL": "https://p01.icloud-content.com/changed.jpg",
                        "size": 1024,
                        "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                    }
                },
                "resOriginalWidth": {"value": 100, "type": "INT64"},
                "resOriginalHeight": {"value": 100, "type": "INT64"},
                "resOriginalFileType": {"value": "public.jpeg"},
                "itemType": {"value": "public.jpeg"},
                "adjustmentRenderType": {"value": 0, "type": "INT64"}
            },
            "recordChangeTag": "ct1"
        }),
        json!({
            "recordName": format!("asset-{record_name}"),
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                    "value": {
                        "recordName": record_name,
                        "zoneID": {"zoneName": "PrimarySync"}
                    },
                    "type": "REFERENCE"
                },
                "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
            },
            "recordChangeTag": "ct2"
        }),
    ]
}

pub(super) fn incremental_photo_records_with_favorite(
    record_name: &str,
    is_favorite: bool,
) -> Vec<Value> {
    let mut records = incremental_photo_records(record_name);
    records[1]["fields"]["isFavorite"] = json!({"value": i64::from(is_favorite), "type": "INT64"});
    records
}

pub(super) async fn seed_downloaded_metadata_asset(
    db: &SqliteStateDb,
    config: &DownloadConfig,
    pass: &AlbumPass,
    asset: &PhotoAsset,
) -> PathBuf {
    let pass_config = config.with_pass(pass);
    let expected_paths = filter::expected_paths_for(asset, &pass_config);
    for expected in &expected_paths {
        tokio::fs::create_dir_all(expected.path.parent().expect("path has parent"))
            .await
            .expect("create expected parent");
        tokio::fs::write(
            &expected.path,
            vec![0u8; usize::try_from(expected.size).expect("test asset size fits usize")],
        )
        .await
        .expect("seed existing media");
        let filename = expected
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("path has UTF-8 filename");
        let record = TestAssetRecord::new(asset.state_id())
            .library(asset.source_zone().unwrap_or(&pass_config.library))
            .checksum(&expected.checksum)
            .filename(filename)
            .created_at(asset.created())
            .added_at(asset.added_date())
            .size(expected.size)
            .version_size(expected.version_size)
            .metadata(
                (*filter::metadata_for_selected_version(
                    asset,
                    &pass_config,
                    expected.version_size,
                ))
                .clone(),
            )
            .build();
        db.upsert_seen(&record).await.expect("seed state row");
        db.mark_downloaded(
            asset.source_zone().unwrap_or(&pass_config.library),
            asset.state_id(),
            expected.version_size.as_str(),
            &expected.path,
            "seeded-local-sha256",
            None,
        )
        .await
        .expect("mark seeded media downloaded");
    }
    expected_paths
        .into_iter()
        .next()
        .expect("asset should have an expected path")
        .path
}

pub(super) fn incremental_photo_records_with_url(
    record_name: &str,
    filename: &str,
    download_url: &str,
    size: u64,
) -> Vec<Value> {
    let mut records =
        mock_photo_records_for_zone_with_filename(record_name, "PrimarySync", filename);
    records[0]["fields"]["resOriginalRes"]["value"]["downloadURL"] = json!(download_url);
    records[0]["fields"]["resOriginalRes"]["value"]["size"] = json!(size);
    records
}

pub(super) fn relation_delta_record(container_id: &str, asset_record_name: &str) -> Value {
    json!({
        "recordName": format!("{asset_record_name}-IN-{container_id}"),
        "recordType": "CPLContainerRelation",
        "fields": {
            "containerId": {"value": container_id},
            "itemId": {"value": asset_record_name}
        }
    })
}

pub(super) fn relation_delete_record(container_id: &str, asset_record_name: &str) -> Value {
    json!({
        "recordName": format!("{asset_record_name}-IN-{container_id}"),
        "recordType": "CPLContainerRelation",
        "deleted": true
    })
}

pub(super) async fn seed_complete_album_snapshot(
    db: &SqliteStateDb,
    container_id: &str,
    album_name: &str,
    memberships: &[(&str, &str)],
) {
    db.upsert_album_container("PrimarySync", container_id, album_name, "album")
        .await
        .unwrap();
    let generation = db
        .start_album_membership_snapshot("PrimarySync", container_id, Some("hash-test"))
        .await
        .unwrap();
    for (asset_record_name, master_record_name) in memberships {
        db.add_album_membership_to_snapshot(
            "PrimarySync",
            container_id,
            generation,
            asset_record_name,
            Some(master_record_name),
            "icloud",
        )
        .await
        .unwrap();
    }
    db.complete_album_membership_snapshot("PrimarySync", container_id, generation)
        .await
        .unwrap();
}

pub(super) fn mock_album(
    name: &str,
    session: crate::test_helpers::MockPhotosSession,
) -> PhotoAlbum {
    album_with_session("PrimarySync", name, Box::new(session))
}

pub(super) fn mock_album_with_container(
    name: &str,
    container_id: &str,
    session: crate::test_helpers::MockPhotosSession,
) -> PhotoAlbum {
    album_with_session_and_container("PrimarySync", name, Some(container_id), Box::new(session))
}

#[derive(Clone, Debug)]
pub(super) struct PendingLookupSession {
    pub(super) records: Arc<Vec<Value>>,
}

#[async_trait::async_trait]
impl PhotosSession for PendingLookupSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/changes/zone?") {
            return Ok(changes_zone_response(Vec::new(), "zone-token-next"));
        }
        if url.contains("/records/lookup?") {
            return Ok(json!({"records": self.records.as_ref().clone()}));
        }
        Ok(json!({"records": [], "syncToken": "ignored-query-token"}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

pub(super) fn changes_zone_response(records: Vec<Value>, sync_token: &str) -> Value {
    json!({
        "zones": [{
            "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
            "syncToken": sync_token,
            "moreComing": false,
            "records": records,
        }]
    })
}

pub(super) fn album_with_session(
    zone: &str,
    name: &str,
    session: Box<dyn PhotosSession>,
) -> PhotoAlbum {
    album_with_session_and_container(zone, name, None, session)
}

pub(super) fn album_with_session_and_container(
    zone: &str,
    name: &str,
    container_id: Option<&str>,
    session: Box<dyn PhotosSession>,
) -> PhotoAlbum {
    album_with_session_and_retry_config(zone, name, container_id, RetryConfig::default(), session)
}

pub(super) fn album_with_session_and_retry_config(
    zone: &str,
    name: &str,
    container_id: Option<&str>,
    retry_config: RetryConfig,
    session: Box<dyn PhotosSession>,
) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": zone})),
            retry_config,
            container_id: container_id.map(Arc::from),
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}

pub(super) fn mock_photo_records_with_filename(record_name: &str, filename: &str) -> Vec<Value> {
    vec![
        mock_master_record_with_filename(record_name, filename),
        mock_asset_record_for(&format!("asset-{record_name}"), record_name),
    ]
}

pub(super) fn mock_master_record_with_filename(record_name: &str, filename: &str) -> Value {
    json!({
        "recordName": record_name,
        "recordType": "CPLMaster",
        "fields": {
            "filenameEnc": {"value": filename, "type": "STRING"},
            "resOriginalRes": {
                "value": {
                    "downloadURL": "https://p01.icloud-content.com/photo.jpg",
                    "size": 1024,
                    "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                }
            },
            "resOriginalWidth": {"value": 100, "type": "INT64"},
            "resOriginalHeight": {"value": 100, "type": "INT64"},
            "resOriginalFileType": {"value": "public.jpeg"},
            "itemType": {"value": "public.jpeg"},
            "adjustmentRenderType": {"value": 0, "type": "INT64"}
        },
        "recordChangeTag": "ct1"
    })
}

pub(super) fn mock_asset_record_for(asset_record_name: &str, master_record_name: &str) -> Value {
    json!({
        "recordName": asset_record_name,
        "recordType": "CPLAsset",
        "fields": {
            "masterRef": {
                "value": {
                    "recordName": master_record_name,
                    "zoneID": {"zoneName": "PrimarySync"}
                },
                "type": "REFERENCE"
            },
            "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
            "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
        },
        "recordChangeTag": "ct2"
    })
}

pub(super) fn hard_deleted_change_record(record_name: &str) -> Value {
    json!({
        "recordName": record_name,
        "recordType": null,
        "fields": {},
        "deleted": true,
    })
}

pub(super) async fn read_delta(
    client: &reqwest::Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    cursor: &str,
    controls: super::models::DownloadControls,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<super::delta::IncrementalDeltaSummary> {
    match super::incremental::read_zone_delta(client, passes, config, cursor, controls, &cancel)
        .await?
    {
        std::ops::ControlFlow::Continue(delta) => Ok(delta),
        std::ops::ControlFlow::Break(refused) => anyhow::bail!("unexpected refusal: {refused:?}"),
    }
}

pub(super) async fn dispatch_delta(
    client: &reqwest::Client,
    passes: &[AlbumPass],
    config: &Arc<DownloadConfig>,
    cursor: &str,
    controls: super::models::DownloadControls,
    cancel: tokio_util::sync::CancellationToken,
) -> anyhow::Result<super::models::SyncResult> {
    let mut config = config.as_ref().clone();
    config.sync_mode = SyncMode::Incremental {
        zone_sync_token: cursor.into(),
    };
    super::dispatch::download_photos_with_sync(client, passes, Arc::new(config), controls, cancel)
        .await
}
