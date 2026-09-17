//! Shared fixtures for orchestration production-path tests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::commands::{AlbumPass, PassKind};
use crate::download::filter::DownloadTask;
use crate::download::{file, filter};
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoAsset, PhotosSession};
use crate::retry::RetryConfig;
use crate::state::{SqliteStateDb, VersionSizeKey};
use crate::test_helpers::{TestAssetRecord, mock_photo_records_for_zone_with_filename};

use super::config::DownloadConfig;
use super::models::SyncMode;

pub(super) fn test_config() -> DownloadConfig {
    DownloadConfig::test_default()
}

pub(super) fn retry_test_task(
    asset_id: &str,
    version_size: VersionSizeKey,
    path: &str,
) -> DownloadTask {
    DownloadTask {
        url: format!("https://p01.icloud-content.com/{asset_id}").into(),
        download_path: Path::new("/tmp/codex/kei/retry-tests").join(path),
        publication: file::FinalPublication::NoReplace,
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
    changes_zone_session_with_query_page(changes_zone_calls, records, json!({"records": []}), 0)
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
        _body: String,
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
