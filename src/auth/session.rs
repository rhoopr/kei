use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use reqwest::{Client, Response};
use serde_json::Value;
use tokio::fs;

/// Maps HTTP response headers to session data keys.
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

/// Sanitize a username by keeping only word characters (alphanumeric + underscore).
/// Equivalent to Python's `re.match(r"\w", c)` filter.
pub fn sanitize_username(username: &str) -> String {
    username
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// HTTP session wrapper that persists cookies and session data to disk.
#[allow(dead_code)]
pub struct Session {
    client: Client,
    pub(crate) _cookie_jar: Arc<reqwest::cookie::Jar>,
    pub session_data: HashMap<String, String>,
    cookie_dir: PathBuf,
    sanitized_username: String,
    home_endpoint: String,
    _timeout: Duration,
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

        // Ensure cookie directory exists
        fs::create_dir_all(&cookie_dir).await
            .with_context(|| format!("Failed to create cookie directory: {}", cookie_dir.display()))?;

        // Load cookie jar from file
        let cookie_jar = Arc::new(reqwest::cookie::Jar::default());

        let cookiejar_path = cookie_dir.join(&sanitized);
        if cookiejar_path.exists() {
            match fs::read_to_string(&cookiejar_path).await {
                Ok(contents) => {
                    // Parse LWP cookie jar format or simple cookie lines
                    for line in contents.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with('#') || trimmed.is_empty() || trimmed.starts_with("Set-Cookie3:") {
                            // Skip comments, empty lines, LWP header
                            continue;
                        }
                        // Try to parse as "url\tcookie_header" pairs we save
                        if let Some((url_str, cookie_str)) = trimmed.split_once('\t') {
                            if let Ok(url) = url_str.parse::<url::Url>() {
                                cookie_jar.add_cookie_str(cookie_str, &url);
                            }
                        }
                    }
                    tracing::debug!("Read cookies from {}", cookiejar_path.display());
                }
                Err(e) => {
                    tracing::warn!("Failed to read cookiejar {}: {}", cookiejar_path.display(), e);
                }
            }
        }

        // Build default headers
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            ORIGIN,
            HeaderValue::from_str(home_endpoint)?,
        );
        default_headers.insert(
            REFERER,
            HeaderValue::from_str(&format!("{}/", home_endpoint))?,
        );
        default_headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));

        let client = Client::builder()
            .cookie_provider(cookie_jar.clone())
            .default_headers(default_headers)
            .timeout(timeout)
            .build()?;

        // Load session data from file
        let session_path = cookie_dir.join(format!("{}.session", sanitized));
        let session_data = if session_path.exists() {
            match fs::read_to_string(&session_path).await {
                Ok(contents) => {
                    match serde_json::from_str::<HashMap<String, Value>>(&contents) {
                        Ok(map) => {
                            tracing::debug!("Loaded session data from {}", session_path.display());
                            map.into_iter()
                                .map(|(k, v)| {
                                    match v {
                                        Value::String(s) => (k, s),
                                        other => (k, other.to_string()),
                                    }
                                })
                                .collect()
                        }
                        Err(_) => {
                            tracing::info!("Session file corrupt, starting fresh");
                            HashMap::new()
                        }
                    }
                }
                Err(_) => {
                    tracing::info!("Session file does not exist");
                    HashMap::new()
                }
            }
        } else {
            tracing::info!("Session file does not exist");
            HashMap::new()
        };

        tracing::debug!("Using session file {}", session_path.display());

        Ok(Self {
            client,
            _cookie_jar: cookie_jar,
            session_data,
            cookie_dir,
            sanitized_username: sanitized,
            home_endpoint: home_endpoint.to_string(),
            _timeout: timeout,
        })
    }

    /// Path for cookie jar persistence.
    pub fn cookiejar_path(&self) -> PathBuf {
        self.cookie_dir.join(&self.sanitized_username)
    }

    /// Path for session data JSON file.
    pub fn session_path(&self) -> PathBuf {
        self.cookie_dir.join(format!("{}.session", self.sanitized_username))
    }

    /// Get the client_id from session data, or None.
    pub fn client_id(&self) -> Option<&String> {
        self.session_data.get("client_id")
    }

    /// Set client_id in session data.
    pub fn set_client_id(&mut self, client_id: &str) {
        self.session_data
            .insert("client_id".to_string(), client_id.to_string());
    }

    /// Send a POST request, extract headers, save session data and cookies.
    pub async fn post(
        &mut self,
        url: &str,
        body: Option<String>,
        extra_headers: Option<HeaderMap>,
    ) -> Result<Response> {
        let mut builder = self.client.post(url);
        if let Some(h) = extra_headers {
            builder = builder.headers(h);
        }
        if let Some(b) = body {
            builder = builder
                .header("Content-Type", "application/json")
                .body(b);
        }

        tracing::debug!("POST {}", url);
        let response = builder.send().await?;
        self.extract_and_save(&response).await?;
        Ok(response)
    }

    /// Send a GET request, extract headers, save session data and cookies.
    pub async fn get(
        &mut self,
        url: &str,
        extra_headers: Option<HeaderMap>,
    ) -> Result<Response> {
        let mut builder = self.client.get(url);
        if let Some(h) = extra_headers {
            builder = builder.headers(h);
        }

        tracing::debug!("GET {}", url);
        let response = builder.send().await?;
        self.extract_and_save(&response).await?;
        Ok(response)
    }

    /// Extract tracked headers from the response into session_data, then persist.
    async fn extract_and_save(&mut self, response: &Response) -> Result<()> {
        let headers = response.headers();
        for &(header_name, session_key) in HEADER_DATA {
            if let Some(val) = headers.get(header_name) {
                if let Ok(val_str) = val.to_str() {
                    self.session_data
                        .insert(session_key.to_string(), val_str.to_string());
                }
            }
        }

        // Save session data to JSON file
        let session_path = self.session_path();
        let json = serde_json::to_string_pretty(&self.session_data)?;
        fs::write(&session_path, json).await
            .with_context(|| format!("Failed to write session data to {}", session_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&session_path, perms)?;
        }
        tracing::debug!("Saved session data to file");

        // Save cookies — we store them as url\tcookie lines for simplicity
        // Note: reqwest::cookie::Jar doesn't expose iteration, so we capture
        // Set-Cookie headers from the response and append them to our file.
        let cookiejar_path = self.cookiejar_path();
        let url_str = response.url().to_string();
        let mut cookie_lines: Vec<String> = if cookiejar_path.exists() {
            fs::read_to_string(&cookiejar_path).await
                .with_context(|| format!("Failed to read cookie jar from {}", cookiejar_path.display()))?
                .lines()
                .map(|l| l.to_string())
                .collect()
        } else {
            Vec::new()
        };

        for cookie_header in headers.get_all("set-cookie") {
            if let Ok(val) = cookie_header.to_str() {
                // Extract cookie name for deduplication
                let new_name = val.split('=').next().unwrap_or("");
                // Remove old entries with same cookie name from same URL
                cookie_lines.retain(|line| {
                    if let Some((line_url, line_cookie)) = line.split_once('\t') {
                        if line_url == url_str {
                            let existing_name = line_cookie.split('=').next().unwrap_or("");
                            return existing_name != new_name;
                        }
                    }
                    true
                });
                cookie_lines.push(format!("{}\t{}", url_str, val));
            }
        }
        fs::write(&cookiejar_path, cookie_lines.join("\n")).await
            .with_context(|| format!("Failed to write cookies to {}", cookiejar_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&cookiejar_path, perms)?;
        }
        tracing::debug!("Cookies saved to {}", cookiejar_path.display());

        Ok(())
    }

    /// Get the home endpoint URL.
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
}
