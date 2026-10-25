use super::PhotoAlbum;
use crate::icloud::photos::{PhotoAlbumConfig, PhotosSession, inbox::ShadowCapture};
use crate::state::{SqliteStateDb, db::account::AccountOwner};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct NoRequests;
#[async_trait::async_trait]
impl PhotosSession for NoRequests {
    async fn post(
        &self,
        _url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        panic!("scope resolution must not make provider requests")
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}
fn album(zone: &str, owner: &str) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(std::collections::HashMap::new()),
            service_endpoint: "https://invalid.example".into(),
            name: "selected".into(),
            list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
            obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
            query_filter: None,
            page_size: 1,
            zone_id: Arc::new(json!({"zoneName":zone,"ownerRecordName":owner})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(NoRequests),
    )
}
#[tokio::test]
async fn v027_publication_scope_preserves_attached_auth_and_rejects_mixed_sources() {
    let directory = tempfile::tempdir().unwrap();
    let owner = AccountOwner::authenticated(
        "scope@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"scope-test"}})).unwrap(),
    )
    .unwrap();
    let db = Arc::new(
        SqliteStateDb::open_owned(&directory.path().join("state.db"), &owner)
            .await
            .unwrap(),
    );
    let capture = ShadowCapture::new(db.clone(), owner, "com");
    let mut selected = album("PrimarySync", "_defaultOwner");
    assert!(selected.publication_scope("PrimarySync").unwrap().is_none());
    selected.set_shadow_capture(capture.clone(), "private".into());
    let primary = selected.publication_scope("PrimarySync").unwrap().unwrap();
    assert!(Arc::ptr_eq(&primary.0.db, &db));
    assert_eq!(
        serde_json::from_str::<Value>(&primary.1).unwrap()["database"],
        "private"
    );
    let mut source = album("SharedSync-source", "shared-owner");
    source.set_shadow_capture(capture.clone(), "shared".into());
    selected = selected.with_cross_zone_sources(vec![source.clone()]);
    let shared = selected
        .publication_scope("SharedSync-source")
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&shared.0.db, &db));
    assert_eq!(shared.2["ownerRecordName"], "shared-owner");
    assert_eq!(
        serde_json::from_str::<Value>(&shared.1).unwrap()["database"],
        "shared"
    );
    assert!(selected.publication_scope("PrimarySync").unwrap().is_some());

    let unowned = album("SharedSync-source", "shared-owner");
    assert!(
        selected
            .clone()
            .with_cross_zone_sources(vec![unowned, source.clone()])
            .publication_scope("SharedSync-source")
            .is_err()
    );
    let mut other = album("SharedSync-source", "different-owner");
    other.set_shadow_capture(capture.clone(), "private".into());
    assert!(
        selected
            .clone()
            .with_cross_zone_sources(vec![source.clone(), other])
            .publication_scope("SharedSync-source")
            .is_err()
    );
    let mut invalid = album("SharedSync-source", "");
    invalid.set_shadow_capture(capture, "shared".into());
    assert!(
        selected
            .with_cross_zone_sources(vec![source, invalid])
            .publication_scope("SharedSync-source")
            .is_err()
    );
}
