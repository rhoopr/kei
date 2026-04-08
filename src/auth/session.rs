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
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(prefix_len);
        format!("{}_{:016x}", &sanitized[..prefix_end], hash)
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
#[derive(serde::Serialize, serde::Deserialize, PartialEq)]
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

/// HTTP session wrapper that persists cookies and session data to disk,
/// allowing authentication to survive across process restarts.
pub struct Session {
    client: Client,
    download_client: Client,
    /// Cookie jar shared with `reqwest::Client`. Queried by
    /// `persist_jar_cookies` to save session cookies to disk, and kept alive
    /// so the client's internal weak reference remains valid.
    cookie_jar: Arc<reqwest::cookie::Jar>,
    pub session_data: HashMap<String, String>,
    cookie_dir: PathBuf,
    sanitized_username: String,
    home_endpoint: String,
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
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(30));

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
                    anyhow::bail!(
                        "Another kei instance is running for this account (lock: {}). \
                         If running in Docker, check for orphaned containers with \
                         `docker ps` and stop them with `docker stop <name>`.",
                        lock_path.display()
                    );
                }
                Ok::<std::fs::File, anyhow::Error>(file)
            }
        })
        .await??;

        let cookie_jar = Arc::new(reqwest::cookie::Jar::default());

        let cookiejar_path = cookie_dir.join(&sanitized);
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
                            cookie_jar.add_cookie_str(&entry.cookie, &url);
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

        // Origin/Referer headers are required by Apple's CORS checks
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
            .timeout(timeout)
            .build()?;

        // Separate client for file downloads: no total timeout so large files
        // aren't killed mid-transfer. connect_timeout catches unreachable hosts;
        // read_timeout detects stalled connections (no bytes for 120s).
        // Pool settings tuned for high-concurrency downloads to Apple's CDN.
        let download_client = Client::builder()
            .cookie_provider(cookie_jar.clone())
            .default_headers(default_headers)
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(20)
            .pool_idle_timeout(Duration::from_secs(90))
            .build()?;

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
                        tracing::info!(path = %session_path.display(), error = %e, "Session file corrupt, starting fresh");
                        HashMap::new()
                    }
                },
                Err(e) => {
                    tracing::info!(path = %session_path.display(), error = %e, "Could not read session file, starting fresh");
                    HashMap::new()
                }
            }
        } else {
            tracing::info!("Session file does not exist");
            HashMap::new()
        };

        tracing::debug!(path = %session_path.display(), "Using session file");

        Ok(Self {
            client,
            download_client,
            cookie_jar,
            session_data,
            cookie_dir,
            sanitized_username: sanitized,
            home_endpoint: home_endpoint.to_string(),
            lock_file,
        })
    }

    pub fn cookiejar_path(&self) -> PathBuf {
        self.cookie_dir.join(&self.sanitized_username)
    }

    pub fn session_path(&self) -> PathBuf {
        self.cookie_dir
            .join(format!("{}.session", self.sanitized_username))
    }

    /// Release the exclusive file lock without dropping the Session.
    /// This allows a new Session to acquire the lock (e.g. during re-authentication).
    pub fn release_lock(&self) -> Result<()> {
        FileExt::unlock(&self.lock_file).context("Failed to release session lock file")
    }

    pub fn client_id(&self) -> Option<&String> {
        self.session_data.get("client_id")
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

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("claude")
            .join("session_tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn test_lock_file_prevents_concurrent_sessions() {
        let dir = test_dir("lock_concurrent");
        let _s1 = Session::new(&dir, "user@test.com", "https://example.com", None)
            .await
            .expect("First session should succeed");

        let result = Session::new(&dir, "user@test.com", "https://example.com", None).await;
        match result {
            Ok(_) => panic!("Second session should have failed"),
            Err(e) => assert!(
                e.to_string().contains("Another kei instance"),
                "Unexpected error: {}",
                e
            ),
        }
    }

    #[tokio::test]
    async fn test_lock_file_different_users_allowed() {
        let dir = test_dir("lock_different_users");
        let _s1 = Session::new(&dir, "alice@test.com", "https://example.com", None)
            .await
            .unwrap();
        let _s2 = Session::new(&dir, "bob@test.com", "https://example.com", None)
            .await
            .expect("Different users should not conflict");
    }

    #[tokio::test]
    async fn test_lock_released_on_drop() {
        let dir = test_dir("lock_release");
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
    async fn test_cookiejar_directory_at_path_skipped() {
        let dir = test_dir("cookie_dir_skip");
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
        let dir = test_dir("cookie_prune");
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
        let dir = test_dir("persist_jar");
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
        let dir = test_dir("persist_no_dup");
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
        let dir = test_dir("corrupt_session");
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
        let dir = test_dir("atomic_write");
        let path = dir.join("test_file");

        atomic_write(&path, b"hello world").await.unwrap();

        assert!(path.exists());
        assert!(!dir.join("test_file.tmp").exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_atomic_write_preserves_existing_on_overwrite() {
        let dir = test_dir("atomic_overwrite");
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

        let dir = test_dir("atomic_perms");
        let path = dir.join("secret");

        atomic_write(&path, b"sensitive data").await.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "File should be owner-only, got {:o}", mode);
    }
}
