use super::hydration_progress;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value;

use crate::retry::{self, RetryAction, RetryConfig, parse_retry_after_header};

/// Upper bound on any `Retry-After` hint from CloudKit, chosen so a
/// pathological server value can't stall the retry loop.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(120);

#[cfg(debug_assertions)]
async fn dump_body(dir: &std::path::Path, id: usize, kind: &str, body: &str) {
    use tokio::io::AsyncWriteExt;

    let timestamp = chrono::Utc::now().timestamp_millis();
    let result: std::io::Result<()> = async {
        let mut directory = tokio::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        directory.mode(0o700);
        directory.create(dir).await?;
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(dir.join(format!("{id:06}.{kind}.{timestamp}.json")))
            .await?;
        file.write_all(body.as_bytes()).await?;
        file.flush().await
    }
    .await;
    if result.is_err() {
        tracing::warn!(id, kind, "Could not dump Photos body");
    }
}

/// Fixed diagnostic failures contain no URL, body, credentials or provider text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLookupError {
    Unavailable,
    Request,
    HttpStatus,
    TooLarge,
    Malformed,
}

/// Privacy-safe streaming: no ordinary error logs or debug body dumps.
pub(crate) async fn diagnostic_bytes(
    client: &reqwest::Client,
    url: &str,
    body: String,
    headers: &[(&str, &str)],
    limit: usize,
) -> Result<Vec<u8>, DiagnosticLookupError> {
    let mut builder = client.post(url).body(body);
    for &(key, value) in headers {
        builder = builder.header(key, value);
    }
    let mut response = builder
        .send()
        .await
        .map_err(|_private_error| DiagnosticLookupError::Request)?;
    if !response.status().is_success() {
        return Err(DiagnosticLookupError::HttpStatus);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|value| value != "identity")
    {
        return Err(DiagnosticLookupError::Malformed);
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(DiagnosticLookupError::TooLarge);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_private_error| DiagnosticLookupError::Request)?
    {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(DiagnosticLookupError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

const DIAGNOSTIC_RESPONSE_BYTES: usize = 64 * 1024;

/// Async HTTP session trait for the photos service.
///
/// Abstracted as a trait so album/library code can be tested with stubs
/// without hitting the real iCloud API.
/// One transport invocation; adapters cannot construct or clone this token.
struct InvocationIdentity {
    id: uuid::Uuid,
}
pub struct InvocationToken(std::sync::Arc<InvocationIdentity>);

/// A non-clone accepted invocation; state uses its ID for durable uniqueness
/// and checks the private in-process identity against the same original body.
pub(crate) struct AcceptedInvocation(std::sync::Arc<InvocationIdentity>);
impl AcceptedInvocation {
    pub(crate) fn id(&self) -> uuid::Uuid {
        self.0.id
    }
    pub(crate) fn matches_original(&self, original: &OriginalBody) -> bool {
        original
            .invocation
            .as_ref()
            .is_some_and(|inner| std::sync::Arc::ptr_eq(inner, &self.0))
    }
}

/// Raw changes body for the existing strict parser, with separately optional
/// evidence. Missing proof never changes the body used by ingestion.
pub struct ChangesResponse {
    pub(crate) body: Vec<u8>,
    pub(crate) original: Option<OriginalBody>,
    pub(crate) accepted: Option<AcceptedInvocation>,
}

/// Request identity for a runtime attestation. URL, body and headers exist only
/// in memory and are skipped during serialization; persisted digests alone do
/// not independently reconstruct or authenticate the original request.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RetainedRequest {
    // Present only in memory for the actual request; never serialized.
    #[serde(skip)]
    url: String,
    #[serde(skip)]
    body: String,
    #[serde(skip)]
    headers: Vec<(String, String)>,
    request_binding: [u8; 32],
    body_digest: [u8; 32],
    changes: Option<SafeChangesRequest>,
}

/// Nonsecret facts attested at actual request time. On replay this is an
/// integrity-checked runtime attestation, not reconstruction of the wire.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SafeChangesRequest {
    zone_digest: [u8; 32],
    cursor_digest: [u8; 32],
    results_limit: u64,
}
fn safe_changes_request(body: &str) -> Option<SafeChangesRequest> {
    use sha2::{Digest, Sha256};
    let value = super::changes_json::parse(body.as_bytes()).ok()?;
    let root = value.as_object()?;
    if root.len() != 1 {
        return None;
    }
    let zones = root.get("zones")?.as_array()?;
    if zones.len() != 1 {
        return None;
    }
    let zone = zones.first()?.as_object()?;
    if zone.len() != 3 {
        return None;
    }
    let identity = zone.get("zoneID")?;
    let cursor = zone.get("syncToken")?.as_str()?;
    let limit = zone.get("resultsLimit")?.as_u64()?;
    if cursor.trim().is_empty() || limit != 200 {
        return None;
    }
    Some(SafeChangesRequest {
        zone_digest: Sha256::digest(serde_json::to_vec(identity).ok()?).into(),
        cursor_digest: Sha256::digest(cursor.as_bytes()).into(),
        results_limit: limit,
    })
}
impl RetainedRequest {
    fn new(url: &str, body: &str, headers: &[(&str, &str)]) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            request_binding: original_request_hash(url, body, headers),
            body_digest: Sha256::digest(body.as_bytes()).into(),
            changes: safe_changes_request(body),
            url: url.to_owned(),
            body: body.to_owned(),
            headers: headers
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
        }
    }
    pub(crate) fn valid_changes_zone_request(&self, zone: &Value) -> bool {
        use sha2::{Digest, Sha256};
        let Some(changes) = &self.changes else {
            return false;
        };
        let Ok(encoded) = serde_json::to_vec(zone) else {
            return false;
        };
        let digest: [u8; 32] = Sha256::digest(encoded).into();
        changes.zone_digest == digest && changes.results_limit == 200
    }

    pub(crate) fn cursor_digest(&self) -> Option<[u8; 32]> {
        self.changes.as_ref().map(|changes| changes.cursor_digest)
    }

    pub(crate) fn matches_body(&self, body: &str) -> bool {
        use sha2::{Digest, Sha256};
        let actual: [u8; 32] = Sha256::digest(body.as_bytes()).into();
        self.body_digest == actual
    }

    fn header_refs(&self) -> Vec<(&str, &str)> {
        self.headers
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect()
    }
}

/// Same-response bytes sealed by the live transport. No constructor is exposed
/// to Value-based adapters; obtaining a body does not by itself prove owner or scope.
pub(crate) struct OriginalBody {
    bytes: Vec<u8>,
    request_hash: [u8; 32],
    request_url: String,
    retained_request: Option<RetainedRequest>,
    principal: Option<String>,
    namespace: Option<String>,
    service_root: Option<String>,
    // Existing session ownership only; this does not attest immutable cookies
    // or complete redirect history.
    session_bound: bool,
    invocation: Option<std::sync::Arc<InvocationIdentity>>,
    // Held through duplicate-safe parsing and until evidence is consumed.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

// At most four pages are being captured/validated or retained as evidence.
// This bounds proof input, not the heap expansion of ordinary JSON Values.
pub(crate) const MAX_ORIGINAL_IN_FLIGHT_PAGES: usize = 4;
#[cfg(test)]
tokio::task_local! {
    static TEST_ORIGINAL_SLOTS: std::sync::Arc<tokio::sync::Semaphore>;
}

static ORIGINAL_SLOTS: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| {
        std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_ORIGINAL_IN_FLIGHT_PAGES))
    });

fn valid_source_scheme(url: &reqwest::Url) -> bool {
    if url.scheme() == "https" {
        return true;
    }
    // Synthetic local HTTP fixture only; production always requires HTTPS.
    #[cfg(test)]
    if url.scheme() == "http" && url.host_str() == Some("127.0.0.1") {
        return true;
    }
    false
}

/// Snapshot of the current session source identity. This capability does not
/// by itself fence concurrent cookie-jar mutation or prove a page observation.
#[derive(Clone)]
pub struct SourceAuthority {
    namespace: String,
    principal: String,
    service_root: String,
}

impl SourceAuthority {
    #[cfg(test)]
    pub(crate) fn synthetic(
        owner: &crate::state::db::account::AccountOwner,
        service_root: &str,
    ) -> Option<Self> {
        let (namespace, principal) = owner.authenticated_keys().ok()?;
        Some(Self {
            namespace: namespace.to_owned(),
            principal: principal.to_owned(),
            service_root: service_root.to_owned(),
        })
    }

    /// Recompute the retained request binding under a current authenticated
    /// source snapshot. Final dispatch must still revalidate its live fence.
    pub(crate) fn validate_retained_request(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        database: &str,
        operation: &str,
        request: &RetainedRequest,
        expected: &[u8; 32],
    ) -> bool {
        if self.binding(owner, database, operation).is_none() || request.url.is_empty() {
            return false;
        }
        let Ok(root) = reqwest::Url::parse(&self.service_root) else {
            return false;
        };
        let Ok(actual) = reqwest::Url::parse(&request.url) else {
            return false;
        };
        let path = format!(
            "{}/database/1/com.apple.photos.cloud/production/{database}/{operation}",
            root.path().trim_end_matches('/')
        );
        if actual.origin() != root.origin()
            || actual.path() != path
            || actual.fragment().is_some()
            || !actual.username().is_empty()
            || actual.password().is_some()
        {
            return false;
        }
        request.request_binding == *expected
            && original_request_hash(&request.url, &request.body, &request.header_refs())
                == *expected
    }

    pub(crate) fn binding(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        database: &str,
        operation: &str,
    ) -> Option<[u8; 32]> {
        use sha2::{Digest, Sha256};
        let (namespace, principal) = owner.authenticated_keys().ok()?;
        if self.namespace != namespace
            || self.principal != principal
            || !matches!(database, "private" | "shared")
            || !matches!(
                operation,
                "records/query" | "changes/zone" | "records/lookup"
            )
        {
            return None;
        }
        let root = reqwest::Url::parse(&self.service_root).ok()?;
        if !valid_source_scheme(&root)
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
        {
            return None;
        }
        let mut hash = Sha256::new();
        for field in [
            "kei-source-authority-v1",
            namespace,
            principal,
            &self.service_root,
            database,
            operation,
        ] {
            hash.update((field.len() as u64).to_be_bytes());
            hash.update(field.as_bytes());
        }
        Some(hash.finalize().into())
    }
}

/// Acquire a fresh current authority directly from the pinned shared session.
/// Caller must separately establish credential-generation safety.
pub(crate) async fn source_authority(
    session: &crate::auth::SharedSession,
) -> Option<SourceAuthority> {
    let session = session.read().await;
    // This follows the existing SharedSession ownership model. It does not
    // prove that the shared cookie jar is immutable during a request.
    Some(SourceAuthority {
        namespace: session.account_namespace().to_owned(),
        principal: session.principal()?.to_owned(),
        service_root: session.photos_service_root()?.to_owned(),
    })
}

impl OriginalBody {
    fn slot() -> Option<tokio::sync::OwnedSemaphorePermit> {
        #[cfg(test)]
        if let Ok(result) = TEST_ORIGINAL_SLOTS.try_with(Self::slot_from) {
            return result;
        }
        Self::slot_from(&ORIGINAL_SLOTS)
    }

    fn slot_from(
        pool: &std::sync::Arc<tokio::sync::Semaphore>,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        std::sync::Arc::clone(pool).try_acquire_owned().ok()
    }

    fn seal(
        bytes: &[u8],
        permit: tokio::sync::OwnedSemaphorePermit,
        request_hash: [u8; 32],
        request_url: &str,
    ) -> Option<Self> {
        if bytes.len() > super::inbox::MAX_CHANGES_PAGE_BYTES {
            return None;
        }
        Some(Self {
            bytes: bytes.to_vec(),
            request_hash,
            request_url: request_url.to_owned(),
            retained_request: None,
            principal: None,
            namespace: None,
            service_root: None,
            session_bound: false,
            invocation: None,
            _permit: permit,
        })
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn retained_request(&self) -> Option<&RetainedRequest> {
        self.retained_request.as_ref()
    }

    /// State must independently validate the account namespace, root, pass,
    /// zone and request semantics. A bare reqwest client has no principal.
    pub(crate) fn matches_authenticated_request(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        url: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> bool {
        let Ok((namespace, provider)) = owner.authenticated_keys() else {
            return false;
        };
        self.namespace.as_deref() == Some(namespace)
            && self.principal.as_deref() == Some(provider)
            && self.request_hash == original_request_hash(url, body, headers)
    }

    /// Verify the request was sent to the Photos endpoint pinned from the
    /// authenticated login response. Account namespace, zone and page
    /// semantics remain the responsibility of the state transaction.
    pub(crate) fn matches_authenticated_source(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        database: &str,
        operation: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> bool {
        if !self.session_bound
            || !matches!(database, "private" | "shared")
            || !matches!(
                operation,
                "records/query" | "changes/zone" | "records/lookup"
            )
            || !self.matches_authenticated_request(owner, &self.request_url, body, headers)
        {
            return false;
        }
        let Some(root) = self
            .service_root
            .as_deref()
            .and_then(|value| reqwest::Url::parse(value).ok())
        else {
            return false;
        };
        let Ok(actual) = reqwest::Url::parse(&self.request_url) else {
            return false;
        };
        if !valid_source_scheme(&root)
            || !root.username().is_empty()
            || root.password().is_some()
            || root.query().is_some()
            || root.fragment().is_some()
            || actual.origin() != root.origin()
            || actual.fragment().is_some()
            || !actual.username().is_empty()
            || actual.password().is_some()
        {
            return false;
        }
        let expected_path = format!(
            "{}/database/1/com.apple.photos.cloud/production/{database}/{operation}",
            root.path().trim_end_matches('/')
        );
        actual.path() == expected_path
    }

    pub(crate) fn attested_bindings(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        database: &str,
        operation: &str,
    ) -> Option<([u8; 32], [u8; 32], &RetainedRequest)> {
        let request = self.retained_request()?;
        let headers = request.header_refs();
        let request_binding =
            self.request_binding(owner, database, operation, &request.body, &headers)?;
        let authority_binding =
            self.binding(owner, database, operation, &request.body, &headers)?;
        Some((request_binding, authority_binding, request))
    }

    /// Stable redacted identity of the exact successful request. It is not a
    /// substitute for the run/pass causal capability or source authority.
    pub(crate) fn request_binding(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        database: &str,
        operation: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Option<[u8; 32]> {
        self.matches_authenticated_source(owner, database, operation, body, headers)
            .then_some(self.request_hash)
    }

    pub(crate) fn binding(
        &self,
        owner: &crate::state::db::account::AccountOwner,
        database: &str,
        operation: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Option<[u8; 32]> {
        if !self.matches_authenticated_source(owner, database, operation, body, headers) {
            return None;
        }
        SourceAuthority {
            namespace: self.namespace.clone()?,
            principal: self.principal.clone()?,
            service_root: self.service_root.clone()?,
        }
        .binding(owner, database, operation)
    }

    #[cfg(test)]
    pub(crate) fn synthetic_accepted(mut self) -> (Self, AcceptedInvocation) {
        let invocation = std::sync::Arc::new(InvocationIdentity {
            id: uuid::Uuid::new_v4(),
        });
        self.invocation = Some(std::sync::Arc::clone(&invocation));
        (self, AcceptedInvocation(invocation))
    }

    #[cfg(test)]
    pub(crate) fn synthetic_source(
        owner: &crate::state::db::account::AccountOwner,
        root: &str,
        url: &str,
        request: &str,
        headers: &[(&str, &str)],
        bytes: &[u8],
    ) -> Option<Self> {
        super::changes_json::parse(bytes).ok()?;
        let (namespace, principal) = owner.authenticated_keys().ok()?;
        let pool = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let mut original = Self::seal(
            bytes,
            Self::slot_from(&pool)?,
            original_request_hash(url, request, headers),
            url,
        )?;
        original.namespace = Some(namespace.to_owned());
        original.principal = Some(principal.to_owned());
        original.service_root = Some(root.to_owned());
        original.retained_request = Some(RetainedRequest::new(url, request, headers));
        original.session_bound = true;
        Some(original)
    }

    #[cfg(test)]
    fn synthetic(bytes: &[u8]) -> Option<Self> {
        let pool = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        Self::seal(
            bytes,
            Self::slot_from(&pool)?,
            [0; 32],
            "https://synthetic.invalid",
        )
    }
}

fn original_request_hash(url: &str, body: &str, headers: &[(&str, &str)]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for field in std::iter::once(url)
        .chain(std::iter::once(body))
        .chain(headers.iter().flat_map(|(name, value)| [*name, *value]))
    {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    hash.finalize().into()
}

impl ChangesResponse {
    /// Explicit synthetic raw adapter; never available in production.
    #[cfg(test)]
    pub(crate) fn synthetic(
        owner: &crate::state::db::account::AccountOwner,
        service_root: &str,
        url: &str,
        request: &str,
        headers: &[(&str, &str)],
        response_body: &[u8],
        invocation: InvocationToken,
    ) -> anyhow::Result<Self> {
        let mut original = OriginalBody::synthetic_source(
            owner,
            service_root,
            url,
            request,
            headers,
            response_body,
        );
        if let Some(original) = original.as_mut() {
            original.invocation = Some(invocation.0);
        }
        Ok(Self {
            body: response_body.to_vec(),
            original,
            accepted: None,
        })
    }
}

#[async_trait::async_trait]
pub trait PhotosSession: Send + Sync {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value>;

    /// Only a pinned shared session can provide current source authority.
    async fn current_source_authority(&self) -> Option<SourceAuthority> {
        None
    }

    /// Optional diagnostic transport: no retries, body dumps or error-body logs.
    /// Adapters must opt in; falling back to ordinary post can expose secrets.
    async fn post_diagnostic(
        &self,
        _url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> Result<Value, DiagnosticLookupError> {
        Err(DiagnosticLookupError::Unavailable)
    }

    /// Bounded original changes response. Value-based test/session adapters can
    /// use the default; live reqwest and SharedSession preserve original bytes.
    async fn post_changes_body(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        let body = serde_json::to_vec(&self.post(url, body, headers).await?)?;
        anyhow::ensure!(
            body.len() <= super::inbox::MAX_CHANGES_PAGE_BYTES,
            super::error::ShadowPageError::from(anyhow::anyhow!(
                "Provider changes page exceeded the capture byte limit"
            ))
        );
        Ok(body)
    }

    /// Preserve the existing bounded changes body even when evidence is
    /// unavailable. Value-based adapters cannot manufacture an original seal.
    async fn post_changes_with_evidence(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
        invocation: InvocationToken,
    ) -> anyhow::Result<ChangesResponse> {
        let _ = invocation;
        Ok(ChangesResponse {
            body: self.post_changes_body(url, body, headers).await?,
            original: None,
            accepted: None,
        })
    }

    /// Clone this session into a new boxed trait object.
    fn clone_box(&self) -> Box<dyn PhotosSession>;
}

// Blanket impl lets `reqwest::Client` (from auth) be used directly as a
// `PhotosSession` without an adapter, since Client is Arc-backed and cheap to clone.
#[async_trait::async_trait]
impl PhotosSession for reqwest::Client {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let (response, _dump) = post_response(self, url, body, headers).await?;
        // Keep reqwest's decode error type and the existing retry classification
        // for ordinary JSON requests such as bounded resource lookups.
        let json: Value = response.json().await?;
        #[cfg(debug_assertions)]
        if let Some((dir, id)) = &_dump {
            dump_body(dir, *id, "res", &json.to_string()).await;
        }
        Ok(json)
    }

    async fn post_diagnostic(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<Value, DiagnosticLookupError> {
        let bytes = diagnostic_bytes(self, url, body, headers, DIAGNOSTIC_RESPONSE_BYTES).await?;
        serde_json::from_slice(&bytes).map_err(|_private_error| DiagnosticLookupError::Malformed)
    }

    async fn post_changes_body(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        let (mut response, _dump) = post_response(self, url, body, headers).await?;
        let limit = super::inbox::MAX_CHANGES_PAGE_BYTES;
        anyhow::ensure!(
            response
                .content_length()
                .is_none_or(|size| size <= limit as u64),
            super::error::ShadowPageError::from(anyhow::anyhow!(
                "Provider changes page exceeded the capture byte limit"
            ))
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                chunk.len() <= limit.saturating_sub(bytes.len()),
                super::error::ShadowPageError::from(anyhow::anyhow!(
                    "Provider changes page exceeded the capture byte limit"
                ))
            );
            bytes.extend_from_slice(&chunk);
        }
        #[cfg(debug_assertions)]
        if let Some((dir, id)) = &_dump {
            dump_body(dir, *id, "res", &String::from_utf8_lossy(&bytes)).await;
        }
        Ok(bytes)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

type BodyDump = Option<(std::path::PathBuf, usize)>;

async fn post_response(
    client: &reqwest::Client,
    url: &str,
    body: String,
    headers: &[(&str, &str)],
) -> anyhow::Result<(reqwest::Response, BodyDump)> {
    #[cfg(debug_assertions)]
    let dump = std::env::var_os("KEI_REQUEST_DUMP_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|dir| {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
            (
                std::path::PathBuf::from(dir),
                NEXT_ID.fetch_add(1, Ordering::Relaxed),
            )
        });
    #[cfg(debug_assertions)]
    if let Some((dir, id)) = &dump {
        dump_body(dir, *id, "req", &body).await;
    }
    let mut builder = client.post(url).body(body);
    for &(k, v) in headers {
        builder = builder.header(k, v);
    }
    let resp = builder.send().await?;
    let status = resp.status();

    if status.is_client_error() || status.is_server_error() {
        let url = resp.url().to_string();
        let retry_after = parse_retry_after_header(resp.headers(), RETRY_AFTER_MAX);
        let resp_body = read_bounded_error_body(resp, &url).await;
        #[cfg(debug_assertions)]
        if let Some((dir, id)) = &dump {
            dump_body(dir, *id, "res", &resp_body).await;
        }
        if !resp_body.is_empty() {
            // 421 bodies are the most diagnostic signal for distinguishing
            // ADP-class from session-class misdirected requests (e.g. the
            // "Missing X-APPLE-WEBAUTH-USER cookie" string from issue
            // #199). Surface at WARN so reporters don't need RUST_LOG=debug.
            if status.as_u16() == 421 {
                tracing::warn!(
                    status = %status,
                    url = %url,
                    body = %resp_body,
                    "CloudKit 421 Misdirected Request response body"
                );
            } else {
                tracing::debug!(
                    status = %status,
                    url = %url,
                    body = %resp_body,
                    "CloudKit error response body"
                );
            }
        }
        let preserved = if resp_body.is_empty() {
            None
        } else {
            Some(truncate_body(&resp_body))
        };
        return Err(HttpStatusError {
            status: status.as_u16(),
            url,
            retry_after,
            body: preserved,
        }
        .into());
    }

    #[cfg(not(debug_assertions))]
    let dump = None;
    Ok((resp, dump))
}

/// Read exactly one ordinary changes response while observing its effective
/// endpoint and status. Normal raw ingestion gets identical bytes; a response
/// at a different final URL or without 2xx cannot issue source evidence.
async fn source_changes_body(
    client: &reqwest::Client,
    url: &str,
    body: String,
    headers: &[(&str, &str)],
) -> anyhow::Result<(Vec<u8>, bool)> {
    let (mut response, _dump) = post_response(client, url, body, headers).await?;
    let source_endpoint = response.status().is_success() && response.url().as_str() == url;
    let limit = super::inbox::MAX_CHANGES_PAGE_BYTES;
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|size| size <= limit as u64),
        super::error::ShadowPageError::from(anyhow::anyhow!(
            "Provider changes page exceeded the capture byte limit"
        ))
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            chunk.len() <= limit.saturating_sub(bytes.len()),
            super::error::ShadowPageError::from(anyhow::anyhow!(
                "Provider changes page exceeded the capture byte limit"
            ))
        );
        bytes.extend_from_slice(&chunk);
    }
    #[cfg(debug_assertions)]
    if let Some((dir, id)) = &_dump {
        dump_body(dir, *id, "res", &String::from_utf8_lossy(&bytes)).await;
    }
    Ok((bytes, source_endpoint))
}

// SharedSession delegates to the inner Session's http_client(). The read lock
// is held only long enough to clone the Arc-backed Client, then released before
// the actual HTTP call so other tasks can read concurrently.
#[async_trait::async_trait]
impl PhotosSession for crate::auth::SharedSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let client = self.read().await.http_client().clone();
        PhotosSession::post(&client, url, body, headers).await
    }

    async fn current_source_authority(&self) -> Option<SourceAuthority> {
        source_authority(self).await
    }

    async fn post_diagnostic(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> Result<Value, DiagnosticLookupError> {
        let client = self.read().await.http_client().clone();
        PhotosSession::post_diagnostic(&client, url, body, headers).await
    }

    async fn post_changes_body(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        let client = self.read().await.http_client().clone();
        PhotosSession::post_changes_body(&client, url, body, headers).await
    }

    /// Same authenticated SharedSession and strict raw request used by the
    /// ordinary path. This follows kei's existing session model; it does not
    /// assert that the shared cookie jar is immutable or record every redirect.
    async fn post_changes_with_evidence(
        &self,
        url: &str,
        body: String,
        headers: &[(&str, &str)],
        invocation: InvocationToken,
    ) -> anyhow::Result<ChangesResponse> {
        let (client, principal, namespace, service_root) = {
            let session = self.read().await;
            (
                session.http_client().clone(),
                session.principal().map(str::to_owned),
                session.account_namespace().to_owned(),
                session.photos_service_root().map(str::to_owned),
            )
        };
        if principal.is_none() || service_root.is_none() {
            return Ok(ChangesResponse {
                body: PhotosSession::post_changes_body(&client, url, body, headers).await?,
                original: None,
                accepted: None,
            });
        }
        let request_hash = original_request_hash(url, &body, headers);
        let request = RetainedRequest::new(url, &body, headers);
        let (bytes, source_endpoint) = source_changes_body(&client, url, body, headers).await?;
        let unchanged = {
            let session = self.read().await;
            session.principal() == principal.as_deref()
                && session.account_namespace() == namespace
                && session.photos_service_root() == service_root.as_deref()
        };
        let original = if unchanged && source_endpoint {
            OriginalBody::slot()
                .and_then(|permit| OriginalBody::seal(&bytes, permit, request_hash, url))
                .map(|mut original| {
                    original.invocation = Some(invocation.0);
                    original.principal = principal;
                    original.namespace = Some(namespace);
                    original.service_root = service_root;
                    original.retained_request = Some(request);
                    // This means session-bound under the existing model, not
                    // a proof of immutable cookies or redirect history.
                    original.session_bound = true;
                    original
                })
        } else {
            None
        };
        Ok(ChangesResponse {
            body: bytes,
            original,
            accepted: None,
        })
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

/// HTTP error with structured status code for typed error handling.
/// Wraps non-success HTTP responses from CloudKit endpoints.
///
/// `retry_after` is populated from the `Retry-After` response header when
/// present, so callers can honor the server-provided delay on 429/503
/// instead of falling back to exponential backoff alone.
///
/// `body` carries a truncated copy of the response body so downstream
/// error mapping (e.g. detecting "no auth method found" in a CloudKit 401
/// that typically indicates FIDO/security keys on the account) can read
/// the payload without re-requesting. Truncated to keep memory bounded
/// when CloudKit occasionally returns large HTML error pages.
#[derive(Debug, thiserror::Error)]
#[error("Apple returned HTTP {status} for {url}")]
pub(crate) struct HttpStatusError {
    pub status: u16,
    pub url: String,
    pub retry_after: Option<Duration>,
    pub body: Option<String>,
}

/// Recognize provider session failures without inspecting provider text or URLs.
/// Callers must retain their checkpoint and route these through bounded reauthentication.
#[must_use]
pub(crate) fn is_session_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HttpStatusError>()
        .is_some_and(|http| matches!(http.status, 401 | 403 | 421))
        || error
            .downcast_ref::<super::album::ProviderLookupError>()
            .is_some_and(super::album::ProviderLookupError::is_authentication)
        || error
            .downcast_ref::<crate::icloud::error::ICloudError>()
            .is_some_and(crate::icloud::error::ICloudError::is_session_error)
}

/// Maximum number of bytes preserved from an HTTP error body. Apple's
/// CloudKit error JSON is typically a few hundred bytes; HTML error pages
/// and stack traces are occasionally much larger. Cap it so a degenerate
/// response can't bloat the error path.
const MAX_PRESERVED_BODY: usize = 1024;

/// Read a CloudKit error-response body, capping at `MAX_PRESERVED_BODY`
/// bytes plus a small UTF-8 grace margin. The full body can be megabytes
/// (rate-limit HTML pages, stack traces) but the caller only ever keeps
/// the first ~1 KiB for diagnostics, so streaming the first chunks is
/// safer than materialising a 10 MB String we're about to throw away.
async fn read_bounded_error_body(resp: reqwest::Response, url: &str) -> String {
    use futures_util::StreamExt;
    // A few extra bytes past the cap so `truncate_body` has room to land
    // on a char boundary without re-fetching.
    const CAP: usize = MAX_PRESERVED_BODY + 16;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let remaining = CAP.saturating_sub(buf.len());
                if remaining == 0 {
                    break;
                }
                let take = bytes.len().min(remaining);
                #[allow(
                    clippy::indexing_slicing,
                    reason = "`take` is bounded above by `bytes.len()` via min()"
                )]
                buf.extend_from_slice(&bytes[..take]);
                if buf.len() >= CAP {
                    break;
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    url = %url,
                    "CloudKit error-response body read failed; proceeding with partial body"
                );
                break;
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Shorten `body` to at most `MAX_PRESERVED_BODY` bytes without splitting
/// a multi-byte UTF-8 character.
fn truncate_body(body: &str) -> String {
    if body.len() <= MAX_PRESERVED_BODY {
        return body.to_string();
    }
    format!("{}…", crate::truncate_str(body, MAX_PRESERVED_BODY))
}

/// `CloudKit` server error codes that indicate a transient condition.
/// These arrive as HTTP 200 with a `serverErrorCode` field in the JSON body.
const RETRYABLE_SERVER_ERRORS: &[&str] =
    &["RETRY_LATER", "TRY_AGAIN_LATER", "CAS_OP_LOCK", "THROTTLED"];

/// `CloudKit` server error codes that indicate the iCloud service is not
/// activated or accessible (e.g. ADP enabled, incomplete iCloud setup).
const SERVICE_NOT_ACTIVATED_ERRORS: &[&str] = &["ZONE_NOT_FOUND", "AUTHENTICATION_FAILED"];

/// Error type for `CloudKit` server errors embedded in the JSON response body.
/// These are distinct from HTTP-level errors and represent API-level failures.
#[derive(Debug, thiserror::Error)]
#[error("Apple CloudKit reported {code}: {reason}")]
pub struct CloudKitServerError {
    pub(crate) code: Box<str>,
    pub(crate) reason: Box<str>,
    pub(crate) retryable: bool,
    /// True when the error indicates the iCloud service is not activated
    /// (ADP enabled, incomplete setup, or private db access disabled).
    pub(crate) service_not_activated: bool,
}

/// Check whether an error code or reason indicates the iCloud service is not
/// activated (ADP enabled, incomplete setup, or private db access disabled).
fn is_service_not_activated(code: &str, reason: &str) -> bool {
    SERVICE_NOT_ACTIVATED_ERRORS
        .iter()
        .any(|&s| s.eq_ignore_ascii_case(code))
        || code.eq_ignore_ascii_case("ACCESS_DENIED")
        || reason
            .to_ascii_lowercase()
            .contains("private db access disabled")
}

/// Check a `CloudKit` JSON response for `serverErrorCode` or per-record errors.
/// Returns `Err` if a server error is found, `Ok(response)` otherwise.
fn check_cloudkit_errors(response: Value) -> anyhow::Result<Value> {
    // Top-level serverErrorCode (e.g. from CAS Op-Lock)
    if let Some(code) = response.get("serverErrorCode").and_then(Value::as_str) {
        let reason = response
            .get("reason")
            .and_then(Value::as_str)
            .or_else(|| response.get("serverErrorMessage").and_then(Value::as_str))
            .unwrap_or("unknown");
        let retryable = RETRYABLE_SERVER_ERRORS
            .iter()
            .any(|&s| s.eq_ignore_ascii_case(code));
        let service_not_activated = is_service_not_activated(code, reason);
        tracing::warn!(
            error_code = code,
            retryable,
            service_not_activated,
            "CloudKit server error: {reason}"
        );
        return Err(CloudKitServerError {
            code: code.into(),
            reason: reason.into(),
            retryable,
            service_not_activated,
        }
        .into());
    }

    // Per-record errors make the whole page unusable. Filtering errored
    // records out of a mixed page can make enumeration look complete while
    // silently dropping assets from the snapshot.
    if let Some(records) = response.get("records").and_then(Value::as_array) {
        let has_errors = records
            .iter()
            .any(|r| r.get("serverErrorCode").and_then(Value::as_str).is_some());

        if has_errors {
            let mut first_retryable = None;
            let mut first_permanent = None;
            for record in records {
                if let Some(code) = record.get("serverErrorCode").and_then(Value::as_str) {
                    let reason = record
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let record_name = record
                        .get("recordName")
                        .and_then(Value::as_str)
                        .unwrap_or("(unknown)");
                    let retryable = RETRYABLE_SERVER_ERRORS
                        .iter()
                        .any(|&s| s.eq_ignore_ascii_case(code));
                    let service_not_activated = is_service_not_activated(code, reason);
                    if retryable {
                        tracing::warn!(
                            record_name,
                            error_code = code,
                            retryable,
                            service_not_activated,
                            "CloudKit per-record error (retryable): {reason}"
                        );
                        first_retryable.get_or_insert_with(|| CloudKitServerError {
                            code: code.into(),
                            reason: reason.into(),
                            retryable,
                            service_not_activated,
                        });
                    } else {
                        tracing::error!(
                            record_name,
                            error_code = code,
                            retryable,
                            service_not_activated,
                            "CloudKit per-record error (permanent): {reason}"
                        );
                        first_permanent.get_or_insert_with(|| CloudKitServerError {
                            code: code.into(),
                            reason: reason.into(),
                            retryable,
                            service_not_activated,
                        });
                    }
                }
            }

            if let Some(err) = first_retryable.or(first_permanent) {
                return Err(err.into());
            }
            anyhow::bail!(
                "CloudKit response contained per-record errors, \
                 but no per-record server error was available"
            );
        }
    }

    Ok(response)
}

/// Classify API errors for retry: network failures, server-side errors
/// (5xx, 429), and retryable `CloudKit` server errors are transient;
/// client errors (4xx) and non-retryable server errors are permanent.
fn classify_api_error(e: &anyhow::Error) -> RetryAction {
    if let Some(ck_err) = e.downcast_ref::<CloudKitServerError>() {
        return if ck_err.retryable {
            RetryAction::Retry
        } else {
            RetryAction::Abort
        };
    }
    if let Some(http_err) = e.downcast_ref::<HttpStatusError>() {
        if http_err.status == 429 || http_err.status >= 500 {
            return match http_err.retry_after {
                Some(d) => RetryAction::RetryAfter(d),
                None => RetryAction::Retry,
            };
        }
        return RetryAction::Abort;
    }
    if let Some(reqwest_err) = e.downcast_ref::<reqwest::Error>() {
        if let Some(status) = reqwest_err.status() {
            if status.as_u16() == 429 || status.as_u16() >= 500 {
                return RetryAction::Retry;
            }
            return RetryAction::Abort;
        }
        return RetryAction::Retry;
    }
    RetryAction::Abort
}

fn classify_rate_limit_error(e: &anyhow::Error) -> bool {
    if let Some(http_err) = e.downcast_ref::<HttpStatusError>() {
        return matches!(http_err.status, 429 | 503);
    }
    e.downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        .is_some_and(|status| matches!(status.as_u16(), 429 | 503))
}

#[derive(Debug)]
pub(crate) struct RetriedPostResponse {
    pub(crate) response: anyhow::Result<Value>,
    pub(crate) rate_limit_observations: usize,
}

/// Retry a `session.post()` call with default exponential backoff.
///
/// Inspects each response for `CloudKit` server errors (`serverErrorCode`)
/// and converts retryable ones (e.g. `TRY_AGAIN_LATER`, `CAS_OP_LOCK`)
/// into transient errors that trigger automatic retry.
pub async fn retry_post(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
    retry_config: &RetryConfig,
) -> anyhow::Result<Value> {
    // CloudKit API retries (album listing, query, sync-token paging) typically
    // run before or alongside a download bar but resolve in well under a
    // second; friendly retry-pause narration would flicker on every transient
    // 503 / CAS_OP_LOCK and add noise without adding signal. Calling the
    // no-mode variant pins to Off; download retries (where the user is actively
    // watching) get the friendly framing via `retry_with_backoff_with_mode`.
    hydration_progress::request_started();
    let result = retry::retry_with_backoff(retry_config, classify_api_error, || async {
        hydration_progress::attempt_started();
        let result = session
            .post(url, body.to_owned(), headers)
            .await
            .and_then(check_cloudkit_errors);
        hydration_progress::attempt_finished();
        result
    })
    .await;
    hydration_progress::request_finished();
    result
}

/// Same strict changes parser and size limit as the existing ingestion path;
/// optional provenance never substitutes a reserialized Value for raw bytes.
pub(crate) async fn retry_post_changes_with_evidence(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
    retry_config: &RetryConfig,
) -> anyhow::Result<ChangesResponse> {
    hydration_progress::request_started();
    let result = retry::retry_with_backoff(retry_config, classify_api_error, || async {
        hydration_progress::attempt_started();
        let result = async {
            let invocation = std::sync::Arc::new(InvocationIdentity {
                id: uuid::Uuid::new_v4(),
            });
            let mut response = session
                .post_changes_with_evidence(
                    url,
                    body.to_owned(),
                    headers,
                    InvocationToken(std::sync::Arc::clone(&invocation)),
                )
                .await?;
            // Never trust an adapter-provided acceptance capability.
            response.accepted = None;
            let valid_original = response.original.as_ref().is_some_and(|original| {
                original
                    .invocation
                    .as_ref()
                    .is_some_and(|actual| std::sync::Arc::ptr_eq(actual, &invocation))
                    && original.as_bytes() == response.body.as_slice()
            });
            if !valid_original {
                response.original = None;
            }
            anyhow::ensure!(
                response.body.len() <= super::inbox::MAX_CHANGES_PAGE_BYTES,
                super::error::ShadowPageError::from(anyhow::anyhow!(
                    "Provider changes page exceeded the capture byte limit"
                ))
            );
            let value = super::changes_json::parse(&response.body)?;
            check_cloudkit_errors(value)?;
            if valid_original {
                response.accepted = Some(AcceptedInvocation(invocation));
            }
            Ok(response)
        }
        .await;
        hydration_progress::attempt_finished();
        result
    })
    .await;
    hydration_progress::request_finished();
    result
}

pub(crate) async fn retry_post_changes_body(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
    retry_config: &RetryConfig,
) -> anyhow::Result<Vec<u8>> {
    hydration_progress::request_started();
    let result = retry::retry_with_backoff(retry_config, classify_api_error, || async {
        hydration_progress::attempt_started();
        let result = async {
            let bytes = session
                .post_changes_body(url, body.to_owned(), headers)
                .await?;
            anyhow::ensure!(
                bytes.len() <= super::inbox::MAX_CHANGES_PAGE_BYTES,
                super::error::ShadowPageError::from(anyhow::anyhow!(
                    "Provider changes page exceeded the capture byte limit"
                ))
            );
            let value = super::changes_json::parse(&bytes)?;
            check_cloudkit_errors(value)?;
            Ok(bytes)
        }
        .await;
        hydration_progress::attempt_finished();
        result
    })
    .await;
    hydration_progress::request_finished();
    result
}

/// Retry transport and HTTP failures while leaving record-level CloudKit
/// errors in the response for a batch-aware caller to classify. This is used
/// by `/records/lookup`, where one explicit `UNKNOWN_ITEM` is a successful
/// deletion result and must not fail unrelated records in the same batch.
/// Retain rate-limit observations even when every retry fails.
pub(crate) async fn retry_post_allowing_record_errors(
    session: &dyn PhotosSession,
    url: &str,
    body: &str,
    headers: &[(&str, &str)],
    retry_config: &RetryConfig,
) -> RetriedPostResponse {
    let rate_limit_observations = AtomicUsize::new(0);
    hydration_progress::request_started();
    let response = retry::retry_with_backoff(
        retry_config,
        |error| {
            if classify_rate_limit_error(error) {
                rate_limit_observations.fetch_add(1, Ordering::Relaxed);
            }
            classify_api_error(error)
        },
        || async {
            hydration_progress::attempt_started();
            let result = session.post(url, body.to_owned(), headers).await;
            hydration_progress::attempt_finished();
            result
        },
    )
    .await;
    hydration_progress::request_finished();
    RetriedPostResponse {
        response,
        rate_limit_observations: rate_limit_observations.load(Ordering::Relaxed),
    }
}

/// Errors from `changes/zone` when syncToken is invalid.
#[derive(Debug, thiserror::Error)]
pub enum SyncTokenError {
    /// Token is invalid/corrupted — fall back to full enumeration
    #[error("The saved iCloud sync token is no longer valid: {reason}")]
    InvalidToken { reason: Box<str> },
    /// Zone no longer exists — stop syncing this zone
    #[error("Apple no longer reports iCloud Photos zone {zone_name}")]
    ZoneNotFound { zone_name: Box<str> },
    /// Unexpected zone-level error (e.g. `RETRY_LATER`, THROTTLED) —
    /// treat as transient; do NOT advance the sync token.
    #[error("Apple returned an unexpected iCloud Photos zone error for {zone_name}: {error_code}")]
    UnexpectedZoneError {
        zone_name: Box<str>,
        error_code: Box<str>,
    },
}

impl SyncTokenError {
    /// Whether this error should trigger a fallback from incremental to full sync.
    /// Only token/zone-level issues warrant full re-enumeration; transient errors
    /// (THROTTLED, `RETRY_LATER`) should propagate without triggering an expensive fallback.
    pub fn should_fallback_to_full(&self) -> bool {
        matches!(self, Self::InvalidToken { .. } | Self::ZoneNotFound { .. })
    }
}

/// Check if a `ChangesZoneResult` contains a zone-level error.
/// Returns `Ok(())` if no error, `Err(SyncTokenError)` if there is one.
pub fn check_changes_zone_error(
    server_error_code: Option<&str>,
    reason: Option<&str>,
    zone_name: &str,
) -> Result<(), SyncTokenError> {
    match server_error_code {
        Some("BAD_REQUEST") => Err(SyncTokenError::InvalidToken {
            reason: reason.unwrap_or("Unknown sync continuation type").into(),
        }),
        Some("ZONE_NOT_FOUND") => Err(SyncTokenError::ZoneNotFound {
            zone_name: zone_name.into(),
        }),
        Some(code) => Err(SyncTokenError::UnexpectedZoneError {
            zone_name: zone_name.into(),
            error_code: code.into(),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {

    #[tokio::test]
    async fn synthetic_shared_session_actual_response_grants_scoped_source() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let endpoint = "/database/1/com.apple.photos.cloud/production/private/changes/zone";
        let raw = r#"{"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"records":[],"syncToken":"next","moreComing":false}]}"#;
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_string(raw))
            .expect(1)
            .mount(&server)
            .await;
        let tmp = tempfile::tempdir().unwrap();
        let mut session = crate::auth::session::Session::new(
            tmp.path(),
            "synthetic@example.invalid",
            "https://www.icloud.com",
            None,
        )
        .await
        .unwrap();
        let login: crate::auth::AccountLoginResponse =
            serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic"}})).unwrap();
        session.bind_principal(&login, None).unwrap();
        session
            .session_data
            .insert("ckdatabasews_url".to_owned(), server.uri());
        let owner = crate::state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &login,
        )
        .unwrap();
        let shared: crate::auth::SharedSession =
            std::sync::Arc::new(tokio::sync::RwLock::new(session));
        let request = serde_json::json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"saved","resultsLimit":200}]}).to_string();
        let headers = [("Content-type", "text/plain")];
        let retry = crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let response = super::retry_post_changes_with_evidence(
            &shared,
            &format!("{}{endpoint}", server.uri()),
            &request,
            &headers,
            &retry,
        )
        .await
        .unwrap();
        assert_eq!(response.body, raw.as_bytes());
        let original = response.original.unwrap();
        let accepted = response.accepted.unwrap();
        assert!(accepted.matches_original(&original));
        assert!(original.matches_authenticated_source(
            &owner,
            "private",
            "changes/zone",
            &request,
            &headers
        ));
        assert!(
            original
                .attested_bindings(&owner, "private", "changes/zone")
                .is_some()
        );
    }

    #[test]
    fn retained_request_serialization_never_contains_url_body_or_headers() {
        let request = super::RetainedRequest::new(
            "https://synthetic.invalid/path?secret=synthetic-url-secret",
            "synthetic-body-secret",
            &[("Authorization", "synthetic-header-secret")],
        );
        let bytes = serde_json::to_vec(&request).unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        for secret in [
            "synthetic-url-secret",
            "synthetic-body-secret",
            "synthetic-header-secret",
            "Authorization",
        ] {
            assert!(!text.contains(secret));
        }
        let decoded: super::RetainedRequest = serde_json::from_slice(&bytes).unwrap();
        assert!(decoded.matches_body("synthetic-body-secret"));
        assert!(!decoded.matches_body("different"));
        assert!(decoded.url.is_empty() && decoded.body.is_empty() && decoded.headers.is_empty());
    }

    #[test]
    fn retained_zone_attestation_omits_cursor_but_rechecks_zone() {
        let zone = serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let body = serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"synthetic-secret-cursor","resultsLimit":200}]}).to_string();
        let request = super::RetainedRequest::new("https://synthetic.invalid", &body, &[]);
        assert!(request.valid_changes_zone_request(&zone));
        let bytes = serde_json::to_vec(&request).unwrap();
        assert!(
            !std::str::from_utf8(&bytes)
                .unwrap()
                .contains("synthetic-secret-cursor")
        );
        let restored: super::RetainedRequest = serde_json::from_slice(&bytes).unwrap();
        assert!(restored.valid_changes_zone_request(&zone));
        assert!(!restored.valid_changes_zone_request(&serde_json::json!({"zoneName":"foreign"})));
    }

    #[test]
    fn retained_zone_attestation_rejects_malformed_request_shapes() {
        let zone = serde_json::json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"});
        let good =
            serde_json::json!({"zones":[{"zoneID":zone,"syncToken":"cursor","resultsLimit":200}]});
        let mut invalid = Vec::new();
        let mut v = good.clone();
        v["zones"][0]["syncToken"] = serde_json::json!(" ");
        invalid.push(v);
        let mut v = good.clone();
        v["zones"][0]["syncToken"] = serde_json::json!(null);
        invalid.push(v);
        let mut v = good.clone();
        v["zones"][0]["resultsLimit"] = serde_json::json!(201);
        invalid.push(v);
        let mut v = good.clone();
        v["zones"][0]["extra"] = serde_json::json!(true);
        invalid.push(v);
        let mut v = good.clone();
        v["extra"] = serde_json::json!(true);
        invalid.push(v);
        let mut v = good.clone();
        v["zones"]
            .as_array_mut()
            .unwrap()
            .push(good["zones"][0].clone());
        invalid.push(v);
        for value in invalid {
            let request =
                super::RetainedRequest::new("https://synthetic.invalid", &value.to_string(), &[]);
            assert!(!request.valid_changes_zone_request(&zone));
        }
        let request =
            super::RetainedRequest::new("https://synthetic.invalid", &good.to_string(), &[]);
        assert!(!request.valid_changes_zone_request(
            &serde_json::json!({"zoneName":"foreign","ownerRecordName":"_defaultOwner"})
        ));
        assert!(!request.matches_body("altered"));
    }

    #[test]
    fn synthetic_source_requires_matching_authenticated_owner_and_request() {
        let make_owner = |id: &str| {
            crate::state::db::account::AccountOwner::authenticated(
                "source@example.invalid",
                "com",
                &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":id}})).unwrap(),
            )
            .unwrap()
        };
        let owner = make_owner("source-a");
        let other = make_owner("source-b");
        let headers = [("Content-type", "text/plain")];
        let url = "https://source.invalid/query";
        let response = super::OriginalBody::synthetic_source(
            &owner,
            "https://source.invalid",
            url,
            "{}",
            &headers,
            br#"{"records":[]}"#,
        )
        .unwrap();
        let proof = response;
        assert!(proof.matches_authenticated_request(&owner, url, "{}", &headers));
        assert!(!proof.matches_authenticated_request(&other, url, "{}", &headers));
        assert!(!proof.matches_authenticated_request(
            &owner,
            "https://other.invalid/query",
            "{}",
            &headers
        ));
        assert!(!proof.matches_authenticated_request(&owner, url, "{ }", &headers));
        assert!(!proof.matches_authenticated_request(&owner, url, "{}", &[]));
    }

    #[test]
    fn synthetic_source_checks_endpoint_operation_and_exact_namespace() {
        let login =
            serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"one-provider"}})).unwrap();
        let owner = crate::state::db::account::AccountOwner::authenticated(
            "a@example.invalid",
            "com",
            &login,
        )
        .unwrap();
        let alias = crate::state::db::account::AccountOwner::authenticated(
            "b@example.invalid",
            "com",
            &login,
        )
        .unwrap();
        let headers = [("Content-type", "text/plain")];
        let url = "https://photos.invalid/database/1/com.apple.photos.cloud/production/private/changes/zone?x=1";
        let response = super::OriginalBody::synthetic_source(
            &owner,
            "https://photos.invalid",
            url,
            "{}",
            &headers,
            br#"{"zones":[]}"#,
        )
        .unwrap();
        let proof = response;
        assert!(proof.matches_authenticated_source(
            &owner,
            "private",
            "changes/zone",
            "{}",
            &headers
        ));
        assert!(!proof.matches_authenticated_source(
            &alias,
            "private",
            "changes/zone",
            "{}",
            &headers
        ));
        assert!(!proof.matches_authenticated_source(
            &owner,
            "shared",
            "changes/zone",
            "{}",
            &headers
        ));
        assert!(!proof.matches_authenticated_source(
            &owner,
            "private",
            "records/query",
            "{}",
            &headers
        ));
        let foreign = super::OriginalBody::synthetic_source(&owner, "https://photos.invalid", "https://photos.invalid.evil/database/1/com.apple.photos.cloud/production/private/changes/zone", "{}", &headers, br#"{"zones":[]}"#).unwrap();
        assert!(!foreign.matches_authenticated_source(
            &owner,
            "private",
            "changes/zone",
            "{}",
            &headers
        ));
    }

    #[test]
    fn fifth_evidence_page_falls_back_without_consuming_ingestion() {
        let pool = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
        let mut held = Vec::new();
        for _ in 0..4 {
            let permit = super::OriginalBody::slot_from(&pool).unwrap();
            held.push(
                super::OriginalBody::seal(b"{}", permit, [0; 32], "https://synthetic.invalid")
                    .unwrap(),
            );
        }
        assert!(super::OriginalBody::slot_from(&pool).is_none());
        // The ordinary raw changes body remains parseable without a proof slot.
        assert!(super::super::changes_json::parse(br#"{"zones":[]}"#).is_ok());
        held.pop();
        assert!(super::OriginalBody::slot_from(&pool).is_some());
    }

    #[test]
    fn evidence_slots_release_on_drop_and_reject_oversized() {
        let pool = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
        let first = super::OriginalBody::seal(
            b"one",
            super::OriginalBody::slot_from(&pool).unwrap(),
            [0; 32],
            "https://synthetic.invalid",
        )
        .unwrap();
        let second = super::OriginalBody::seal(
            b"two",
            super::OriginalBody::slot_from(&pool).unwrap(),
            [0; 32],
            "https://synthetic.invalid",
        )
        .unwrap();
        assert!(super::OriginalBody::slot_from(&pool).is_none());
        std::thread::spawn(move || drop(first)).join().unwrap();
        let reacquired = super::OriginalBody::slot_from(&pool).unwrap();
        let too_big = vec![b'x'; super::super::inbox::MAX_CHANGES_PAGE_BYTES + 1];
        assert!(
            super::OriginalBody::seal(&too_big, reacquired, [0; 32], "https://synthetic.invalid")
                .is_none()
        );
        assert!(super::OriginalBody::slot_from(&pool).is_some());
        drop(second);
    }

    #[test]
    fn malformed_raw_provenance_is_unavailable() {
        let owner = crate::state::db::account::AccountOwner::authenticated(
            "synthetic@example.invalid",
            "com",
            &serde_json::from_value(serde_json::json!({"dsInfo":{"dsid":"synthetic"}})).unwrap(),
        )
        .unwrap();
        assert!(
            super::OriginalBody::synthetic_source(
                &owner,
                "https://source.invalid",
                "https://source.invalid/query",
                "{}",
                &[],
                br#"{"records":["#
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn adapter_supplied_stale_acceptance_is_cleared() {
        struct Stale;
        #[async_trait::async_trait]
        impl super::PhotosSession for Stale {
            async fn post(
                &self,
                _: &str,
                _: String,
                _: &[(&str, &str)],
            ) -> anyhow::Result<serde_json::Value> {
                panic!("raw path expected")
            }
            async fn post_changes_with_evidence(
                &self,
                _: &str,
                _: String,
                _: &[(&str, &str)],
                _: super::InvocationToken,
            ) -> anyhow::Result<super::ChangesResponse> {
                Ok(super::ChangesResponse {
                    body: br#"{"zones":[]}"#.to_vec(),
                    original: None,
                    accepted: Some(super::AcceptedInvocation(std::sync::Arc::new(
                        super::InvocationIdentity {
                            id: uuid::Uuid::new_v4(),
                        },
                    ))),
                })
            }
            fn clone_box(&self) -> Box<dyn super::PhotosSession> {
                Box::new(Stale)
            }
        }
        let retry = crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let response = super::retry_post_changes_with_evidence(&Stale, "", "{}", &[], &retry)
            .await
            .unwrap();
        assert!(response.original.is_none());
        assert!(response.accepted.is_none());
    }

    #[tokio::test]
    async fn strict_changes_ingestion_rejects_duplicate_even_without_proof() {
        struct DuplicateChanges;
        #[async_trait::async_trait]
        impl super::PhotosSession for DuplicateChanges {
            async fn post(
                &self,
                _: &str,
                _: String,
                _: &[(&str, &str)],
            ) -> anyhow::Result<serde_json::Value> {
                panic!("raw changes path expected")
            }
            async fn post_changes_body(
                &self,
                _: &str,
                _: String,
                _: &[(&str, &str)],
            ) -> anyhow::Result<Vec<u8>> {
                Ok(br#"{"zones":[],"zones":[]}"#.to_vec())
            }
            fn clone_box(&self) -> Box<dyn super::PhotosSession> {
                Box::new(DuplicateChanges)
            }
        }
        let retry = crate::retry::RetryConfig {
            max_retries: 0,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };
        let result =
            super::retry_post_changes_with_evidence(&DuplicateChanges, "", "{}", &[], &retry).await;
        assert!(result.is_err());
    }

    use super::*;

    #[tokio::test]
    async fn missing_reference_transport_bounds_and_redacts_real_http_failures() {
        use crate::test_helpers::TracingCapture;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let (capture, _guard) = TracingCapture::install();
        let server = MockServer::start().await;
        for (route, status, body, expected) in [
            ("valid", 200, "{}".to_owned(), Ok(serde_json::json!({}))),
            (
                "private-url",
                421,
                "private-response-token".to_owned(),
                Err(DiagnosticLookupError::HttpStatus),
            ),
            (
                "malformed",
                200,
                "private-invalid-json".to_owned(),
                Err(DiagnosticLookupError::Malformed),
            ),
            (
                "oversize",
                200,
                "x".repeat(DIAGNOSTIC_RESPONSE_BYTES + 1),
                Err(DiagnosticLookupError::TooLarge),
            ),
        ] {
            Mock::given(method("POST"))
                .and(path(format!("/{route}")))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            assert_eq!(
                PhotosSession::post_diagnostic(
                    &reqwest::Client::new(),
                    &format!("{}/{route}?token=private-query-token", server.uri()),
                    "private-request-token".into(),
                    &[]
                )
                .await,
                expected
            );
        }
        for event in capture.events() {
            assert!(
                event
                    .fields
                    .values()
                    .all(|value| !value.contains("private-"))
            );
        }
    }

    #[tokio::test]
    async fn missing_reference_transport_bounds_chunked_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let chunk = vec![b'x'; 4096];
            for _ in 0..17 {
                if socket.write_all(b"1000\r\n").await.is_err()
                    || socket.write_all(&chunk).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        assert_eq!(
            PhotosSession::post_diagnostic(
                &reqwest::Client::new(),
                &format!("http://{address}"),
                "{}".into(),
                &[]
            )
            .await,
            Err(DiagnosticLookupError::TooLarge)
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn normal_photos_transport_retains_decode_error_retry_classification() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/records/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{"))
            .mount(&server)
            .await;
        let error = PhotosSession::post(
            &reqwest::Client::new(),
            &format!("{}/records/lookup", server.uri()),
            "{}".into(),
            &[],
        )
        .await
        .unwrap_err();
        assert!(error.is::<reqwest::Error>());
        assert!(matches!(classify_api_error(&error), RetryAction::Retry));
    }

    #[tokio::test]
    async fn raw_changes_transport_preserves_bytes_and_bounds_content_length() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let body = br#" { "number":9007199254740993.0, "unknown":[true,null] } "#;
        Mock::given(method("POST"))
            .and(path("/original"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.as_slice()))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        assert_eq!(
            PhotosSession::post_changes_body(
                &client,
                &format!("{}/original", server.uri()),
                "{}".into(),
                &[]
            )
            .await
            .unwrap(),
            body
        );
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let oversized = super::super::inbox::MAX_CHANGES_PAGE_BYTES + 1;
        let response = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {oversized}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        });
        let error = PhotosSession::post_changes_body(
            &client,
            &format!("http://{address}"),
            "{}".into(),
            &[],
        )
        .await
        .unwrap_err();
        response.await.unwrap();
        assert!(error.to_string().contains("capture byte limit"));
    }

    #[tokio::test]
    async fn raw_changes_transport_bounds_chunked_response_without_declared_length() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            let chunk = vec![b'x'; 1024 * 1024];
            for _ in 0..17 {
                if socket.write_all(b"100000\r\n").await.is_err()
                    || socket.write_all(&chunk).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        let error = PhotosSession::post_changes_body(
            &reqwest::Client::new(),
            &format!("http://{address}"),
            "{}".into(),
            &[],
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("capture byte limit"));
        server.await.unwrap();
    }

    #[test]
    fn provider_session_error_classification_uses_typed_status_not_body() {
        for status in [400, 401, 403, 404, 410, 421, 429, 500, 503] {
            let error = anyhow::Error::new(HttpStatusError {
                status,
                url: "https://example.invalid/private?token=secret".into(),
                retry_after: None,
                body: Some("Invalid global session HTTP 421".into()),
            })
            .context("wrapped provider request");
            assert_eq!(is_session_error(&error), matches!(status, 401 | 403 | 421));
        }
        assert!(!is_session_error(&anyhow::anyhow!(
            "HTTP 421 Invalid global session"
        )));
        let error = super::super::album::ProviderLookupError::Authentication {
            status: 421,
            message: "private response".into(),
        };
        assert_eq!(error.diagnostic(), "authentication");
        assert!(is_session_error(
            &anyhow::Error::new(error).context("lookup")
        ));
    }

    #[test]
    fn test_classify_non_reqwest_error_aborts() {
        let e: anyhow::Error = anyhow::anyhow!("some other error");
        assert_eq!(classify_api_error(&e), RetryAction::Abort);
    }

    #[tokio::test]
    async fn test_shared_session_implements_photos_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = crate::auth::session::Session::new(
            dir.path(),
            "test@shared.com",
            "https://example.com",
            None,
        )
        .await
        .unwrap();
        let shared: crate::auth::SharedSession =
            std::sync::Arc::new(tokio::sync::RwLock::new(session));

        // Verify it can be boxed as a PhotosSession
        let boxed: Box<dyn PhotosSession> = Box::new(shared.clone());
        assert_eq!(
            std::sync::Arc::strong_count(&shared),
            2,
            "boxing SharedSession as PhotosSession must retain the same shared session"
        );
        let _cloned = boxed.clone_box();
        assert_eq!(
            std::sync::Arc::strong_count(&shared),
            3,
            "clone_box must clone the underlying SharedSession"
        );

        // Verify clone_box produces a valid trait object
        let _cloned2 = _cloned.clone_box();
        assert_eq!(
            std::sync::Arc::strong_count(&shared),
            4,
            "cloned trait object must remain cloneable"
        );
    }

    #[test]
    fn test_check_cloudkit_errors_pass_through_normal() {
        let response = serde_json::json!({"records": [{"recordName": "A"}]});
        let result = check_cloudkit_errors(response.clone());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response);
    }

    #[test]
    fn test_check_cloudkit_errors_top_level_retryable() {
        let response = serde_json::json!({
            "serverErrorCode": "TRY_AGAIN_LATER",
            "reason": "Sync zone CAS Op-Lock failed"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "TRY_AGAIN_LATER");
        assert!(ck_err.retryable);
        assert!(!ck_err.service_not_activated);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
    }

    #[test]
    fn test_check_cloudkit_errors_top_level_non_retryable() {
        let response = serde_json::json!({
            "serverErrorCode": "ZONE_NOT_FOUND",
            "reason": "Zone not found"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
    }

    #[test]
    fn test_check_cloudkit_errors_per_record_mixed() {
        let response = serde_json::json!({
            "records": [
                {"recordName": "A"},
                {"serverErrorCode": "RETRY_LATER", "reason": "busy"}
            ]
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "RETRY_LATER");
        assert!(ck_err.retryable);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
    }

    #[test]
    fn test_check_cloudkit_errors_per_record_all_errored() {
        // When ALL records are errored, return Err
        let response = serde_json::json!({
            "records": [
                {"serverErrorCode": "RETRY_LATER", "reason": "busy"},
                {"serverErrorCode": "RETRY_LATER", "reason": "still busy"}
            ]
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "RETRY_LATER");
        assert!(ck_err.retryable);
    }

    #[test]
    fn test_check_cloudkit_errors_cas_op_lock() {
        let response = serde_json::json!({
            "serverErrorCode": "CAS_OP_LOCK",
            "reason": "concurrent write rejected"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(ck_err.retryable);
        assert!(!ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_throttled() {
        let response = serde_json::json!({
            "serverErrorCode": "THROTTLED",
            "reason": "rate limited"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(ck_err.retryable);
        assert!(!ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_zone_not_found_is_service_not_activated() {
        let response = serde_json::json!({
            "serverErrorCode": "ZONE_NOT_FOUND",
            "reason": "CKError: Zone not found"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_authentication_failed_is_service_not_activated() {
        let response = serde_json::json!({
            "serverErrorCode": "AUTHENTICATION_FAILED",
            "reason": "Authentication failed"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_access_denied_is_service_not_activated() {
        let response = serde_json::json!({
            "serverErrorCode": "ACCESS_DENIED",
            "reason": "private db access disabled for this account"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(!ck_err.retryable);
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_check_cloudkit_errors_private_db_disabled_by_reason() {
        // Even with an unknown error code, "private db access disabled" in the
        // reason should trigger service_not_activated detection.
        let response = serde_json::json!({
            "serverErrorCode": "UNKNOWN_CODE",
            "reason": "private db access disabled for this account"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(ck_err.service_not_activated);
    }

    #[test]
    fn test_classify_network_error_retries() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(reqwest::Client::new().get("http://127.0.0.1:1").send())
            .unwrap_err();
        let e: anyhow::Error = err.into();
        assert_eq!(classify_api_error(&e), RetryAction::Retry);
    }

    #[test]
    fn test_classify_retryable_cloudkit_error() {
        let err: anyhow::Error = CloudKitServerError {
            code: "RETRY_LATER".into(),
            reason: "busy".into(),
            retryable: true,
            service_not_activated: false,
        }
        .into();
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
    }

    #[test]
    fn test_classify_non_retryable_cloudkit_error() {
        let err: anyhow::Error = CloudKitServerError {
            code: "ZONE_NOT_FOUND".into(),
            reason: "missing".into(),
            retryable: false,
            service_not_activated: true,
        }
        .into();
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
    }

    #[test]
    fn test_is_service_not_activated_normal_error() {
        assert!(!is_service_not_activated("RETRY_LATER", "busy"));
    }

    #[test]
    fn test_check_cloudkit_errors_server_error_message_fallback() {
        let response = serde_json::json!({
            "serverErrorCode": "SOME_ERROR",
            "serverErrorMessage": "fallback message"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.reason, "fallback message");
    }

    #[test]
    fn test_check_cloudkit_errors_no_reason_defaults_to_unknown() {
        let response = serde_json::json!({
            "serverErrorCode": "SOME_ERROR"
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.reason, "unknown");
    }

    #[test]
    fn test_check_cloudkit_errors_empty_records_ok() {
        let response = serde_json::json!({"records": []});
        assert!(check_cloudkit_errors(response).is_ok());
    }

    #[test]
    fn test_cloudkit_server_error_display() {
        let err = CloudKitServerError {
            code: "TEST".into(),
            reason: "test reason".into(),
            retryable: false,
            service_not_activated: false,
        };
        let msg = err.to_string();
        assert!(msg.contains("TEST"));
        assert!(msg.contains("test reason"));
    }

    #[test]
    fn test_check_changes_zone_error_no_error() {
        let result = check_changes_zone_error(None, None, "PrimarySync");
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_changes_zone_error_unknown_code_is_unexpected() {
        let result = check_changes_zone_error(Some("SOME_OTHER_CODE"), None, "PrimarySync");
        assert!(result.is_err());
        match result.unwrap_err() {
            SyncTokenError::UnexpectedZoneError {
                zone_name,
                error_code,
            } => {
                assert_eq!(&*zone_name, "PrimarySync");
                assert_eq!(&*error_code, "SOME_OTHER_CODE");
            }
            other => panic!("Expected UnexpectedZoneError, got {other:?}"),
        }
    }

    #[test]
    fn test_check_changes_zone_error_bad_request() {
        let result = check_changes_zone_error(
            Some("BAD_REQUEST"),
            Some("Unknown sync continuation type"),
            "PrimarySync",
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(&*reason, "Unknown sync continuation type");
            }
            other => panic!("Expected InvalidToken, got {other:?}"),
        }
    }

    #[test]
    fn test_check_changes_zone_error_bad_request_no_reason() {
        let result = check_changes_zone_error(Some("BAD_REQUEST"), None, "PrimarySync");
        match result.unwrap_err() {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(&*reason, "Unknown sync continuation type");
            }
            other => panic!("Expected InvalidToken, got {other:?}"),
        }
    }

    #[test]
    fn test_check_changes_zone_error_zone_not_found() {
        let result = check_changes_zone_error(Some("ZONE_NOT_FOUND"), None, "SharedSync-123");
        assert!(result.is_err());
        match result.unwrap_err() {
            SyncTokenError::ZoneNotFound { zone_name } => {
                assert_eq!(&*zone_name, "SharedSync-123");
            }
            other => panic!("Expected ZoneNotFound, got {other:?}"),
        }
    }

    #[test]
    fn test_sync_token_error_display_invalid_token() {
        let err = SyncTokenError::InvalidToken {
            reason: "bad token".into(),
        };
        assert_eq!(
            err.to_string(),
            "The saved iCloud sync token is no longer valid: bad token"
        );
    }

    #[test]
    fn test_sync_token_error_display_zone_not_found() {
        let err = SyncTokenError::ZoneNotFound {
            zone_name: "SharedSync-ABC".into(),
        };
        assert_eq!(
            err.to_string(),
            "Apple no longer reports iCloud Photos zone SharedSync-ABC"
        );
    }

    #[test]
    fn test_sync_token_error_downcast_from_anyhow() {
        let err: anyhow::Error = SyncTokenError::InvalidToken {
            reason: "expired".into(),
        }
        .into();
        let downcasted = err.downcast_ref::<SyncTokenError>();
        assert!(downcasted.is_some());
        assert_eq!(
            downcasted.unwrap().to_string(),
            "The saved iCloud sync token is no longer valid: expired"
        );
    }

    #[test]
    fn test_sync_token_error_display_empty_reason() {
        let err = SyncTokenError::InvalidToken { reason: "".into() };
        assert_eq!(
            err.to_string(),
            "The saved iCloud sync token is no longer valid: "
        );
    }

    /// T-2: Mock session returns HTTP 503 on first call, 200 on second.
    /// `retry_post` should retry the call and return the successful response.
    #[tokio::test]
    async fn test_retry_post_retries_on_503() {
        struct RetrySession {
            call_count: std::sync::atomic::AtomicU32,
        }

        #[async_trait::async_trait]
        impl PhotosSession for RetrySession {
            async fn post(
                &self,
                _url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                let n = self
                    .call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if n == 0 {
                    // First call: simulate 503 via CloudKit serverErrorCode
                    Ok(serde_json::json!({
                        "serverErrorCode": "TRY_AGAIN_LATER",
                        "reason": "Service Unavailable"
                    }))
                } else {
                    // Second call: success
                    Ok(serde_json::json!({
                        "records": [{"recordName": "A1"}]
                    }))
                }
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                panic!("not needed for test")
            }
        }

        let session = RetrySession {
            call_count: std::sync::atomic::AtomicU32::new(0),
        };
        let config = RetryConfig {
            max_retries: 3,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };

        let result = retry_post(&session, "https://example.com/api", "{}", &[], &config).await;
        assert!(
            result.is_ok(),
            "retry_post should succeed on second attempt"
        );

        let response = result.unwrap();
        let records = response["records"].as_array().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["recordName"], "A1");

        // Verify exactly 2 calls were made (1 retry + 1 success)
        assert_eq!(
            session.call_count.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    /// Build a reqwest::Error with the given HTTP status code (no network needed).
    fn reqwest_status_error(status: u16) -> anyhow::Error {
        let http_resp = http::Response::builder()
            .status(status)
            .body(Vec::<u8>::new())
            .unwrap();
        let resp = reqwest::Response::from(http_resp);
        resp.error_for_status().unwrap_err().into()
    }

    #[test]
    fn test_post_503_is_retryable() {
        let err = reqwest_status_error(503);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
        assert!(classify_rate_limit_error(&err));
    }

    #[test]
    fn test_post_429_is_retryable() {
        let err = reqwest_status_error(429);
        assert_eq!(classify_api_error(&err), RetryAction::Retry);
        assert!(classify_rate_limit_error(&err));
    }

    #[test]
    fn test_post_421_aborts() {
        let err = reqwest_status_error(421);
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
        assert!(!classify_rate_limit_error(&err));
    }

    // ── Gap: empty records array passes through cleanly ──────────────

    #[test]
    fn test_check_cloudkit_errors_empty_records_array() {
        let response = serde_json::json!({"records": []});
        let result = check_cloudkit_errors(response.clone());
        assert!(result.is_ok());
        let val = result.unwrap();
        assert_eq!(val["records"].as_array().unwrap().len(), 0);
    }

    // ── Gap: response without records key passes through ─────────────

    #[test]
    fn test_check_cloudkit_errors_no_records_key() {
        // Some CloudKit endpoints return data without a "records" key
        // (e.g., zones/list). Should pass through unmodified.
        let response = serde_json::json!({"zones": [{"zoneID": {"zoneName": "PrimarySync"}}]});
        let result = check_cloudkit_errors(response.clone());
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response);
    }

    // ── Gap: per-record mixed with non-retryable error ───────────────

    #[test]
    fn test_check_cloudkit_errors_per_record_non_retryable_still_filters() {
        let response = serde_json::json!({
            "records": [
                {"recordName": "VALID_1"},
                {"serverErrorCode": "ZONE_NOT_FOUND", "reason": "not found"},
                {"recordName": "VALID_2"}
            ]
        });
        let err = check_cloudkit_errors(response).unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert_eq!(&*ck_err.code, "ZONE_NOT_FOUND");
        assert!(!ck_err.retryable);
        assert_eq!(classify_api_error(&err), RetryAction::Abort);
    }

    // ── Gap: SyncTokenError::should_fallback_to_full classification ──

    #[test]
    fn test_sync_token_error_fallback_classification() {
        let invalid = SyncTokenError::InvalidToken {
            reason: "bad token".into(),
        };
        assert!(
            invalid.should_fallback_to_full(),
            "InvalidToken should trigger full fallback"
        );

        let zone_gone = SyncTokenError::ZoneNotFound {
            zone_name: "Primary".into(),
        };
        assert!(
            zone_gone.should_fallback_to_full(),
            "ZoneNotFound should trigger full fallback"
        );

        let transient = SyncTokenError::UnexpectedZoneError {
            zone_name: "Primary".into(),
            error_code: "RETRY_LATER".into(),
        };
        assert!(
            !transient.should_fallback_to_full(),
            "UnexpectedZoneError should NOT trigger full fallback"
        );
    }

    #[test]
    fn classify_http_status_error_retries_5xx() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 503,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Retry));
    }

    #[test]
    fn classify_http_status_error_retries_429() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 429,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Retry));
    }

    #[test]
    fn classify_http_status_error_honors_retry_after() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 429,
            url: "https://example.com".to_string(),
            retry_after: Some(Duration::from_secs(7)),
            body: None,
        });
        match classify_api_error(&err) {
            RetryAction::RetryAfter(d) => assert_eq!(d, Duration::from_secs(7)),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_status_error_503_with_retry_after() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 503,
            url: "https://example.com".to_string(),
            retry_after: Some(Duration::from_secs(2)),
            body: None,
        });
        match classify_api_error(&err) {
            RetryAction::RetryAfter(d) => assert_eq!(d, Duration::from_secs(2)),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
    }

    #[test]
    fn classify_http_status_error_aborts_4xx() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 401,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Abort));
    }

    #[test]
    fn classify_http_status_error_aborts_403() {
        let err = anyhow::Error::new(HttpStatusError {
            status: 403,
            url: "https://example.com".to_string(),
            retry_after: None,
            body: None,
        });
        assert!(matches!(classify_api_error(&err), RetryAction::Abort));
    }

    #[test]
    fn truncate_body_leaves_short_bodies_unchanged() {
        let body = r#"{"serverErrorCode":"AUTHENTICATION_FAILED","reason":"no auth method found"}"#;
        assert_eq!(truncate_body(body), body);
    }

    #[test]
    fn truncate_body_clips_oversized_bodies() {
        let body = "x".repeat(MAX_PRESERVED_BODY * 2);
        let out = truncate_body(&body);
        assert!(
            out.len() <= MAX_PRESERVED_BODY + 4,
            "truncated body must stay under the cap plus the marker, got len {}",
            out.len()
        );
        assert!(
            out.ends_with('…'),
            "truncation marker must be appended to signal the clip"
        );
    }

    #[test]
    fn truncate_body_respects_utf8_boundaries() {
        // Multi-byte chars must not be split — pad with a 3-byte char so
        // the naive byte-slice would land in the middle.
        let mut body = "a".repeat(MAX_PRESERVED_BODY - 1);
        body.push('€'); // 3 bytes: 0xE2 0x82 0xAC
        body.push_str(&"b".repeat(MAX_PRESERVED_BODY));
        let out = truncate_body(&body);
        // Must be valid UTF-8 (the format! call would panic otherwise).
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
    }

    // ────────────────────────────────────────────────────────────────
    // wiremock-based tests: exercise the real reqwest path through
    // `PhotosSession::post` to prove that CloudKit error bodies
    // actually end up on `HttpStatusError.body`. The unit-level tests
    // above only cover `truncate_body`; these tests close the loop
    // between "server returned a body" and "caller can read it".
    // ────────────────────────────────────────────────────────────────

    use wiremock::matchers::method as wm_method;
    use wiremock::{Mock, ResponseTemplate};

    #[cfg(all(unix, debug_assertions))]
    #[test]
    fn dump_files_are_private_in_existing_directory() {
        use std::os::unix::fs::PermissionsExt;

        if std::env::var_os("KEI_DUMP_PERMISSION_CHILD").is_some() {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let server = wiremock::MockServer::start().await;
                Mock::given(wm_method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_json(serde_json::json!({"records": []})),
                    )
                    .mount(&server)
                    .await;
                let response =
                    PhotosSession::post(&reqwest::Client::new(), &server.uri(), "{}".into(), &[])
                        .await
                        .unwrap();
                assert_eq!(response, serde_json::json!({"records": []}));
            });
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        // Isolate process-wide umask and environment changes from parallel tests.
        let output = std::process::Command::new("sh")
            .args(["-c", "umask 022; exec \"$@\"", "sh"])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "icloud::photos::session::tests::dump_files_are_private_in_existing_directory",
                "--nocapture",
            ])
            .env("KEI_DUMP_PERMISSION_CHILD", "1")
            .env("KEI_REQUEST_DUMP_DIR", dir.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(files.len(), 2);
        for file in files {
            assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
            let expected = if file.file_name().to_str().unwrap().contains(".req.") {
                "{}"
            } else {
                "{\"records\":[]}"
            };
            assert_eq!(std::fs::read_to_string(file.path()).unwrap(), expected);
        }
    }

    /// The FIDO failure mode from issue #221: CloudKit returns 401 with
    /// `"no auth method found"` in the JSON body. A real reqwest client
    /// posting to a wiremock endpoint must surface that body on the
    /// resulting `HttpStatusError` so the library.rs mapping can log
    /// the security-key hint.
    #[tokio::test]
    async fn wiremock_401_preserves_body_in_http_status_error() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_401_preserves_body_in_http_status_error",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        let body = r#"{"serverErrorCode":"AUTHENTICATION_FAILED","reason":"no auth method found"}"#;
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string(body))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("401 must propagate as an error");
        let http_err = err
            .downcast_ref::<HttpStatusError>()
            .expect("expected HttpStatusError");
        assert_eq!(http_err.status, 401);
        let preserved = http_err
            .body
            .as_deref()
            .expect("401 body must be preserved for downstream FIDO/auth diagnostics");
        assert!(
            preserved.contains("no auth method found"),
            "preserved body must include the FIDO-indicating signal, got: {preserved}"
        );
    }

    /// A 503 body (transient error) is preserved too, so the retry
    /// path's warnings can include server-provided detail. Guards
    /// against a refactor that accidentally scopes body preservation
    /// to 401 only.
    #[tokio::test]
    async fn wiremock_5xx_preserves_body_in_http_status_error() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_5xx_preserves_body_in_http_status_error",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_string("service unavailable"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("503 must propagate as an error");
        let http_err = err.downcast_ref::<HttpStatusError>().unwrap();
        assert_eq!(http_err.status, 503);
        assert_eq!(http_err.body.as_deref(), Some("service unavailable"));
    }

    /// An empty error body yields `body: None`, not `Some("")`. Guards
    /// the downstream check `http_err.body.as_deref()` from having to
    /// special-case "" vs absent.
    #[tokio::test]
    async fn wiremock_empty_body_stays_none() {
        if crate::test_helpers::skip_if_loopback_bind_blocked("wiremock_empty_body_stays_none") {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(403)) // no body
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("403 must propagate");
        let http_err = err.downcast_ref::<HttpStatusError>().unwrap();
        assert_eq!(http_err.status, 403);
        assert!(
            http_err.body.is_none(),
            "empty response body must be None, not Some(\"\"), got: {:?}",
            http_err.body
        );
    }

    /// HTTP 200 with a truncated / malformed JSON body must surface as an
    /// error, not a silently-empty parse. A silent parse would let a
    /// pathological CloudKit page pretend to be a valid zero-record
    /// response and halt enumeration prematurely.
    #[tokio::test]
    async fn wiremock_200_with_truncated_json_returns_error() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_200_with_truncated_json_returns_error",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        Mock::given(wm_method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("{\"records\": [{\"incomplete\""),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let result = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await;
        assert!(
            result.is_err(),
            "200 with malformed JSON body must be reported as an error, not a silent empty parse"
        );
    }

    /// `read_bounded_error_body` stops reading off the wire once it
    /// has `MAX_PRESERVED_BODY + 16` bytes, even if the server sent
    /// orders of magnitude more. A regression to `resp.text().await`
    /// would buffer the full body and fail this assertion. The
    /// existing `wiremock_oversized_body_is_truncated` test only
    /// proves the *output* is clipped (because `truncate_body` runs
    /// after the read), so it can't distinguish "streamed and stopped"
    /// from "buffered and trimmed".
    #[tokio::test]
    async fn read_bounded_error_body_caps_at_max_plus_grace() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "read_bounded_error_body_caps_at_max_plus_grace",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        let huge = "x".repeat(64 * 1024);
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string(&huge))
            .mount(&server)
            .await;

        let resp = reqwest::Client::new()
            .post(format!("{}/records/query", server.uri()))
            .body("{}")
            .send()
            .await
            .expect("request");
        assert!(resp.status().is_server_error());

        let body = read_bounded_error_body(resp, "test").await;
        assert!(
            body.len() <= MAX_PRESERVED_BODY + 16,
            "streaming cap breached: body.len() = {}, cap = {}",
            body.len(),
            MAX_PRESERVED_BODY + 16
        );
    }

    /// An oversized body is truncated with the `…` marker so a
    /// pathological CloudKit response (HTML error page, stack trace)
    /// can't blow up the error path.
    #[tokio::test]
    async fn wiremock_oversized_body_is_truncated() {
        if crate::test_helpers::skip_if_loopback_bind_blocked(
            "wiremock_oversized_body_is_truncated",
        ) {
            return;
        }
        let server = crate::start_wiremock_or_skip!();
        let huge = "x".repeat(MAX_PRESERVED_BODY * 3);
        Mock::given(wm_method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string(huge))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let err = PhotosSession::post(
            &client,
            &format!("{}/records/query", server.uri()),
            "{}".to_string(),
            &[],
        )
        .await
        .expect_err("500 must propagate");
        let http_err = err.downcast_ref::<HttpStatusError>().unwrap();
        let preserved = http_err.body.as_deref().unwrap();
        assert!(
            preserved.ends_with('…'),
            "oversized body must be clipped with the truncation marker, got (len {}): {}",
            preserved.len(),
            preserved
        );
        assert!(
            preserved.len() <= MAX_PRESERVED_BODY + '…'.len_utf8(),
            "truncated body must stay under the cap plus the marker, got len {}",
            preserved.len()
        );
    }

    #[tokio::test]
    async fn retry_post_persistent_503_terminates_within_max_attempts() {
        struct Always503 {
            call_count: std::sync::atomic::AtomicU32,
        }

        #[async_trait::async_trait]
        impl PhotosSession for Always503 {
            async fn post(
                &self,
                _url: &str,
                _body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                self.call_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(serde_json::json!({
                    "serverErrorCode": "TRY_AGAIN_LATER",
                    "reason": "Service Unavailable"
                }))
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                panic!("not needed for test")
            }
        }

        let session = Always503 {
            call_count: std::sync::atomic::AtomicU32::new(0),
        };
        let config = RetryConfig {
            max_retries: 4,
            base_delay_secs: 0,
            max_delay_secs: 0,
        };

        let result = retry_post(&session, "https://example.com/api", "{}", &[], &config).await;
        assert!(result.is_err(), "persistent 503 must eventually fail");

        let err = result.unwrap_err();
        let ck_err = err.downcast_ref::<CloudKitServerError>().unwrap();
        assert!(
            ck_err.retryable,
            "final error must still be tagged retryable"
        );

        let total_calls = session.call_count.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            total_calls, 5,
            "must be exactly max_retries + 1 = 5 attempts, got {total_calls}"
        );
    }
}
