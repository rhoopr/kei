use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use super::queries::encode_params;
use super::session::PhotosSession;
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
            log_cloudkit_auth_hint(http_err);
            return ICloudError::SessionExpired {
                status: http_err.status,
            };
        }
    }
    ICloudError::Connection(error.to_string())
}

fn log_cloudkit_auth_hint(http_err: &crate::icloud::photos::session::HttpStatusError) {
    if http_err.status != 401 {
        return;
    }
    let Some(body) = http_err.body.as_deref() else {
        return;
    };
    if body.contains("no auth method found") {
        tracing::warn!(
            "CloudKit 401 'no auth method found'. Sign in to iCloud.com and check for \
             updated terms or other account prompts. If you complete a prompt, run \
             `kei login` again and retry. If your account uses FIDO/WebAuthn security \
             keys, those are another possible cause (see #221)."
        );
    }
}

pub struct PhotoLibrary {
    indexing_finished: Arc<AtomicBool>,
    #[cfg(test)]
    stub_indexing_finished: Option<bool>,
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
            indexing_finished: Arc::clone(&self.indexing_finished),
            #[cfg(test)]
            stub_indexing_finished: self.stub_indexing_finished,
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
    /// Create a library with explicit FINISHED or provisional RUNNING readiness.
    pub async fn new(
        service_endpoint: String,
        params: Arc<HashMap<String, Value>>,
        session: Box<dyn PhotosSession>,
        zone_id: Arc<Value>,
        library_type: String,
        retry_config: RetryConfig,
    ) -> Result<Self, ICloudError> {
        let library = Self {
            indexing_finished: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            stub_indexing_finished: None,
            service_endpoint: Arc::from(service_endpoint),
            params,
            session,
            zone_id,
            library_type: Arc::from(library_type),
            retry_config,
        };
        // Each sync cycle reads the state again and decides what waits for
        // indexing, so only a failure to reach iCloud stops loading here.
        match library.check_indexing_once().await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("iCloud is still indexing this photo library");
            }
            Err(ICloudError::IndexingUnavailable(reason)) => {
                tracing::warn!(
                    reason,
                    "Loading the photo library without its indexing state"
                );
            }
            Err(error) => return Err(error),
        }
        Ok(library)
    }

    /// Remembered readiness is not checkpoint authority. False also retains
    /// an earlier RUNNING observation across owner qualification. The cycle
    /// persists that obligation before rechecking readiness.
    pub(crate) fn last_indexing_finished(&self) -> bool {
        self.indexing_finished.load(Ordering::Relaxed)
    }

    /// Qualification can replace a name-only library after another readiness
    /// probe. Keep any earlier RUNNING observation until the cycle persists its
    /// hold before checking readiness again.
    pub(super) fn retain_provisional_readiness_from(&self, prior: &Self) {
        if !prior.last_indexing_finished() {
            self.retain_provisional_readiness();
        }
    }

    pub(super) fn retain_provisional_readiness(&self) {
        self.indexing_finished.store(false, Ordering::Relaxed);
    }

    /// Read iCloud's indexing state once. An unknown or missing state is
    /// `IndexingUnavailable`; callers treat it as not finished.
    pub(crate) async fn check_indexing_once(&self) -> Result<bool, ICloudError> {
        #[cfg(test)]
        if let Some(finished) = self.stub_indexing_finished {
            return Ok(finished);
        }
        self.retain_provisional_readiness();
        let url = format!(
            "{}/records/query?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "query": {"recordType": "CheckIndexingState"},
            "zoneID": &*self.zone_id,
        });

        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
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
            return Err(ICloudError::IndexingUnavailable(
                "Apple did not report whether the photo library is fully indexed; results may be incomplete",
            ));
        };
        let finished = match indexing_state {
            "FINISHED" => true,
            "RUNNING" => false,
            _ => {
                return Err(ICloudError::IndexingUnavailable(
                    "Apple reported an unknown photo library indexing state; results may be incomplete",
                ));
            }
        };
        self.indexing_finished.store(finished, Ordering::Relaxed);
        Ok(finished)
    }

    /// Only the exact private primary from validated discovery can be published.
    /// Readiness is retained separately and never grants checkpoint authority.
    pub(crate) fn is_qualified_private_primary(&self) -> bool {
        self.library_type.as_ref() == "private"
            && self.zone_id.get("zoneName").and_then(Value::as_str) == Some(PRIMARY_ZONE_NAME)
            && super::has_explicit_owner(self.zone_id.get("ownerRecordName"))
    }

    pub(super) fn owner_diagnostic_shape(&self) -> (&'static str, &'static str) {
        super::owner_diagnostic_shape(self.zone_id.get("ownerRecordName"))
    }

    pub(crate) fn selection_zone(&self) -> &Value {
        &self.zone_id
    }

    pub(crate) fn selection_database(&self) -> &str {
        &self.library_type
    }

    /// Returns the zone name (e.g., "`PrimarySync`", "SharedSync-{UUID}").
    pub fn zone_name(&self) -> &str {
        self.zone_id
            .get("zoneName")
            .and_then(|v| v.as_str())
            .unwrap_or(PRIMARY_ZONE_NAME)
    }

    /// Build an endpoint URL for this library's database with its query parameters.
    pub(super) fn endpoint_url(&self, path: &str) -> String {
        format!(
            "{}/{path}?{}",
            self.service_endpoint,
            encode_params(&self.params)
        )
    }

    pub(super) fn session(&self) -> &dyn PhotosSession {
        self.session.as_ref()
    }

    pub(super) fn zone_id(&self) -> &Value {
        &self.zone_id
    }

    pub(super) fn retry_config(&self) -> &RetryConfig {
        &self.retry_config
    }
}

#[cfg(test)]
impl PhotoLibrary {
    /// Test-only constructor that bypasses the indexing check.
    pub(crate) fn new_stub(session: Box<dyn PhotosSession>) -> Self {
        Self {
            indexing_finished: Arc::new(AtomicBool::new(true)),
            stub_indexing_finished: Some(true),
            service_endpoint: Arc::from("https://stub.example.com"),
            params: Arc::new(HashMap::new()),
            session,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            library_type: Arc::from("private"),
            retry_config: RetryConfig::default(),
        }
    }

    /// Make the stub report this indexing state.
    pub(crate) fn with_stub_indexing(mut self, finished: bool) -> Self {
        self.stub_indexing_finished = Some(finished);
        self.indexing_finished.store(finished, Ordering::Relaxed);
        self
    }

    /// Test-only constructor that pins a custom zone name on the stub.
    /// Used by `resolve_libraries` tests that need distinct zones to
    /// exercise selector matching.
    pub(crate) fn new_stub_with_zone(session: Box<dyn PhotosSession>, zone_name: &str) -> Self {
        Self {
            indexing_finished: Arc::new(AtomicBool::new(true)),
            stub_indexing_finished: Some(true),
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
            indexing_finished: Arc::new(AtomicBool::new(true)),
            stub_indexing_finished: Some(true),
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
    async fn indexing_state_running_initializes_provisional_library() {
        let library = new_library_with_indexing_response(json!({
            "records": [{"fields": {"state": {"value": "RUNNING"}}}]
        }))
        .await
        .unwrap();
        assert!(!library.last_indexing_finished());
        assert!(!library.check_indexing_once().await.unwrap());
    }

    #[tokio::test]
    async fn indexing_state_missing_or_unknown_still_loads_the_library() {
        for response in [
            json!({"records": []}),
            json!({"records": [{"fields": {"state": {"value": "PAUSED"}}}]}),
        ] {
            let library = new_library_with_indexing_response(response).await.unwrap();
            assert!(!library.last_indexing_finished());
            assert!(matches!(
                library.check_indexing_once().await,
                Err(ICloudError::IndexingUnavailable(_))
            ));
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

    struct NoAuthMethodFoundSession(u16);

    #[async_trait::async_trait]
    impl PhotosSession for NoAuthMethodFoundSession {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            Err(crate::icloud::photos::session::HttpStatusError {
                status: self.0,
                url: "https://example.com/private-url-marker".into(),
                retry_after: None,
                body: Some(
                    r#"{"serverErrorCode":"AUTHENTICATION_FAILED","reason":"no auth method found","requestId":"private-body-marker"}"#
                        .into(),
                ),
            }.into())
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(NoAuthMethodFoundSession(self.0))
        }
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn http_401_no_auth_method_found_still_maps_to_session_expired() {
        let err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(NoAuthMethodFoundSession(401)),
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
            "the hint must preserve session recovery, got: {err:?}"
        );
        assert!(logs_contain("no auth method found"));
        assert!(logs_contain(
            "Sign in to iCloud.com and check for updated terms"
        ));
        assert!(logs_contain(
            "If you complete a prompt, run `kei login` again"
        ));
        assert!(logs_contain(
            "If your account uses FIDO/WebAuthn security keys"
        ));
        assert!(!logs_contain("private-body-marker"));
        assert!(!logs_contain("private-url-marker"));
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn http_403_no_auth_method_found_does_not_log_auth_hint() {
        let err = PhotoLibrary::new(
            "https://example.com".into(),
            Arc::new(HashMap::new()),
            Box::new(NoAuthMethodFoundSession(403)),
            Arc::new(json!({"zoneName": "PrimarySync"})),
            "private".into(),
            RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ICloudError::SessionExpired { status: 403 }));
        assert!(!logs_contain("Sign in to iCloud.com"));
        assert!(!logs_contain("security keys"));
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn http_401_without_matching_body_does_not_log_auth_hint() {
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

        assert!(!logs_contain("Sign in to iCloud.com"));
        assert!(!logs_contain("security keys"));
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
}
