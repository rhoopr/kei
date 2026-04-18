use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use base64::Engine;
use serde_json::{json, Value};
use tracing::warn;

use super::album::{PhotoAlbum, PhotoAlbumConfig};
use super::queries::encode_params;
use super::session::PhotosSession;
use super::smart_folders::smart_folders;
use crate::icloud::error::ICloudError;
use crate::retry::RetryConfig;

// Apple's sentinel folder IDs — these are containers, not real albums.
const ROOT_FOLDER: &str = "----Root-Folder----";
const PROJECT_ROOT_FOLDER: &str = "----Project-Root-Folder----";

/// Default page size for `CloudKit` queries.
const DEFAULT_PAGE_SIZE: usize = 100;

// CloudKit record/query types for photo enumeration.
const QUERY_ALL_LIST: &str = "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted";
const QUERY_ALL_OBJ: &str = "CPLAssetByAssetDateWithoutHiddenOrDeleted";
const QUERY_FOLDER_LIST: &str = "CPLContainerRelationLiveByAssetDate";

pub struct PhotoLibrary {
    service_endpoint: Arc<str>,
    params: Arc<HashMap<String, Value>>,
    session: Box<dyn PhotosSession>,
    zone_id: Arc<Value>,
    library_type: Arc<str>,
    retry_config: RetryConfig,
}

impl Clone for PhotoLibrary {
    fn clone(&self) -> Self {
        Self {
            service_endpoint: Arc::clone(&self.service_endpoint),
            params: Arc::clone(&self.params),
            session: self.session.clone_box(),
            zone_id: Arc::clone(&self.zone_id),
            library_type: Arc::clone(&self.library_type),
            retry_config: self.retry_config,
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
        zone_id: Arc<Value>,
        library_type: String,
        retry_config: RetryConfig,
    ) -> Result<Self, ICloudError> {
        let url = format!(
            "{}/records/query?{}",
            service_endpoint,
            encode_params(&params)
        );
        let service_endpoint: Arc<str> = Arc::from(service_endpoint);
        let library_type: Arc<str> = Arc::from(library_type);
        let body = json!({
            "query": {"recordType": "CheckIndexingState"},
            "zoneID": &*zone_id,
        });

        let response = super::session::retry_post(
            session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &retry_config,
        )
        .await
        .map_err(|e| {
            if let Some(ck) = e.downcast_ref::<super::session::CloudKitServerError>() {
                if ck.service_not_activated {
                    return ICloudError::ServiceNotActivated {
                        code: ck.code.to_string(),
                        reason: ck.reason.to_string(),
                    };
                }
            }
            if let Some(http_err) = e.downcast_ref::<super::session::HttpStatusError>() {
                // HTTP 421: HTTP/2 connection routed to the wrong CloudKit
                // partition. Caller resets the pool and retries.
                if http_err.status == 421 {
                    return ICloudError::MisdirectedRequest;
                }
                // HTTP 401 / 403 both route through SessionExpired so the sync
                // loop re-auths. 401 is the classic stale-session signal; 403
                // has many causes (rate limits, rotated routing cookies, and
                // ADP edge cases not caught by `i_cdp_enabled` or by the
                // CloudKit body errors handled above). If the 403 truly is
                // persistent ADP, AUTH_ERROR_THRESHOLD in the download
                // pipeline stops the sync rather than spamming retries.
                if http_err.status == 401 || http_err.status == 403 {
                    return ICloudError::SessionExpired {
                        status: http_err.status,
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
            retry_config,
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
                            service_endpoint: Arc::clone(&self.service_endpoint),
                            name: Arc::from(name),
                            list_type: Arc::from(def.list_type),
                            obj_type: Arc::from(def.obj_type),
                            query_filter: def.query_filter,
                            page_size: DEFAULT_PAGE_SIZE,
                            zone_id: Arc::clone(&self.zone_id),
                            retry_config: self.retry_config,
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

                let folder_id = record_name.clone();
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

                let query_filter = Some(Arc::new(json!([{
                    "fieldName": "parentId",
                    "comparator": "EQUALS",
                    "fieldValue": {"type": "STRING", "value": &folder_id},
                }])));

                albums.insert(
                    folder_name.clone(),
                    PhotoAlbum::new(
                        PhotoAlbumConfig {
                            params: Arc::clone(&self.params),
                            service_endpoint: Arc::clone(&self.service_endpoint),
                            name: Arc::from(folder_name),
                            list_type: Arc::from(QUERY_FOLDER_LIST),
                            obj_type: Arc::from(folder_obj_type),
                            query_filter,
                            page_size: DEFAULT_PAGE_SIZE,
                            zone_id: Arc::clone(&self.zone_id),
                            retry_config: self.retry_config,
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
                params: Arc::clone(&self.params),
                service_endpoint: Arc::clone(&self.service_endpoint),
                name: Arc::from(""),
                list_type: Arc::from(QUERY_ALL_LIST),
                obj_type: Arc::from(QUERY_ALL_OBJ),
                query_filter: None,
                page_size: DEFAULT_PAGE_SIZE,
                zone_id: Arc::clone(&self.zone_id),
                retry_config: self.retry_config,
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
            "zoneID": &*self.zone_id,
        });
        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;

        let query: super::cloudkit::QueryResponse =
            serde_json::from_value(response).context("failed to parse library query response")?;
        Ok(query.records)
    }

    /// Returns the zone name (e.g., "`PrimarySync`", "SharedSync-{UUID}").
    pub fn zone_name(&self) -> &str {
        self.zone_id
            .get("zoneName")
            .and_then(|v| v.as_str())
            .unwrap_or("PrimarySync")
    }

    /// Clone the session for a new album/library — preserves the shared
    /// cookie jar via the Arc inside `reqwest::Client`.
    fn clone_session(&self) -> Box<dyn PhotosSession> {
        self.session.clone_box()
    }
}

#[cfg(test)]
impl PhotoLibrary {
    /// Test-only constructor that bypasses the indexing check.
    pub(crate) fn new_stub(session: Box<dyn PhotosSession>) -> Self {
        Self {
            service_endpoint: Arc::from("https://stub.example.com"),
            params: Arc::new(HashMap::new()),
            session,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            library_type: Arc::from("private"),
            retry_config: RetryConfig::default(),
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
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            panic!("StubSession::post should not be called in zone_name tests");
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(StubSession)
        }
    }

    /// Build a `PhotoLibrary` directly (bypassing `new()` which requires a live session).
    fn make_library(zone_id: Value) -> PhotoLibrary {
        PhotoLibrary {
            service_endpoint: Arc::from("https://example.com"),
            params: Arc::new(HashMap::new()),
            session: Box::new(StubSession),
            zone_id: Arc::new(zone_id),
            library_type: Arc::from("personal"),
            retry_config: RetryConfig::default(),
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

    /// Stub that returns an HTTP 403 error (the typed error produced by `PhotosSession::post`).
    struct Forbidden403Session;

    #[async_trait::async_trait]
    impl PhotosSession for Forbidden403Session {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            Err(crate::icloud::photos::session::HttpStatusError {
                status: 403,
                url: "https://p60-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/private/records/query".into(),
                retry_after: None,
            }.into())
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(Forbidden403Session)
        }
    }

    #[tokio::test]
    async fn http_403_maps_to_session_expired() {
        // Bare HTTP 403 (without a CloudKit body error) has too many causes
        // to assume ADP. Route it through SessionExpired so the sync loop
        // re-authenticates once; genuine ADP is surfaced via `i_cdp_enabled`
        // before we reach CloudKit, and CloudKit-body errors (ZONE_NOT_FOUND,
        // ACCESS_DENIED) still map to ServiceNotActivated via `service_not_activated`.
        let err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(Forbidden403Session),
            Arc::new(json!({"zoneName": "PrimarySync"})),
            "private".into(),
            RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, ICloudError::SessionExpired { status: 403 }),
            "expected SessionExpired {{ 403 }} so the message tracks the actual status, got: {err:?}"
        );
        assert!(
            err.to_string().contains("HTTP 403"),
            "display must mention HTTP 403, got: {err}"
        );
    }

    /// Stub whose CloudKit body reports an `ACCESS_DENIED` service error.
    /// These are the ADP-class signals; they should still produce a clear
    /// `ServiceNotActivated` error with the ADP guidance message.
    struct AccessDeniedBodySession;

    #[async_trait::async_trait]
    impl PhotosSession for AccessDeniedBodySession {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            Ok(json!({
                "serverErrorCode": "ACCESS_DENIED",
                "reason": "private db access disabled for this account",
            }))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(AccessDeniedBodySession)
        }
    }

    #[tokio::test]
    async fn cloudkit_access_denied_still_maps_to_service_not_activated() {
        let err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(AccessDeniedBodySession),
            Arc::new(json!({"zoneName": "PrimarySync"})),
            "private".into(),
            RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, ICloudError::ServiceNotActivated { .. }),
            "ADP body signal must still surface ServiceNotActivated, got: {err:?}"
        );
        let display = err.to_string();
        assert!(
            display.contains("Advanced Data Protection"),
            "expected ADP guidance, got: {display}"
        );
    }

    /// Stub that returns HTTP 401, the signature of a stale cached session
    /// surviving the 421 auth-cache fallback.
    struct Unauthorized401Session;

    #[async_trait::async_trait]
    impl PhotosSession for Unauthorized401Session {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            Err(crate::icloud::photos::session::HttpStatusError {
                status: 401,
                url: "https://p60-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/private/records/query".into(),
                retry_after: None,
            }.into())
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(Unauthorized401Session)
        }
    }

    #[tokio::test]
    async fn http_401_maps_to_session_expired() {
        let err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(Unauthorized401Session),
            Arc::new(json!({"zoneName": "PrimarySync"})),
            "private".into(),
            RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, ICloudError::SessionExpired { status: 401 }),
            "expected SessionExpired {{ 401 }} so sync_loop can invalidate cache and \
             re-authenticate, got: {err:?}"
        );
    }

    /// Stub that returns HTTP 421, the signature of a misdirected CloudKit
    /// connection that survived the `init_photos_service` pool-reset retry.
    struct Misdirected421Session;

    #[async_trait::async_trait]
    impl PhotosSession for Misdirected421Session {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            Err(crate::icloud::photos::session::HttpStatusError {
                status: 421,
                url: "https://p60-ckdatabasews.icloud.com/database/1/com.apple.photos.cloud/production/private/records/query".into(),
                retry_after: None,
            }.into())
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(Misdirected421Session)
        }
    }

    #[tokio::test]
    async fn http_421_maps_to_misdirected_request() {
        let err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(Misdirected421Session),
            Arc::new(json!({"zoneName": "PrimarySync"})),
            "private".into(),
            RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
        )
        .await
        .unwrap_err();

        assert!(
            matches!(err, ICloudError::MisdirectedRequest),
            "expected MisdirectedRequest so sync_loop can invalidate cache and \
             force SRP re-auth, got: {err:?}"
        );
    }
}
