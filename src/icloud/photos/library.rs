use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use serde_json::{json, Value};
use tracing::warn;

use super::album::{PhotoAlbum, PhotoAlbumConfig};
use super::queries::encode_params;
use super::session::PhotosSession;
use super::smart_folders::smart_folders;
use crate::icloud::error::ICloudError;

// Apple's sentinel folder IDs — these are containers, not real albums.
const ROOT_FOLDER: &str = "----Root-Folder----";
const PROJECT_ROOT_FOLDER: &str = "----Project-Root-Folder----";

/// Default page size for CloudKit queries.
const DEFAULT_PAGE_SIZE: usize = 100;

// CloudKit record/query types for photo enumeration.
const QUERY_ALL_LIST: &str = "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted";
const QUERY_ALL_OBJ: &str = "CPLAssetByAssetDateWithoutHiddenOrDeleted";
const QUERY_DELETED_LIST: &str = "CPLAssetAndMasterDeletedByExpungedDate";
const QUERY_DELETED_OBJ: &str = "CPLAssetDeletedByExpungedDate";
const QUERY_FOLDER_LIST: &str = "CPLContainerRelationLiveByAssetDate";

pub struct PhotoLibrary {
    service_endpoint: String,
    params: Arc<HashMap<String, Value>>,
    session: Box<dyn PhotosSession>,
    zone_id: Value,
    library_type: String,
}

impl Clone for PhotoLibrary {
    fn clone(&self) -> Self {
        Self {
            service_endpoint: self.service_endpoint.clone(),
            params: Arc::clone(&self.params),
            session: self.session.clone_box(),
            zone_id: self.zone_id.clone(),
            library_type: self.library_type.clone(),
        }
    }
}

impl std::fmt::Debug for PhotoLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotoLibrary")
            .field("service_endpoint", &self.service_endpoint)
            .field("library_type", &self.library_type)
            .finish_non_exhaustive()
    }
}

impl PhotoLibrary {
    /// Create a new `PhotoLibrary`, warning if indexing has not finished.
    pub async fn new(
        service_endpoint: String,
        params: Arc<HashMap<String, Value>>,
        session: Box<dyn PhotosSession>,
        zone_id: Value,
        library_type: String,
    ) -> Result<Self, ICloudError> {
        let url = format!(
            "{}/records/query?{}",
            service_endpoint,
            encode_params(&params)
        );
        let body = json!({
            "query": {"recordType": "CheckIndexingState"},
            "zoneID": &zone_id,
        });

        let response = super::session::retry_post(
            session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
        )
        .await
        .map_err(|e| {
            if let Some(ck) = e.downcast_ref::<super::session::CloudKitServerError>() {
                if ck.service_not_activated {
                    return ICloudError::ServiceNotActivated {
                        code: ck.code.clone(),
                        reason: ck.reason.clone(),
                    };
                }
            }
            ICloudError::Connection(e.to_string())
        })?;

        let query: super::cloudkit::QueryResponse =
            serde_json::from_value(response).map_err(|e| ICloudError::Connection(e.to_string()))?;
        let indexing_state = query
            .records
            .first()
            .and_then(|r| r.fields["state"]["value"].as_str())
            .unwrap_or("");
        if indexing_state != "FINISHED" {
            warn!(
                state = indexing_state,
                "Photo library indexing state is not FINISHED — proceeding anyway, \
                 results may be incomplete"
            );
        }

        Ok(Self {
            service_endpoint,
            params,
            session,
            zone_id,
            library_type,
        })
    }

    /// Return smart-folder albums plus user-created albums.
    pub async fn albums(&self) -> anyhow::Result<HashMap<String, PhotoAlbum>> {
        let mut albums = HashMap::new();

        // Shared libraries don't support smart folder or user album queries —
        // their CloudKit zones lack the required indexes. Check the zone name
        // because Apple's private endpoint also returns SharedSync zones
        // (library_type reflects the API endpoint, not the zone type).
        if !self.zone_name().starts_with("SharedSync") {
            for (name, def) in smart_folders() {
                albums.insert(
                    name.to_string(),
                    PhotoAlbum::new(
                        PhotoAlbumConfig {
                            params: Arc::clone(&self.params),
                            service_endpoint: self.service_endpoint.clone(),
                            name: name.to_string(),
                            list_type: def.list_type.to_string(),
                            obj_type: def.obj_type.to_string(),
                            query_filter: def.query_filter,
                            page_size: DEFAULT_PAGE_SIZE,
                            zone_id: self.zone_id.clone(),
                        },
                        self.clone_session(),
                    ),
                );
            }
            let folders = self.fetch_folders().await?;
            for folder in &folders {
                let record_name = &folder.record_name;
                if record_name == ROOT_FOLDER || record_name == PROJECT_ROOT_FOLDER {
                    continue;
                }
                if folder.fields["isDeleted"]["value"]
                    .as_bool()
                    .unwrap_or(false)
                {
                    continue;
                }

                let folder_id = record_name.to_string();
                let folder_obj_type =
                    format!("CPLContainerRelationNotDeletedByAssetDate:{folder_id}");

                let folder_name = match folder.fields["albumNameEnc"]["value"].as_str() {
                    Some(enc) => {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(enc)
                            .unwrap_or_default();
                        let raw_name =
                            String::from_utf8(decoded).unwrap_or_else(|_| folder_id.clone());
                        crate::download::paths::sanitize_path_component(&raw_name)
                    }
                    None => folder_id.clone(),
                };

                let query_filter = Some(json!([{
                    "fieldName": "parentId",
                    "comparator": "EQUALS",
                    "fieldValue": {"type": "STRING", "value": &folder_id},
                }]));

                albums.insert(
                    folder_name.clone(),
                    PhotoAlbum::new(
                        PhotoAlbumConfig {
                            params: Arc::clone(&self.params),
                            service_endpoint: self.service_endpoint.clone(),
                            name: folder_name,
                            list_type: QUERY_FOLDER_LIST.to_string(),
                            obj_type: folder_obj_type,
                            query_filter,
                            page_size: DEFAULT_PAGE_SIZE,
                            zone_id: self.zone_id.clone(),
                        },
                        self.clone_session(),
                    ),
                );
            }
        }

        Ok(albums)
    }

    pub fn all(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: self.params.clone(),
                service_endpoint: self.service_endpoint.clone(),
                name: String::new(),
                list_type: QUERY_ALL_LIST.to_string(),
                obj_type: QUERY_ALL_OBJ.to_string(),
                query_filter: None,
                page_size: DEFAULT_PAGE_SIZE,
                zone_id: self.zone_id.clone(),
            },
            self.clone_session(),
        )
    }

    #[allow(dead_code)] // for --auto-delete feature
    pub fn recently_deleted(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: self.params.clone(),
                service_endpoint: self.service_endpoint.clone(),
                name: String::new(),
                list_type: QUERY_DELETED_LIST.to_string(),
                obj_type: QUERY_DELETED_OBJ.to_string(),
                query_filter: None,
                page_size: DEFAULT_PAGE_SIZE,
                zone_id: self.zone_id.clone(),
            },
            self.clone_session(),
        )
    }

    async fn fetch_folders(&self) -> anyhow::Result<Vec<super::cloudkit::Record>> {
        let url = format!(
            "{}/records/query?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "query": {"recordType": "CPLAlbumByPositionLive"},
            "zoneID": &self.zone_id,
        });
        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
        )
        .await?;

        let query: super::cloudkit::QueryResponse = serde_json::from_value(response)?;
        Ok(query.records)
    }

    /// Returns the zone name (e.g., "PrimarySync", "SharedSync-{UUID}").
    pub fn zone_name(&self) -> &str {
        self.zone_id
            .get("zoneName")
            .and_then(|v| v.as_str())
            .unwrap_or("PrimarySync")
    }

    /// Clone the session for a new album/library — preserves the shared
    /// cookie jar via the Arc inside reqwest::Client.
    fn clone_session(&self) -> Box<dyn PhotosSession> {
        self.session.clone_box()
    }
}

#[cfg(test)]
impl PhotoLibrary {
    /// Test-only constructor that bypasses the indexing check.
    pub(super) fn new_stub(session: Box<dyn PhotosSession>) -> Self {
        Self {
            service_endpoint: "https://stub.example.com".to_string(),
            params: Arc::new(HashMap::new()),
            session,
            zone_id: json!({"zoneName": "PrimarySync"}),
            library_type: "private".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Minimal stub that satisfies `PhotosSession` for unit tests.
    struct StubSession;

    #[async_trait::async_trait]
    impl PhotosSession for StubSession {
        async fn post(
            &self,
            _url: &str,
            _body: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            panic!("StubSession::post should not be called in zone_name tests");
        }

        async fn get(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<reqwest::Response> {
            panic!("StubSession::get should not be called in zone_name tests");
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(StubSession)
        }
    }

    /// Build a `PhotoLibrary` directly (bypassing `new()` which requires a live session).
    fn make_library(zone_id: Value) -> PhotoLibrary {
        PhotoLibrary {
            service_endpoint: "https://example.com".to_string(),
            params: Arc::new(HashMap::new()),
            session: Box::new(StubSession),
            zone_id,
            library_type: "personal".to_string(),
        }
    }

    #[test]
    fn test_zone_name_primary() {
        let lib = make_library(json!({"zoneName": "PrimarySync", "zoneType": "DEFAULT_ZONE"}));
        assert_eq!(lib.zone_name(), "PrimarySync");
    }

    #[test]
    fn test_zone_name_shared() {
        let lib = make_library(json!({"zoneName": "SharedSync-ABCD-1234"}));
        assert_eq!(lib.zone_name(), "SharedSync-ABCD-1234");
    }

    #[test]
    fn test_zone_name_missing_defaults_to_primary() {
        let lib = make_library(json!({"zoneType": "DEFAULT_ZONE"}));
        assert_eq!(lib.zone_name(), "PrimarySync");
    }

    #[test]
    fn test_zone_name_null_defaults_to_primary() {
        let lib = make_library(json!({"zoneName": null}));
        assert_eq!(lib.zone_name(), "PrimarySync");
    }

    #[test]
    fn test_clone_preserves_zone_name() {
        let lib = make_library(json!({"zoneName": "SharedSync-ABCD-1234"}));
        let cloned = lib.clone();
        assert_eq!(cloned.zone_name(), lib.zone_name());
    }

    #[test]
    fn test_clone_preserves_service_endpoint() {
        let lib = make_library(json!({"zoneName": "PrimarySync"}));
        let cloned = lib.clone();
        let debug = format!("{:?}", cloned);
        assert!(debug.contains("https://example.com"));
    }

    #[test]
    fn test_clone_independence() {
        let lib = make_library(json!({"zoneName": "PrimarySync"}));
        let cloned = lib.clone();
        drop(lib);
        assert_eq!(cloned.zone_name(), "PrimarySync");
    }
}
