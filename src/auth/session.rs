use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
///
/// Truncates to [`MAX_SANITIZED_USERNAME_LEN`] with a hash suffix if too long,
/// preventing OS "File name too long" errors.
#[expect(
    clippy::string_slice,
    reason = "indices from char_indices() are always valid char boundaries"
)]
pub fn sanitize_username(username: &str) -> String {
    let mut sanitized = String::with_capacity(username.len());
    sanitized.extend(
        username
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_'),
    );
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

pub(crate) fn persisted_auth_files(cookie_dir: &Path, username: &str) -> [PathBuf; 3] {
    let sanitized = sanitize_username(username);
    [
        cookie_dir.join(&sanitized),
        cookie_dir.join(format!("{sanitized}.session")),
        cookie_dir.join(format!("{sanitized}.cache")),
    ]
}

/// Per-account lock plus a generation that invalidates sessions released for watch sleep.
#[derive(Debug)]
pub(crate) struct SessionStateGuard {
    cookie_dir: PathBuf,
    sanitized_username: String,
    lock_file: std::fs::File,
    generation: AtomicU64,
    locked: AtomicBool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionGeneration(u64);

fn read_lock_generation(file: &std::fs::File) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let mut value = String::new();
    file.read_to_string(&mut value)?;
    let value = value.trim();
    if value.is_empty() {
        Ok(0)
    } else {
        value
            .parse()
            .context("Session lock file contains an invalid generation")
    }
}

fn write_lock_generation(file: &std::fs::File, generation: u64) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = file.try_clone()?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{generation}")?;
    file.sync_all()?;
    Ok(())
}

impl SessionStateGuard {
    pub(crate) async fn acquire(cookie_dir: &Path, username: &str) -> Result<Self> {
        Self::acquire_with_generation(cookie_dir, username, None).await
    }

    async fn acquire_with_generation(
        cookie_dir: &Path,
        username: &str,
        expected_generation: Option<SessionGeneration>,
    ) -> Result<Self> {
        let cookie_dir = cookie_dir.to_path_buf();
        fs::create_dir_all(&cookie_dir).await.with_context(|| {
            format!("Could not create cookie directory {}", cookie_dir.display())
        })?;
        let sanitized_username = sanitize_username(username);
        let lock_path = cookie_dir.join(format!("{sanitized_username}.lock"));
        let (lock_file, generation) = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
                .with_context(|| {
                    format!("Could not create session lock file {}", lock_path.display())
                })?;
            let acquired = file.try_lock_exclusive().with_context(|| {
                format!("Could not acquire session lock {}", lock_path.display())
            })?;
            if !acquired {
                return Err(crate::auth::error::AuthError::LockContention(format!(
                    "Another kei instance is running for this account (lock: {}). If running in Docker, check for orphaned containers with `docker ps` and stop them with `docker stop <name>`.",
                    lock_path.display()
                ))
                .into());
            }
            let generation = read_lock_generation(&file)?;
            Ok::<_, anyhow::Error>((file, generation))
        })
        .await??;
        if expected_generation.is_some_and(|expected| expected.0 != generation) {
            return Err(crate::auth::error::AuthError::SessionReset.into());
        }
        Ok(Self {
            cookie_dir,
            sanitized_username,
            lock_file,
            generation: AtomicU64::new(generation),
            locked: AtomicBool::new(true),
        })
    }

    pub(crate) fn files(&self) -> [PathBuf; 3] {
        persisted_auth_files(&self.cookie_dir, &self.sanitized_username)
    }

    fn has_replayable_auth(&self) -> bool {
        self.files()[..2].iter().any(|path| path.exists())
    }

    fn generation(&self) -> SessionGeneration {
        SessionGeneration(self.generation.load(Ordering::Acquire))
    }

    pub(crate) async fn discard(&self) -> Result<Vec<PathBuf>> {
        anyhow::ensure!(
            self.locked.load(Ordering::Acquire),
            "Cannot discard iCloud authentication state without the account lock"
        );
        let generation = self
            .generation
            .load(Ordering::Acquire)
            .checked_add(1)
            .context("Session lock generation overflow")?;
        let lock_file = self.lock_file.try_clone()?;
        tokio::task::spawn_blocking(move || write_lock_generation(&lock_file, generation))
            .await??;
        self.generation.store(generation, Ordering::Release);

        let mut removed = Vec::new();
        let mut first_error = None;
        for path in self.files() {
            match fs::remove_file(&path).await {
                Ok(()) => removed.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if first_error.is_none() => {
                    first_error = Some(
                        anyhow::Error::new(e)
                            .context(format!("Could not remove {}", path.display())),
                    );
                }
                Err(_) => {}
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(removed),
        }
    }

    fn release(&self) -> Result<()> {
        if !self.locked.load(Ordering::Acquire) {
            return Ok(());
        }
        FileExt::unlock(&self.lock_file).context("Could not release the session lock file")?;
        self.locked.store(false, Ordering::Release);
        Ok(())
    }

    fn reacquire(&self) -> Result<()> {
        if self.locked.load(Ordering::Acquire) {
            return Ok(());
        }
        let acquired = self
            .lock_file
            .try_lock_exclusive()
            .context("Could not reacquire the session lock")?;
        if !acquired {
            return Err(crate::auth::error::AuthError::LockContention(
                "Another kei instance acquired the session lock while it was released.".into(),
            )
            .into());
        }
        let generation = match read_lock_generation(&self.lock_file) {
            Ok(generation) => generation,
            Err(error) => {
                let _ = FileExt::unlock(&self.lock_file);
                return Err(error);
            }
        };
        if generation != self.generation.load(Ordering::Acquire) {
            FileExt::unlock(&self.lock_file)?;
            return Err(crate::auth::error::AuthError::SessionReset.into());
        }
        self.locked.store(true, Ordering::Release);
        Ok(())
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
    if let Ok(parsed) = cookie::Cookie::parse(cookie_str)
        && let Some(expires) = parsed.expires_datetime()
    {
        let expires_utc =
            chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::from(expires));
        return expires_utc < *now;
    }
    false
}

/// A single persisted cookie entry (URL + Set-Cookie header value).
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
struct CookieEntry {
    url: String,
    cookie: String,
}

/// Atomically write `data` to `path` via a temp file + rename.
///
/// Sets 0o600 permissions on Unix before renaming, so the file is never
/// world-readable even momentarily. The temp file is fsynced before the
/// rename and the parent directory is fsynced afterwards (Unix only) so
/// a power loss between the rename returning and the kernel committing
/// data + directory blocks can't leave `path` pointing at uninitialised
/// content or vanish on the next mount.
async fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)
            .await
            .with_context(|| format!("Could not open temporary file {}", tmp.display()))?;
        f.write_all(data)
            .await
            .with_context(|| format!("Could not write temporary file {}", tmp.display()))?;
        f.sync_all()
            .await
            .with_context(|| format!("Could not fsync temporary file {}", tmp.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).await?;
    }
    fs::rename(&tmp, path)
        .await
        .with_context(|| format!("Could not rename {} to {}", tmp.display(), path.display()))?;
    crate::fs_util::fsync_parent_dir_async_best_effort(path).await;
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
async fn strip_session_routing_state_file(session_file: &Path) {
    let contents = match fs::read_to_string(session_file).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::warn!(
                path = %session_file.display(),
                error = %e,
                "Permission denied reading session file; skipping strip (check PUID/PGID if running in Docker)"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                path = %session_file.display(),
                error = %e,
                "Could not read session file, removing"
            );
            crate::fs_util::log_remove_async(session_file).await;
            return;
        }
    };

    let mut map: HashMap<String, String> = match serde_json::from_str(&contents) {
        Ok(m) => m,
        Err(_) => {
            crate::fs_util::log_remove_async(session_file).await;
            return;
        }
    };

    map.retain(|k, _| k == "trust_token" || k == "client_id");

    match serde_json::to_string_pretty(&map) {
        Ok(json) => {
            if let Err(e) = atomic_write(session_file, json.as_bytes()).await {
                tracing::warn!(error = %e, "Could not rewrite session file, removing");
                crate::fs_util::log_remove_async(session_file).await;
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "Could not serialise stripped session, removing");
            crate::fs_util::log_remove_async(session_file).await;
        }
    }

    // Invalidate validation cache since session is being reset
    let cache_file = session_file.with_extension("cache");
    crate::fs_util::log_remove_async(&cache_file).await;
}

pub(crate) async fn strip_session_routing_state(
    cookie_dir: &Path,
    username: &str,
    expected_generation: Option<SessionGeneration>,
) -> Result<()> {
    let guard =
        SessionStateGuard::acquire_with_generation(cookie_dir, username, expected_generation)
            .await?;
    let session_file = guard.files()[1].clone();
    strip_session_routing_state_file(&session_file).await;
    Ok(())
}

/// Common cookie jar and browser headers for all session clients.
fn client_builder(
    cookie_jar: &Arc<reqwest::cookie::Jar>,
    home_endpoint: &str,
) -> Result<reqwest::ClientBuilder> {
    let mut default_headers = HeaderMap::new();
    default_headers.insert(ORIGIN, HeaderValue::from_str(home_endpoint)?);
    default_headers.insert(
        REFERER,
        HeaderValue::from_str(&format!("{home_endpoint}/"))?,
    );
    default_headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));

    Ok(Client::builder()
        .cookie_provider(cookie_jar.clone())
        .default_headers(default_headers))
}

/// The API client has a total timeout; downloads use connect + read timeouts
/// without a total cap so large transfers aren't killed mid-stream.
fn build_clients(
    cookie_jar: &Arc<reqwest::cookie::Jar>,
    home_endpoint: &str,
    api_timeout: Duration,
) -> Result<(Client, Client)> {
    let client = client_builder(cookie_jar, home_endpoint)?
        .timeout(api_timeout)
        .build()?;

    let download_client = client_builder(cookie_jar, home_endpoint)?
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
    photos_capture_client: Option<Client>,
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
    state_guard: SessionStateGuard,
    loaded_persisted_auth: bool,
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
        Self::new_with_generation(cookie_dir, username, home_endpoint, timeout_secs, None).await
    }

    pub(crate) async fn new_after_release(
        cookie_dir: &Path,
        username: &str,
        home_endpoint: &str,
        timeout_secs: Option<u64>,
        generation: SessionGeneration,
    ) -> Result<Self> {
        Self::new_with_generation(
            cookie_dir,
            username,
            home_endpoint,
            timeout_secs,
            Some(generation),
        )
        .await
    }

    async fn new_with_generation(
        cookie_dir: &Path,
        username: &str,
        home_endpoint: &str,
        timeout_secs: Option<u64>,
        expected_generation: Option<SessionGeneration>,
    ) -> Result<Self> {
        let sanitized = sanitize_username(username);
        let cookie_dir = cookie_dir.to_path_buf();

        let state_guard =
            SessionStateGuard::acquire_with_generation(&cookie_dir, username, expected_generation)
                .await?;
        let loaded_persisted_auth = state_guard.has_replayable_auth();

        Self::build(
            cookie_dir,
            &sanitized,
            home_endpoint,
            timeout_secs,
            state_guard,
            loaded_persisted_auth,
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
        state_guard: SessionStateGuard,
        loaded_persisted_auth: bool,
    ) -> Result<Self> {
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(30));
        let cookie_jar = Arc::new(reqwest::cookie::Jar::default());

        let cookiejar_path = cookie_dir.join(sanitized);
        if cookiejar_path.is_file() {
            match fs::read_to_string(&cookiejar_path).await {
                Ok(contents) => match serde_json::from_str::<Vec<CookieEntry>>(&contents) {
                    Ok(entries) => {
                        let now = chrono::Utc::now();
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
                                tracing::warn!(
                                    error = %e,
                                    "Could not set cookie file permissions"
                                );
                            }
                        }
                        tracing::debug!(path = %cookiejar_path.display(), "Read cookies");
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %cookiejar_path.display(),
                            error = %e,
                            "Cookiejar file is not valid JSON; removing cookies and validation cache"
                        );
                        crate::fs_util::log_remove_async(&cookiejar_path).await;
                        let cache_path = cookie_dir.join(format!("{sanitized}.cache"));
                        crate::fs_util::log_remove_async(&cache_path).await;
                    }
                },
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
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::warn!(
                        path = %session_path.display(),
                        error = %e,
                        "Permission denied reading session file (check PUID/PGID if running in Docker)"
                    );
                    HashMap::new()
                }
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
            photos_capture_client: None,
            cookie_jar,
            session_data,
            cookie_dir,
            sanitized_username: sanitized.to_owned(),
            home_endpoint: home_endpoint.to_string(),
            api_timeout: timeout,
            state_guard,
            loaded_persisted_auth,
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

    /// Replace HTTP clients with fresh ones, dropping the old connection
    /// pools. The existing cookie jar and session data are preserved so no
    /// re-authentication is needed. Used for 421 recovery where the issue is
    /// stale HTTP/2 connection routing, not invalid auth state.
    pub(crate) fn reset_http_clients(&mut self) -> Result<()> {
        let (client, download_client) =
            build_clients(&self.cookie_jar, &self.home_endpoint, self.api_timeout)?;
        self.client = client;
        self.download_client = download_client;
        self.photos_capture_client = None;
        Ok(())
    }

    /// Reuse the API settings and live cookie jar, but expose each redirect body.
    pub(crate) fn photos_capture_client(&mut self) -> Result<Client> {
        if let Some(client) = &self.photos_capture_client {
            return Ok(client.clone());
        }
        let client = client_builder(&self.cookie_jar, &self.home_endpoint)?
            .timeout(self.api_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        self.photos_capture_client = Some(client.clone());
        Ok(client)
    }

    /// Release the exclusive file lock without dropping the Session.
    /// This allows a new Session to acquire the lock (e.g. during re-authentication).
    pub(crate) fn release_lock(&self) -> Result<()> {
        self.state_guard.release()
    }

    /// Re-acquire the exclusive file lock after a prior `release_lock()`.
    ///
    /// Returns `Err(AuthError::LockContention)` if another process acquired
    /// the lock in the interim (e.g. a concurrent `get-code` or `submit-code`).
    pub(crate) fn reacquire_lock(&self) -> Result<()> {
        self.state_guard.reacquire()
    }

    pub(crate) fn loaded_persisted_auth(&self) -> bool {
        self.loaded_persisted_auth
    }

    pub(crate) fn generation(&self) -> SessionGeneration {
        self.state_guard.generation()
    }

    pub(crate) async fn discard_persisted_auth(&self) -> Result<Vec<PathBuf>> {
        self.state_guard.discard().await
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
            if let Some(val) = headers.get(header_name)
                && let Ok(val_str) = val.to_str()
            {
                let existing = self.session_data.get(session_key);
                if existing.map(std::string::String::as_str) != Some(val_str) {
                    self.session_data
                        .insert(session_key.to_string(), val_str.to_string());
                    session_changed = true;
                }
            }
        }

        if session_changed {
            let session_path = self.session_path();
            let json = serde_json::to_string_pretty(&self.session_data)?;
            atomic_write(&session_path, json.as_bytes())
                .await
                .with_context(|| {
                    format!("Could not write session data to {}", session_path.display())
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
        if cookiejar_path.exists()
            && let Ok(contents) = fs::read_to_string(&cookiejar_path).await
            && let Ok(existing) = serde_json::from_str::<Vec<CookieEntry>>(&contents)
            && existing == entries
        {
            return Ok(());
        }

        atomic_write(
            &cookiejar_path,
            serde_json::to_string_pretty(&entries)?.as_bytes(),
        )
        .await
        .with_context(|| format!("Could not write cookies to {}", cookiejar_path.display()))?;

        Ok(())
    }

    pub fn home_endpoint(&self) -> &str {
        &self.home_endpoint
    }

    /// Return a reference to the underlying HTTP client (with cookie jar
    /// attached).
    ///
    /// `reqwest::Client` is `Arc`-backed, so callers that need a handle
    /// to move into a spawned task should `.clone()` — making the refcount
    /// bump visible at the call site instead of hiding it behind the accessor.
    pub(crate) fn http_client(&self) -> &Client {
        &self.client
    }

    /// Return a reference to the download-specific HTTP client.
    ///
    /// Unlike `http_client()`, this client has no total request timeout so
    /// large file transfers aren't killed mid-stream. It uses a 30s connect
    /// timeout and 120s read timeout for stall detection.
    ///
    /// `reqwest::Client` is `Arc`-backed, so callers that need to move a
    /// handle into a spawned task should `.clone()` — making the refcount
    /// bump visible at the call site instead of hiding it behind the accessor.
    pub(crate) fn download_client(&self) -> &Client {
        &self.download_client
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{self, responses};

    /// Return a unique temp directory for a session test.
    ///
    /// Uses `tempfile` instead of a fixed `/tmp/codex/kei/...` path so
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
    async fn session_reset_invalidates_a_released_session() {
        let (_td, dir) = test_dir("reset_while_released");
        let username = "user@test.com";
        let files = persisted_auth_files(&dir, username);
        for path in &files {
            std::fs::write(path, b"stale").unwrap();
        }
        let session = Session::new(&dir, username, "https://example.com", None)
            .await
            .unwrap();
        session.release_lock().unwrap();

        let reset = SessionStateGuard::acquire(&dir, username).await.unwrap();
        reset.discard().await.unwrap();
        drop(reset);

        let err = session
            .reacquire_lock()
            .expect_err("released session must observe the reset");
        assert!(matches!(
            err.downcast_ref::<crate::auth::error::AuthError>(),
            Some(crate::auth::error::AuthError::SessionReset)
        ));
        let _replacement = Session::new(&dir, username, "https://example.com", None)
            .await
            .expect("reset generation should allow a fresh session");
    }

    #[tokio::test]
    async fn session_reset_invalidates_a_replacement_session() {
        let (_td, dir) = test_dir("reset_during_replacement");
        let username = "user@test.com";
        let session = Session::new(&dir, username, "https://example.com", None)
            .await
            .unwrap();
        let generation = session.generation();
        session.release_lock().unwrap();
        drop(session);

        let reset = SessionStateGuard::acquire(&dir, username).await.unwrap();
        reset.discard().await.unwrap();
        drop(reset);

        let err =
            Session::new_after_release(&dir, username, "https://example.com", None, generation)
                .await
                .expect_err("replacement must not adopt a reset generation");
        assert!(matches!(
            err.downcast_ref::<crate::auth::error::AuthError>(),
            Some(crate::auth::error::AuthError::SessionReset)
        ));
    }

    #[tokio::test]
    async fn failed_reset_still_invalidates_a_released_session() {
        let (_td, dir) = test_dir("failed_reset_while_released");
        let username = "user@test.com";
        let files = persisted_auth_files(&dir, username);
        std::fs::create_dir(&files[0]).unwrap();
        std::fs::write(&files[1], b"stale").unwrap();
        let session = Session::new(&dir, username, "https://example.com", None)
            .await
            .unwrap();
        session.release_lock().unwrap();

        let reset = SessionStateGuard::acquire(&dir, username).await.unwrap();
        reset
            .discard()
            .await
            .expect_err("directory cannot be removed as an auth file");
        assert!(
            !files[1].exists(),
            "later auth files must still be removed after an earlier failure"
        );
        drop(reset);

        let err = session
            .reacquire_lock()
            .expect_err("failed reset must still invalidate released sessions");
        assert!(matches!(
            err.downcast_ref::<crate::auth::error::AuthError>(),
            Some(crate::auth::error::AuthError::SessionReset)
        ));
    }

    #[tokio::test]
    async fn generation_overflow_preserves_auth_files() {
        let (_td, dir) = test_dir("generation_overflow");
        let username = "user@test.com";
        let files = persisted_auth_files(&dir, username);
        std::fs::write(&files[0], b"cookie").unwrap();
        let lock_path = dir.join(format!("{}.lock", sanitize_username(username)));
        std::fs::write(&lock_path, u64::MAX.to_string()).unwrap();
        let guard = SessionStateGuard::acquire(&dir, username).await.unwrap();

        guard.discard().await.expect_err("generation must not wrap");

        assert!(files[0].exists());
    }

    #[tokio::test]
    async fn session_reacquire_lock_contention_error_is_actionable() {
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
        let err = result.expect_err("Should fail to reacquire while held");
        assert!(
            err.downcast_ref::<crate::auth::error::AuthError>()
                .is_some_and(crate::auth::error::AuthError::is_lock_contention),
            "Expected LockContention, got: {err:#}",
        );
        assert!(
            err.to_string().contains("Another kei instance"),
            "Lock contention error should tell the user another process has the lock: {err:#}"
        );
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
        let username = "user@test.com";
        let cookie_path = auth::cookiejar_file_path(&dir, username);

        // Write a cookie file with one expired and one valid cookie
        let entries = vec![
            CookieEntry {
                url: "https://example.com".to_string(),
                cookie: "expired_cookie=val; Expires=Thu, 01 Jan 2020 00:00:00 GMT".to_string(),
            },
            CookieEntry {
                url: "https://example.com".to_string(),
                cookie: "valid_cookie=val; Expires=Thu, 01 Jan 2099 00:00:00 GMT".to_string(),
            },
        ];
        std::fs::write(
            &cookie_path,
            serde_json::to_string_pretty(&entries).unwrap(),
        )
        .unwrap();

        let session = Session::new(&dir, username, "https://example.com", None)
            .await
            .unwrap();

        // The expired cookie should have been pruned; valid one kept
        // We can't directly inspect the cookie jar, but we can verify the session loaded
        assert!(session.cookiejar_path().exists());
    }

    #[tokio::test]
    async fn corrupt_cookiejar_json_is_removed_on_load() {
        let (_td, dir) = test_dir("cookie_corrupt_json");
        let username = "user@test.com";
        let cookie_path = auth::cookiejar_file_path(&dir, username);
        let cache_path = auth::validation_cache_file_path(&dir, username);
        std::fs::write(&cookie_path, "not valid json {{{").unwrap();
        std::fs::write(&cache_path, r#"{"validated_at":1,"account_data":{}}"#).unwrap();

        let session = Session::new(&dir, username, "https://example.com", None)
            .await
            .unwrap();

        assert_eq!(session.cookiejar_path(), cookie_path);
        assert!(
            !session.cookiejar_path().exists(),
            "corrupt current-format cookie files should be removed so re-auth can rebuild them"
        );
        assert!(
            !cache_path.exists(),
            "corrupt cookie files should invalidate the validation cache"
        );
    }

    #[tokio::test]
    async fn legacy_tab_separated_cookiejar_is_not_loaded() {
        let (_td, dir) = test_dir("cookie_legacy_tab");
        let username = "user@test.com";
        let cookie_path = auth::cookiejar_file_path(&dir, username);
        std::fs::write(
            &cookie_path,
            "https://example.com\tlegacy_cookie=val; Expires=Thu, 01 Jan 2099 00:00:00 GMT",
        )
        .unwrap();

        let session = Session::new(&dir, username, "https://example.com", None)
            .await
            .unwrap();

        let url: url::Url = "https://example.com/".parse().unwrap();
        assert!(
            session.cookie_jar.cookies(&url).is_none(),
            "legacy tab-separated cookies must not be loaded"
        );
        assert!(
            !session.cookiejar_path().exists(),
            "legacy cookie files should follow the corrupt JSON recovery path"
        );
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

    #[test]
    fn persisted_auth_files_cover_only_session_artifacts() {
        let dir = Path::new("/data");
        let files = persisted_auth_files(dir, "user@test.com");
        let names: Vec<String> = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        let sanitized = sanitize_username("user@test.com");
        assert_eq!(
            names,
            vec![
                sanitized.clone(),
                format!("{sanitized}.session"),
                format!("{sanitized}.cache"),
            ]
        );
    }

    #[tokio::test]
    async fn validation_cache_alone_is_not_replayable_auth_state() {
        let dir = tempfile::tempdir().unwrap();
        let files = persisted_auth_files(dir.path(), "user@test.com");
        std::fs::write(&files[2], b"cache").unwrap();

        let guard = SessionStateGuard::acquire(dir.path(), "user@test.com")
            .await
            .unwrap();
        assert!(!guard.has_replayable_auth());
    }

    #[tokio::test]
    async fn discard_persisted_auth_removes_only_session_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let username = "user@test.com";
        let files = persisted_auth_files(dir.path(), username);
        for path in &files {
            std::fs::write(path, b"auth").unwrap();
        }
        let credential = dir
            .path()
            .join(format!("{}.credential", sanitize_username(username)));
        let db = dir
            .path()
            .join(format!("{}.db", sanitize_username(username)));
        std::fs::write(&credential, b"credential").unwrap();
        std::fs::write(&db, b"database").unwrap();

        let guard = SessionStateGuard::acquire(dir.path(), username)
            .await
            .unwrap();
        let removed = guard.discard().await.unwrap();

        assert_eq!(removed, files);
        assert!(files.iter().all(|path| !path.exists()));
        assert!(credential.exists());
        assert!(db.exists());
    }

    #[tokio::test]
    async fn discard_persisted_auth_refuses_lock_contention() {
        let dir = tempfile::tempdir().unwrap();
        let username = "user@test.com";
        let session = Session::new(dir.path(), username, "https://example.com", None)
            .await
            .unwrap();
        let files = persisted_auth_files(dir.path(), username);
        std::fs::write(&files[0], b"cookie").unwrap();

        let err = SessionStateGuard::acquire(dir.path(), username)
            .await
            .unwrap_err();

        assert!(
            err.downcast_ref::<crate::auth::error::AuthError>()
                .is_some_and(crate::auth::error::AuthError::is_lock_contention)
        );
        assert!(files[0].exists());
        drop(session);
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
        assert!(
            entries
                .iter()
                .any(|e| e.cookie.contains("X-APPLE-WEBAUTH-TOKEN"))
        );
        assert!(
            entries
                .iter()
                .any(|e| e.cookie.contains("X-APPLE-DS-WEB-SESSION-TOKEN"))
        );

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

        strip_session_routing_state_file(&path).await;

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

        strip_session_routing_state_file(&path).await;

        assert!(!path.exists());
    }

    #[tokio::test]
    async fn strip_session_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.session");

        strip_session_routing_state_file(&path).await;

        assert!(!path.exists());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn strip_session_permission_denied_keeps_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("locked.session");
        let data = serde_json::json!({ "session_token": "tok_abc", "trust_token": "trust_xyz" });
        std::fs::write(&path, data.to_string()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        strip_session_routing_state_file(&path).await;

        // restore so tempdir cleanup doesn't fail
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(path.exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, data.to_string());
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

        strip_session_routing_state_file(&path).await;

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

        strip_session_routing_state_file(&session_path).await;

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
