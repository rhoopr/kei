//! Photos service — fetches albums, assets, and download URLs from iCloud's
//! CloudKit-based photos backend. Mirrors the Python `PhotosService` class.

mod album;
pub(crate) mod asset;
pub mod cloudkit;
pub(crate) mod enc;
pub mod error;
mod library;
pub(crate) mod metadata;
pub mod queries;
pub mod session;
pub(crate) mod smart_folders;
pub mod types;

pub use album::PhotoAlbum;
pub use asset::{PhotoAsset, VersionsMap};
pub use library::PhotoLibrary;
pub use session::{PhotosSession, SyncTokenError};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use serde_json::{json, Value};

use crate::icloud::error::ICloudError;
use crate::icloud::photos::cloudkit::ChangesDatabaseResponse;
use crate::icloud::photos::queries::encode_params;
use crate::retry::RetryConfig;

pub struct PhotosService {
    service_root: String,
    session: Box<dyn PhotosSession>,
    params: Arc<HashMap<String, Value>>,
    primary_library: PhotoLibrary,
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

impl PhotosService {
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
            service_root,
            session,
            params,
            primary_library,
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
    /// Checks the primary library first ("`PrimarySync`"), then searches private
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
            "Unknown library: '{name}'. Run `kei list libraries` to see available libraries."
        )
    }

    /// Return all available libraries: primary + private (non-PrimarySync) + shared.
    pub async fn all_libraries(&mut self) -> anyhow::Result<Vec<PhotoLibrary>> {
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

    /// Fetch private libraries (lazily, first call triggers the HTTP request).
    pub async fn fetch_private_libraries(
        &mut self,
    ) -> anyhow::Result<&HashMap<String, PhotoLibrary>> {
        if self.private_libraries.is_none() {
            let libs = self.fetch_libraries("private").await?;
            self.private_libraries = Some(libs);
        }
        // Safe: we just ensured private_libraries is Some above
        #[allow(
            clippy::unreachable,
            reason = "private_libraries was set to Some on the line above; None is impossible here"
        )]
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
        #[allow(
            clippy::unreachable,
            reason = "shared_libraries was set to Some on the line above; None is impossible here"
        )]
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
            &self.retry_config,
        )
        .await?;

        let zone_list: cloudkit::ZoneListResponse =
            serde_json::from_value(response).context("failed to parse zone list response")?;

        for zone in &zone_list.zones {
            if zone.deleted.unwrap_or(false) {
                continue;
            }
            let zone_name = zone.zone_id.zone_name.clone();
            let zone_id = Arc::new(serde_json::to_value(&zone.zone_id)?);
            let ep = self.get_service_endpoint(library_type);
            let lib_session = self.session.clone_box();

            match PhotoLibrary::new(
                ep,
                Arc::clone(&self.params),
                lib_session,
                zone_id,
                library_type.to_string(),
                self.retry_config,
            )
            .await
            {
                Ok(lib) => {
                    tracing::debug!(zone = %zone_name, "Loaded library zone");
                    libraries.insert(zone_name, lib);
                }
                Err(e) => {
                    tracing::error!(zone = %zone_name, error = %e, "Failed to load library zone");
                }
            }
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
            .context("failed to parse changes database response")?;
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
            service_root: "https://p00-ckdatabasews.icloud.com".to_string(),
            session,
            params: Arc::new(params),
            primary_library: dummy_library,
            private_libraries: None,
            shared_libraries: None,
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
            service_root: "https://p00-ckdatabasews.icloud.com".to_string(),
            session,
            params: Arc::new(params),
            primary_library: dummy_library,
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

    /// `get_library("PrimarySync")` short-circuits and returns the
    /// pre-built primary library without hitting the network.
    #[tokio::test]
    async fn test_get_library_primary_sync_short_circuits() {
        // A session that panics on clone confirms we never spin up a
        // new PhotoLibrary for PrimarySync.
        let mut svc = make_service(Box::new(PanicSession), HashMap::new());
        let lib = svc.get_library("PrimarySync").await.unwrap();
        assert_eq!(lib.zone_name(), "PrimarySync");
    }

    /// Cloneable capturing session - unlike CapturingSession above,
    /// clone_box produces a working clone so fetch_libraries can hand
    /// sessions to each constructed PhotoLibrary.
    struct CloneableSession {
        response: Value,
    }

    #[async_trait::async_trait]
    impl session::PhotosSession for CloneableSession {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            Ok(self.response.clone())
        }

        fn clone_box(&self) -> Box<dyn session::PhotosSession> {
            Box::new(CloneableSession {
                response: self.response.clone(),
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
                        "zoneID": {"zoneName": "CMMLibrary-ABC"},
                        "syncToken": "tok2",
                    }
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let libs = svc.fetch_private_libraries().await.unwrap();
        assert_eq!(libs.len(), 2);
        assert!(libs.contains_key("PrimarySync"));
        assert!(libs.contains_key("CMMLibrary-ABC"));
    }

    /// Deleted zones are filtered out.
    #[tokio::test]
    async fn test_fetch_libraries_skips_deleted_zones() {
        let session = CloneableSession {
            response: json!({
                "zones": [
                    {
                        "zoneID": {"zoneName": "LiveZone"},
                        "syncToken": "tok",
                    },
                    {
                        "zoneID": {"zoneName": "GoneZone"},
                        "syncToken": "tok2",
                        "deleted": true,
                    }
                ]
            }),
        };
        let mut svc = make_service(Box::new(session), HashMap::new());
        let libs = svc.fetch_private_libraries().await.unwrap();
        assert_eq!(libs.len(), 1);
        assert!(libs.contains_key("LiveZone"));
        assert!(!libs.contains_key("GoneZone"));
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
            msg.contains("Unknown library") && msg.contains("DoesNotExist"),
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
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
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
                    {"zoneID": {"zoneName": "ExtraPrivate"}, "syncToken": "t2"}
                ]
            }),
            shared_response: json!({
                "zones": [
                    {"zoneID": {"zoneName": "SharedOne"}, "syncToken": "t3"}
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
            names.contains(&"ExtraPrivate".to_string()),
            "non-primary private zone must be included: {names:?}"
        );
        assert!(
            names.contains(&"SharedOne".to_string()),
            "shared zone must be included: {names:?}"
        );
    }
}
