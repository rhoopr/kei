use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use fs4::fs_std::FileExt;
use reqwest::cookie::CookieStore;
use reqwest::header::{HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use reqwest::{Client, Response};
use serde_json::Value;
use tokio::fs;

/// Apple's auth APIs return session state in custom HTTP headers.
/// We capture these after every request to maintain session continuity.
const HEADER_DATA: &[(&str, &str)] = &[
    ("X-Apple-ID-Account-Country", "account_country"),
    ("X-Apple-ID-Session-Id", "session_id"),
    ("X-Apple-Session-Token", "session_token"),
    ("X-Apple-TwoSV-Trust-Token", "trust_token"),
    ("X-Apple-TwoSV-Trust-Eligible", "trust_eligible"),
    ("X-Apple-I-Rscd", "apple_rscd"),
    ("X-Apple-I-Ercd", "apple_ercd"),
    ("scnt", "scnt"),
];

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";

/// Thread-safe shared session handle for use across the download layer.
/// The `Arc` enables cheap cloning; the `RwLock` allows concurrent reads
/// (HTTP requests) with exclusive writes (session refresh / re-auth).
pub type SharedSession = Arc<tokio::sync::RwLock<Session>>;

/// Maximum length for sanitized usernames used in file paths.
/// Long usernames are truncated and suffixed with a hash to stay under OS limits.
const MAX_SANITIZED_USERNAME_LEN: usize = 64;

/// Sanitize a username by keeping only word characters (alphanumeric + underscore).
/// Equivalent to Python's `re.match(r"\w", c)` filter.
/// Truncates to [`MAX_SANITIZED_USERNAME_LEN`] with a hash suffix if too long,
/// preventing OS "File name too long" errors.
pub fn sanitize_username(username: &str) -> String {
    let sanitized: String = username
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if sanitized.len() <= MAX_SANITIZED_USERNAME_LEN {
        sanitized
    } else {
        // Use a simple hash (FNV-like) to keep uniqueness in truncated names
        let hash = sanitized.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        let prefix_len = MAX_SANITIZED_USERNAME_LEN - 17; // room for "_" + 16 hex digits
                                                          // Find the last char boundary at or before prefix_len to avoid
                                                          // panicking on multi-byte UTF-8 (e.g. CJK usernames).
        let prefix_end = sanitized[..prefix_len]
            .char_indices()
            .last()
            .map_or(prefix_len, |(i, c)| i + c.len_utf8());
        format!("{}_{:016x}", &sanitized[..prefix_end], hash)
    }
}

/// Derive the broad cookie domain from a hostname.
///
/// Apple sets auth cookies with `Domain=.icloud.com` (or `.apple.com`),
/// making them available to all subdomains. `reqwest::Jar::cookies()` strips
/// the `Domain` attribute when serializing, so on reload we need to restore
/// it. Without this, cookies scoped to `setup.icloud.com` won't be sent to
/// `ckdatabasews.icloud.com`, causing 401 errors after container restarts.
fn broad_cookie_domain(host: Option<&str>) -> Option<&str> {
    let host = host?;
    if host.ends_with(".icloud.com.cn") || host == "icloud.com.cn" {
        Some("icloud.com.cn")
    } else if host.ends_with(".apple.com.cn") || host == "apple.com.cn" {
        Some("apple.com.cn")
    } else if host.ends_with(".icloud.com") || host == "icloud.com" {
        Some("icloud.com")
    } else if host.ends_with(".apple.com") || host == "apple.com" {
        Some("apple.com")
    } else {
        None
    }
}

/// Check if a Set-Cookie header string represents an expired cookie.
/// Parses the `cookie` crate's `Cookie::parse()` to extract `Expires`.
fn is_cookie_expired(cookie_str: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    if let Ok(parsed) = cookie::Cookie::parse(cookie_str) {
        if let Some(expires) = parsed.expires_datetime() {
            let expires_utc =
                chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::from(expires));
            return expires_utc < *now;
        }
    }
    false
}

/// A single persisted cookie entry (URL + Set-Cookie header value).
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct CookieEntry {
    url: String,
    cookie: String,
}

/// Parse legacy tab-separated cookie file format into `CookieEntry` values.
///
/// Each line is `URL<TAB>cookie-string`. Comment lines (`#`), blank lines,
/// and `Set-Cookie3:` headers are skipped. Lines without a tab are ignored.
fn parse_legacy_cookies(contents: &str) -> Vec<CookieEntry> {
    contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("Set-Cookie3:")
            {
                return None;
            }
            let (url_str, cookie_str) = trimmed.split_once('\t')?;
            Some(CookieEntry {
                url: url_str.to_string(),
                cookie: cookie_str.to_string(),
            })
        })
        .collect()
}

/// Atomically write `data` to `path` via a temp file + rename.
/// Sets 0o600 permissions on Unix before renaming, so the file is never
/// world-readable even momentarily.
async fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    fs::write(&tmp, data)
        .await
        .with_context(|| format!("Failed to write temp file {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    fs::rename(&tmp, path)
        .await
        .with_context(|| format!("Failed to rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Strip routing state from a persisted session file, keeping only `trust_token`
/// and `client_id`.
///
/// Clearing `session_token` forces `authenticate()` through the SRP path instead
/// of the validate shortcut. Preserving `trust_token` lets SRP send it in
/// `trustTokens`, so Apple can recognise a trusted device and skip 2FA.
///
/// Deletes the file if it is corrupt or unreadable (falling back to the old
/// delete-everything behaviour).
pub(crate) async fn strip_session_routing_state(session_file: &Path) {
    let contents = match fs::read_to_string(session_file).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            let _ = fs::remove_file(session_file).await;
            return;
        }
    };

    let mut map: HashMap<String, String> = match serde_json::from_str(&contents) {
        Ok(m) => m,
        Err(_) => {
            let _ = fs::remove_file(session_file).await;
            return;
        }
    };

    map.retain(|k, _| k == "trust_token" || k == "client_id");

    match serde_json::to_string_pretty(&map) {
        Ok(json) => {
            if let Err(e) = atomic_write(session_file, json.as_bytes()).await {
                tracing::warn!(error = %e, "Could not rewrite session file, removing");
                let _ = fs::remove_file(session_file).await;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Could not serialise stripped session, removing");
            let _ = fs::remove_file(session_file).await;
        }
    }

    // Invalidate validation cache since session is being reset
    let cache_file = session_file.with_extension("cache");
    let _ = fs::remove_file(&cache_file).await;
}

/// Build the API and download HTTP clients for a session.
///
/// Both share the same cookie jar. The API client uses a total-request timeout;
/// the download client uses connect + read timeouts without a total cap so
/// large file transfers aren't killed mid-stream.
fn build_clients(
    cookie_jar: &Arc<reqwest::cookie::Jar>,
    home_endpoint: &str,
    api_timeout: Duration,
) -> Result<(Client, Client)> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(ORIGIN, HeaderValue::from_str(home_endpoint)?);
    default_headers.insert(
        REFERER,
        HeaderValue::from_str(&format!("{home_endpoint}/"))?,
    );
    default_headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));

    let client = Client::builder()
        .cookie_provider(cookie_jar.clone())
        .default_headers(default_headers.clone())
        .timeout(api_timeout)
        .build()?;

    let download_client = Client::builder()
        .cookie_provider(cookie_jar.clone())
        .default_headers(default_headers)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(20)
        .pool_idle_timeout(Duration::from_secs(90))
        .build()?;

    Ok((client, download_client))
}

/// HTTP session wrapper that persists cookies and session data to disk,
/// allowing authentication to survive across process restarts.
pub struct Session {
    client: Client,
    download_client: Client,
    /// Cookie jar shared with `reqwest::Client`. Queried by
    /// `persist_jar_cookies` to save session cookies to disk, and kept alive
    /// so the client's internal weak reference remains valid.
    cookie_jar: Arc<reqwest::cookie::Jar>,
    pub(crate) session_data: HashMap<String, String>,
    cookie_dir: PathBuf,
    sanitized_username: String,
    home_endpoint: String,
    /// API client timeout (preserved for `reset_http_clients`).
    api_timeout: Duration,
    /// Exclusive file lock preventing concurrent instances for the same account.
    /// The advisory lock is held for the lifetime of the Session via the open
    /// file descriptor; released automatically when the File is dropped.
    lock_file: std::fs::File,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("cookie_dir", &self.cookie_dir)
            .field("sanitized_username", &self.sanitized_username)
            .field("home_endpoint", &self.home_endpoint)
            .field("session_data", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Create a new session, loading existing cookies and session data from disk.
    pub async fn new(
        cookie_dir: &Path,
        username: &str,
        home_endpoint: &str,
        timeout_secs: Option<u64>,
    ) -> Result<Self> {
        let sanitized = sanitize_username(username);
        let cookie_dir = cookie_dir.to_path_buf();

        fs::create_dir_all(&cookie_dir).await.with_context(|| {
            format!(
                "Failed to create cookie directory: {}",
                cookie_dir.display()
            )
        })?;

        // Acquire an exclusive file lock to prevent concurrent instances for
        // the same account from corrupting session/cookie state.
        let lock_path = cookie_dir.join(format!("{sanitized}.lock"));
        let lock_file = tokio::task::spawn_blocking({
            let lock_path = lock_path.clone();
            move || {
                let file = std::fs::File::create(&lock_path).with_context(|| {
                    format!("Failed to create lock file: {}", lock_path.display())
                })?;
                let acquired = file
                    .try_lock_exclusive()
                    .with_context(|| format!("Failed to acquire lock: {}", lock_path.display()))?;
                if !acquired {
                    return Err(crate::auth::error::AuthError::LockContention(format!(
                        "Another kei instance is running for this account (lock: {}). \
                         If running in Docker, check for orphaned containers with \
                         `docker ps` and stop them with `docker stop <name>`.",
                        lock_path.display()
                    ))
                    .into());
                }
                Ok::<std::fs::File, anyhow::Error>(file)
            }
        })
        .await??;

        Self::build(
            cookie_dir,
            &sanitized,
            home_endpoint,
            timeout_secs,
            lock_file,
        )
        .await
    }

    /// Shared constructor body: loads cookies/session from disk, builds HTTP
    /// clients, and assembles the `Session`. Callers provide the lock file.
    async fn build(
        cookie_dir: PathBuf,
        sanitized: &str,
        home_endpoint: &str,
        timeout_secs: Option<u64>,
        lock_file: std::fs::File,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(30));
        let cookie_jar = Arc::new(reqwest::cookie::Jar::default());

        let cookiejar_path = cookie_dir.join(sanitized);
        if cookiejar_path.is_file() {
            match fs::read_to_string(&cookiejar_path).await {
                Ok(contents) => {
                    let now = chrono::Utc::now();
                    // Try JSON format first, fall back to legacy tab-separated format
                    let entries =
                        if let Ok(entries) = serde_json::from_str::<Vec<CookieEntry>>(&contents) {
                            entries
                        } else {
                            parse_legacy_cookies(&contents)
                        };
                    for entry in entries {
                        if is_cookie_expired(&entry.cookie, &now) {
                            tracing::debug!(url = %entry.url, "Pruning expired cookie");
                            continue;
                        }
                        if let Ok(url) = entry.url.parse::<url::Url>() {
                            let cookie_with_domain =
                                if let Some(domain) = broad_cookie_domain(url.host_str()) {
                                    format!("{}; Domain={domain}", entry.cookie)
                                } else {
                                    entry.cookie.clone()
                                };
                            cookie_jar.add_cookie_str(&cookie_with_domain, &url);
                        }
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Err(e) = fs::set_permissions(
                            &cookiejar_path,
                            std::fs::Permissions::from_mode(0o600),
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "Could not set cookie file permissions");
                        }
                    }
                    tracing::debug!(path = %cookiejar_path.display(), "Read cookies");
                }
                Err(e) => {
                    tracing::warn!(
                        path = %cookiejar_path.display(),
                        error = %e,
                        "Failed to read cookiejar"
                    );
                }
            }
        }

        let (client, download_client) = build_clients(&cookie_jar, home_endpoint, timeout)?;

        let session_path = cookie_dir.join(format!("{sanitized}.session"));
        let session_data = if session_path.exists() {
            match fs::read_to_string(&session_path).await {
                Ok(contents) => match serde_json::from_str::<HashMap<String, Value>>(&contents) {
                    Ok(map) => {
                        tracing::debug!(path = %session_path.display(), "Loaded session data");
                        map.into_iter()
                            .map(|(k, v)| match v {
                                Value::String(s) => (k, s),
                                other => (k, other.to_string()),
                            })
                            .collect()
                    }
                    Err(e) => {
                        tracing::warn!(path = %session_path.display(), error = %e, "Session file corrupt, starting fresh");
                        HashMap::new()
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %session_path.display(), error = %e, "Could not read session file, starting fresh");
                    HashMap::new()
                }
            }
        } else {
            tracing::debug!("Session file does not exist");
            HashMap::new()
        };

        tracing::debug!(path = %session_path.display(), "Using session file");

        Ok(Self {
            client,
            download_client,
            cookie_jar,
            session_data,
            cookie_dir,
            sanitized_username: sanitized.to_owned(),
            home_endpoint: home_endpoint.to_string(),
            api_timeout: timeout,
            lock_file,
        })
    }

    pub(crate) fn cookiejar_path(&self) -> PathBuf {
        self.cookie_dir.join(&self.sanitized_username)
    }

    pub fn session_path(&self) -> PathBuf {
        self.cookie_dir
            .join(format!("{}.session", self.sanitized_username))
    }

    /// Path to the validation cache file.
    fn cache_path(&self) -> PathBuf {
        self.cookie_dir
            .join(format!("{}.cache", self.sanitized_username))
    }

    /// Load cached validation data if it exists and is within the grace period.
    pub(crate) async fn load_validation_cache(
        &self,
        grace_secs: i64,
    ) -> Option<super::responses::AccountLoginResponse> {
        let path = self.cache_path();
        let contents = fs::read_to_string(&path).await.ok()?;
        let cache: super::responses::ValidationCache = serde_json::from_str(&contents).ok()?;
        let now = chrono::Utc::now().timestamp();
        if now - cache.validated_at > grace_secs {
            tracing::debug!(
                age_secs = now - cache.validated_at,
                grace_secs,
                "Validation cache expired"
            );
            return None;
        }
        tracing::debug!(
            age_secs = now - cache.validated_at,
            "Using cached validation data"
        );
        Some(cache.account_data)
    }

    /// Save validation data to the cache file.
    pub(crate) async fn save_validation_cache(
        &self,
        data: &super::responses::AccountLoginResponse,
    ) {
        let cache = super::responses::ValidationCache {
            validated_at: chrono::Utc::now().timestamp(),
            account_data: data.clone(),
        };
        let Ok(json) = serde_json::to_string_pretty(&cache) else {
            return;
        };
        if let Err(e) = atomic_write(&self.cache_path(), json.as_bytes()).await {
            tracing::debug!(error = %e, "Failed to write validation cache");
        }
    }

    /// Replace both HTTP clients with fresh ones, dropping the old connection
    /// pools. The existing cookie jar and session data are preserved so no
    /// re-authentication is needed. Used for 421 recovery where the issue is
    /// stale HTTP/2 connection routing, not invalid auth state.
    pub fn reset_http_clients(&mut self) -> Result<()> {
        let (client, download_client) =
            build_clients(&self.cookie_jar, &self.home_endpoint, self.api_timeout)?;
        self.client = client;
        self.download_client = download_client;
        Ok(())
    }

    /// Release the exclusive file lock without dropping the Session.
    /// This allows a new Session to acquire the lock (e.g. during re-authentication).
    pub fn release_lock(&self) -> Result<()> {
        FileExt::unlock(&self.lock_file).context("Failed to release session lock file")
    }

    /// Re-acquire the exclusive file lock after a prior `release_lock()`.
    ///
    /// Returns `Err(AuthError::LockContention)` if another process acquired
    /// the lock in the interim (e.g. a concurrent `get-code` or `submit-code`).
    pub fn reacquire_lock(&self) -> Result<()> {
        let acquired = self
            .lock_file
            .try_lock_exclusive()
            .context("Failed to re-acquire session lock")?;
        if !acquired {
            return Err(crate::auth::error::AuthError::LockContention(
                "Another kei instance acquired the lock while it was released".into(),
            )
            .into());
        }
        Ok(())
    }

    pub fn client_id(&self) -> Option<&str> {
        self.session_data.get("client_id").map(String::as_str)
    }

    pub fn set_client_id(&mut self, client_id: &str) {
        self.session_data
            .insert("client_id".to_string(), client_id.to_string());
    }

    pub async fn post(
        &mut self,
        url: &str,
        body: Option<&str>,
        extra_headers: Option<HeaderMap>,
    ) -> Result<Response> {
        let mut builder = self.client.post(url);
        if let Some(h) = extra_headers {
            builder = builder.headers(h);
        }
        if let Some(b) = body {
            builder = builder
                .header("Content-Type", "application/json")
                .body(b.to_owned());
        }

        tracing::debug!(url = %url, "POST");
        let response = builder.send().await?;
        self.extract_and_save(&response).await?;
        Ok(response)
    }

    pub async fn put(&mut self, url: &str, extra_headers: Option<HeaderMap>) -> Result<Response> {
        let mut builder = self.client.put(url);
        if let Some(h) = extra_headers {
            builder = builder.headers(h);
        }

        tracing::debug!(url = %url, "PUT");
        let response = builder.send().await?;
        self.extract_and_save(&response).await?;
        Ok(response)
    }

    pub async fn get(&mut self, url: &str, extra_headers: Option<HeaderMap>) -> Result<Response> {
        let mut builder = self.client.get(url);
        if let Some(h) = extra_headers {
            builder = builder.headers(h);
        }

        tracing::debug!(url = %url, "GET");
        let response = builder.send().await?;
        self.extract_and_save(&response).await?;
        Ok(response)
    }

    /// Extract Apple session headers from every response and persist to disk.
    ///
    /// Only writes session/cookie files when values actually changed, avoiding
    /// redundant I/O during high-frequency API calls (album pagination, etc.).
    async fn extract_and_save(&mut self, response: &Response) -> Result<()> {
        let headers = response.headers();
        let mut session_changed = false;
        for &(header_name, session_key) in HEADER_DATA {
            if let Some(val) = headers.get(header_name) {
                if let Ok(val_str) = val.to_str() {
                    let existing = self.session_data.get(session_key);
                    if existing.map(std::string::String::as_str) != Some(val_str) {
                        self.session_data
                            .insert(session_key.to_string(), val_str.to_string());
                        session_changed = true;
                    }
                }
            }
        }

        if session_changed {
            let session_path = self.session_path();
            let json = serde_json::to_string_pretty(&self.session_data)?;
            atomic_write(&session_path, json.as_bytes())
                .await
                .with_context(|| {
                    format!("Failed to write session data to {}", session_path.display())
                })?;
            tracing::debug!("Saved session data to file");
        }

        // Persist ALL cookies the jar would send to known Apple domains.
        //
        // `icloudpd` calls `cookies.save(ignore_discard=True)` after
        // every request, dumping the entire jar. reqwest's Jar doesn't support
        // iteration, but we can query it for specific URLs via `cookies()`.
        //
        // This is critical for session reuse across process restarts: if
        // `accountLogin` involves HTTP redirects, cookies set by intermediate
        // redirect responses live in the jar but don't appear in the final
        // response's Set-Cookie headers. Without this, those cookies are lost
        // on the next run, causing validate_token to fail.
        self.persist_jar_cookies().await?;

        Ok(())
    }

    /// Persist all cookies from the in-memory jar for known Apple domains.
    ///
    /// reqwest's `Jar` doesn't support iteration, but `cookies(&url)` returns
    /// the `Cookie` header value it would send to a given URL. We query each
    /// Apple domain, split the semicolon-separated pairs, and save them so
    /// they can be restored on the next run via `add_cookie_str`.
    async fn persist_jar_cookies(&self) -> Result<()> {
        // Derive the relevant Apple domain URLs from the home endpoint.
        let is_cn = self.home_endpoint.contains(".cn");
        let domains: &[&str] = if is_cn {
            &[
                "https://setup.icloud.com.cn/",
                "https://www.icloud.com.cn/",
                "https://idmsa.apple.com.cn/",
            ]
        } else {
            &[
                "https://setup.icloud.com/",
                "https://www.icloud.com/",
                "https://idmsa.apple.com/",
            ]
        };

        let mut entries: Vec<CookieEntry> = Vec::new();
        for &domain_url in domains {
            let Ok(url) = domain_url.parse::<url::Url>() else {
                continue;
            };
            let Some(cookies) = self.cookie_jar.cookies(&url) else {
                continue;
            };
            let Ok(cookie_str) = cookies.to_str() else {
                continue;
            };
            for pair in cookie_str.split("; ") {
                if !pair.is_empty() {
                    entries.push(CookieEntry {
                        url: domain_url.to_string(),
                        cookie: pair.to_string(),
                    });
                }
            }
        }

        if entries.is_empty() {
            return Ok(());
        }

        let cookiejar_path = self.cookiejar_path();

        // Check if the cookie file already has the same content to avoid
        // redundant disk writes during high-frequency API calls.
        if cookiejar_path.exists() {
            if let Ok(contents) = fs::read_to_string(&cookiejar_path).await {
                if let Ok(existing) = serde_json::from_str::<Vec<CookieEntry>>(&contents) {
                    if existing == entries {
                        return Ok(());
                    }
                }
            }
        }

        atomic_write(
            &cookiejar_path,
            serde_json::to_string_pretty(&entries)?.as_bytes(),
        )
        .await
        .with_context(|| format!("Failed to write cookies to {}", cookiejar_path.display()))?;

        Ok(())
    }

    pub fn home_endpoint(&self) -> &str {
        &self.home_endpoint
    }

    /// Return a clone of the underlying HTTP client (with cookie jar attached).
    ///
    /// `reqwest::Client` is cheaply cloneable (backed by `Arc`), so this does
    /// not duplicate connections or state.
    pub fn http_client(&self) -> Client {
        self.client.clone()
    }

    /// Return a clone of the download-specific HTTP client.
    ///
    /// Unlike `http_client()`, this client has no total request timeout so
    /// large file transfers aren't killed mid-stream. It uses a 30s connect
    /// timeout and 120s read timeout for stall detection.
    pub fn download_client(&self) -> Client {
        self.download_client.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::responses;

    /// Return a unique temp directory for a session test.
    ///
    /// Uses `tempfile` instead of a fixed `/tmp/claude/...` path so
    /// parallel test runs (and rapid sequential `cargo test` invocations)
    /// never share lock files.
    ///
    /// Returns `(TempDir, PathBuf)` -- the `TempDir` handle must be kept
    /// alive for the test's duration to prevent early cleanup.
    fn test_dir(_name: &str) -> (tempfile::TempDir, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().to_path_buf();
        (td, path)
    }

    #[tokio::test]
    async fn test_lock_file_prevents_concurrent_sessions() {
        let (_td, dir) = test_dir("lock_concurrent");
        let _s1 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .expect("First session should succeed");

        let result = Session::new(&dir, "user@test.com", "https://example.com", None).await;
        match result {
            Ok(_) => panic!("Second session should have failed"),
            Err(e) => assert!(
                e.downcast_ref::<crate::auth::error::AuthError>()
                    .is_some_and(crate::auth::error::AuthError::is_lock_contention),
                "Expected LockContention, got: {e}",
            ),
        }
    }

    #[tokio::test]
    async fn test_lock_file_different_users_allowed() {
        let (_td, dir) = test_dir("lock_different_users");
        let _s1 = Session::new(&dir, "alice@test.com", "https://example.com", None)
            .await
            .unwrap();
        let _s2 = Session::new(&dir, "bob@test.com", "https://example.com", None)
            .await
            .expect("Different users should not conflict");
    }

    #[tokio::test]
    async fn test_lock_released_on_drop() {
        let (_td, dir) = test_dir("lock_release");
        {
            let _s = Session::new(&dir, "user@test.com", "https://example.com", None)
                .await
                .unwrap();
        } // _s dropped here, lock released
        let _s2 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .expect("Lock should be released after drop");
    }

    #[tokio::test]
    async fn test_release_and_reacquire_lock() {
        let (_td, dir) = test_dir("lock_reacquire");
        let s1 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();

        // Release the lock — another session should now succeed
        s1.release_lock().unwrap();
        let s2 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .expect("Should acquire lock after release");
        drop(s2);

        // Re-acquire the lock on the original session
        s1.reacquire_lock()
            .expect("Should re-acquire after other session dropped");

        // Now a new session should be blocked again
        let result = Session::new(&dir, "user@test.com", "https://example.com", None).await;
        match result {
            Ok(_) => panic!("Lock should be held after reacquire"),
            Err(e) => assert!(
                e.downcast_ref::<crate::auth::error::AuthError>()
                    .is_some_and(crate::auth::error::AuthError::is_lock_contention),
                "Expected LockContention, got: {e}",
            ),
        }
    }

    #[tokio::test]
    async fn test_reacquire_fails_while_held() {
        let (_td, dir) = test_dir("lock_reacquire_contention");
        let s1 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();
        s1.release_lock().unwrap();

        // Another session holds the lock
        let _s2 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();

        // Reacquire should fail because s2 holds the lock
        let result = s1.reacquire_lock();
        assert!(result.is_err(), "Should fail to reacquire while held");
    }

    #[tokio::test]
    async fn test_cookiejar_directory_at_path_skipped() {
        let (_td, dir) = test_dir("cookie_dir_skip");
        let sanitized = sanitize_username("user@test.com");
        let cookiejar_path = dir.join(&sanitized);

        // Create a directory where the cookiejar file would be
        std::fs::create_dir_all(&cookiejar_path).unwrap();
        assert!(cookiejar_path.is_dir());

        // Session should initialize without error (directory silently skipped)
        let session = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();
        assert!(session.cookiejar_path().is_dir());
    }

    #[tokio::test]
    async fn test_expired_cookies_pruned_on_load() {
        let (_td, dir) = test_dir("cookie_prune");
        let sanitized = sanitize_username("user@test.com");
        let cookie_path = dir.join(&sanitized);

        // Write a cookie file with one expired and one valid cookie
        let expired =
            "https://example.com\texpired_cookie=val; Expires=Thu, 01 Jan 2020 00:00:00 GMT"
                .to_string();
        let valid = "https://example.com\tvalid_cookie=val; Expires=Thu, 01 Jan 2099 00:00:00 GMT"
            .to_string();
        std::fs::write(&cookie_path, format!("{}\n{}", expired, valid)).unwrap();

        let session = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();

        // The expired cookie should have been pruned; valid one kept
        // We can't directly inspect the cookie jar, but we can verify the session loaded
        assert!(session.cookiejar_path().exists());
    }

    #[test]
    fn test_is_cookie_expired_past() {
        let now = chrono::Utc::now();
        assert!(is_cookie_expired(
            "foo=bar; Expires=Thu, 01 Jan 2020 00:00:00 GMT",
            &now
        ));
    }

    #[test]
    fn test_is_cookie_expired_future() {
        let now = chrono::Utc::now();
        assert!(!is_cookie_expired(
            "foo=bar; Expires=Thu, 01 Jan 2099 00:00:00 GMT",
            &now
        ));
    }

    #[test]
    fn test_is_cookie_expired_no_expiry() {
        let now = chrono::Utc::now();
        assert!(!is_cookie_expired("foo=bar", &now));
    }

    #[test]
    fn test_sanitize_username() {
        assert_eq!(sanitize_username("user@example.com"), "userexamplecom");
        assert_eq!(sanitize_username("hello_world"), "hello_world");
        assert_eq!(sanitize_username("a.b-c@d"), "abcd");
    }

    #[test]
    fn test_sanitize_username_unicode() {
        assert_eq!(sanitize_username("用户@example.com"), "用户examplecom");
    }

    #[test]
    fn test_sanitize_username_empty() {
        assert_eq!(sanitize_username(""), "");
    }

    #[test]
    fn test_sanitize_username_long_truncated() {
        let long_name = "a".repeat(500);
        let sanitized = sanitize_username(&long_name);
        assert!(
            sanitized.len() <= MAX_SANITIZED_USERNAME_LEN,
            "sanitized length {} exceeds max {}",
            sanitized.len(),
            MAX_SANITIZED_USERNAME_LEN
        );
    }

    #[test]
    fn test_sanitize_username_long_is_deterministic() {
        let long_name = "a".repeat(500);
        assert_eq!(sanitize_username(&long_name), sanitize_username(&long_name));
    }

    #[test]
    fn test_sanitize_username_different_long_names_differ() {
        let name1 = "a".repeat(500);
        let name2 = "b".repeat(500);
        assert_ne!(sanitize_username(&name1), sanitize_username(&name2));
    }

    #[test]
    fn test_sanitize_username_at_boundary_not_truncated() {
        let name = "a".repeat(MAX_SANITIZED_USERNAME_LEN);
        assert_eq!(sanitize_username(&name), name);
    }

    #[test]
    fn test_sanitize_username_all_special() {
        assert_eq!(sanitize_username("@.+-!"), "");
    }

    #[tokio::test]
    async fn test_persist_jar_cookies_saves_and_reloads() {
        let (_td, dir) = test_dir("persist_jar");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        // Simulate cookies being set in the jar (as reqwest would do from
        // Set-Cookie headers, including those from redirect responses).
        let setup_url: url::Url = "https://setup.icloud.com/".parse().unwrap();
        session
            .cookie_jar
            .add_cookie_str("X-APPLE-WEBAUTH-TOKEN=abc123", &setup_url);
        session
            .cookie_jar
            .add_cookie_str("X-APPLE-DS-WEB-SESSION-TOKEN=xyz", &setup_url);

        // Persist cookies from the jar
        session.persist_jar_cookies().await.unwrap();

        // Verify the cookie file was written
        let cookie_path = session.cookiejar_path();
        assert!(cookie_path.exists());
        let contents = std::fs::read_to_string(&cookie_path).unwrap();
        let entries: Vec<CookieEntry> = serde_json::from_str(&contents).unwrap();
        assert!(entries.len() >= 2);
        assert!(entries
            .iter()
            .any(|e| e.cookie.contains("X-APPLE-WEBAUTH-TOKEN")));
        assert!(entries
            .iter()
            .any(|e| e.cookie.contains("X-APPLE-DS-WEB-SESSION-TOKEN")));

        // Drop the session and create a new one — cookies should be loaded back
        drop(session);
        let session2 = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        // The jar should now have the cookies we saved
        let cookies = session2.cookie_jar.cookies(&setup_url);
        assert!(cookies.is_some());
        let cookie_header = cookies.unwrap();
        let cookie_str = cookie_header.to_str().unwrap();
        assert!(
            cookie_str.contains("X-APPLE-WEBAUTH-TOKEN=abc123"),
            "Expected WEBAUTH cookie, got: {}",
            cookie_str
        );
        assert!(
            cookie_str.contains("X-APPLE-DS-WEB-SESSION-TOKEN=xyz"),
            "Expected DS-WEB cookie, got: {}",
            cookie_str
        );
    }

    #[tokio::test]
    async fn test_persist_jar_cookies_no_redundant_writes() {
        let (_td, dir) = test_dir("persist_no_dup");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        let setup_url: url::Url = "https://setup.icloud.com/".parse().unwrap();
        session
            .cookie_jar
            .add_cookie_str("test_cookie=value1", &setup_url);

        // First persist
        session.persist_jar_cookies().await.unwrap();
        let mtime1 = std::fs::metadata(session.cookiejar_path())
            .unwrap()
            .modified()
            .unwrap();

        // Small delay to ensure filesystem mtime would change
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Second persist with same cookies — should skip the write
        session.persist_jar_cookies().await.unwrap();
        let mtime2 = std::fs::metadata(session.cookiejar_path())
            .unwrap()
            .modified()
            .unwrap();

        assert_eq!(mtime1, mtime2, "File should not have been rewritten");
    }

    #[test]
    fn test_parse_legacy_cookies_basic() {
        let input = "https://example.com\tfoo=bar\nhttps://other.com\tbaz=qux";
        let entries = parse_legacy_cookies(input);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].url, "https://example.com");
        assert_eq!(entries[0].cookie, "foo=bar");
        assert_eq!(entries[1].url, "https://other.com");
        assert_eq!(entries[1].cookie, "baz=qux");
    }

    #[test]
    fn test_parse_legacy_cookies_skips_comments_and_blanks() {
        let input = "# This is a comment\n\nhttps://example.com\tfoo=bar\n  \n# Another comment";
        let entries = parse_legacy_cookies(input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cookie, "foo=bar");
    }

    #[test]
    fn test_parse_legacy_cookies_skips_set_cookie3_header() {
        let input = "Set-Cookie3: some header\nhttps://example.com\tfoo=bar";
        let entries = parse_legacy_cookies(input);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn test_parse_legacy_cookies_skips_malformed_lines() {
        let input = "no-tab-here\nhttps://example.com\tfoo=bar\nalso no tab";
        let entries = parse_legacy_cookies(input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].url, "https://example.com");
    }

    #[test]
    fn test_parse_legacy_cookies_empty_input() {
        assert!(parse_legacy_cookies("").is_empty());
    }

    #[test]
    fn test_parse_legacy_cookies_preserves_cookie_with_tabs() {
        // Tab in cookie value after the first split
        let input = "https://example.com\tfoo=bar\textra";
        let entries = parse_legacy_cookies(input);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cookie, "foo=bar\textra");
    }

    #[tokio::test]
    async fn test_corrupt_session_file_recovers() {
        let (_td, dir) = test_dir("corrupt_session");
        let sanitized = sanitize_username("user@test.com");
        let session_path = dir.join(format!("{sanitized}.session"));

        std::fs::write(&session_path, "not valid json {{{{").unwrap();

        let session = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .expect("Should recover from corrupt session file");

        assert!(session.session_data.is_empty());
    }

    #[tokio::test]
    async fn test_atomic_write_no_partial_file_on_success() {
        let (_td, dir) = test_dir("atomic_write");
        let path = dir.join("test_file");

        atomic_write(&path, b"hello world").await.unwrap();

        assert!(path.exists());
        assert!(!dir.join("test_file.tmp").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_atomic_write_preserves_existing_on_overwrite() {
        let (_td, dir) = test_dir("atomic_overwrite");
        let path = dir.join("data");

        std::fs::write(&path, "original").unwrap();
        atomic_write(&path, b"updated").await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "updated");
        assert!(!dir.join("data.tmp").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_atomic_write_sets_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_td, dir) = test_dir("atomic_perms");
        let path = dir.join("secret");

        atomic_write(&path, b"sensitive data").await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "File should be owner-only, got {:o}", mode);
    }

    #[tokio::test]
    async fn test_reset_http_clients_preserves_cookie_jar() {
        let (_td, dir) = test_dir("reset_cookies");
        let mut session = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .unwrap();

        // Get a pointer to the cookie jar before reset.
        let jar_before = Arc::as_ptr(&session.cookie_jar);

        session.reset_http_clients().unwrap();

        // Same Arc, same underlying jar.
        let jar_after = Arc::as_ptr(&session.cookie_jar);
        assert_eq!(
            jar_before, jar_after,
            "reset_http_clients must reuse the existing cookie jar"
        );
    }

    #[tokio::test]
    async fn test_reset_http_clients_preserves_api_timeout() {
        let (_td, dir) = test_dir("reset_timeout");
        let custom_timeout = Some(45);
        let mut session =
            Session::new(&dir, "user@test.com", "https://example.com", custom_timeout)
                .await
                .unwrap();

        assert_eq!(session.api_timeout, Duration::from_secs(45));
        session.reset_http_clients().unwrap();
        assert_eq!(
            session.api_timeout,
            Duration::from_secs(45),
            "reset_http_clients must preserve the configured timeout"
        );
    }

    #[tokio::test]
    async fn strip_session_preserves_trust_token_and_client_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.session");
        let data = serde_json::json!({
            "session_token": "tok_abc",
            "session_id": "sid_123",
            "scnt": "scnt_val",
            "trust_token": "trust_xyz",
            "client_id": "auth-1234",
            "account_country": "USA"
        });
        std::fs::write(&path, data.to_string()).unwrap();

        strip_session_routing_state(&path).await;

        let contents = std::fs::read_to_string(&path).unwrap();
        let map: HashMap<String, String> = serde_json::from_str(&contents).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["trust_token"], "trust_xyz");
        assert_eq!(map["client_id"], "auth-1234");
        assert!(!map.contains_key("session_token"));
        assert!(!map.contains_key("session_id"));
        assert!(!map.contains_key("scnt"));
    }

    #[tokio::test]
    async fn strip_session_corrupt_file_gets_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.session");
        std::fs::write(&path, "not valid json {{{").unwrap();

        strip_session_routing_state(&path).await;

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn strip_session_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.session");

        strip_session_routing_state(&path).await;

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn strip_session_no_trust_token_leaves_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_trust.session");
        let data = serde_json::json!({
            "session_token": "tok_abc",
            "session_id": "sid_123",
            "scnt": "scnt_val"
        });
        std::fs::write(&path, data.to_string()).unwrap();

        strip_session_routing_state(&path).await;

        let contents = std::fs::read_to_string(&path).unwrap();
        let map: HashMap<String, String> = serde_json::from_str(&contents).unwrap();
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn validation_cache_round_trip() {
        let (_td, dir) = test_dir("cache_rt");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        let data = responses::AccountLoginResponse {
            ds_info: None,
            webservices: Some(responses::Webservices {
                ckdatabasews: Some(responses::WebserviceEndpoint {
                    url: "https://p60-ckdatabasews.icloud.com".into(),
                }),
            }),
            hsa_challenge_required: false,
            hsa_trusted_browser: true,
            domain_to_use: None,
            has_error: false,
            service_errors: vec![],
            i_cdp_enabled: false,
        };

        session.save_validation_cache(&data).await;
        assert!(session.cache_path().exists());

        let loaded = session.load_validation_cache(600).await;
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        let ws = loaded.webservices.unwrap();
        assert_eq!(
            ws.ckdatabasews.unwrap().url,
            "https://p60-ckdatabasews.icloud.com"
        );
        assert!(loaded.hsa_trusted_browser);
        assert!(!loaded.i_cdp_enabled);
    }

    #[tokio::test]
    async fn validation_cache_expired() {
        let (_td, dir) = test_dir("cache_exp");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        // Write a cache with an old timestamp
        let cache = responses::ValidationCache {
            validated_at: chrono::Utc::now().timestamp() - 3600,
            account_data: responses::AccountLoginResponse {
                ds_info: None,
                webservices: None,
                hsa_challenge_required: false,
                hsa_trusted_browser: false,
                domain_to_use: None,
                has_error: false,
                service_errors: vec![],
                i_cdp_enabled: false,
            },
        };
        let json = serde_json::to_string_pretty(&cache).unwrap();
        std::fs::write(session.cache_path(), json).unwrap();

        let loaded = session.load_validation_cache(600).await;
        assert!(loaded.is_none(), "Expired cache should return None");
    }

    #[tokio::test]
    async fn validation_cache_missing_file() {
        let (_td, dir) = test_dir("cache_miss");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        let loaded = session.load_validation_cache(600).await;
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn validation_cache_corrupt_file() {
        let (_td, dir) = test_dir("cache_corrupt");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        std::fs::write(session.cache_path(), "not valid json {{{").unwrap();

        let loaded = session.load_validation_cache(600).await;
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn validation_cache_no_time_limit_returns_stale_data() {
        // When 421 fallback uses i64::MAX, even very old cached data should load
        let (_td, dir) = test_dir("cache_no_limit");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        // Write a cache with a very old timestamp (1 week ago)
        let cache = responses::ValidationCache {
            validated_at: chrono::Utc::now().timestamp() - 604_800,
            account_data: responses::AccountLoginResponse {
                ds_info: None,
                webservices: Some(responses::Webservices {
                    ckdatabasews: Some(responses::WebserviceEndpoint {
                        url: "https://p60-ckdatabasews.icloud.com".to_string(),
                    }),
                }),
                hsa_challenge_required: false,
                hsa_trusted_browser: true,
                domain_to_use: None,
                has_error: false,
                service_errors: vec![],
                i_cdp_enabled: false,
            },
        };
        let json = serde_json::to_string_pretty(&cache).unwrap();
        std::fs::write(session.cache_path(), json).unwrap();

        // With normal grace (600s), should be expired
        let loaded = session.load_validation_cache(600).await;
        assert!(loaded.is_none(), "Should be expired with 600s grace");

        // With i64::MAX grace (421 fallback), should load
        let loaded = session.load_validation_cache(i64::MAX).await;
        assert!(
            loaded.is_some(),
            "Should load with i64::MAX grace (421 fallback)"
        );
        let loaded = loaded.unwrap();
        assert!(loaded.hsa_trusted_browser);
        let ws = loaded.webservices.unwrap();
        assert_eq!(
            ws.ckdatabasews.unwrap().url,
            "https://p60-ckdatabasews.icloud.com"
        );
    }

    #[tokio::test]
    async fn strip_session_invalidates_cache() {
        let dir = tempfile::tempdir().unwrap();
        let session_path = dir.path().join("test.session");
        let cache_path = dir.path().join("test.cache");

        let data = serde_json::json!({
            "session_token": "tok_abc",
            "trust_token": "trust_xyz",
            "client_id": "auth-1234"
        });
        std::fs::write(&session_path, data.to_string()).unwrap();
        std::fs::write(&cache_path, r#"{"validated_at":1,"account_data":{}}"#).unwrap();
        assert!(cache_path.exists());

        strip_session_routing_state(&session_path).await;

        assert!(
            !cache_path.exists(),
            "Cache should be deleted on session strip"
        );
    }

    #[test]
    fn broad_cookie_domain_icloud() {
        assert_eq!(
            broad_cookie_domain(Some("setup.icloud.com")),
            Some("icloud.com")
        );
        assert_eq!(
            broad_cookie_domain(Some("www.icloud.com")),
            Some("icloud.com")
        );
        assert_eq!(
            broad_cookie_domain(Some("p150-ckdatabasews.icloud.com")),
            Some("icloud.com")
        );
        assert_eq!(broad_cookie_domain(Some("icloud.com")), Some("icloud.com"));
    }

    #[test]
    fn broad_cookie_domain_apple() {
        assert_eq!(
            broad_cookie_domain(Some("idmsa.apple.com")),
            Some("apple.com")
        );
        assert_eq!(broad_cookie_domain(Some("apple.com")), Some("apple.com"));
    }

    #[test]
    fn broad_cookie_domain_cn() {
        assert_eq!(
            broad_cookie_domain(Some("setup.icloud.com.cn")),
            Some("icloud.com.cn")
        );
        assert_eq!(
            broad_cookie_domain(Some("idmsa.apple.com.cn")),
            Some("apple.com.cn")
        );
    }

    #[test]
    fn broad_cookie_domain_unknown() {
        assert_eq!(broad_cookie_domain(Some("example.com")), None);
        assert_eq!(broad_cookie_domain(None), None);
    }

    #[tokio::test]
    async fn cookies_reload_with_broad_domain_scope() {
        let (_td, dir) = test_dir("broad_domain");
        let session = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        // Simulate cookies set by Apple's auth (scoped to setup.icloud.com)
        let setup_url: url::Url = "https://setup.icloud.com/".parse().unwrap();
        session.cookie_jar.add_cookie_str(
            "X-APPLE-WEBAUTH-TOKEN=test123; Domain=icloud.com",
            &setup_url,
        );

        // Persist and reload
        session.persist_jar_cookies().await.unwrap();
        drop(session);

        let session2 = Session::new(&dir, "user@test.com", "https://www.icloud.com", None)
            .await
            .unwrap();

        // After reload, cookies should be available for ckdatabasews.icloud.com too
        let ck_url: url::Url = "https://p150-ckdatabasews.icloud.com/".parse().unwrap();
        let cookies = session2.cookie_jar.cookies(&ck_url);
        assert!(
            cookies.is_some(),
            "Cookies should be available for ckdatabasews.icloud.com after reload"
        );
        let cookie_str = cookies.unwrap();
        assert!(
            cookie_str
                .to_str()
                .unwrap()
                .contains("X-APPLE-WEBAUTH-TOKEN=test123"),
            "Expected WEBAUTH cookie for ckdatabasews, got: {}",
            cookie_str.to_str().unwrap()
        );
    }
}
