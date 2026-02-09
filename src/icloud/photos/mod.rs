//! Photos service — fetches albums, assets, and download URLs from iCloud's
//! CloudKit-based photos backend. Mirrors the Python `PhotosService` class.

mod album;
mod asset;
pub mod cloudkit;
pub mod error;
mod library;
pub mod queries;
pub mod session;
mod smart_folders;
pub mod types;

pub use album::PhotoAlbum;
pub use asset::{PhotoAsset, VersionsMap};
pub use library::PhotoLibrary;
pub use session::PhotosSession;
pub use types::{AssetItemType, AssetVersionSize};

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{json, Value};
use tracing::{debug, error};

use crate::icloud::error::ICloudError;

pub struct PhotosService {
    service_root: String,
    session: Box<dyn PhotosSession>,
    params: Arc<HashMap<String, Value>>,
    primary_library: PhotoLibrary,
    private_libraries: Option<HashMap<String, PhotoLibrary>>,
    shared_libraries: Option<HashMap<String, PhotoLibrary>>,
}

impl std::fmt::Debug for PhotosService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosService")
            .field("service_root", &self.service_root)
            .field("primary_library", &self.primary_library)
            .finish_non_exhaustive()
    }
}

impl PhotosService {
    /// Create a new `PhotosService`.
    ///
    /// This checks that the primary library has finished indexing.
    pub async fn new(
        service_root: String,
        session: Box<dyn PhotosSession>,
        mut params: HashMap<String, Value>,
    ) -> Result<Self, ICloudError> {
        params.insert("remapEnums".to_string(), Value::Bool(true));
        params.insert("getCurrentSyncToken".to_string(), Value::Bool(true));

        let params = Arc::new(params);
        let service_endpoint = Self::build_service_endpoint(&service_root, "private");
        let zone_id = json!({"zoneName": "PrimarySync"});

        let lib_session = session.clone_box();

        let primary_library = PhotoLibrary::new(
            service_endpoint,
            Arc::clone(&params),
            lib_session,
            zone_id,
            "private".to_string(),
        )
        .await?;

        Ok(Self {
            service_root,
            session,
            params,
            primary_library,
            private_libraries: None,
            shared_libraries: None,
        })
    }

    /// Compute the service endpoint URL for a given library type.
    pub fn get_service_endpoint(&self, library_type: &str) -> String {
        Self::build_service_endpoint(&self.service_root, library_type)
    }

    fn build_service_endpoint(service_root: &str, library_type: &str) -> String {
        format!("{service_root}/database/1/com.apple.photos.cloud/production/{library_type}")
    }

    /// Return the "All Photos" album from the primary library.
    pub fn all(&self) -> PhotoAlbum {
        self.primary_library.all()
    }

    /// Look up a library by zone name.
    ///
    /// Checks the primary library first ("PrimarySync"), then searches private
    /// and shared libraries. Lazily fetches library lists on first call.
    pub async fn get_library(&mut self, name: &str) -> anyhow::Result<&PhotoLibrary> {
        if name == "PrimarySync" {
            return Ok(&self.primary_library);
        }
        // Ensure both library lists are fetched
        self.fetch_private_libraries().await?;
        self.fetch_shared_libraries().await?;

        if let Some(lib) = self.private_libraries.as_ref().and_then(|m| m.get(name)) {
            return Ok(lib);
        }
        if let Some(lib) = self.shared_libraries.as_ref().and_then(|m| m.get(name)) {
            return Ok(lib);
        }
        anyhow::bail!(
            "Unknown library: '{}'. Use --list-libraries to see available libraries.",
            name
        )
    }

    /// Fetch private libraries (lazily, first call triggers the HTTP request).
    pub async fn fetch_private_libraries(
        &mut self,
    ) -> anyhow::Result<&HashMap<String, PhotoLibrary>> {
        if self.private_libraries.is_none() {
            let libs = self.fetch_libraries("private").await?;
            self.private_libraries = Some(libs);
        }
        // Safe: we just ensured private_libraries is Some above
        Ok(self
            .private_libraries
            .as_ref()
            .unwrap_or_else(|| unreachable!("private_libraries was just set to Some")))
    }

    /// Fetch shared libraries (lazily, first call triggers the HTTP request).
    pub async fn fetch_shared_libraries(
        &mut self,
    ) -> anyhow::Result<&HashMap<String, PhotoLibrary>> {
        if self.shared_libraries.is_none() {
            let libs = self.fetch_libraries("shared").await?;
            self.shared_libraries = Some(libs);
        }
        // Safe: we just ensured shared_libraries is Some above
        Ok(self
            .shared_libraries
            .as_ref()
            .unwrap_or_else(|| unreachable!("shared_libraries was just set to Some")))
    }

    async fn fetch_libraries(
        &self,
        library_type: &str,
    ) -> anyhow::Result<HashMap<String, PhotoLibrary>> {
        let mut libraries = HashMap::new();
        let service_endpoint = self.get_service_endpoint(library_type);
        let url = format!("{service_endpoint}/zones/list");

        let response = session::retry_post(
            self.session.as_ref(),
            &url,
            "{}",
            &[("Content-type", "text/plain")],
        )
        .await?;

        let zone_list: cloudkit::ZoneListResponse = serde_json::from_value(response)?;

        for zone in &zone_list.zones {
            if zone.deleted.unwrap_or(false) {
                continue;
            }
            let zone_name = zone.zone_id.zone_name.clone();
            let zone_id = serde_json::to_value(&zone.zone_id)?;
            let ep = self.get_service_endpoint(library_type);
            let lib_session = self.session.clone_box();

            match PhotoLibrary::new(
                ep,
                Arc::clone(&self.params),
                lib_session,
                zone_id,
                library_type.to_string(),
            )
            .await
            {
                Ok(lib) => {
                    debug!("Loaded library zone: {}", zone_name);
                    libraries.insert(zone_name, lib);
                }
                Err(e) => {
                    error!("Failed to load library zone {}: {}", zone_name, e);
                }
            }
        }

        Ok(libraries)
    }
}
