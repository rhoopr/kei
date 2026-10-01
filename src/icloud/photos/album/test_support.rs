//! Shared album test fixtures.

use super::{PhotoAlbum, PhotoAlbumConfig, PhotoStream};
use crate::retry::RetryConfig;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

use crate::icloud::photos::session::PhotosSession;

/// Panic-on-call `PhotosSession` for tests that inspect a `PhotoAlbum` by
/// name/metadata only. Any actual network call is a test bug.
#[cfg(test)]
pub(crate) struct StubSession;

#[cfg(test)]
#[async_trait::async_trait]
impl PhotosSession for StubSession {
    async fn post(
        &self,
        _url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        unimplemented!("stub")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(StubSession)
    }
}

pub(super) fn make_album(
    page_size: usize,
    query_filter: Option<Arc<Value>>,
    zone_id: Value,
) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter,
            page_size,
            zone_id: Arc::new(zone_id),
            retry_config: RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(StubSession),
    )
}

pub(super) fn default_zone() -> Value {
    json!({"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner", "zoneType": "REGULAR_CUSTOM_ZONE"})
}

pub(super) fn make_album_with_session(
    page_size: usize,
    session: Box<dyn PhotosSession>,
) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size,
            zone_id: Arc::new(default_zone()),
            retry_config: RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        session,
    )
}

pub(super) async fn drain_photo_stream_count(stream: PhotoStream) -> u32 {
    use tokio_stream::StreamExt;

    tokio::pin!(stream);
    let mut count = 0u32;
    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
        count += 1;
    }
    count
}

pub(super) async fn drain_photo_stream_ids(stream: PhotoStream) -> Vec<String> {
    use tokio_stream::StreamExt;

    tokio::pin!(stream);
    let mut ids = Vec::new();
    while let Some(result) = stream.next().await {
        ids.push(result.expect("photo asset should be Ok").id().to_string());
    }
    ids
}

pub(super) async fn drain_photo_stream(stream: PhotoStream) -> (Vec<String>, Vec<String>) {
    use tokio_stream::StreamExt;

    tokio::pin!(stream);
    let mut ids = Vec::new();
    let mut errors = Vec::new();
    while let Some(result) = stream.next().await {
        match result {
            Ok(asset) => ids.push(asset.id().to_string()),
            Err(e) => errors.push(e.to_string()),
        }
    }
    (ids, errors)
}

pub(super) fn test_records(record_name: &str) -> Vec<Value> {
    vec![
        test_master_record(record_name),
        test_asset_record(record_name),
    ]
}

pub(super) fn test_master_record(record_name: &str) -> Value {
    json!({
        "recordName": record_name,
        "recordType": "CPLMaster",
        "fields": {
            "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
            "resOriginalRes": {
                "value": {
                    "downloadURL": "https://p01.icloud-content.com/photo.jpg",
                    "size": 1024,
                    "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                }
            },
            "resOriginalFileType": {"value": "public.jpeg"},
            "itemType": {"value": "public.jpeg"}
        }
    })
}

pub(super) fn test_asset_record(record_name: &str) -> Value {
    test_asset_record_for(&format!("asset-{record_name}"), record_name)
}

pub(super) fn test_asset_record_for(asset_record_name: &str, master_record_name: &str) -> Value {
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
            "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
        }
    })
}

/// Build a canned `ChangesZoneResponse` JSON with the given records,
/// syncToken, and moreComing flag.
pub(super) fn canned_changes_page(records: &[Value], sync_token: &str, more_coming: bool) -> Value {
    json!({
        "zones": [{
            "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
            "syncToken": sync_token,
            "moreComing": more_coming,
            "records": records
        }]
    })
}

/// Build a CPLMaster record for changes/zone tests.
pub(super) fn changes_master(record_name: &str) -> Value {
    json!({
        "recordName": record_name,
        "recordType": "CPLMaster",
        "fields": {
            "filenameEnc": {"value": "dGVzdC5qcGc=", "type": "STRING"},
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

/// Build a CPLAsset record that references the given master.
pub(super) fn changes_asset(record_name: &str, master_ref: &str) -> Value {
    json!({
        "recordName": record_name,
        "recordType": "CPLAsset",
        "fields": {
            "masterRef": {
                "value": {"recordName": master_ref, "zoneID": {"zoneName": "PrimarySync"}},
                "type": "REFERENCE"
            },
            "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
            "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
        },
        "recordChangeTag": "ct2"
    })
}
