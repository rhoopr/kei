use std::collections::HashMap;

use base64::Engine;
use serde_json::{json, Value};

use super::album::{PhotoAlbum, PhotoAlbumConfig};
use super::queries::encode_params;
use super::session::PhotosSession;
use super::smart_folders::smart_folders;
use crate::icloud::error::ICloudError;

// Apple's sentinel folder IDs — these are containers, not real albums.
const ROOT_FOLDER: &str = "----Root-Folder----";
const PROJECT_ROOT_FOLDER: &str = "----Project-Root-Folder----";

// CloudKit record/query types for photo enumeration.
const QUERY_ALL_LIST: &str = "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted";
const QUERY_ALL_OBJ: &str = "CPLAssetByAssetDateWithoutHiddenOrDeleted";
const QUERY_DELETED_LIST: &str = "CPLAssetAndMasterDeletedByExpungedDate";
const QUERY_DELETED_OBJ: &str = "CPLAssetDeletedByExpungedDate";
const QUERY_FOLDER_LIST: &str = "CPLContainerRelationLiveByAssetDate";

pub struct PhotoLibrary {
    service_endpoint: String,
    params: HashMap<String, Value>,
    session: Box<dyn PhotosSession>,
    zone_id: Value,
    library_type: String,
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
    /// Create a new `PhotoLibrary`, verifying that indexing has finished.
    pub async fn new(
        service_endpoint: String,
        params: HashMap<String, Value>,
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
        .map_err(|e| ICloudError::Connection(e.to_string()))?;

        let query: super::cloudkit::QueryResponse =
            serde_json::from_value(response).map_err(|e| ICloudError::Connection(e.to_string()))?;
        let indexing_state = query
            .records
            .first()
            .and_then(|r| r.fields["state"]["value"].as_str())
            .unwrap_or("");
        if indexing_state != "FINISHED" {
            return Err(ICloudError::IndexingNotFinished);
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

        for (name, def) in smart_folders() {
            albums.insert(
                name.to_string(),
                PhotoAlbum::new(
                    PhotoAlbumConfig {
                        params: self.params.clone(),
                        service_endpoint: self.service_endpoint.clone(),
                        name: name.to_string(),
                        list_type: def.list_type.to_string(),
                        obj_type: def.obj_type.to_string(),
                        query_filter: def.query_filter,
                        page_size: 100,
                        zone_id: self.zone_id.clone(),
                    },
                    self.clone_session(),
                ),
            );
        }

        // Shared libraries use a different album structure — skip user albums
        if self.library_type != "shared" {
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
                            params: self.params.clone(),
                            service_endpoint: self.service_endpoint.clone(),
                            name: folder_name,
                            list_type: QUERY_FOLDER_LIST.to_string(),
                            obj_type: folder_obj_type,
                            query_filter,
                            page_size: 100,
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
                page_size: 100,
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
                page_size: 100,
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

    /// Clone the session for a new album/library — preserves the shared
    /// cookie jar via the Arc inside reqwest::Client.
    fn clone_session(&self) -> Box<dyn PhotosSession> {
        self.session.clone_box()
    }
}
