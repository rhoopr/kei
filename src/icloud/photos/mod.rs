//! Photos service — fetches albums, assets, and download URLs from iCloud's
//! CloudKit-based photos backend. Mirrors the Python `PhotosService` class.

mod album;
pub(crate) mod asset;
mod changes_json;
pub mod cloudkit;
pub(crate) mod enc;
pub mod error;
pub(crate) mod inbox;
mod library;
pub(crate) mod metadata;
mod projection;
pub mod queries;
pub mod session;
pub(crate) mod smart_folders;
pub mod types;

#[cfg(test)]
pub(crate) use album::MAX_EMPTY_PAGE_PROBES;
pub use album::PhotoAlbum;
#[cfg(test)]
pub use album::PhotoAlbumConfig;
#[cfg(test)]
pub(crate) use album::ProviderLookupError;
pub(crate) use album::catalog_observed_page;
pub(crate) use album::work::current_asset;
pub(crate) use album::{CompleteLegacyInventory, classify_legacy_inventory_error};
pub(crate) use album::{
    ProviderRecordId, RecordLookupRequest, RecordResolution, RecordResolutionBatch,
};
pub use asset::{PhotoAsset, VersionsMap};
pub use library::PhotoLibrary;
pub(crate) use library::{PRIMARY_ZONE_NAME, is_shared_zone};
pub use session::{PhotosSession, SyncTokenError};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use serde_json::{Value, json};

use crate::icloud::error::ICloudError;
use crate::icloud::photos::cloudkit::ChangesDatabaseResponse;
use crate::icloud::photos::queries::encode_params;
use crate::retry::RetryConfig;

pub struct PhotosService {
    shadow_capture: Option<inbox::ShadowCapture>,
    service_root: String,
    session: Box<dyn PhotosSession>,
    params: Arc<HashMap<String, Value>>,
    primary_library: PhotoLibrary,
    private_zones: Option<Vec<cloudkit::Zone>>,
    private_libraries: Option<HashMap<String, PhotoLibrary>>,
    shared_libraries: Option<HashMap<String, PhotoLibrary>>,
    retry_config: RetryConfig,
}

impl std::fmt::Debug for PhotosService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosService")
            .field("service_root", &self.service_root)
            .field("primary_library", &self.primary_library)
            .finish_non_exhaustive()
    }
}

/// Whether a CloudKit zone is a photo library.
///
/// `zones/list` also returns share-link bundles (`CMM-{UUID}`) and shared-album
/// zones (`SharedCollection-{UUID}`), which carry no album index.
fn is_photo_library_zone(zone_name: &str) -> bool {
    zone_name == PRIMARY_ZONE_NAME || is_shared_zone(zone_name)
}

impl PhotosService {
    pub(crate) async fn replay_catalog(
        &self,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        if let Some(capture) = &self.shadow_capture {
            capture.replay_catalog(cancel).await?;
        }
        Ok(())
    }

    pub(crate) fn set_shadow_capture(&mut self, capture: inbox::ShadowCapture) {
        self.primary_library.shadow_capture = Some(capture.clone());
        for libraries in [&mut self.private_libraries, &mut self.shared_libraries]
            .into_iter()
            .flatten()
        {
            for library in libraries.values_mut() {
                library.shadow_capture = Some(capture.clone());
            }
        }
        self.shadow_capture = Some(capture);
    }

    /// Create a new `PhotosService`.
    ///
    /// This checks that the primary library has finished indexing.
    pub async fn new(
        service_root: String,
        session: Box<dyn PhotosSession>,
        mut params: HashMap<String, Value>,
        retry_config: RetryConfig,
    ) -> Result<Self, ICloudError> {
        params.insert("remapEnums".to_string(), Value::Bool(true));
        params.insert("getCurrentSyncToken".to_string(), Value::Bool(true));

        let params = Arc::new(params);
        let service_endpoint = Self::build_service_endpoint(&service_root, "private");
        let zone_id = Arc::new(json!({"zoneName": "PrimarySync"}));

        let lib_session = session.clone_box();

        let primary_library = PhotoLibrary::new(
            service_endpoint,
            Arc::clone(&params),
            lib_session,
            zone_id,
            "private".to_string(),
            retry_config,
        )
        .await?;

        Ok(Self {
            shadow_capture: None,
            service_root,
            session,
            params,
            primary_library,
            private_zones: None,
            private_libraries: None,
            shared_libraries: None,
            retry_config,
        })
    }

    /// Compute the service endpoint URL for a given library type.
    pub(crate) fn get_service_endpoint(&self, library_type: &str) -> String {
        Self::build_service_endpoint(&self.service_root, library_type)
    }

    fn build_service_endpoint(service_root: &str, library_type: &str) -> String {
        format!("{service_root}/database/1/com.apple.photos.cloud/production/{library_type}")
    }

    /// Look up a library by zone name.
    ///
    /// Primary lookup validates private discovery and initializes its selected
    /// scope only. Other names search complete initialized private/shared maps.
    pub async fn get_library(&mut self, name: &str) -> anyhow::Result<&PhotoLibrary> {
        if name == PRIMARY_ZONE_NAME {
            // The constructor deliberately has no owner. Only authenticated
            // private discovery may replace it with explicit provider scope.
            self.qualify_primary_library().await?;
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
            "iCloud Photos library `{name}` was not found. Run `kei list libraries` to see available libraries."
        )
    }

    /// Return all available libraries: primary + private (non-PrimarySync) + shared.
    pub async fn all_libraries(&mut self) -> anyhow::Result<Vec<PhotoLibrary>> {
        self.fetch_private_libraries().await?;
        let mut libs = vec![self.primary_library.clone()];

        let private = self.fetch_private_libraries().await?;
        for (name, lib) in private {
            if name != "PrimarySync" {
                libs.push(lib.clone());
            }
        }

        let shared = self.fetch_shared_libraries().await?;
        for lib in shared.values() {
            libs.push(lib.clone());
        }

        Ok(libs)
    }

    /// Validate the complete private listing without initializing unrelated libraries.
    async fn fetch_private_zones(&mut self) -> anyhow::Result<()> {
        if self.private_zones.is_none() {
            self.private_zones = Some(self.fetch_zone_list("private").await?);
        }
        Ok(())
    }

    async fn qualify_primary_library(&mut self) -> anyhow::Result<()> {
        if self.primary_library.is_private_default_owner() {
            return Ok(());
        }
        if let Some(libraries) = &self.private_libraries {
            if let Some(primary) = libraries.get(PRIMARY_ZONE_NAME)
                && primary.is_private_default_owner()
            {
                self.primary_library = primary.clone();
            }
            return Ok(());
        }
        self.fetch_private_zones().await?;
        let zones = self
            .private_zones
            .as_ref()
            .context("Private zone list was not cached")?;
        if let Some(zone) = zones.iter().find(|zone| {
            zone.zone_id.zone_name == PRIMARY_ZONE_NAME
                && !zone.deleted.unwrap_or(false)
                && zone
                    .zone_id
                    .extra
                    .get("ownerRecordName")
                    .and_then(Value::as_str)
                    == Some("_defaultOwner")
        }) {
            // Publish qualification only after this exact selected scope passes
            // its own indexing check. A validated listing is not indexing proof.
            self.primary_library = self.initialize_library(zone, "private").await?;
        }
        Ok(())
    }

    /// Fetch private libraries (lazily, first call triggers the HTTP request).
    pub async fn fetch_private_libraries(
        &mut self,
    ) -> anyhow::Result<&HashMap<String, PhotoLibrary>> {
        if self.private_libraries.is_none() {
            self.fetch_private_zones().await?;
            let zones = self
                .private_zones
                .as_ref()
                .context("Private zone list was not cached")?;
            let libs = self.initialize_libraries(zones, "private").await?;
            if let Some(primary) = libs.get(PRIMARY_ZONE_NAME)
                && primary.is_private_default_owner()
            {
                self.primary_library = primary.clone();
            }
            self.private_libraries = Some(libs);
        }
        self.private_libraries
            .as_ref()
            .context("Internal error: private iCloud Photos libraries were not cached")
    }

    /// Fetch shared libraries (lazily, first call triggers the HTTP request).
    pub async fn fetch_shared_libraries(
        &mut self,
    ) -> anyhow::Result<&HashMap<String, PhotoLibrary>> {
        if self.shared_libraries.is_none() {
            let zones = self.fetch_zone_list("shared").await?;
            let libs = self.initialize_libraries(&zones, "shared").await?;
            self.shared_libraries = Some(libs);
        }
        self.shared_libraries
            .as_ref()
            .context("Internal error: shared iCloud Photos libraries were not cached")
    }

    async fn fetch_zone_list(&self, library_type: &str) -> anyhow::Result<Vec<cloudkit::Zone>> {
        let service_endpoint = self.get_service_endpoint(library_type);
        let url = format!("{service_endpoint}/zones/list");

        let body = session::retry_post_changes_body(
            self.session.as_ref(),
            &url,
            "{}",
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;

        let response = changes_json::parse(&body)?;
        let zones = response
            .get("zones")
            .and_then(Value::as_array)
            .context("Missing or invalid library zone list")?;
        anyhow::ensure!(
            response
                .get("moreComing")
                .is_none_or(|value| value.as_bool() == Some(false))
                && response
                    .get("continuationMarker")
                    .is_none_or(Value::is_null),
            "Unsupported library zone list continuation"
        );
        anyhow::ensure!(
            zones
                .iter()
                .all(|zone| zone.get("serverErrorCode").is_none_or(Value::is_null)),
            "Library zone discovery failed"
        );
        let zone_list: cloudkit::ZoneListResponse = serde_json::from_value(response)
            .map_err(|_invalid| anyhow::anyhow!("Invalid library zone list shape"))?;
        let mut names = std::collections::HashSet::with_capacity(zone_list.zones.len());
        for zone in &zone_list.zones {
            anyhow::ensure!(
                !zone.zone_id.zone_name.trim().is_empty()
                    && names.insert(zone.zone_id.zone_name.as_str()),
                "Ambiguous library zone discovery"
            );
        }

        Ok(zone_list.zones)
    }

    async fn initialize_library(
        &self,
        zone: &cloudkit::Zone,
        library_type: &str,
    ) -> anyhow::Result<PhotoLibrary> {
        let zone_name = &zone.zone_id.zone_name;
        let mut library = PhotoLibrary::new(
            self.get_service_endpoint(library_type),
            Arc::clone(&self.params),
            self.session.clone_box(),
            Arc::new(serde_json::to_value(&zone.zone_id)?),
            library_type.to_string(),
            self.retry_config,
        )
        .await
        .map_err(|error| {
            tracing::error!(zone = %zone_name, error = %error, "Failed to load library zone");
            anyhow::anyhow!("Could not load iCloud Photos library zone {zone_name}: {error}")
        })?;
        library.shadow_capture = self.shadow_capture.clone();
        tracing::debug!(zone = %zone_name, "Loaded library zone");
        Ok(library)
    }

    async fn initialize_libraries(
        &self,
        zones: &[cloudkit::Zone],
        library_type: &str,
    ) -> anyhow::Result<HashMap<String, PhotoLibrary>> {
        let mut libraries = HashMap::new();
        for zone in zones {
            if zone.deleted.unwrap_or(false) {
                continue;
            }
            let zone_name = &zone.zone_id.zone_name;
            if !is_photo_library_zone(zone_name) {
                tracing::debug!(zone = %zone_name, "Skipping zone that is not a photo library");
                continue;
            }
            let library = self.initialize_library(zone, library_type).await?;
            libraries.insert(zone_name.clone(), library);
        }
        Ok(libraries)
    }

    /// Check if any zones have changes since the given sync token.
    ///
    /// This is the cheapest possible API call — returns immediately if nothing changed.
    /// Returns the response with the list of changed zones and a new database-level sync token.
    ///
    /// Pass `None` for `sync_token` on first call to get all zones (bootstrap).
    pub async fn changes_database(
        &self,
        sync_token: Option<&str>,
    ) -> anyhow::Result<ChangesDatabaseResponse> {
        let service_endpoint = self.get_service_endpoint("private");
        let url = format!(
            "{}/changes/database?{}",
            service_endpoint,
            encode_params(&self.params)
        );
        let body = queries::build_changes_database_request(sync_token);
        let response = session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;
        let parsed: ChangesDatabaseResponse = serde_json::from_value(response)
            .context("Could not read Apple's changes/database response")?;
        Ok(parsed)
    }
}

#[cfg(test)]
impl PhotosService {
    /// Test-only constructor that bypasses [`Self::new`]'s indexing
    /// check. Mirrors the `make_service` helper used by this module's
    /// own tests, but visible to other crate-internal test modules so
    /// they can drive `changes_database`, `fetch_*_libraries`, etc.
    /// without spinning up real CloudKit traffic.
    pub(crate) fn for_testing(
        session: Box<dyn PhotosSession>,
        params: HashMap<String, Value>,
    ) -> Self {
        let dummy_library = PhotoLibrary::new_stub(session.clone_box());
        Self {
            shadow_capture: None,
            service_root: "https://p00-ckdatabasews.icloud.com".to_string(),
            session,
            params: Arc::new(params),
            primary_library: dummy_library,
            private_zones: None,
            private_libraries: None,
            shared_libraries: None,
            retry_config: RetryConfig::default(),
        }
    }

    /// Test-only constructor with pre-populated library maps. Lets
    /// `resolve_libraries` tests exercise multi-library matching without
    /// spinning up CloudKit fixtures for the lazy zone-listing endpoints.
    pub(crate) fn for_testing_with_libraries(
        session: Box<dyn PhotosSession>,
        primary: PhotoLibrary,
        private: HashMap<String, PhotoLibrary>,
        shared: HashMap<String, PhotoLibrary>,
    ) -> Self {
        Self {
            shadow_capture: None,
            service_root: "https://p00-ckdatabasews.icloud.com".to_string(),
            session,
            params: Arc::new(HashMap::new()),
            primary_library: primary,
            private_zones: None,
            private_libraries: Some(private),
            shared_libraries: Some(shared),
            retry_config: RetryConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Captured request from a stub session call.
    #[derive(Debug, Clone)]
    struct CapturedRequest {
        url: String,
        body: String,
    }

    /// Stub session that captures the POST request and returns a canned response.
    struct CapturingSession {
        response: Value,
        captured: Arc<Mutex<Option<CapturedRequest>>>,
    }

    #[async_trait::async_trait]
    impl session::PhotosSession for CapturingSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            *self.captured.lock().unwrap() = Some(CapturedRequest {
                url: url.to_string(),
                body,
            });
            Ok(self.response.clone())
        }

        fn clone_box(&self) -> Box<dyn session::PhotosSession> {
            panic!("CapturingSession::clone_box should not be called");
        }
    }

    /// Build a `PhotosService` directly, bypassing `new()` which requires indexing check.
    fn make_service(
        session: Box<dyn session::PhotosSession>,
        params: HashMap<String, Value>,
    ) -> PhotosService {
        let dummy_library = PhotoLibrary::new_stub(Box::new(PanicSession));

        PhotosService {
            shadow_capture: None,
            service_root: "https://p00-ckdatabasews.icloud.com".to_string(),
            session,
            params: Arc::new(params),
            primary_library: dummy_library,
            private_zones: None,
            private_libraries: None,
            shared_libraries: None,
            retry_config: RetryConfig::default(),
        }
    }

    /// Stub that panics on any call — used for the dummy primary library.
    struct PanicSession;

    #[async_trait::async_trait]
    impl session::PhotosSession for PanicSession {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            panic!("PanicSession::post should not be called");
        }

        fn clone_box(&self) -> Box<dyn session::PhotosSession> {
            Box::new(PanicSession)
        }
    }

    #[tokio::test]
    async fn test_changes_database_none_token() {
        let captured = Arc::new(Mutex::new(None));
        let response = json!({
            "syncToken": "db-token-abc",
            "moreComing": false,
            "zones": [
                {
                    "zoneID": {"zoneName": "PrimarySync"},
                    "syncToken": "zone-token-1"
                }
            ]
        });
        let session = CapturingSession {
            response,
            captured: Arc::clone(&captured),
        };

        let svc = make_service(Box::new(session), HashMap::new());
        let result = svc.changes_database(None).await.unwrap();

        assert_eq!(result.sync_token, "db-token-abc");
        assert!(!result.more_coming);
        assert_eq!(result.zones.len(), 1);
        assert_eq!(result.zones[0].zone_id.zone_name, "PrimarySync");
        assert_eq!(result.zones[0].sync_token, "zone-token-1");

        let req = captured.lock().unwrap().clone().unwrap();
        assert!(req.url.contains("/changes/database"));
        assert!(req.url.contains("production/private"));
        let body: Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body, json!({}));
    }

    #[tokio::test]
    async fn test_changes_database_with_token() {
        let captured = Arc::new(Mutex::new(None));
        let response = json!({
            "syncToken": "db-token-new",
            "moreComing": false,
            "zones": []
        });
        let session = CapturingSession {
            response,
            captured: Arc::clone(&captured),
        };

        let svc = make_service(Box::new(session), HashMap::new());
        let result = svc.changes_database(Some("db-token-old")).await.unwrap();

        assert_eq!(result.sync_token, "db-token-new");
        assert!(!result.more_coming);
        assert!(result.zones.is_empty());

        let req = captured.lock().unwrap().clone().unwrap();
        let body: Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body, json!({"syncToken": "db-token-old"}));
    }

    #[tokio::test]
    async fn test_changes_database_with_params_in_url() {
        let captured = Arc::new(Mutex::new(None));
        let response = json!({
            "syncToken": "tok",
            "moreComing": false,
            "zones": []
        });
        let session = CapturingSession {
            response,
            captured: Arc::clone(&captured),
        };

        let mut params = HashMap::new();
        params.insert("remapEnums".to_string(), Value::Bool(true));
        params.insert("getCurrentSyncToken".to_string(), Value::Bool(true));

        let svc = make_service(Box::new(session), params);
        svc.changes_database(None).await.unwrap();

        let req = captured.lock().unwrap().clone().unwrap();
        assert!(req.url.contains("getCurrentSyncToken=true"));
        assert!(req.url.contains("remapEnums=true"));
    }

    #[tokio::test]
    async fn test_changes_database_multiple_zones() {
        let captured = Arc::new(Mutex::new(None));
        let response = json!({
            "syncToken": "db-tok",
            "moreComing": true,
            "zones": [
                {
                    "zoneID": {"zoneName": "PrimarySync"},
                    "syncToken": "ps-tok"
                },
                {
                    "zoneID": {"zoneName": "SharedSync-ABCD"},
                    "syncToken": "ss-tok"
                }
            ]
        });
        let session = CapturingSession {
            response,
            captured: Arc::clone(&captured),
        };

        let svc = make_service(Box::new(session), HashMap::new());
        let result = svc.changes_database(Some("prev-tok")).await.unwrap();

        assert_eq!(result.sync_token, "db-tok");
        assert!(result.more_coming);
        assert_eq!(result.zones.len(), 2);
        assert_eq!(result.zones[0].zone_id.zone_name, "PrimarySync");
        assert_eq!(result.zones[1].zone_id.zone_name, "SharedSync-ABCD");
        assert_eq!(result.zones[1].sync_token, "ss-tok");
    }

    #[test]
    fn test_changes_database_url_construction() {
        let service_root = "https://p00-ckdatabasews.icloud.com";
        let endpoint = PhotosService::build_service_endpoint(service_root, "private");
        assert_eq!(
            endpoint,
            "https://p00-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/private"
        );
    }

    #[test]
    fn test_build_service_endpoint_shared_library_type() {
        assert_eq!(
            PhotosService::build_service_endpoint("https://example.test", "shared"),
            "https://example.test/database/1/com.apple.photos.cloud/production/shared"
        );
    }

    #[test]
    fn test_get_service_endpoint_uses_service_root_and_type() {
        let svc = make_service(Box::new(PanicSession), HashMap::new());
        assert_eq!(
            svc.get_service_endpoint("private"),
            "https://p00-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/private"
        );
        assert_eq!(
            svc.get_service_endpoint("shared"),
            "https://p00-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/shared"
        );
    }

    /// Cached private discovery keeps default selection from repeating network
    /// work; an empty list cannot synthesize a primary owner.
    #[tokio::test]
    async fn test_get_library_primary_sync_uses_cached_private_discovery() {
        let mut svc = make_service(Box::new(PanicSession), HashMap::new());
        svc.private_libraries = Some(HashMap::new());
        let lib = svc.get_library("PrimarySync").await.unwrap();
        assert_eq!(lib.zone_name(), "PrimarySync");
        assert!(!lib.is_private_default_owner());
    }

    #[derive(Clone)]
    struct RawDiscoverySession {
        private: Arc<Mutex<Vec<u8>>>,
        shared: Vec<u8>,
        listings: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for RawDiscoverySession {
        async fn post(
            &self,
            _url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(request["query"]["recordType"], "CheckIndexingState");
            Ok(json!({"records":[{"fields":{"state":{"value":"FINISHED"}}}]}))
        }
        async fn post_changes_body(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Vec<u8>> {
            assert!(url.ends_with("/zones/list"));
            self.listings.lock().unwrap().push(url.to_owned());
            Ok(if url.contains("/private/") {
                self.private.lock().unwrap().clone()
            } else {
                self.shared.clone()
            })
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn raw_discovery(private: Vec<u8>) -> RawDiscoverySession {
        RawDiscoverySession {
            private: Arc::new(Mutex::new(private)),
            shared: serde_json::to_vec(&json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}]})).unwrap(),
            listings: Arc::new(Mutex::new(Vec::new())),
        }
    }

    #[derive(Clone)]
    struct UnrelatedIndexingSession {
        discovery: RawDiscoverySession,
        indexing_requests: Arc<Mutex<Vec<Value>>>,
        blocked_zone: Arc<Mutex<Option<String>>>,
    }

    #[async_trait::async_trait]
    impl PhotosSession for UnrelatedIndexingSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let request: Value = serde_json::from_str(&body)?;
            assert_eq!(request["query"]["recordType"], "CheckIndexingState");
            self.indexing_requests
                .lock()
                .unwrap()
                .push(request["zoneID"].clone());
            if request["zoneID"]["ownerRecordName"] == "_defaultOwner"
                && self.blocked_zone.lock().unwrap().as_deref()
                    == request["zoneID"]["zoneName"].as_str()
            {
                return Ok(json!({"records":[{"fields":{"state":{"value":"RUNNING"}}}]}));
            }
            self.discovery.post(url, body, headers).await
        }
        async fn post_changes_body(
            &self,
            url: &str,
            body: String,
            headers: &[(&str, &str)],
        ) -> anyhow::Result<Vec<u8>> {
            self.discovery.post_changes_body(url, body, headers).await
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    #[tokio::test]
    async fn primary_discovery_default_selection_does_not_initialize_unselected_private_libraries()
    {
        let session = UnrelatedIndexingSession {
            discovery: raw_discovery(serde_json::to_vec(&json!({"zones":[
                {"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},
                {"zoneID":{"zoneName":"SharedSync-unselected","ownerRecordName":"_defaultOwner"}}
            ]})).unwrap()),
            indexing_requests: Arc::new(Mutex::new(Vec::new())),
            blocked_zone: Arc::new(Mutex::new(Some("SharedSync-unselected".into()))),
        };
        let mut service = PhotosService::new(
            "https://example.invalid".into(),
            Box::new(session.clone()),
            HashMap::new(),
            RetryConfig::default(),
        )
        .await
        .unwrap();
        let selected = crate::commands::resolve_libraries(
            &crate::selection::LibrarySelector::default(),
            &mut service,
        )
        .await
        .expect("unselected private library indexing must not block selected primary ownership");
        assert_eq!(selected.len(), 1);
        assert!(
            selected[0].is_private_default_owner(),
            "ownership must remain explicitly provider-qualified"
        );
        let requests = session.indexing_requests.lock().unwrap().clone();
        assert!(
            requests
                .iter()
                .all(|zone| zone["zoneName"] == PRIMARY_ZONE_NAME)
        );
        assert_eq!(requests.last().unwrap()["ownerRecordName"], "_defaultOwner");
        assert_eq!(session.discovery.listings.lock().unwrap().len(), 1);
        assert!(
            service
                .get_library(PRIMARY_ZONE_NAME)
                .await
                .unwrap()
                .is_private_default_owner()
        );
        assert_eq!(*session.indexing_requests.lock().unwrap(), requests);
        assert_eq!(session.discovery.listings.lock().unwrap().len(), 1);
        // Full discovery still initializes its requested complete library map.
        let error = service.all_libraries().await.unwrap_err();
        assert!(error.to_string().contains("RUNNING"));
        assert!(
            service.private_libraries.is_none(),
            "failed full initialization must not publish a partial library map"
        );
        assert!(
            service
                .get_library(PRIMARY_ZONE_NAME)
                .await
                .unwrap()
                .is_private_default_owner()
        );
        assert_eq!(session.discovery.listings.lock().unwrap().len(), 1);
        *session.blocked_zone.lock().unwrap() = None;
        assert_eq!(service.all_libraries().await.unwrap().len(), 3);
        assert!(service.private_libraries.is_some());
        assert_eq!(session.discovery.listings.lock().unwrap().len(), 2);
        let requests = session.indexing_requests.lock().unwrap().len();
        assert_eq!(service.all_libraries().await.unwrap().len(), 3);
        assert_eq!(session.indexing_requests.lock().unwrap().len(), requests);
        assert_eq!(session.discovery.listings.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn primary_discovery_selected_indexing_failure_does_not_publish_ownership() {
        let session = UnrelatedIndexingSession {
            discovery: raw_discovery(
                serde_json::to_vec(&json!({"zones":[
                    {"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}
                ]}))
                .unwrap(),
            ),
            indexing_requests: Arc::new(Mutex::new(Vec::new())),
            blocked_zone: Arc::new(Mutex::new(Some(PRIMARY_ZONE_NAME.into()))),
        };
        let mut service = PhotosService::new(
            "https://example.invalid".into(),
            Box::new(session.clone()),
            HashMap::new(),
            RetryConfig::default(),
        )
        .await
        .unwrap();
        assert!(
            service
                .get_library(PRIMARY_ZONE_NAME)
                .await
                .unwrap_err()
                .to_string()
                .contains("RUNNING")
        );
        assert!(!service.primary_library.is_private_default_owner());
        assert!(service.private_zones.is_some());
        assert!(service.private_libraries.is_none());
        *session.blocked_zone.lock().unwrap() = None;
        assert!(
            service
                .get_library(PRIMARY_ZONE_NAME)
                .await
                .unwrap()
                .is_private_default_owner()
        );
        assert_eq!(session.discovery.listings.lock().unwrap().len(), 1);
        assert_eq!(session.indexing_requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn primary_discovery_requires_unique_complete_error_free_zone_list_before_caching() {
        let invalid = [
            "{}",
            r#"{"zones":null}"#,
            r#"{"zones":{}}"#,
            r#"{"zones":[{"zoneID":{"zoneName":""}}]}"#,
            r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},{"zoneID":{"zoneName":"PrimarySync"}}]}"#,
            r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},{"zoneID":{"zoneName":"PrimarySync"},"deleted":true}]}"#,
            r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"other","ownerRecordName":"_defaultOwner"}}]}"#,
            r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"serverErrorCode":"PRIVATE-ERROR"}]}"#,
            r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"deleted":"false"}]}"#,
            r#"{"zones":[],"moreComing":true}"#,
            r#"{"zones":[],"moreComing":null}"#,
            r#"{"zones":[],"continuationMarker":"PRIVATE-CURSOR"}"#,
        ];
        for body in invalid {
            let session = raw_discovery(body.as_bytes().to_vec());
            let mut svc = make_service(Box::new(session.clone()), HashMap::new());
            let error = svc
                .get_library(PRIMARY_ZONE_NAME)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                !error.contains("PRIVATE-ERROR")
                    && !error.contains("PRIVATE-CURSOR")
                    && !error.contains("PrimarySync"),
                "{error}"
            );
            assert!(svc.private_libraries.is_none());
            assert!(svc.private_zones.is_none());
            assert!(!svc.primary_library.is_private_default_owner());
            // Failure did not publish a cache; valid evidence can recover.
            *session.private.lock().unwrap() = serde_json::to_vec(&json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}}]})).unwrap();
            assert!(
                svc.get_library(PRIMARY_ZONE_NAME)
                    .await
                    .unwrap()
                    .is_private_default_owner()
            );
            assert_eq!(session.listings.lock().unwrap().len(), 2);
            assert!(
                svc.get_library(PRIMARY_ZONE_NAME)
                    .await
                    .unwrap()
                    .is_private_default_owner()
            );
            assert_eq!(session.listings.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn primary_discovery_does_not_infer_owner_or_use_shared_database_evidence() {
        for zones in [
            json!([]),
            json!([{"zoneID":{"zoneName":"PrimarySync"}}]),
            json!([{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":null}}]),
            json!([{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":""}}]),
            json!([{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"different"}}]),
            json!([{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"deleted":true}]),
            json!([{"zoneID":{"zoneName":"PrimarySyncExtra","ownerRecordName":"_defaultOwner"}}]),
        ] {
            let session = raw_discovery(serde_json::to_vec(&json!({"zones":zones})).unwrap());
            let mut svc = make_service(Box::new(session.clone()), HashMap::new());
            let primary = svc.get_library(PRIMARY_ZONE_NAME).await.unwrap();
            assert!(!primary.is_private_default_owner());
            assert_eq!(
                session.listings.lock().unwrap().len(),
                1,
                "default lookup needs only private discovery"
            );
            let all = svc.all_libraries().await.unwrap();
            // Shared discovery contains an explicit PrimarySync owner. It must
            // never replace or authorize the selected private primary library.
            assert!(
                !svc.get_library(PRIMARY_ZONE_NAME)
                    .await
                    .unwrap()
                    .is_private_default_owner()
            );
            assert_eq!(
                all.iter()
                    .filter(|lib| lib.zone_name() == PRIMARY_ZONE_NAME
                        && lib.is_private_default_owner())
                    .count(),
                0
            );
            assert_eq!(session.listings.lock().unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn primary_discovery_qualifies_all_first_and_reuses_cached_owner() {
        let session = raw_discovery(serde_json::to_vec(&json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner","zoneType":"REGULAR_CUSTOM_ZONE"}}]})).unwrap());
        let mut svc = make_service(Box::new(session.clone()), HashMap::new());
        let all = svc.all_libraries().await.unwrap();
        let primary = all
            .iter()
            .find(|lib| lib.is_private_default_owner())
            .unwrap();
        assert_eq!(primary.zone_name(), PRIMARY_ZONE_NAME);
        assert!(
            svc.get_library(PRIMARY_ZONE_NAME)
                .await
                .unwrap()
                .is_private_default_owner()
        );
        assert_eq!(session.listings.lock().unwrap().len(), 2);
    }

    /// Cloneable capturing session - unlike CapturingSession above,
    /// clone_box produces a working clone so library initialization can hand
    /// sessions to each constructed PhotoLibrary.
    struct CloneableSession {
        response: Value,
    }

    #[async_trait::async_trait]
    impl session::PhotosSession for CloneableSession {
        async fn post(
            &self,
            _url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if body.contains("CheckIndexingState") {
                return Ok(json!({
                    "records": [{
                        "fields": {"state": {"value": "FINISHED"}}
                    }]
                }));
            }
            Ok(self.response.clone())
        }

        fn clone_box(&self) -> Box<dyn session::PhotosSession> {
            Box::new(CloneableSession {
                response: self.response.clone(),
            })
        }
    }

    struct RunningIndexSession {
        zone_response: Value,
    }

    #[async_trait::async_trait]
    impl session::PhotosSession for RunningIndexSession {
        async fn post(
            &self,
            _url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if body.contains("CheckIndexingState") {
                return Ok(json!({
                    "records": [{
                        "fields": {"state": {"value": "RUNNING"}}
                    }]
                }));
            }
            Ok(self.zone_response.clone())
        }

        fn clone_box(&self) -> Box<dyn session::PhotosSession> {
            Box::new(RunningIndexSession {
                zone_response: self.zone_response.clone(),
            })
        }
    }

    #[tokio::test]
    async fn test_fetch_private_libraries_parses_zone_list() {
        let session = CloneableSession {
            response: json!({
                "zones": [
                    {
                        "zoneID": {"zoneName": "PrimarySync"},
                        "syncToken": "tok",
                    },
                    {
                        "zoneID": {"zoneName": "SharedSync-ABC"},
                        "syncToken": "tok2",
                    }
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let libs = svc.fetch_private_libraries().await.unwrap();
        assert_eq!(libs.len(), 2);
        assert!(libs.contains_key("PrimarySync"));
        assert!(libs.contains_key("SharedSync-ABC"));
    }

    /// Deleted zones are filtered out.
    #[tokio::test]
    async fn test_fetch_libraries_skips_deleted_zones() {
        let session = CloneableSession {
            response: json!({
                "zones": [
                    {
                        "zoneID": {"zoneName": "PrimarySync"},
                        "syncToken": "tok",
                    },
                    {
                        "zoneID": {"zoneName": "SharedSync-GONE"},
                        "syncToken": "tok2",
                        "deleted": true,
                    }
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let libs = svc.fetch_private_libraries().await.unwrap();
        assert_eq!(libs.len(), 1);
        assert!(libs.contains_key("PrimarySync"));
        assert!(!libs.contains_key("SharedSync-GONE"));
    }

    /// CMM-{UUID} share-link zones are filtered out of the library map.
    /// They use a different record schema and aren't Shared Photo Libraries,
    /// so probing them produces noisy errors and they can't be synced.
    #[tokio::test]
    async fn test_fetch_libraries_skips_cmm_share_link_zones() {
        let session = CloneableSession {
            response: json!({
                "zones": [
                    {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "t"},
                    {
                        "zoneID": {"zoneName": "CMM-657AE284-D1E0-4C7F-9B4D-987888651AC6"},
                        "syncToken": "t2",
                    },
                    {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "t3"}
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let libs = svc.fetch_private_libraries().await.unwrap();
        assert!(libs.contains_key("PrimarySync"));
        assert!(libs.contains_key("SharedSync-ABCD"));
        assert!(
            !libs.contains_key("CMM-657AE284-D1E0-4C7F-9B4D-987888651AC6"),
            "CMM zones must be skipped: {:?}",
            libs.keys().collect::<Vec<_>>()
        );
    }

    /// Only `PrimarySync` and `SharedSync-{UUID}` are photo libraries; other
    /// zones in the list are not enumerable and must be skipped.
    #[tokio::test]
    async fn test_fetch_libraries_skips_non_library_zones() {
        let session = CloneableSession {
            response: json!({
                "zones": [
                    {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "t"},
                    {"zoneID": {"zoneName": "SharedSync-ABCD"}, "syncToken": "t2"},
                    {"zoneID": {"zoneName": "SharedCollection-254E3E8C"}, "syncToken": "t3"},
                    {"zoneID": {"zoneName": "CMM-657AE284"}, "syncToken": "t4"},
                    {"zoneID": {"zoneName": "SharedSyncExtra"}, "syncToken": "t5"},
                    {"zoneID": {"zoneName": "PrimarySyncExtra"}, "syncToken": "t6"}
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let libs = svc.fetch_private_libraries().await.unwrap();
        let names: Vec<_> = libs.keys().map(String::as_str).collect();
        assert!(libs.contains_key("PrimarySync"), "got {names:?}");
        assert!(libs.contains_key("SharedSync-ABCD"), "got {names:?}");
        assert_eq!(libs.len(), 2, "only photo libraries, got {names:?}");
    }

    #[tokio::test]
    async fn test_fetch_libraries_fails_on_non_deleted_zone_initialization_failure() {
        let session = RunningIndexSession {
            zone_response: json!({
                "zones": [
                    {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "tok"},
                    {
                        "zoneID": {"zoneName": "CMM-657AE284-D1E0-4C7F-9B4D-987888651AC6"},
                        "syncToken": "tok2",
                    },
                    {
                        "zoneID": {"zoneName": "SharedSync-GONE"},
                        "syncToken": "tok3",
                        "deleted": true,
                    }
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let err = svc.fetch_private_libraries().await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("PrimarySync") && msg.contains("RUNNING"),
            "regular non-deleted zone failure must fail discovery: {msg}"
        );
        assert!(
            svc.private_libraries.is_none(),
            "failed discovery must not cache a partial private library map"
        );
    }

    /// Second call to fetch_private_libraries reuses the cached map
    /// rather than re-issuing the HTTP request. A session that only
    /// responds correctly once would fail on the second call if caching
    /// were broken.
    #[tokio::test]
    async fn test_fetch_private_libraries_is_lazy_and_cached() {
        let session = CloneableSession {
            response: json!({
                "zones": [{"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "t"}]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        // First call hits the stub.
        let libs1 = svc.fetch_private_libraries().await.unwrap();
        let first_len = libs1.len();
        // Second call returns the cached map.
        let libs2 = svc.fetch_private_libraries().await.unwrap();
        assert_eq!(libs2.len(), first_len);
    }

    #[tokio::test]
    async fn test_get_library_unknown_returns_error() {
        let session = CloneableSession {
            response: json!({"zones": []}),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let err = svc.get_library("DoesNotExist").await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("iCloud Photos library") && msg.contains("DoesNotExist"),
            "error should name the unknown library: {msg}"
        );
    }

    /// Session that returns different zones depending on whether the
    /// URL path contains `/private/` or `/shared/`. Needed to test
    /// `all_libraries` which calls both.
    struct RoutingSession {
        private_response: Value,
        shared_response: Value,
    }

    #[async_trait::async_trait]
    impl session::PhotosSession for RoutingSession {
        async fn post(
            &self,
            url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if body.contains("CheckIndexingState") {
                return Ok(json!({
                    "records": [{
                        "fields": {"state": {"value": "FINISHED"}}
                    }]
                }));
            }
            if url.contains("/private/") {
                Ok(self.private_response.clone())
            } else if url.contains("/shared/") {
                Ok(self.shared_response.clone())
            } else {
                anyhow::bail!("unexpected URL: {url}")
            }
        }

        fn clone_box(&self) -> Box<dyn session::PhotosSession> {
            Box::new(RoutingSession {
                private_response: self.private_response.clone(),
                shared_response: self.shared_response.clone(),
            })
        }
    }

    /// `all_libraries` returns the primary library plus the non-primary
    /// entries from the private zone list plus every shared zone.
    #[tokio::test]
    async fn test_all_libraries_combines_primary_private_and_shared() {
        let session = RoutingSession {
            private_response: json!({
                "zones": [
                    {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "t"},
                    {"zoneID": {"zoneName": "SharedSync-EXTRA"}, "syncToken": "t2"}
                ]
            }),
            shared_response: json!({
                "zones": [
                    {"zoneID": {"zoneName": "SharedSync-ONE"}, "syncToken": "t3"}
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let all = svc.all_libraries().await.unwrap();
        let names: Vec<_> = all.iter().map(|l| l.zone_name().to_string()).collect();

        // Primary appears exactly once (from the primary_library slot;
        // the private-list copy is filtered out).
        let primary_count = names.iter().filter(|n| *n == "PrimarySync").count();
        assert_eq!(
            primary_count, 1,
            "PrimarySync must appear once, got {names:?}"
        );
        assert!(
            names.contains(&"SharedSync-EXTRA".to_string()),
            "non-primary private zone must be included: {names:?}"
        );
        assert!(
            names.contains(&"SharedSync-ONE".to_string()),
            "shared zone must be included: {names:?}"
        );
    }
}
