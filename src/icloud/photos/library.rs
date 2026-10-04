use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use base64::Engine;
use serde_json::{Value, json};

use super::album::{
    DEFAULT_PAGE_SIZE, PhotoAlbum, PhotoAlbumConfig, QUERY_ALL_LIST, QUERY_ALL_OBJ,
    QUERY_FOLDER_LIST,
};
use super::queries::encode_params;
use super::session::PhotosSession;
use super::smart_folders::smart_folders;
use crate::icloud::error::ICloudError;

/// CloudKit zone name for the user's own (non-shared) library. Used as the
/// fallback when a `zone_id` JSON object lacks `zoneName`, as the backfill
/// value for the v8 schema migration, and as the default scope for
/// commands that operate library-blind (e.g. `import-existing`).
pub(crate) const PRIMARY_ZONE_NAME: &str = "PrimarySync";

/// Prefix CloudKit uses for every shared-library zone name. Centralised so
/// `--library` matching, the `{library}` truncation rule, and stub-library
/// constructors all classify zones the same way.
pub(crate) const SHARED_ZONE_PREFIX: &str = "SharedSync-";

/// True when `zone_name` is a CloudKit shared library (every name beginning
/// with [`SHARED_ZONE_PREFIX`]).
pub(crate) fn is_shared_zone(zone_name: &str) -> bool {
    zone_name.starts_with(SHARED_ZONE_PREFIX)
}
use crate::retry::RetryConfig;

// Apple's sentinel folder IDs — these are containers, not real albums.
const ROOT_FOLDER: &str = "----Root-Folder----";
const PROJECT_ROOT_FOLDER: &str = "----Project-Root-Folder----";

/// Map typed CloudKit initialization failures to the library-level iCloud
/// contract consumed by sync orchestration.
fn map_library_init_error(error: &anyhow::Error) -> ICloudError {
    if let Some(ck) = error.downcast_ref::<crate::icloud::photos::session::CloudKitServerError>()
        && ck.service_not_activated
    {
        return ICloudError::ServiceNotActivated {
            code: ck.code.to_string(),
            reason: ck.reason.to_string(),
        };
    }
    if let Some(http_err) = error.downcast_ref::<crate::icloud::photos::session::HttpStatusError>()
    {
        // HTTP 421: HTTP/2 connection routed to the wrong CloudKit
        // partition. Caller resets the pool and retries.
        if http_err.status == 421 {
            return ICloudError::MisdirectedRequest;
        }
        // HTTP 401 / 403 both route through SessionExpired so the sync loop
        // re-auths. 401 is the classic stale-session signal; 403 has many
        // causes (rate limits, rotated routing cookies, and ADP edge cases not
        // caught by `i_cdp_enabled` or by CloudKit body errors handled above).
        // If the 403 truly is persistent ADP, AUTH_ERROR_THRESHOLD in the
        // download pipeline stops the sync rather than spamming retries.
        if http_err.status == 401 || http_err.status == 403 {
            log_fido_security_key_hint(http_err);
            return ICloudError::SessionExpired {
                status: http_err.status,
            };
        }
    }
    ICloudError::Connection(error.to_string())
}

fn log_fido_security_key_hint(http_err: &crate::icloud::photos::session::HttpStatusError) {
    if http_err.status != 401 {
        return;
    }
    let Some(body) = http_err.body.as_deref() else {
        return;
    };
    if body.contains("no auth method found") {
        tracing::warn!(
            url = %http_err.url,
            body = %body,
            "CloudKit 401 'no auth method found' - usually means \
             FIDO/WebAuthn security keys are on the account (issue \
             #221). Remove them at Settings > Apple ID & iCloud > \
             Sign-In & Security > Security Keys."
        );
    }
}

pub struct PhotoLibrary {
    pub(crate) shadow_capture: Option<super::inbox::ShadowCapture>,
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
            shadow_capture: self.shadow_capture.clone(),
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
    /// Create a new `PhotoLibrary`, failing if indexing has not finished.
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
        .map_err(|e| map_library_init_error(&e))?;

        let query: super::cloudkit::QueryResponse =
            serde_json::from_value(response).map_err(|e| ICloudError::Connection(e.to_string()))?;
        let indexing_state = query.records.first().and_then(|r| {
            r.fields
                .get("state")
                .and_then(|f| f.get("value"))
                .and_then(Value::as_str)
        });
        let Some(indexing_state) = indexing_state else {
            return Err(ICloudError::Connection(
                "Apple did not report whether the photo library is fully indexed; results may be incomplete".into(),
            ));
        };
        if indexing_state != "FINISHED" {
            return Err(ICloudError::Connection(format!(
                "Apple says the photo library is still indexing ({indexing_state}); wait until indexing is finished, then retry."
            )));
        }

        Ok(Self {
            shadow_capture: None,
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

        // Smart folders are user-scoped and can include assets from shared
        // zones, so always inject their query definitions for every zone.
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
                        container_id: None,
                        cross_zone_sources: Vec::new(),
                    },
                    self.clone_session(),
                ),
            );
        }

        // Shared zones currently skip user-created album folder queries.
        if !is_shared_zone(self.zone_name()) {
            let folders = self.fetch_folders().await?;
            for folder in &folders {
                let record_name = &folder.record_name;
                if record_name == ROOT_FOLDER || record_name == PROJECT_ROOT_FOLDER {
                    continue;
                }
                if folder
                    .fields
                    .get("isDeleted")
                    .and_then(|f| f.get("value"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    continue;
                }

                let folder_obj_type =
                    format!("CPLContainerRelationNotDeletedByAssetDate:{record_name}");

                let folder_name = match folder
                    .fields
                    .get("albumNameEnc")
                    .and_then(|f| f.get("value"))
                    .and_then(Value::as_str)
                {
                    Some(enc) => {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(enc)
                            .unwrap_or_default();
                        let raw_name =
                            String::from_utf8(decoded).unwrap_or_else(|_| record_name.clone());
                        crate::download::paths::sanitize_path_component(&raw_name)
                    }
                    None => record_name.clone(),
                };

                let query_filter = Some(Arc::new(json!([{
                    "fieldName": "parentId",
                    "comparator": "EQUALS",
                    "fieldValue": {"type": "STRING", "value": record_name},
                }])));

                let name_arc: Arc<str> = Arc::from(folder_name.as_str());
                albums.insert(
                    folder_name,
                    PhotoAlbum::new(
                        PhotoAlbumConfig {
                            params: Arc::clone(&self.params),
                            service_endpoint: Arc::clone(&self.service_endpoint),
                            name: name_arc,
                            list_type: Arc::from(QUERY_FOLDER_LIST),
                            obj_type: Arc::from(folder_obj_type),
                            query_filter,
                            page_size: DEFAULT_PAGE_SIZE,
                            zone_id: Arc::clone(&self.zone_id),
                            retry_config: self.retry_config,
                            container_id: Some(Arc::from(record_name.as_str())),
                            cross_zone_sources: Vec::new(),
                        },
                        self.clone_session(),
                    ),
                );
            }
        }

        if let Some(capture) = &self.shadow_capture {
            for album in albums.values_mut() {
                album.set_shadow_capture(capture.clone(), Arc::clone(&self.library_type));
            }
        }
        Ok(albums)
    }

    pub fn all(&self) -> PhotoAlbum {
        let mut album = PhotoAlbum::new(
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
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            self.clone_session(),
        );
        if let Some(capture) = &self.shadow_capture {
            album.set_shadow_capture(capture.clone(), Arc::clone(&self.library_type));
        }
        album
    }

    async fn fetch_folders(&self) -> anyhow::Result<Vec<super::cloudkit::Record>> {
        let url = format!(
            "{}/records/query?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );

        // Apple's `records/query` defaults to resultsLimit=200 and returns
        // a `continuationMarker` when more records are available. Loop
        // until the marker is absent. Cap defensively at FOLDER_PAGE_CAP
        // pages so a server bug or pathological account can't spin
        // forever — at 200 records/page the cap is well past any
        // plausible real iCloud library.
        const FOLDER_PAGE_CAP: usize = 64;
        let mut all_records = Vec::new();
        let mut continuation: Option<String> = None;

        for page in 0..FOLDER_PAGE_CAP {
            let body = match &continuation {
                Some(marker) => json!({
                    "query": {"recordType": "CPLAlbumByPositionLive"},
                    "zoneID": &*self.zone_id,
                    "continuationMarker": marker,
                }),
                None => json!({
                    "query": {"recordType": "CPLAlbumByPositionLive"},
                    "zoneID": &*self.zone_id,
                }),
            };

            let response = super::session::retry_post(
                self.session.as_ref(),
                &url,
                &body.to_string(),
                &[("Content-type", "text/plain")],
                &self.retry_config,
            )
            .await?;

            let query: super::cloudkit::QueryResponse = serde_json::from_value(response)
                .context("Could not read Apple's library query response")?;

            let page_size = query.records.len();
            all_records.extend(query.records);

            match query.continuation_marker {
                Some(marker) if !marker.is_empty() => {
                    tracing::debug!(
                        page = page,
                        page_size,
                        running_total = all_records.len(),
                        "fetch_folders: continuationMarker present, fetching next page"
                    );
                    continuation = Some(marker);
                }
                _ => return Ok(all_records),
            }
        }

        // Fell through the cap with the marker still set. Surface loudly
        // so a regression that loosens the cap or a server pathology is
        // visible in routine logs. Return what we have rather than an
        // Err — partial results are more useful than a hard failure on
        // the album-discovery path, and downstream consumers
        // (`pick_album_names`) already bail loudly on missing names.
        tracing::warn!(
            cap = FOLDER_PAGE_CAP,
            running_total = all_records.len(),
            "fetch_folders: hit pagination cap with more pages still indicated; \
             returning truncated album list. If you genuinely have >12,000 albums \
             please file an issue."
        );
        Ok(all_records)
    }

    /// Returns the zone name (e.g., "`PrimarySync`", "SharedSync-{UUID}").
    pub fn zone_name(&self) -> &str {
        self.zone_id
            .get("zoneName")
            .and_then(|v| v.as_str())
            .unwrap_or(PRIMARY_ZONE_NAME)
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
            shadow_capture: None,
            service_endpoint: Arc::from("https://stub.example.com"),
            params: Arc::new(HashMap::new()),
            session,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            library_type: Arc::from("private"),
            retry_config: RetryConfig::default(),
        }
    }

    /// Test-only constructor that pins a custom zone name on the stub.
    /// Used by `resolve_libraries` tests that need distinct zones to
    /// exercise selector matching.
    pub(crate) fn new_stub_with_zone(session: Box<dyn PhotosSession>, zone_name: &str) -> Self {
        Self {
            shadow_capture: None,
            service_endpoint: Arc::from("https://stub.example.com"),
            params: Arc::new(HashMap::new()),
            session,
            zone_id: Arc::new(json!({"zoneName": zone_name})),
            library_type: Arc::from(if is_shared_zone(zone_name) {
                "shared"
            } else {
                "private"
            }),
            retry_config: RetryConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn http_status_error(status: u16) -> anyhow::Error {
        crate::icloud::photos::session::HttpStatusError {
            status,
            url: "https://p60-ckdatabasews.icloud.com/database".into(),
            retry_after: None,
            body: None,
        }
        .into()
    }

    #[test]
    fn map_library_init_error_maps_service_not_activated_body() {
        let err: anyhow::Error = crate::icloud::photos::session::CloudKitServerError {
            code: "ACCESS_DENIED".into(),
            reason: "private db access disabled".into(),
            retryable: false,
            service_not_activated: true,
        }
        .into();

        assert!(
            matches!(
                map_library_init_error(&err),
                ICloudError::ServiceNotActivated { .. }
            ),
            "service-not-activated CloudKit body should map to user-facing ADP/setup guidance"
        );
    }

    #[test]
    fn map_library_init_error_maps_421_401_and_403_http_statuses() {
        assert!(
            matches!(
                map_library_init_error(&http_status_error(421)),
                ICloudError::MisdirectedRequest
            ),
            "421 should force the SRP/routing recovery path"
        );
        assert!(
            matches!(
                map_library_init_error(&http_status_error(401)),
                ICloudError::SessionExpired { status: 401 }
            ),
            "401 should invalidate the cached session"
        );
        assert!(
            matches!(
                map_library_init_error(&http_status_error(403)),
                ICloudError::SessionExpired { status: 403 }
            ),
            "403 should route through bounded reauth rather than ADP guessing"
        );
    }

    #[test]
    fn map_library_init_error_peers_through_context_wrappers() {
        let err = http_status_error(421).context("initializing photos library");
        assert!(
            matches!(
                map_library_init_error(&err),
                ICloudError::MisdirectedRequest
            ),
            "context-wrapped 421 should keep the typed mapping"
        );
    }

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

    struct IndexingStateSession {
        response: Value,
    }

    #[async_trait::async_trait]
    impl PhotosSession for IndexingStateSession {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            Ok(self.response.clone())
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(IndexingStateSession {
                response: self.response.clone(),
            })
        }
    }

    /// Build a `PhotoLibrary` directly (bypassing `new()` which requires a live session).
    fn make_library(zone_id: Value) -> PhotoLibrary {
        PhotoLibrary {
            shadow_capture: None,
            service_endpoint: Arc::from("https://example.com"),
            params: Arc::new(HashMap::new()),
            session: Box::new(StubSession),
            zone_id: Arc::new(zone_id),
            library_type: Arc::from("personal"),
            retry_config: RetryConfig::default(),
        }
    }

    async fn new_library_with_indexing_response(
        response: Value,
    ) -> Result<PhotoLibrary, ICloudError> {
        PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(IndexingStateSession { response }),
            Arc::new(json!({"zoneName": "PrimarySync"})),
            "private".into(),
            RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
        )
        .await
    }

    #[tokio::test]
    async fn indexing_state_finished_initializes_library() {
        let lib = new_library_with_indexing_response(json!({
            "records": [{"fields": {"state": {"value": "FINISHED"}}}]
        }))
        .await
        .unwrap();

        assert_eq!(lib.zone_name(), "PrimarySync");
    }

    #[tokio::test]
    async fn indexing_state_running_fails_library_initialization() {
        let err = new_library_with_indexing_response(json!({
            "records": [{"fields": {"state": {"value": "RUNNING"}}}]
        }))
        .await
        .unwrap_err();

        assert!(
            err.to_string().contains("RUNNING") && err.to_string().contains("indexing is finished"),
            "error should name observed and expected indexing states: {err}"
        );
    }

    #[tokio::test]
    async fn indexing_state_missing_fails_library_initialization() {
        let err = new_library_with_indexing_response(json!({"records": []}))
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("did not report whether the photo library is fully indexed"),
            "missing indexing state must fail closed: {err}"
        );
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
                body: None,
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
                body: None,
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

    /// Stub that returns HTTP 401 with a CloudKit "no auth method found" body.
    /// This is the FIDO-account failure mode from issue #221 when the SRP-side
    /// up-front detection has been bypassed (e.g. Apple drops the
    /// `fsaChallenge` field in a future flow).
    struct NoAuthMethodFoundSession;

    #[async_trait::async_trait]
    impl PhotosSession for NoAuthMethodFoundSession {
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
                body: Some(
                    r#"{"serverErrorCode":"AUTHENTICATION_FAILED","reason":"no auth method found"}"#
                        .into(),
                ),
            }.into())
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(NoAuthMethodFoundSession)
        }
    }

    /// Even when the CloudKit 401 body identifies a FIDO-class failure, the
    /// mapping still routes to `SessionExpired` so the existing `sync_loop`
    /// re-auth path runs (capped by `AUTH_ERROR_THRESHOLD`). The up-front
    /// detection in `auth/srp.rs` is the primary fix; this test pins the
    /// belt-and-suspenders behavior so a future refactor doesn't silently
    /// stop producing the warning or change the retry contract.
    #[tokio::test]
    async fn http_401_no_auth_method_found_still_maps_to_session_expired() {
        let err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(NoAuthMethodFoundSession),
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
            "FIDO-class 401 bodies must still route to SessionExpired so \
             AUTH_ERROR_THRESHOLD in sync_loop bounds the re-auth loop, got: {err:?}"
        );
    }

    /// When the 401 body contains "no auth method found", the mapping must
    /// emit a WARN naming security keys as the likely cause and pointing
    /// at issue #221. This is the defense-in-depth hint that fires if a
    /// future Apple flow change bypasses the SRP-level FIDO detection.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn http_401_no_auth_method_found_logs_security_key_hint() {
        let _err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(NoAuthMethodFoundSession),
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
            logs_contain("no auth method found"),
            "WARN must quote the CloudKit signal so reporters can grep for it"
        );
        assert!(
            logs_contain("FIDO/WebAuthn security keys"),
            "WARN must name FIDO/WebAuthn as the likely cause"
        );
        assert!(
            logs_contain("#221"),
            "WARN must link to the tracking issue so the user can find context"
        );
        assert!(
            logs_contain("Sign-In & Security"),
            "WARN must include the settings path to remove the keys"
        );
    }

    /// A plain 401 without the "no auth method found" signal must NOT
    /// emit the FIDO hint — that would confuse users whose session is
    /// stale for ordinary reasons (expired trust token, rotated cookies).
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn http_401_without_fido_body_does_not_log_security_key_hint() {
        let _err = PhotoLibrary::new(
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
            !logs_contain("FIDO/WebAuthn security keys"),
            "ordinary stale-session 401 must not trigger the FIDO hint"
        );
        assert!(
            !logs_contain("Sign-In & Security"),
            "only 'no auth method found' bodies should produce the settings hint"
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
                body: None,
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

    // ── fetch_folders pagination (CF-1, 2026-05-03 robustness review) ────
    //
    // Apple's CloudKit `records/query` endpoint defaults to
    // resultsLimit=200 and returns a `continuationMarker` when more
    // records exist. Before the fix, `fetch_folders` issued a single
    // POST and trusted the response was complete — users with >200 user
    // albums silently lost the tail. Tail-album members would route
    // into the unfiled pass instead of `{album}` paths, and
    // `--album all !TailAlbum` would bail "Excluded album not found"
    // even though the album exists in iCloud.

    /// Mock session that returns a queue of canned responses on
    /// successive POSTs and records every body it received. Used to
    /// exercise the pagination loop in `fetch_folders` without a real
    /// HTTP server.
    struct PaginatingFolderSession {
        responses: std::sync::Mutex<Vec<Value>>,
        received_bodies: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl PaginatingFolderSession {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                received_bodies: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn body_log(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
            std::sync::Arc::clone(&self.received_bodies)
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for PaginatingFolderSession {
        async fn post(
            &self,
            _url: &str,
            body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            self.received_bodies.lock().unwrap().push(body);
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                anyhow::bail!("PaginatingFolderSession: out of canned responses");
            }
            Ok(responses.remove(0))
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            unimplemented!(
                "PaginatingFolderSession::clone_box is not exercised by \
                 fetch_folders; if a future test needs it, share Arcs over \
                 the response queue and body log"
            )
        }
    }

    fn make_folder_records(count: usize, prefix: &str) -> Vec<Value> {
        use base64::engine::general_purpose::STANDARD;
        (0..count)
            .map(|i| {
                let name = format!("Album-{prefix}-{i}");
                let enc = STANDARD.encode(name.as_bytes());
                json!({
                    "recordName": format!("{prefix}-{i}"),
                    "recordType": "CPLAlbum",
                    "fields": {
                        "albumNameEnc": {"value": enc},
                    },
                })
            })
            .collect()
    }

    fn make_library_with_session(session: Box<dyn PhotosSession>) -> PhotoLibrary {
        PhotoLibrary {
            shadow_capture: None,
            service_endpoint: Arc::from("https://example.com"),
            params: Arc::new(HashMap::new()),
            session,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            library_type: Arc::from("private"),
            retry_config: RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
        }
    }

    /// CF-1: with >200 albums, the second page must be fetched and
    /// merged. Pre-fix, fetch_folders returned only the first page.
    #[tokio::test]
    async fn fetch_folders_follows_continuation_marker_until_exhausted() {
        let page1 = make_folder_records(200, "p1");
        let page2 = make_folder_records(100, "p2");
        let session = PaginatingFolderSession::new(vec![
            json!({"records": page1, "continuationMarker": "ck-p1"}),
            json!({"records": page2}),
        ]);
        let body_log = session.body_log();
        let lib = make_library_with_session(Box::new(session));

        let folders = lib.fetch_folders().await.unwrap();
        assert_eq!(
            folders.len(),
            300,
            "fetch_folders must follow continuationMarker until the \
             response omits it; got {} records, expected 300",
            folders.len()
        );

        let bodies = body_log.lock().unwrap();
        assert_eq!(
            bodies.len(),
            2,
            "expected exactly two POSTs (one per page), got {}",
            bodies.len()
        );
        assert!(
            !bodies[0].contains("continuationMarker"),
            "first POST must not include a continuationMarker"
        );
        assert!(
            bodies[1].contains("ck-p1"),
            "second POST must echo the prior page's continuationMarker; \
             body was: {}",
            bodies[1]
        );
    }

    /// CF-1 negative: a single-page response (no marker) must short-circuit
    /// after one POST. Catches a regression that always sends a second
    /// request.
    #[tokio::test]
    async fn fetch_folders_single_page_response_does_not_paginate() {
        let page1 = make_folder_records(50, "only");
        let session = PaginatingFolderSession::new(vec![json!({"records": page1})]);
        let body_log = session.body_log();
        let lib = make_library_with_session(Box::new(session));

        let folders = lib.fetch_folders().await.unwrap();
        assert_eq!(folders.len(), 50);
        assert_eq!(
            body_log.lock().unwrap().len(),
            1,
            "no continuationMarker means no second POST"
        );
    }

    /// CF-1 defensive cap: if the server keeps returning a marker forever
    /// (server bug or pathological account), fetch_folders must bail
    /// rather than loop unbounded. The cap and warn message are part of
    /// the contract — a future refactor that drops them would silently
    /// allow infinite loops.
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn fetch_folders_caps_iteration_when_continuation_never_clears() {
        // Far more responses than the cap; every one carries a marker.
        let mut responses = Vec::new();
        for i in 0..200 {
            responses.push(json!({
                "records": make_folder_records(1, &format!("page-{i}")),
                "continuationMarker": format!("marker-{i}"),
            }));
        }
        let session = PaginatingFolderSession::new(responses);
        let body_log = session.body_log();
        let lib = make_library_with_session(Box::new(session));

        let _ = lib.fetch_folders().await;

        let post_count = body_log.lock().unwrap().len();
        assert!(
            post_count <= 64,
            "fetch_folders must stop within the documented cap; made {post_count} POSTs"
        );
        assert!(
            logs_contain("fetch_folders") && logs_contain("cap"),
            "the cap-fired event must surface in logs so a future regression \
             that silently loosens the cap is loud"
        );
    }
}
