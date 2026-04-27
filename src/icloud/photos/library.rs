use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use base64::Engine;
use serde_json::{json, Value};

use super::album::{PhotoAlbum, PhotoAlbumConfig};
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
                    // Defense-in-depth for FIDO/security-key accounts: the
                    // SRP path detects `fsaChallenge` / `keyNames` up front
                    // and bails with `AuthError::FidoNotSupported`, but if
                    // Apple drops those fields in some future flow, the
                    // failure mode is a CloudKit 401 with "no auth method
                    // found" in the body. Log the hint so a reporter sees
                    // it even when kei falls into the re-auth loop. See
                    // issue #221.
                    if http_err.status == 401 {
                        if let Some(body) = http_err.body.as_deref() {
                            if body.contains("no auth method found") {
                                tracing::warn!(
                                    url = %http_err.url,
                                    body = %body,
                                    "CloudKit 401 'no auth method found' — usually means \
                                     FIDO/WebAuthn security keys are on the account (issue \
                                     #221). Remove them at Settings > Apple ID & iCloud > \
                                     Sign-In & Security > Security Keys."
                                );
                            }
                        }
                    }
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
            .and_then(|r| {
                r.fields
                    .get("state")
                    .and_then(|f| f.get("value"))
                    .and_then(Value::as_str)
            })
            .unwrap_or("");
        if indexing_state != "FINISHED" {
            tracing::warn!(
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
}
