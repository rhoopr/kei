//! kei: photo sync engine — Rust rewrite of icloud-photos-downloader.
//!
//! Downloads photos and videos from iCloud via Apple's private `CloudKit` APIs.
//! Authentication uses SRP-6a with Apple's custom variant, followed by optional
//! 2FA. Photos are streamed with exponential-backoff retries on transient
//! failures.

#![warn(clippy::all)]

mod auth;
mod cli;
mod config;
mod credential;
mod download;
mod health;
mod icloud;
mod migration;
mod notifications;
mod password;
pub mod retry;
mod setup;
mod shutdown;
mod state;
mod systemd;
mod types;

#[cfg(test)]
mod test_helpers;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use password::{ExposeSecret, SecretString};
use tracing_subscriber::EnvFilter;

/// A writer wrapper that redacts a password string from log output.
///
/// Wraps any `io::Write` implementor and replaces occurrences of the
/// configured password with `********` in each `write()` call.
struct RedactingWriter<W> {
    inner: W,
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let password = self
            .password
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ref pw) = *password {
            let pw_str = pw.expose_secret();
            if !pw_str.is_empty() {
                // A buffer shorter than the password can't contain it,
                // avoiding the lossy UTF-8 conversion for short log fragments.
                if buf.len() >= pw_str.len() {
                    let s = String::from_utf8_lossy(buf);
                    if s.contains(pw_str) {
                        let redacted = s.replace(pw_str, "********");
                        self.inner.write_all(redacted.as_bytes())?;
                        return Ok(buf.len());
                    }
                }
            }
        }
        self.inner.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// A `MakeWriter` implementation that produces `RedactingWriter` instances.
struct RedactingMakeWriter {
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter<std::io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: std::io::stderr(),
            password: Arc::clone(&self.password),
        }
    }
}

use cli::Command;
use config::TomlConfig;
use notifications::Notifier;
use state::StateDb;
use systemd::SystemdNotifier;

/// Maximum number of re-authentication attempts before giving up.
const MAX_REAUTH_ATTEMPTS: u32 = 3;

/// Prevent core dumps from leaking in-memory credentials.
/// Best-effort: failures are logged but not fatal (Docker containers may
/// restrict these syscalls).
fn harden_process() {
    #[cfg(target_os = "linux")]
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            tracing::debug!("prctl(PR_SET_DUMPABLE, 0) failed");
        }
    }
    #[cfg(unix)]
    unsafe {
        let rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &rlim) != 0 {
            tracing::debug!("setrlimit(RLIMIT_CORE, 0) failed");
        }
    }
}

/// Exit code for partial sync (some downloads failed, but sync was not a total failure).
const EXIT_PARTIAL: u8 = 2;
/// Exit code for authentication failures.
const EXIT_AUTH: u8 = 3;

/// Returned when some (but not all) downloads failed during a sync.
#[derive(Debug)]
struct PartialSyncError(usize);
impl std::fmt::Display for PartialSyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} downloads failed", self.0)
    }
}
impl std::error::Error for PartialSyncError {}

/// Query available disk space on the filesystem containing `path`.
///
/// Returns `None` if the statvfs call fails (e.g. path doesn't exist yet).
#[cfg(unix)]
fn available_disk_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// Widen a platform-dependent statvfs field to u64. `as u64` is the only
    /// portable way since the underlying types (`c_ulong`, `fsblkcnt_t`) vary
    /// across targets.
    #[inline]
    fn widen(v: impl Into<u64>) -> u64 {
        v.into()
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        Some(widen(stat.f_bavail) * widen(stat.f_frsize))
    }
}

#[cfg(not(unix))]
fn available_disk_space(_path: &Path) -> Option<u64> {
    None
}

/// Build a password provider closure from a [`password::PasswordSource`].
///
/// The source is evaluated lazily on each call — for `Command` and `File`
/// sources, this re-executes/re-reads each time, supporting password rotation
/// and keeping no password in memory between auth cycles.
fn make_password_provider(source: password::PasswordSource) -> impl Fn() -> Option<SecretString> {
    move || match source.resolve() {
        Ok(pw) => pw,
        Err(e) => {
            tracing::error!(error = %e, "Password source resolution failed");
            None
        }
    }
}

/// Build a password provider from CLI auth args, TOML config, and resolved auth fields.
///
/// Shared by `run_get_code`, `run_submit_code`, and `run_import_existing`.
fn make_provider_from_auth(
    auth: &cli::AuthArgs,
    password: Option<String>,
    username: &str,
    cookie_directory: &Path,
    toml: Option<&config::TomlConfig>,
) -> impl Fn() -> Option<SecretString> {
    let toml_auth = toml.and_then(|t| t.auth.as_ref());
    let password_command = config::resolve_password_command(auth, toml_auth);
    let password_file = config::resolve_password_file(auth, toml_auth);
    let source = password::build_password_source(
        password.map(SecretString::from).as_ref(),
        password_command.as_deref(),
        password_file.as_deref(),
        credential::CredentialStore::new(username, cookie_directory),
    );
    make_password_provider(source)
}

/// Initialize the photos service with automatic 421 retry.
///
/// On first attempt, uses the ckdatabasews URL from the auth result. If the
/// CloudKit service returns 421 Misdirected Request (stale partition), retries
/// by calling accountLogin to refresh service URLs.
async fn init_photos_service(
    auth_result: auth::AuthResult,
    domain: &str,
    api_retry_config: retry::RetryConfig,
) -> anyhow::Result<(auth::SharedSession, icloud::photos::PhotosService)> {
    let ckdatabasews_url = auth_result
        .data
        .webservices
        .as_ref()
        .and_then(|ws| ws.ckdatabasews.as_ref())
        .map(|ep| ep.url.clone())
        .ok_or_else(|| anyhow::anyhow!("No ckdatabasews URL"))?;

    let client_id = auth_result.session.client_id().cloned().unwrap_or_default();
    let dsid = auth_result
        .data
        .ds_info
        .as_ref()
        .and_then(|ds| ds.dsid.clone());
    let params = build_photos_params(&client_id, dsid.as_deref());

    let shared_session: auth::SharedSession =
        std::sync::Arc::new(tokio::sync::RwLock::new(auth_result.session));
    let session_box: Box<dyn icloud::photos::PhotosSession> = Box::new(shared_session.clone());

    tracing::info!("Initializing photos service...");
    match icloud::photos::PhotosService::new(
        ckdatabasews_url.clone(),
        session_box,
        params.clone(),
        api_retry_config,
    )
    .await
    {
        Ok(service) => Ok((shared_session, service)),
        Err(e) if is_misdirected_request(&e) => {
            tracing::warn!(
                url = %ckdatabasews_url,
                "Service endpoint returned 421 Misdirected Request, \
                 refreshing service URLs via accountLogin"
            );
            let endpoints = auth::endpoints::Endpoints::for_domain(domain)?;
            let fresh_data = {
                let mut session = shared_session.write().await;
                auth::twofa::authenticate_with_token(&mut session, &endpoints).await?
            };

            let fresh_url = fresh_data
                .webservices
                .as_ref()
                .and_then(|ws| ws.ckdatabasews.as_ref())
                .map(|ep| ep.url.clone())
                .ok_or_else(|| anyhow::anyhow!("No ckdatabasews URL from accountLogin"))?;

            let client_id = {
                let session = shared_session.read().await;
                session.client_id().cloned().unwrap_or_default()
            };
            let dsid = fresh_data.ds_info.as_ref().and_then(|ds| ds.dsid.clone());
            let params = build_photos_params(&client_id, dsid.as_deref());

            let session_box: Box<dyn icloud::photos::PhotosSession> =
                Box::new(shared_session.clone());

            tracing::info!(url = %fresh_url, "Retrying with fresh service URL");
            let service = icloud::photos::PhotosService::new(
                fresh_url,
                session_box,
                params,
                api_retry_config,
            )
            .await?;

            Ok((shared_session, service))
        }
        Err(e) => Err(e.into()),
    }
}

/// Check if an iCloud error is a 421 Misdirected Request from the CloudKit service.
///
/// This happens when Apple migrates an account to a different partition but the
/// cached session still references the old ckdatabasews URL. The fix is to
/// force a full SRP re-authentication to obtain fresh webservice URLs.
fn is_misdirected_request(err: &icloud::error::ICloudError) -> bool {
    matches!(err, icloud::error::ICloudError::Connection(msg) if msg.contains("421"))
}

/// Attempt to re-authenticate the session.
///
/// First validates the existing session; if invalid, performs full re-authentication.
/// If 2FA is required in headless mode, returns `AuthError::TwoFactorRequired`
/// so the caller can fire a notification and skip the current cycle.
///
/// # Lock strategy
///
/// A write lock is held across the `validate_session` call because validation
/// mutates the session (refreshes tokens). The lock is dropped before the
/// heavier `authenticate` call to avoid blocking download tasks. A 30-second
/// timeout guards against a hung validation request holding the lock
/// indefinitely.
async fn attempt_reauth<F>(
    shared_session: &auth::SharedSession,
    cookie_directory: &Path,
    username: &str,
    domain: &str,
    password_provider: &F,
) -> anyhow::Result<()>
where
    F: Fn() -> Option<SecretString>,
{
    let mut session = shared_session.write().await;

    // Try validation first — timeout prevents a hung HTTP request from
    // holding the write lock indefinitely and starving download tasks.
    let valid = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        auth::validate_session(&mut session, domain),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Session validation timed out after 30s"))??;
    if valid {
        tracing::debug!("Session still valid after re-validation");
        return Ok(());
    }

    tracing::info!("Session invalid, performing full re-authentication...");
    session.release_lock()?;
    drop(session);

    let new_auth = auth::authenticate(
        cookie_directory,
        username,
        password_provider,
        domain,
        None,
        None,
        None, // no code — interactive prompt or TwoFactorRequired
    )
    .await?;

    let mut session = shared_session.write().await;
    *session = new_auth.session;
    tracing::info!("Re-authentication successful");
    Ok(())
}

/// Wait for `submit-code` to update the session file, with no network traffic.
///
/// Polls the session file's modification time every 5 seconds. When
/// `submit-code` trusts the session it writes updated cookies/session data,
/// changing the mtime and breaking the loop.
async fn wait_for_2fa_submit(cookie_dir: &Path, username: &str) {
    let session_path = auth::session_file_path(cookie_dir, username);
    let initial_mtime = std::fs::metadata(&session_path)
        .and_then(|m| m.modified())
        .ok();

    tracing::info!("Waiting for 2FA code submission...");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        let current_mtime = std::fs::metadata(&session_path)
            .and_then(|m| m.modified())
            .ok();
        if current_mtime != initial_mtime {
            tracing::info!("Session file updated, retrying authentication");
            break;
        }
    }
}

/// Get the database path for a given auth config, merging with TOML defaults.
///
/// Returns an error if the resolved username is empty, since an empty username
/// produces a `.db` filename that silently operates on the wrong database.
fn get_db_path(auth: &cli::AuthArgs, toml: Option<&TomlConfig>) -> anyhow::Result<PathBuf> {
    let (username, _, _, cookie_dir) = config::resolve_auth(auth, toml);
    if username.is_empty() {
        anyhow::bail!(
            "--username is required (or set ICLOUD_USERNAME, or add username to config file)"
        );
    }
    Ok(cookie_dir.join(format!(
        "{}.db",
        auth::session::sanitize_username(&username)
    )))
}

/// Run the status command.
async fn run_status(args: cli::StatusArgs, toml: Option<&TomlConfig>) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    let summary = db.get_summary().await?;

    println!("State Database: {}", db_path.display());
    println!();
    println!("Assets:");
    println!("  Total:      {}", summary.total_assets);
    println!("  Downloaded: {}", summary.downloaded);
    println!("  Pending:    {}", summary.pending);
    println!("  Failed:     {}", summary.failed);
    println!();

    if let Some(started) = &summary.last_sync_started {
        println!(
            "Last sync started:   {}",
            started.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if let Some(completed) = &summary.last_sync_completed {
        println!(
            "Last sync completed: {}",
            completed.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }

    if args.failed && summary.failed > 0 {
        println!();
        println!("Failed assets:");
        let failed = db.get_failed().await?;
        for asset in failed {
            let last_seen = asset.last_seen_at.format("%Y-%m-%d %H:%M:%S");
            println!(
                "  {} ({}) - {} (attempts: {}, last seen: {})",
                asset.filename,
                asset.id,
                asset.last_error.as_deref().unwrap_or("unknown error"),
                asset.download_attempts,
                last_seen
            );
        }
    }

    Ok(())
}

/// Run the reset-state command.
async fn run_reset_state(
    args: cli::ResetStateArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    if !args.yes {
        use std::io::Write;
        println!("This will delete the state database at:");
        println!("  {}", db_path.display());
        println!();
        print!("Are you sure? [y/N] ");
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    tokio::fs::remove_file(&db_path).await?;
    println!("State database deleted.");

    // Also remove WAL and SHM files if they exist
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");
    let _ = tokio::fs::remove_file(&wal_path).await;
    let _ = tokio::fs::remove_file(&shm_path).await;

    Ok(())
}

/// Run the verify command.
async fn run_verify(args: cli::VerifyArgs, toml: Option<&TomlConfig>) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    let summary = db.get_summary().await?;

    println!("Verifying {} downloaded assets...", summary.downloaded);
    println!();

    let mut missing = 0u64;
    let mut corrupted = 0u64;
    let mut verified = 0u64;

    const PAGE_SIZE: u32 = 1000;
    let mut offset = 0u64;
    loop {
        let page = db.get_downloaded_page(offset, PAGE_SIZE).await?;
        if page.is_empty() {
            break;
        }
        offset += page.len() as u64;

        for asset in &page {
            debug_assert_eq!(asset.status, state::AssetStatus::Downloaded);

            if let Some(local_path) = &asset.local_path {
                if !local_path.exists() {
                    let downloaded_at = asset.downloaded_at.map_or_else(
                        || "unknown".to_string(),
                        |dt| dt.format("%Y-%m-%d").to_string(),
                    );
                    println!(
                        "MISSING: {} ({}) - downloaded {}",
                        local_path.display(),
                        asset.id,
                        downloaded_at
                    );
                    missing += 1;
                    continue;
                }

                if args.checksums {
                    if let Some(local_cksum) = &asset.local_checksum {
                        match verify_local_checksum(local_path, local_cksum).await {
                            Ok(true) => verified += 1,
                            Ok(false) => {
                                println!("CORRUPTED: {} ({})", local_path.display(), asset.id);
                                corrupted += 1;
                            }
                            Err(e) => {
                                println!("ERROR: {} - {}", local_path.display(), e);
                                corrupted += 1;
                            }
                        }
                    } else {
                        tracing::debug!(
                            id = %asset.id,
                            "No local checksum stored, skipping verification"
                        );
                        verified += 1;
                    }
                } else {
                    verified += 1;
                }
            } else {
                println!("NO PATH: {} - no local path recorded", asset.id);
                missing += 1;
            }
        }
    }

    println!();
    println!("Results:");
    println!("  Verified:  {verified}");
    println!("  Missing:   {missing}");
    if args.checksums {
        println!("  Corrupted: {corrupted}");
    }

    if missing > 0 || corrupted > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Verify a file's SHA-256 hash against a hex-encoded expected value.
async fn verify_local_checksum(path: &Path, expected_hex: &str) -> anyhow::Result<bool> {
    let actual = download::file::compute_sha256(path).await?;
    Ok(actual == expected_hex)
}

/// Run the get-code command: trigger push notification for 2FA.
/// Run the credential subcommand: set, clear, or show backend.
async fn run_credential(
    args: cli::CredentialArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let (username, _password, _domain, cookie_directory) = config::resolve_auth(&args.auth, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required for credential management");
    }

    let store = credential::CredentialStore::new(&username, &cookie_directory);

    match args.action {
        cli::CredentialAction::Set => {
            let pw = rpassword::prompt_password("iCloud Password: ")
                .map_err(|e| anyhow::anyhow!("Failed to read password: {e}"))?;
            anyhow::ensure!(!pw.is_empty(), "Password must not be empty");
            store.store(&pw)?;
            println!("Password stored in {} backend.", store.backend_name());
        }
        cli::CredentialAction::Clear => {
            store.delete()?;
            println!("Stored credential removed.");
        }
        cli::CredentialAction::Backend => {
            println!("{}", store.backend_name());
        }
    }
    Ok(())
}

async fn run_get_code(args: cli::GetCodeArgs, toml: Option<&TomlConfig>) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(&args.auth, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required for get-code");
    }

    let password_provider =
        make_provider_from_auth(&args.auth, password, &username, &cookie_directory, toml);

    auth::send_2fa_push(
        &cookie_directory,
        &username,
        &password_provider,
        domain.as_str(),
    )
    .await?;

    println!("2FA code requested. Check your trusted devices, then run:");
    println!("  kei submit-code <CODE>");
    Ok(())
}

/// Run the submit-code command: authenticate with a pre-provided 2FA code.
async fn run_submit_code(
    args: cli::SubmitCodeArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(&args.auth, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required for submit-code");
    }

    let password_provider =
        make_provider_from_auth(&args.auth, password, &username, &cookie_directory, toml);

    let result = auth::authenticate(
        &cookie_directory,
        &username,
        &password_provider,
        domain.as_str(),
        None,
        None,
        Some(&args.code),
    )
    .await?;

    if result.requires_2fa {
        println!("2FA code accepted. Session is now authenticated.");
    } else {
        println!("Session is already authenticated.");
    }
    Ok(())
}

/// iCloud web-client build identifiers sent with every CloudKit API request.
/// Apple embeds these in the JS bundle served by `icloud.com`. To find updated
/// values: open `icloud.com/photos` in a browser, inspect any CloudKit XHR, and
/// read `clientBuildNumber` / `clientMasteringNumber` from the query string.
const ICLOUD_CLIENT_BUILD_NUMBER: &str = "2522Project44";
const ICLOUD_CLIENT_MASTERING_NUMBER: &str = "2522B2";

/// Build the query parameters `HashMap` for the iCloud Photos `CloudKit` API.
fn build_photos_params(
    client_id: &str,
    dsid: Option<&str>,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut params: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::with_capacity(4);
    params.insert(
        "clientBuildNumber".into(),
        ICLOUD_CLIENT_BUILD_NUMBER.into(),
    );
    params.insert(
        "clientMasteringNumber".into(),
        ICLOUD_CLIENT_MASTERING_NUMBER.into(),
    );
    params.insert("clientId".into(), client_id.into());
    if let Some(dsid) = dsid {
        params.insert("dsid".into(), dsid.into());
    }
    params
}

/// This imports existing local files into the state database by:
/// 1. Enumerating all iCloud assets via the photos API
/// 2. Computing the expected local path for each asset
/// 3. If the file exists and size matches, marking it as downloaded in the DB
async fn run_import_existing(
    args: cli::ImportArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    use chrono::Local;
    use futures_util::StreamExt;
    use icloud::photos::AssetVersionSize;

    let db_path = get_db_path(&args.auth, toml)?;
    let toml_dl = toml.and_then(|t| t.download.as_ref());
    let toml_photos = toml.and_then(|t| t.photos.as_ref());

    // Resolve directory and path settings from CLI > TOML > default, matching
    // the sync command's resolution so import-existing looks for files at the
    // same paths sync would have created.
    let directory_str = args
        .directory
        .or_else(|| toml_dl.and_then(|d| d.directory.clone()))
        .unwrap_or_default();
    if directory_str.is_empty() {
        anyhow::bail!("--directory is required for import-existing");
    }
    let directory = config::expand_tilde(&directory_str);
    let folder_structure = args
        .folder_structure
        .or_else(|| toml_dl.and_then(|d| d.folder_structure.clone()))
        .unwrap_or_else(|| "%Y/%m/%d".to_string());
    let keep_unicode = args
        .keep_unicode_in_filenames
        .or_else(|| toml_photos.and_then(|p| p.keep_unicode_in_filenames))
        .unwrap_or(false);

    if !directory.exists() {
        anyhow::bail!("Directory does not exist: {}", directory.display());
    }

    // Create or open the state database
    let db = Arc::new(state::SqliteStateDb::open(&db_path).await?);
    tracing::info!(path = %db_path.display(), "State database opened");

    // Resolve auth from CLI + TOML
    let (username, password, domain, cookie_directory) = config::resolve_auth(&args.auth, toml);

    // Authenticate
    let password_provider =
        make_provider_from_auth(&args.auth, password, &username, &cookie_directory, toml);

    let auth_result = auth::authenticate(
        &cookie_directory,
        &username,
        &password_provider,
        domain.as_str(),
        None,
        None,
        None,
    )
    .await?;

    let (_shared_session, photos_service) =
        init_photos_service(auth_result, domain.as_str(), retry::RetryConfig::default()).await?;

    let all_album = photos_service.all();
    let stream = all_album.photo_stream(args.recent, None, 1);
    tokio::pin!(stream);

    if !args.no_progress_bar {
        println!("Scanning iCloud assets and matching with local files...");
    }

    let mut matched = 0u64;
    let mut unmatched = 0u64;
    let mut total = 0u64;

    while let Some(result) = stream.next().await {
        let asset: icloud::photos::PhotoAsset = match result {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "Error fetching asset");
                continue;
            }
        };

        total += 1;

        // Get versions
        if asset.versions().is_empty() {
            tracing::debug!(id = %asset.id(), "Skipping asset with no versions");
            continue;
        }

        // Resolve filename using the same logic as the sync download pipeline:
        // fingerprint fallback → unicode removal → extension mapping.
        let raw_filename = if let Some(f) = asset.filename() {
            f.to_string()
        } else {
            let asset_type = asset
                .versions()
                .first()
                .map_or("", |(_, v)| v.asset_type.as_ref());
            download::paths::generate_fingerprint_filename(asset.id(), asset_type)
        };
        let base_filename = if keep_unicode {
            raw_filename
        } else {
            download::paths::remove_unicode_chars(&raw_filename)
        };

        // Get the created date in local time for path computation
        let created_local = asset.created().with_timezone(&Local);

        // Check each version (we only check "original" for import since that's
        // what the normal sync would download)
        if let Some(version) = asset.get_version(AssetVersionSize::Original) {
            // Map extension from UTI type, matching sync pipeline
            let filename =
                download::paths::map_filename_extension(&base_filename, &version.asset_type);
            let expected_path = download::paths::local_download_path(
                &directory,
                &folder_structure,
                &created_local,
                &filename,
            );

            if expected_path.exists() {
                // Check size matches
                if let Ok(metadata) = std::fs::metadata(&expected_path) {
                    if metadata.len() == version.size {
                        // File exists with matching size - mark as downloaded
                        let version_size = state::VersionSizeKey::Original;
                        let media_type = download::determine_media_type(version_size, &asset);
                        let record = state::AssetRecord::new_pending(
                            asset.id().to_string(),
                            version_size,
                            version.checksum.to_string(),
                            filename.clone(),
                            asset.created(),
                            Some(asset.added_date()),
                            version.size,
                            media_type,
                        );

                        if let Err(e) = db.upsert_seen(&record).await {
                            tracing::warn!(asset_id = %asset.id(), error = %e, "Failed to record asset");
                            continue;
                        }

                        let local_checksum = match download::file::compute_sha256(&expected_path)
                            .await
                        {
                            Ok(hash) => hash,
                            Err(e) => {
                                tracing::warn!(path = %expected_path.display(), error = %e, "Failed to hash file");
                                continue;
                            }
                        };

                        if let Err(e) = db
                            .mark_downloaded(
                                asset.id(),
                                version_size.as_str(),
                                &expected_path,
                                &local_checksum,
                                None,
                            )
                            .await
                        {
                            tracing::warn!(asset_id = %asset.id(), error = %e, "Failed to mark as downloaded");
                            continue;
                        }

                        matched += 1;
                        if !args.no_progress_bar && matched.is_multiple_of(100) {
                            println!("  Matched {matched} files so far...");
                        }
                    } else {
                        unmatched += 1;
                    }
                } else {
                    unmatched += 1;
                }
            } else {
                unmatched += 1;
            }
        }
    }

    println!();
    println!("Import complete:");
    println!("  Total assets scanned: {total}");
    println!("  Files matched:        {matched}");
    println!("  Unmatched versions:   {unmatched}");

    Ok(())
}

/// Resolve which albums to download from.
///
/// When no `--album` names are specified, returns `library.all()` (a cheap
/// in-memory construction, no API call). When names are given, calls
/// `library.albums().await` to discover user-created albums from iCloud.
async fn resolve_albums(
    library: &icloud::photos::PhotoLibrary,
    album_names: &[String],
) -> anyhow::Result<Vec<icloud::photos::PhotoAlbum>> {
    if album_names.is_empty() {
        Ok(vec![library.all()])
    } else {
        let mut album_map = library.albums().await?;
        let mut matched = Vec::new();
        for name in album_names {
            if let Some(album) = album_map.remove(name.as_str()) {
                matched.push(album);
            } else {
                let available: Vec<&String> = album_map.keys().collect();
                anyhow::bail!("Album '{name}' not found. Available albums: {available:?}");
            }
        }
        Ok(matched)
    }
}

/// RAII guard that writes the current PID to a file on creation and removes
/// it when dropped.
struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    fn new(path: PathBuf) -> std::io::Result<Self> {
        std::fs::write(&path, std::process::id().to_string())?;
        tracing::debug!(path = %path.display(), "PID file created");
        Ok(Self { path })
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::debug!(path = %self.path.display(), error = %e, "Failed to remove PID file");
        }
    }
}

/// Per-library state: zone name, sync token key, and resolved albums.
struct LibraryState {
    library: icloud::photos::PhotoLibrary,
    zone_name: String,
    sync_token_key: String,
    albums: Vec<icloud::photos::PhotoAlbum>,
}

fn main() -> ExitCode {
    // Snapshot and scrub the password env var while truly single-threaded,
    // before the tokio runtime creates worker threads.
    let env_password = std::env::var("ICLOUD_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    // SAFETY: no other threads exist yet — the tokio runtime has not been built.
    unsafe { std::env::remove_var("ICLOUD_PASSWORD") };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    match rt.block_on(run(env_password)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // 2FA required is not a failure — kei checked auth, told the user
            // what to do, and is done. Exit 0 so `restart: on-failure` won't
            // restart into a loop that hammers Apple's auth endpoints.
            if e.downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_two_factor_required)
            {
                ExitCode::SUCCESS
            } else {
                eprintln!("Error: {e:#}");
                if e.downcast_ref::<PartialSyncError>().is_some() {
                    ExitCode::from(EXIT_PARTIAL)
                } else if e.downcast_ref::<auth::error::AuthError>().is_some() {
                    ExitCode::from(EXIT_AUTH)
                } else {
                    ExitCode::FAILURE
                }
            }
        }
    }
}

async fn run(env_password: Option<String>) -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // Migrate legacy icloudpd-rs paths before loading config, so the
    // copied config.toml is found at the new location.
    let migration_report = migration::migrate_legacy_paths();

    // Load TOML config early so it can influence log level.
    // If the user explicitly set --config, the file must exist.
    //
    // Docker fallback: when no --config is passed, the default
    // ~/.config/kei/config.toml may not exist inside a container (it
    // resolves to /root/.config/kei/config.toml). Try the Docker
    // convention /config/config.toml as a fallback so that `docker exec`
    // subcommands (get-code, submit-code, credential, etc.) automatically
    // find the same config the Docker CMD uses.
    const DOCKER_FALLBACK_CONFIG: &str = "/config/config.toml";
    let config_explicitly_set =
        cli.config != "~/.config/kei/config.toml" && cli.config != DOCKER_FALLBACK_CONFIG;
    let (config_path, used_docker_fallback) = {
        let expanded = config::expand_tilde(&cli.config);
        if !config_explicitly_set && !expanded.exists() {
            let docker = PathBuf::from(DOCKER_FALLBACK_CONFIG);
            if docker.exists() {
                (docker, true)
            } else {
                (expanded, false)
            }
        } else {
            (expanded, false)
        }
    };
    let mut toml_config = config::load_toml_config(&config_path, config_explicitly_set)?;

    // Resolve log level: CLI > TOML > default (info)
    let effective_log_level = cli
        .log_level
        .or_else(|| toml_config.as_ref().and_then(|t| t.log_level))
        .unwrap_or(types::LogLevel::Info);

    // Scope debug/info to the app crate so dependency crates stay quieter.
    // Users can override with RUST_LOG env var for full control.
    let filter = match effective_log_level {
        types::LogLevel::Debug => "kei=debug,info",
        types::LogLevel::Info => "info",
        types::LogLevel::Warn => "warn",
        types::LogLevel::Error => "error",
    };
    // Password redaction: the password is set after config parsing,
    // but tracing must be initialized earlier. Use a shared slot that
    // starts as None and is populated once the password is known.
    let redact_password: Arc<std::sync::Mutex<Option<SecretString>>> =
        Arc::new(std::sync::Mutex::new(None));
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)),
        )
        .with_writer(RedactingMakeWriter {
            password: Arc::clone(&redact_password),
        })
        .init();

    // Log migration warnings now that tracing is initialized.
    if let Some(report) = &migration_report {
        for msg in &report.warnings {
            tracing::warn!("{msg}");
        }
    }

    if used_docker_fallback {
        tracing::debug!(
            path = %config_path.display(),
            "Using Docker fallback config (default path not found)"
        );
    }

    // Dispatch based on command
    let mut command = cli.effective_command();
    // Inject the password captured from env before the runtime started,
    // since we cleared ICLOUD_PASSWORD before Cli::parse() could see it.
    // Must happen before command dispatch so all subcommands (get-code,
    // submit-code, etc.) receive the password, not just sync.
    command.inject_env_password(env_password);
    let is_retry_failed = matches!(command, Command::RetryFailed(_));
    let mut is_one_shot = is_retry_failed;
    let (auth, sync) = match command {
        Command::Status(args) => return run_status(args, toml_config.as_ref()).await,
        Command::ResetState(args) => return run_reset_state(args, toml_config.as_ref()).await,
        Command::Verify(args) => return run_verify(args, toml_config.as_ref()).await,
        Command::ImportExisting(args) => {
            return run_import_existing(args, toml_config.as_ref()).await;
        }
        Command::GetCode(args) => return run_get_code(args, toml_config.as_ref()).await,
        Command::SubmitCode(args) => return run_submit_code(args, toml_config.as_ref()).await,
        Command::Credential(args) => {
            return run_credential(args, toml_config.as_ref()).await;
        }
        Command::Setup { output } => {
            let path = output.map_or_else(|| config_path.clone(), |o| config::expand_tilde(&o));
            match setup::run_setup(&path)? {
                setup::SetupResult::SyncNow {
                    config_path: cfg_path,
                    env_path,
                } => {
                    // Load .env into process environment for this session
                    let mut env_username = None;
                    let mut env_password = None;
                    if let Ok(contents) = tokio::fs::read_to_string(&env_path).await {
                        for line in contents.lines() {
                            if let Some((key, value)) = line.split_once('=') {
                                let key = key.trim();
                                // Strip surrounding single or double quotes
                                // (the setup wizard single-quotes values to
                                // prevent shell expansion when sourced).
                                let value = value.trim();
                                let value = value
                                    .strip_prefix('\'')
                                    .and_then(|v| v.strip_suffix('\''))
                                    .or_else(|| {
                                        value.strip_prefix('"').and_then(|v| v.strip_suffix('"'))
                                    })
                                    .unwrap_or(value);
                                if key == "ICLOUD_USERNAME" {
                                    env_username = Some(value.to_string());
                                } else if key == "ICLOUD_PASSWORD" {
                                    env_password = Some(value.to_string());
                                }
                            }
                        }
                    }
                    // Reload TOML from the newly written config
                    toml_config = config::load_toml_config(&cfg_path, true)?;
                    let sync_auth = cli::AuthArgs {
                        username: env_username,
                        password: env_password,
                        password_file: None,
                        password_command: None,
                        save_password: false,
                        domain: None,
                        cookie_directory: None,
                    };
                    // Setup "sync now" is a one-shot initial sync, not a daemon.
                    is_one_shot = true;
                    (sync_auth, cli::SyncArgs::default())
                }
                setup::SetupResult::Done => return Ok(()),
            }
        }
        Command::Sync { auth, sync } => (auth, sync),
        Command::RetryFailed(args) => (args.auth, args.sync),
    };
    let mut config = config::Config::build(auth, sync, toml_config)?;

    // One-shot operations — never inherit watch mode from TOML config,
    // which would cause the process to loop forever instead of exiting.
    // retry-failed: one-shot by definition.
    // setup → "sync now": initial test sync, not a daemon.
    if is_one_shot {
        config.watch_with_interval = None;
    }

    // Install password redaction now that we know the password
    if let Some(ref pw) = config.password {
        if let Ok(mut guard) = redact_password.lock() {
            *guard = Some(SecretString::from(pw.expose_secret().to_owned()));
        }
    }

    // Prevent core dumps from leaking in-memory credentials
    harden_process();

    // Write PID file if requested (before auth so the PID is visible immediately)
    let _pid_guard = config
        .pid_file
        .as_ref()
        .map(|p| PidFileGuard::new(p.clone()))
        .transpose()?;

    let sd_notifier = SystemdNotifier::new(config.notify_systemd);
    let notifier = Notifier::new(config.notification_script.clone());

    tracing::info!(concurrency = config.threads_num, "Starting kei");

    if config.username.is_empty() {
        anyhow::bail!("--username is required");
    }

    // retry-failed + dry-run is unsupported: dry-run skips the state DB,
    // but retry-failed needs it to know which assets failed.
    if is_retry_failed && config.dry_run {
        anyhow::bail!(
            "--dry-run cannot be used with retry-failed (retry needs the state database)"
        );
    }

    // Validate --directory early (before auth) for commands that need it.
    // This avoids wasting a 2FA code when the user simply forgot --directory.
    let needs_directory = !config.auth_only && !config.list_albums && !config.list_libraries;
    if needs_directory && config.directory.as_os_str().is_empty() {
        anyhow::bail!(
            "--directory is required for downloading \
             (pass --directory on the CLI or set [download] directory in the config file)"
        );
    }

    let cred_store = credential::CredentialStore::new(&config.username, &config.cookie_directory);
    let source = password::build_password_source(
        config.password.as_ref(),
        config.password_command.as_deref(),
        config.password_file.as_deref(),
        cred_store,
    );
    let password_provider = make_password_provider(source);

    let auth_result = match auth::authenticate(
        &config.cookie_directory,
        &config.username,
        &password_provider,
        config.domain.as_str(),
        None,
        None,
        None,
    )
    .await
    {
        Ok(result) => result,
        Err(e)
            if e.downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_two_factor_required) =>
        {
            let msg = format!(
                "2FA required for {u}. Run: kei get-code",
                u = config.username
            );
            tracing::warn!(message = %msg, "2FA required");
            notifier.notify(notifications::Event::TwoFaRequired, &msg, &config.username);

            // Wait for submit-code to update the session file, then retry
            // auth. Loop because get-code also writes to the session file
            // (during SRP), which changes the mtime and wakes us up before
            // the session is actually trusted.
            'wait_2fa: loop {
                wait_for_2fa_submit(&config.cookie_directory, &config.username).await;

                // Retry auth with back-off for lock contention — submit-code
                // may still be running when we detect the mtime change.
                for attempt in 0..3 {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                    match auth::authenticate(
                        &config.cookie_directory,
                        &config.username,
                        &password_provider,
                        config.domain.as_str(),
                        None,
                        None,
                        None,
                    )
                    .await
                    {
                        Ok(result) => break 'wait_2fa result,
                        Err(e)
                            if e.downcast_ref::<auth::error::AuthError>()
                                .is_some_and(auth::error::AuthError::is_two_factor_required) =>
                        {
                            tracing::info!("Session not yet trusted, continuing to wait...");
                            continue 'wait_2fa;
                        }
                        Err(e) if e.to_string().contains("Another kei instance") => {
                            tracing::debug!("Lock held by another process, retrying...");
                        }
                        Err(e) => return Err(e),
                    }
                }
                // Exhausted lock retries — back to waiting for next file change
                tracing::debug!("Lock still held after retries, resuming wait...");
            }
        }
        Err(e) => return Err(e),
    };

    // Save password to credential store if requested
    if let (true, Some(ref pw)) = (config.save_password, &config.password) {
        let store = credential::CredentialStore::new(&config.username, &config.cookie_directory);
        if let Err(e) = store.store(pw.expose_secret()) {
            tracing::warn!(error = %e, "Failed to save password to credential store");
        } else {
            tracing::info!(
                backend = store.backend_name(),
                "Password saved to credential store"
            );
        }
    }

    if config.auth_only {
        tracing::info!("Authentication completed successfully");
        return Ok(());
    }

    let api_retry_config = retry::RetryConfig {
        max_retries: config.max_retries,
        base_delay_secs: config.retry_delay_secs,
        max_delay_secs: 60,
    };

    let (shared_session, mut photos_service) =
        init_photos_service(auth_result, config.domain.as_str(), api_retry_config).await?;

    if config.list_libraries {
        println!("Private libraries:");
        let private = photos_service.fetch_private_libraries().await?;
        for name in private.keys() {
            println!("  {name}");
        }
        println!("Shared libraries:");
        let shared = photos_service.fetch_shared_libraries().await?;
        for name in shared.keys() {
            println!("  {name}");
        }
        return Ok(());
    }

    // Resolve the selected library/libraries
    let libraries: Vec<icloud::photos::PhotoLibrary> = match &config.library {
        config::LibrarySelection::All => {
            tracing::info!("Using all available libraries");
            photos_service.all_libraries().await?
        }
        config::LibrarySelection::Single(name) => {
            if name != "PrimarySync" {
                tracing::info!(library = %name, "Using non-default library");
            }
            vec![photos_service.get_library(name).await?.clone()]
        }
    };
    tracing::info!(
        count = libraries.len(),
        zones = %libraries.iter().map(|l| l.zone_name().to_string()).collect::<Vec<_>>().join(", "),
        "Resolved libraries"
    );

    if config.list_albums {
        for library in &libraries {
            println!("Library: {}", library.zone_name());
            let albums = library.albums().await?;
            for name in albums.keys() {
                println!("  {name}");
            }
        }
        return Ok(());
    }

    if config.directory.as_os_str().is_empty() {
        anyhow::bail!(
            "--directory is required for downloading \
             (pass --directory on the CLI or set [download] directory in the config file)"
        );
    }

    // Validate download directory is writable before spending time on enumeration
    tokio::fs::create_dir_all(&config.directory)
        .await
        .with_context(|| {
            format!(
                "Failed to create download directory {}",
                config.directory.display()
            )
        })?;
    let probe = config.directory.join(".kei_probe");
    tokio::fs::write(&probe, b"").await.with_context(|| {
        format!(
            "Download directory {} is not writable",
            config.directory.display()
        )
    })?;
    let _ = tokio::fs::remove_file(&probe).await;

    // Warn if available disk space is low
    if let Some(avail) = available_disk_space(&config.directory) {
        const MIN_FREE_BYTES: u64 = 1_073_741_824; // 1 GiB
        if avail < MIN_FREE_BYTES {
            let avail_mb = avail / (1024 * 1024);
            tracing::warn!(
                available_mb = avail_mb,
                path = %config.directory.display(),
                "Low disk space — downloads may fail with disk errors"
            );
        }
    }

    // Initialize state database.
    // Skip for --dry-run so a preview doesn't create the DB or poison
    // sync tokens, which would cause a subsequent real sync to believe
    // nothing has changed and download 0 photos.
    let state_db: Option<Arc<dyn state::StateDb>> = if config.dry_run {
        None
    } else {
        let db_path = config.cookie_directory.join(format!(
            "{}.db",
            auth::session::sanitize_username(&config.username)
        ));
        match state::SqliteStateDb::open(&db_path).await {
            Ok(db) => {
                tracing::debug!(path = %db_path.display(), "State database opened");
                let db = Arc::new(db);

                // For retry-failed, reset failed assets to pending
                if is_retry_failed {
                    match db.reset_failed().await {
                        Ok(0) => {
                            tracing::info!("No failed assets to retry");
                            return Ok(());
                        }
                        Ok(count) => {
                            tracing::info!(count, "Reset failed assets to pending");
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reset failed assets: {e}");
                        }
                    }
                }

                Some(db as Arc<dyn state::StateDb>)
            }
            Err(e) => {
                anyhow::bail!("Failed to open state database {}: {e}", db_path.display());
            }
        }
    };

    // Handle --reset-sync-token: clear stored tokens before the sync loop
    if config.reset_sync_token {
        if let Some(ref db) = state_db {
            db.set_metadata("db_sync_token", "").await.ok();
            for library in &libraries {
                let key = format!("sync_token:{}", library.zone_name());
                db.set_metadata(&key, "").await.ok();
            }
            tracing::info!("Cleared stored sync tokens");
        }
    }

    // Pre-compute config values used each cycle to build DownloadConfig.
    // DownloadConfig is rebuilt per-cycle so sync_mode can vary.
    let skip_created_before = config
        .skip_created_before
        .map(|d| d.with_timezone(&chrono::Utc));
    let skip_created_after = config
        .skip_created_after
        .map(|d| d.with_timezone(&chrono::Utc));
    let retry_config = api_retry_config;
    let live_photo_size = config.live_photo_size.to_asset_version_size();

    let build_download_config = |sync_mode: download::SyncMode| -> Arc<download::DownloadConfig> {
        Arc::new(download::DownloadConfig {
            directory: config.directory.clone(),
            folder_structure: config.folder_structure.clone(),
            size: config.size.into(),
            skip_videos: config.skip_videos,
            skip_photos: config.skip_photos,
            skip_created_before,
            skip_created_after,
            set_exif_datetime: config.set_exif_datetime,
            dry_run: config.dry_run,
            concurrent_downloads: config.threads_num as usize,
            recent: config.recent,
            retry: retry_config,
            skip_live_photos: config.skip_live_photos,
            live_photo_size,
            live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
            align_raw: config.align_raw,
            no_progress_bar: config.no_progress_bar,
            only_print_filenames: config.only_print_filenames,
            file_match_policy: config.file_match_policy,
            force_size: config.force_size,
            keep_unicode_in_filenames: config.keep_unicode_in_filenames,
            temp_suffix: config.temp_suffix.clone(),
            state_db: state_db.clone(),
            retry_only: is_retry_failed,
            sync_mode,
        })
    };

    let shutdown_token = shutdown::install_signal_handler(sd_notifier)?;

    let is_watch_mode = config.watch_with_interval.is_some();
    let mut reauth_attempts = 0u32;

    let mut library_states: Vec<LibraryState> = Vec::with_capacity(libraries.len());
    for library in &libraries {
        let zone_name = library.zone_name().to_string();
        let sync_token_key = format!("sync_token:{zone_name}");
        let albums = resolve_albums(library, &config.albums).await?;
        library_states.push(LibraryState {
            library: library.clone(),
            zone_name,
            sync_token_key,
            albums,
        });
    }
    sd_notifier.notify_ready();

    let mut health = health::HealthStatus::new();

    loop {
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown requested, exiting...");
            break;
        }

        // In watch mode with incremental sync, use changes/database as a
        // cheap pre-check to skip cycles when nothing has changed.
        // Only used for single-library mode; multi-library skips this optimization.
        let skip_cycle = if is_watch_mode && !config.no_incremental && library_states.len() == 1 {
            if let Some(ref db) = state_db {
                let has_token = db
                    .get_metadata(&library_states[0].sync_token_key)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|t| !t.is_empty());
                if has_token {
                    let db_token = db
                        .get_metadata("db_sync_token")
                        .await
                        .ok()
                        .flatten()
                        .filter(|t| !t.is_empty());
                    match photos_service.changes_database(db_token.as_deref()).await {
                        Ok(db_resp) => {
                            if let Err(e) =
                                db.set_metadata("db_sync_token", &db_resp.sync_token).await
                            {
                                tracing::warn!(error = %e, "Failed to store db_sync_token");
                            }
                            if db_resp.more_coming {
                                tracing::debug!(
                                    "changes/database has more pages (moreComing=true)"
                                );
                            }
                            if db_resp.zones.is_empty() && !db_resp.more_coming {
                                tracing::info!(
                                    "No changes detected (changes/database), skipping cycle"
                                );
                                true
                            } else {
                                for z in &db_resp.zones {
                                    tracing::debug!(
                                        zone = %z.zone_id.zone_name,
                                        zone_sync_token = %z.sync_token,
                                        "changes/database: zone has changes"
                                    );
                                }
                                false
                            }
                        }
                        Err(e) => {
                            tracing::debug!(
                                error = %e,
                                "changes/database pre-check failed, proceeding with sync"
                            );
                            false
                        }
                    }
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if !skip_cycle {
            sd_notifier.notify_status("Syncing...");
            sd_notifier.notify_watchdog();

            let mut cycle_failed_count = 0usize;
            let mut cycle_session_expired = false;

            for lib_state in &library_states {
                if shutdown_token.is_cancelled() {
                    break;
                }

                // Check if the download config changed since last sync. If so,
                // clear sync tokens so the subsequent lookup falls back to full
                // enumeration — the stored incremental token would miss assets
                // that are newly eligible under the changed config (e.g. a
                // user switching --size or adding --skip-videos).
                if !config.dry_run {
                    if let Some(ref db) = state_db {
                        // Use a separate key from the download-path's "config_hash"
                        // (which tracks path-affecting fields only). This hash is a
                        // superset that also includes enumeration filters (albums,
                        // library, skip_live_photos). Using the same key would cause
                        // the two hashes to overwrite each other every cycle,
                        // permanently preventing incremental sync.
                        let config_hash = download::compute_config_hash(&config);
                        let stored_hash = db.get_metadata("enum_config_hash").await.unwrap_or(None);
                        if stored_hash.as_deref() != Some(&config_hash) {
                            if stored_hash.is_some() {
                                tracing::info!(
                                    "Download config changed since last sync, clearing sync tokens"
                                );
                                match db.delete_metadata_by_prefix("sync_token:").await {
                                    Ok(n) if n > 0 => {
                                        tracing::info!(cleared = n, "Cleared stale sync tokens");
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            error = %e,
                                            "Failed to clear sync tokens"
                                        );
                                    }
                                    _ => {}
                                }
                            }
                            let _ = db.set_metadata("enum_config_hash", &config_hash).await;
                        }
                    }
                }

                // Determine sync mode per-library
                // retry-failed must always use full enumeration: incremental
                // sync only returns NEW iCloud changes, missing previously-
                // failed assets that were already enumerated but not downloaded.
                let sync_mode = if is_retry_failed || config.no_incremental {
                    if config.no_incremental && library_states.len() == 1 {
                        tracing::info!("Incremental sync disabled via --no-incremental, performing full enumeration");
                    }
                    if is_retry_failed {
                        tracing::info!("Retry-failed requires full enumeration to find previously-failed assets");
                    }
                    download::SyncMode::Full
                } else if let Some(ref db) = state_db {
                    match db.get_metadata(&lib_state.sync_token_key).await {
                        Ok(Some(ref token)) if !token.is_empty() => {
                            tracing::info!(zone = %lib_state.zone_name, "Stored sync token found, using incremental sync");
                            download::SyncMode::Incremental {
                                zone_sync_token: token.clone(),
                            }
                        }
                        Ok(_) => {
                            tracing::info!(zone = %lib_state.zone_name, "No sync token found, performing full enumeration");
                            download::SyncMode::Full
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "Failed to load sync token, falling back to full enumeration");
                            download::SyncMode::Full
                        }
                    }
                } else {
                    download::SyncMode::Full
                };

                let sync_mode_label = match &sync_mode {
                    download::SyncMode::Full => "full",
                    download::SyncMode::Incremental { .. } => "incremental",
                };
                tracing::debug!(sync_mode = sync_mode_label, zone = %lib_state.zone_name, "Starting sync cycle");

                let download_config = build_download_config(sync_mode);
                let download_client = shared_session.read().await.download_client();
                let sync_result = download::download_photos_with_sync(
                    &download_client,
                    &lib_state.albums,
                    download_config,
                    shutdown_token.clone(),
                )
                .await?;

                // Store sync token only when all downloads succeeded.
                // For full sync this is safe (state DB tracks individual failures for retry).
                // For incremental sync, advancing the token on partial failure would lose
                // change events for failed assets — they'd never appear in the next delta.
                // Note: the token is stored after download_photos_with_sync returns, which
                // means all batch flushes are complete. A crash here means the token is
                // NOT advanced, so assets will replay on next sync (safe, not data loss).
                let should_store_token =
                    matches!(sync_result.outcome, download::DownloadOutcome::Success)
                        && !config.dry_run;
                if should_store_token {
                    if let Some(ref token) = sync_result.sync_token {
                        if let Some(ref db) = state_db {
                            if let Err(e) = db.set_metadata(&lib_state.sync_token_key, token).await
                            {
                                tracing::warn!(error = %e, "Failed to store sync token");
                            } else {
                                tracing::info!(zone = %lib_state.zone_name, "Stored sync token for next incremental sync");
                            }
                        }
                    }
                } else if sync_result.sync_token.is_some() {
                    tracing::info!(
                        zone = %lib_state.zone_name,
                        "Sync token NOT advanced (incomplete sync — will replay changes next cycle)"
                    );
                }

                match sync_result.outcome {
                    download::DownloadOutcome::Success => {}
                    download::DownloadOutcome::SessionExpired { auth_error_count } => {
                        tracing::warn!(
                            auth_error_count,
                            zone = %lib_state.zone_name,
                            "Session expired during library sync"
                        );
                        cycle_session_expired = true;
                        break; // Stop iterating libraries — need re-auth
                    }
                    download::DownloadOutcome::PartialFailure { failed_count } => {
                        cycle_failed_count += failed_count;
                    }
                }
            }

            // Update health status for Docker HEALTHCHECK observability.
            if cycle_session_expired {
                health.record_failure("session expired");
            } else if cycle_failed_count > 0 {
                health.record_failure(&format!("{cycle_failed_count} downloads failed"));
            } else {
                health.record_success();
            }
            health.write(&config.cookie_directory);

            // Handle aggregate outcome across all libraries
            if cycle_session_expired {
                reauth_attempts += 1;
                if reauth_attempts >= MAX_REAUTH_ATTEMPTS {
                    anyhow::bail!(
                        "Session expired, giving up after {MAX_REAUTH_ATTEMPTS} re-auth attempts"
                    );
                }
                tracing::warn!(
                    reauth_attempts,
                    max_attempts = MAX_REAUTH_ATTEMPTS,
                    "Session expired, attempting re-auth"
                );
                match attempt_reauth(
                    &shared_session,
                    &config.cookie_directory,
                    &config.username,
                    config.domain.as_str(),
                    &password_provider,
                )
                .await
                {
                    Ok(()) => {
                        tracing::info!("Re-auth successful, resuming download...");
                        continue; // Restart entire cycle
                    }
                    Err(e)
                        if e.downcast_ref::<auth::error::AuthError>()
                            .is_some_and(auth::error::AuthError::is_two_factor_required) =>
                    {
                        // 2FA is user action, not a failed attempt — don't
                        // burn reauth_attempts so false wakeups from get-code
                        // can't exhaust the limit.
                        reauth_attempts -= 1;

                        let msg = format!(
                            "2FA required for {u}. Run: kei get-code",
                            u = config.username
                        );
                        tracing::warn!(message = %msg, "2FA required");
                        notifier.notify(
                            notifications::Event::TwoFaRequired,
                            &msg,
                            &config.username,
                        );
                        if !is_watch_mode {
                            return Err(e);
                        }
                        wait_for_2fa_submit(&config.cookie_directory, &config.username).await;
                        continue;
                    }
                    Err(e) => {
                        notifier.notify(
                            notifications::Event::SessionExpired,
                            &format!("Re-authentication failed: {e}"),
                            &config.username,
                        );
                        return Err(e);
                    }
                }
            } else if cycle_failed_count > 0 {
                notifier.notify(
                    notifications::Event::SyncFailed,
                    &format!("{cycle_failed_count} downloads failed"),
                    &config.username,
                );
                if is_watch_mode {
                    tracing::warn!(
                        failed_count = cycle_failed_count,
                        "Some downloads failed this cycle, will retry next cycle"
                    );
                } else {
                    return Err(PartialSyncError(cycle_failed_count).into());
                }
            } else {
                reauth_attempts = 0;
                notifier.notify(
                    notifications::Event::SyncComplete,
                    "Sync completed successfully",
                    &config.username,
                );
            }
        } else {
            // Skipped cycle (no changes detected) — still update health so
            // Docker HEALTHCHECK doesn't mark the container unhealthy after
            // the 2-hour staleness window when no new photos are uploaded.
            health.record_success();
            health.write(&config.cookie_directory);
        }

        if let Some(interval) = config.watch_with_interval {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, exiting...");
                break;
            }
            sd_notifier.notify_status(&format!("Waiting {interval} seconds..."));
            tracing::info!(interval_secs = interval, "Waiting before next cycle");
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                () = shutdown_token.cancelled() => {
                    tracing::info!("Shutdown during wait, exiting...");
                    break;
                }
            }

            // Validate session before next cycle; re-authenticate if expired
            attempt_reauth(
                &shared_session,
                &config.cookie_directory,
                &config.username,
                config.domain.as_str(),
                &password_provider,
            )
            .await
            .ok(); // Best-effort pre-check; mid-sync re-auth handles failures

            // Re-resolve albums per-library to discover newly created iCloud albums
            for lib_state in &mut library_states {
                match resolve_albums(&lib_state.library, &config.albums).await {
                    Ok(refreshed) => lib_state.albums = refreshed,
                    Err(e) => {
                        tracing::warn!(
                            zone = %lib_state.zone_name,
                            error = %e,
                            "Failed to refresh albums, reusing previous set"
                        );
                    }
                }
            }
        } else {
            break;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_guard_creates_and_removes() {
        let path = std::env::temp_dir().join("icloudpd_test_pid_guard.pid");
        let _ = std::fs::remove_file(&path);

        {
            let guard = PidFileGuard::new(path.clone()).unwrap();
            let contents = std::fs::read_to_string(&path).unwrap();
            assert_eq!(contents, std::process::id().to_string());
            drop(guard);
        }

        assert!(!path.exists());
    }

    #[test]
    fn pid_file_guard_handles_missing_parent() {
        let path = std::env::temp_dir().join("nonexistent_dir_abc123/test.pid");
        assert!(PidFileGuard::new(path).is_err());
    }

    #[tokio::test]
    async fn verify_local_checksum_match() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("local_match.bin");
        let content = b"hello world";
        std::fs::write(&file_path, content).unwrap();

        let hash = download::file::compute_sha256(&file_path).await.unwrap();
        assert!(verify_local_checksum(&file_path, &hash).await.unwrap());
    }

    #[tokio::test]
    async fn verify_local_checksum_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("local_mismatch.bin");
        std::fs::write(&file_path, b"hello world").unwrap();

        assert!(!verify_local_checksum(
            &file_path,
            "0000000000000000000000000000000000000000000000000000000000000000"
        )
        .await
        .unwrap());
    }

    #[tokio::test]
    async fn verify_local_checksum_nonexistent_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let result = verify_local_checksum(&dir.path().join("nonexistent_file.bin"), "abcd").await;
        assert!(result.is_err());
    }

    #[test]
    fn redacting_writer_replaces_password() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"Login with s3cret ok").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert!(!output.contains("s3cret"));
        assert!(output.contains("********"));
    }

    #[test]
    fn redacting_writer_no_password_passthrough() {
        use std::io::Write;

        let password: Arc<std::sync::Mutex<Option<SecretString>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"normal log line").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "normal log line");
    }

    #[test]
    fn redacting_writer_empty_password_passthrough() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from(
            String::new(),
        ))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"normal log line").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "normal log line");
    }

    #[test]
    fn redacting_writer_short_buffer_passthrough() {
        use std::io::Write;

        // Buffer shorter than the password can't contain it
        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from(
            "longpassword",
        ))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
            };
            writer.write_all(b"short").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "short");
    }

    #[test]
    fn redacting_writer_flush() {
        use std::io::Write;

        let password: Arc<std::sync::Mutex<Option<SecretString>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut buf = Vec::new();
        let mut writer = RedactingWriter {
            inner: &mut buf,
            password,
        };
        writer.flush().unwrap();
    }

    #[test]
    fn make_password_provider_with_direct_source() {
        let source = password::PasswordSource::Direct(Arc::new(SecretString::from("mypass")));
        let provider = make_password_provider(source);
        let result = provider().unwrap();
        assert_eq!(result.expose_secret(), "mypass");
        // Can be called multiple times
        let result2 = provider().unwrap();
        assert_eq!(result2.expose_secret(), "mypass");
    }

    #[test]
    fn make_password_provider_with_command_source() {
        let source = password::PasswordSource::Command("echo cmd_test".to_string());
        let provider = make_password_provider(source);
        let result = provider().unwrap();
        assert_eq!(result.expose_secret(), "cmd_test");
    }

    // ── build_photos_params tests ───────────────────────────────────────

    #[test]
    fn build_photos_params_includes_client_id_and_dsid() {
        let params = build_photos_params("test-client-id-123", Some("99999"));

        assert_eq!(
            params.get("clientBuildNumber"),
            Some(&serde_json::Value::String(
                ICLOUD_CLIENT_BUILD_NUMBER.to_string()
            ))
        );
        assert_eq!(
            params.get("clientMasteringNumber"),
            Some(&serde_json::Value::String(
                ICLOUD_CLIENT_MASTERING_NUMBER.to_string()
            ))
        );
        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String("test-client-id-123".to_string()))
        );
        assert_eq!(
            params.get("dsid"),
            Some(&serde_json::Value::String("99999".to_string()))
        );
    }

    #[test]
    fn build_photos_params_no_dsid() {
        let params = build_photos_params("client-abc", None);

        assert!(!params.contains_key("dsid"));
        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String("client-abc".to_string()))
        );
    }

    #[test]
    fn build_photos_params_empty_client_id() {
        let params = build_photos_params("", Some("12345"));

        assert_eq!(
            params.get("clientId"),
            Some(&serde_json::Value::String(String::new()))
        );
        assert_eq!(
            params.get("dsid"),
            Some(&serde_json::Value::String("12345".to_string()))
        );
    }

    // ── Watch-mode control flow tests ──────────────────────────────────

    use tokio_util::sync::CancellationToken;

    /// Run the watch-loop pattern and return how many cycles completed.
    async fn run_watch_loop(
        shutdown_token: &CancellationToken,
        watch_with_interval: Option<u64>,
    ) -> u32 {
        let mut cycles = 0u32;
        loop {
            if shutdown_token.is_cancelled() {
                break;
            }
            cycles += 1;
            if let Some(interval) = watch_with_interval {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                    _ = shutdown_token.cancelled() => { break; }
                }
            } else {
                break;
            }
        }
        cycles
    }

    /// The watch loop uses `tokio::select!` to make the inter-cycle sleep
    /// interruptible by a shutdown signal. Cancellation breaks out promptly
    /// despite a long interval.
    #[tokio::test]
    async fn watch_sleep_exits_promptly_on_shutdown() {
        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token_clone.cancel();
        });

        let start = std::time::Instant::now();
        let cycles = run_watch_loop(&shutdown_token, Some(3600)).await;

        assert_eq!(cycles, 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    /// A pre-cancelled token prevents any cycle from starting.
    #[test]
    fn watch_loop_skips_cycle_when_already_cancelled() {
        let shutdown_token = CancellationToken::new();
        shutdown_token.cancel();

        let mut cycles_started = 0u32;
        loop {
            if shutdown_token.is_cancelled() {
                break;
            }
            cycles_started += 1;
        }
        assert_eq!(cycles_started, 0);
    }

    /// When `watch_with_interval` is None the loop executes exactly once.
    #[tokio::test]
    async fn watch_loop_runs_once_without_interval() {
        let shutdown_token = CancellationToken::new();
        assert_eq!(run_watch_loop(&shutdown_token, None).await, 1);
    }

    /// Shutdown during inter-cycle sleep completes exactly one cycle.
    #[tokio::test]
    async fn watch_loop_completes_one_cycle_then_exits_on_shutdown() {
        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token_clone.cancel();
        });
        assert_eq!(run_watch_loop(&shutdown_token, Some(3600)).await, 1);
    }
}
