use std::collections::HashMap;

use base64::Engine;
use serde_json::{json, Value};


use super::album::PhotoAlbum;
use super::queries::encode_params;
use super::session::PhotosSession;
use super::smart_folders::smart_folders;
use crate::icloud::error::ICloudError;

const ROOT_FOLDER: &str = "----Root-Folder----";
const PROJECT_ROOT_FOLDER: &str = "----Project-Root-Folder----";

pub struct PhotoLibrary {
    service_endpoint: String,
    params: HashMap<String, Value>,
    session: Box<dyn PhotosSession>,
    zone_id: Value,
    library_type: String,
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

        let response = session
            .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
            .await
            .map_err(|e| ICloudError::Connection(e.to_string()))?;

        let indexing_state = response["records"][0]["fields"]["state"]["value"]
            .as_str()
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

        // Smart folders
        for (name, def) in smart_folders() {
            albums.insert(
                name.to_string(),
                PhotoAlbum::new(
                    self.params.clone(),
                    self.clone_session(),
                    self.service_endpoint.clone(),
                    name.to_string(),
                    def.list_type.to_string(),
                    def.obj_type.to_string(),
                    def.query_filter,
                    100,
                    self.zone_id.clone(),
                ),
            );
        }

        // User albums (skip for shared libraries)
        if self.library_type != "shared" {
            let folders = self.fetch_folders().await?;
            for folder in folders {
                let record_name = folder["recordName"].as_str().unwrap_or_else(|| {
                    tracing::warn!("Missing expected field: folder recordName");
                    ""
                });
                if record_name == ROOT_FOLDER
                    || record_name == PROJECT_ROOT_FOLDER
                {
                    continue;
                }
                // Skip deleted folders
                if folder["fields"]["isDeleted"]["value"]
                    .as_bool()
                    .unwrap_or(false)
                {
                    continue;
                }

                let folder_id = record_name.to_string();
                let folder_obj_type =
                    format!("CPLContainerRelationNotDeletedByAssetDate:{folder_id}");

                let folder_name = match folder["fields"]["albumNameEnc"]["value"].as_str() {
                    Some(enc) => {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(enc)
                            .unwrap_or_default();
                        String::from_utf8(decoded).unwrap_or_else(|_| folder_id.clone())
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
                        self.params.clone(),
                        self.clone_session(),
                        self.service_endpoint.clone(),
                        folder_name,
                        "CPLContainerRelationLiveByAssetDate".to_string(),
                        folder_obj_type,
                        query_filter,
                        100,
                        self.zone_id.clone(),
                    ),
                );
            }
        }

        Ok(albums)
    }

    /// Convenience: return a `PhotoAlbum` representing the whole collection.
    pub fn all(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            self.params.clone(),
            self.clone_session(),
            self.service_endpoint.clone(),
            String::new(),
            "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".to_string(),
            "CPLAssetByAssetDateWithoutHiddenOrDeleted".to_string(),
            None,
            100,
            self.zone_id.clone(),
        )
    }

    /// Convenience: return a `PhotoAlbum` for recently deleted items.
    #[allow(dead_code)]
    pub fn recently_deleted(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            self.params.clone(),
            self.clone_session(),
            self.service_endpoint.clone(),
            String::new(),
            "CPLAssetAndMasterDeletedByExpungedDate".to_string(),
            "CPLAssetDeletedByExpungedDate".to_string(),
            None,
            100,
            self.zone_id.clone(),
        )
    }

    // -- internal helpers --

    async fn fetch_folders(&self) -> anyhow::Result<Vec<Value>> {
        let url = format!(
            "{}/records/query?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "query": {"recordType": "CPLAlbumByPositionLive"},
            "zoneID": &self.zone_id,
        });
        let response = self
            .session
            .post(&url, &body.to_string(), &[("Content-type", "text/plain")])
            .await?;

        let records = response["records"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        Ok(records)
    }

    /// Clone the session as a boxed trait object, preserving the original
    /// client's cookies and configuration.
    fn clone_session(&self) -> Box<dyn PhotosSession> {
        self.session.clone_box()
    }
}
