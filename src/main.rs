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
        if let Some(pw) = &*password {
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
        if libc::setrlimit(libc::RLIMIT_CORE, &raw const rlim) != 0 {
            tracing::debug!("setrlimit(RLIMIT_CORE, 0) failed");
        }
    }
}

/// Exit code for partial sync (some downloads failed, but sync was not a total failure).
const EXIT_PARTIAL: u8 = 2;
/// Exit code for authentication failures.
const EXIT_AUTH: u8 = 3;

/// Returned when some (but not all) downloads failed during a sync.
#[derive(Debug, thiserror::Error)]
#[error("{0} downloads failed")]
struct PartialSyncError(usize);

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
        if libc::statvfs(c_path.as_ptr(), &raw mut stat) != 0 {
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

/// Build a password provider from CLI password args, TOML config, and resolved auth fields.
///
/// Shared by `run_login`, `run_list`, and `run_import_existing`.
fn make_provider_from_auth(
    pw: &cli::PasswordArgs,
    password: Option<String>,
    username: &str,
    cookie_directory: &Path,
    toml: Option<&config::TomlConfig>,
) -> impl Fn() -> Option<SecretString> {
    let toml_auth = toml.and_then(|t| t.auth.as_ref());
    let password_command = config::resolve_password_command(pw, toml_auth);
    let password_file = config::resolve_password_file(pw, toml_auth);
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
/// CloudKit service returns 421 Misdirected Request (stale partition), clears
/// persisted session state, creates a completely fresh session via
/// `auth::authenticate`, and retries with the new service URL.
async fn init_photos_service(
    auth_result: auth::AuthResult,
    cookie_directory: &Path,
    username: &str,
    domain: &str,
    password_provider: &dyn Fn() -> Option<SecretString>,
    api_retry_config: retry::RetryConfig,
) -> anyhow::Result<(auth::SharedSession, icloud::photos::PhotosService)> {
    let ckdatabasews_url = auth_result
        .data
        .webservices
        .as_ref()
        .and_then(|ws| ws.ckdatabasews.as_ref())
        .map(|ep| ep.url.clone())
        .ok_or_else(|| anyhow::anyhow!("No ckdatabasews URL"))?;

    let client_id = auth_result
        .session
        .client_id()
        .unwrap_or_default()
        .to_owned();
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
                 performing full re-authentication with clean session"
            );

            // A 421 means Apple's identity service assigned a CloudKit partition
            // that CloudKit itself rejects. Recovery requires a completely fresh
            // session -- new reqwest::Client (clean HTTP/2 connection pool),
            // new cookie jar, and no stale session headers (scnt, session_id).
            // Without this, Apple treats the re-auth as session continuity and
            // returns the same stale partition URL.
            {
                let session = shared_session.write().await;
                session.clear_persisted_files().await?;
                session.release_lock()?;
            }

            let new_auth = auth::authenticate(
                cookie_directory,
                username,
                password_provider,
                domain,
                None,
                None,
                None,
            )
            .await?;

            let fresh_url = new_auth
                .data
                .webservices
                .as_ref()
                .and_then(|ws| ws.ckdatabasews.as_ref())
                .map(|ep| ep.url.clone())
                .ok_or_else(|| anyhow::anyhow!("No ckdatabasews URL after re-authentication"))?;

            if fresh_url == ckdatabasews_url {
                anyhow::bail!(
                    "Re-authentication returned the same service URL ({fresh_url}) that \
                     produced a 421 Misdirected Request. This is likely an Apple-side \
                     partition inconsistency -- please try again later"
                );
            }

            let client_id = new_auth.session.client_id().unwrap_or_default().to_owned();
            let dsid = new_auth
                .data
                .ds_info
                .as_ref()
                .and_then(|ds| ds.dsid.clone());
            let params = build_photos_params(&client_id, dsid.as_deref());

            {
                let mut session = shared_session.write().await;
                *session = new_auth.session;
            }

            let session_box: Box<dyn icloud::photos::PhotosSession> =
                Box::new(shared_session.clone());

            tracing::info!(
                old_url = %ckdatabasews_url,
                new_url = %fresh_url,
                "Retrying with fresh service URL from clean session"
            );
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
/// cached session still references the old ckdatabasews URL. Recovery requires
/// a full SRP re-authentication to obtain fresh service URLs from Apple.
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

/// Interval between polls when waiting for a 2FA code submission.
const TWO_FA_POLL_SECS: u64 = 5;

/// Wait for `submit-code` to update the session file, with no network traffic.
///
/// Polls the session file's modification time every 5 seconds. When
/// `submit-code` trusts the session it writes updated cookies/session data,
/// changing the mtime and breaking the loop.
async fn wait_for_2fa_submit(cookie_dir: &Path, username: &str) {
    let session_path = auth::session_file_path(cookie_dir, username);
    let initial_mtime = tokio::fs::metadata(&session_path)
        .await
        .and_then(|m| m.modified())
        .ok();

    tracing::info!("Waiting for 2FA code submission...");

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(TWO_FA_POLL_SECS)).await;

        let current_mtime = tokio::fs::metadata(&session_path)
            .await
            .and_then(|m| m.modified())
            .ok();
        if current_mtime != initial_mtime {
            tracing::info!("Session file updated, retrying authentication");
            break;
        }
    }
}

/// Wait for a 2FA code submission, then retry authentication with back-off.
///
/// Polls `wait_for_2fa_submit` in a loop. After each mtime change, retries
/// the provided `auth_fn` up to 3 times with 5-second back-off to handle
/// lock contention (submit-code may still be running when mtime changes).
/// False wakeups from get-code's SRP writes (which change the mtime before
/// the session is trusted) are handled by looping back to wait.
async fn wait_and_retry_2fa<T, F, Fut>(
    cookie_dir: &Path,
    username: &str,
    auth_fn: F,
) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    loop {
        wait_for_2fa_submit(cookie_dir, username).await;

        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(TWO_FA_POLL_SECS)).await;
            }
            match (auth_fn)().await {
                Ok(result) => return Ok(result),
                Err(e)
                    if e.downcast_ref::<auth::error::AuthError>()
                        .is_some_and(auth::error::AuthError::is_two_factor_required) =>
                {
                    tracing::info!("Session not yet trusted, continuing to wait...");
                    break; // Back to outer loop (wait_for_2fa_submit)
                }
                Err(e)
                    if e.downcast_ref::<auth::error::AuthError>()
                        .is_some_and(auth::error::AuthError::is_lock_contention) =>
                {
                    tracing::debug!("Lock held by another process, retrying...");
                }
                Err(e) => return Err(e),
            }
        }
        tracing::debug!("Lock still held after retries, resuming wait...");
    }
}

/// Get the database path for a given auth config, merging with TOML defaults.
///
/// Returns an error if the resolved username is empty, since an empty username
/// produces a `.db` filename that silently operates on the wrong database.
fn get_db_path(globals: &config::GlobalArgs, toml: Option<&TomlConfig>) -> anyhow::Result<PathBuf> {
    let (username, _, _, cookie_dir) =
        config::resolve_auth(globals, &cli::PasswordArgs::default(), toml);
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
async fn run_status(
    args: cli::StatusArgs,
    globals: &config::GlobalArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = get_db_path(globals, toml)?;

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
    yes: bool,
    globals: &config::GlobalArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    if !yes {
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

/// Run the reset-sync-token command.
async fn run_reset_sync_token(
    globals: &config::GlobalArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    db.set_metadata("db_sync_token", "").await?;
    let cleared = db.delete_metadata_by_prefix("sync_token:").await?;
    println!(
        "Cleared sync tokens ({} zone token{} + db token). Next sync will do a full enumeration.",
        cleared,
        if cleared == 1 { "" } else { "s" }
    );

    Ok(())
}

/// Run the verify command.
async fn run_verify(
    args: cli::VerifyArgs,
    globals: &config::GlobalArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = get_db_path(globals, toml)?;

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

/// Run the password subcommand: set, clear, or show backend.
fn run_password(
    action: cli::PasswordAction,
    globals: &config::GlobalArgs,
    pw: &cli::PasswordArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let (username, _password, _domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required for password management");
    }

    let store = credential::CredentialStore::new(&username, &cookie_directory);

    match action {
        cli::PasswordAction::Set => {
            let input = rpassword::prompt_password("iCloud Password: ")
                .map_err(|e| anyhow::anyhow!("Failed to read password: {e}"))?;
            anyhow::ensure!(!input.is_empty(), "Password must not be empty");
            store.store(&input)?;
            println!("Password stored in {} backend.", store.backend_name());
        }
        cli::PasswordAction::Clear => {
            store.delete()?;
            println!("Stored credential removed.");
        }
        cli::PasswordAction::Backend => {
            println!("{}", store.backend_name());
        }
    }
    Ok(())
}

/// Run the login command: authenticate, request 2FA push, or submit a 2FA code.
async fn run_login(
    subcommand: Option<cli::LoginCommand>,
    pw: &cli::PasswordArgs,
    globals: &config::GlobalArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required for login");
    }

    let password_provider =
        make_provider_from_auth(pw, password, &username, &cookie_directory, toml);

    match subcommand {
        Some(cli::LoginCommand::GetCode) => {
            retry_on_lock_contention(|| {
                auth::send_2fa_push(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                )
            })
            .await?;
            println!("2FA code requested. Check your trusted devices, then run:");
            println!("  kei login submit-code <CODE>");
        }
        Some(cli::LoginCommand::SubmitCode { code }) => {
            let result = retry_on_lock_contention(|| {
                auth::authenticate(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                    None,
                    None,
                    Some(&code),
                )
            })
            .await?;
            if result.requires_2fa {
                println!("2FA code accepted. Session is now authenticated.");
            } else {
                println!("Session is already authenticated.");
            }
        }
        None => {
            // Bare "kei login" = auth-only
            retry_on_lock_contention(|| {
                auth::authenticate(
                    &cookie_directory,
                    &username,
                    &password_provider,
                    domain.as_str(),
                    None,
                    None,
                    None,
                )
            })
            .await?;
            tracing::info!("Authentication completed successfully");
        }
    }
    Ok(())
}

/// Retry an auth operation on lock contention, with a brief wait.
///
/// Short-lived commands like `login get-code` and `login submit-code` may
/// collide with a `sync` process that is mid-auth (SRP takes a few seconds).
/// Instead of failing immediately, wait for the lock to be released.
async fn retry_on_lock_contention<T, F, Fut>(auth_fn: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    const MAX_ATTEMPTS: u32 = 6;
    const DELAY_SECS: u64 = 3;

    let mut last_err = None;
    for attempt in 0..MAX_ATTEMPTS {
        match (auth_fn)().await {
            Ok(result) => return Ok(result),
            Err(e)
                if e.downcast_ref::<auth::error::AuthError>()
                    .is_some_and(auth::error::AuthError::is_lock_contention) =>
            {
                tracing::warn!(
                    attempt = attempt + 1,
                    max_attempts = MAX_ATTEMPTS,
                    "Another kei process is holding the session lock, retrying..."
                );
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_secs(DELAY_SECS)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("MAX_ATTEMPTS must be >= 1"))
}

/// Run the list command: list albums or libraries.
async fn run_list(
    what: cli::ListCommand,
    pw: &cli::PasswordArgs,
    library: Option<String>,
    globals: &config::GlobalArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(globals, pw, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required");
    }

    let password_provider =
        make_provider_from_auth(pw, password, &username, &cookie_directory, toml);

    let auth_result = retry_on_lock_contention(|| {
        auth::authenticate(
            &cookie_directory,
            &username,
            &password_provider,
            domain.as_str(),
            None,
            None,
            None,
        )
    })
    .await?;

    let api_retry_config = retry::RetryConfig::default();
    let (_shared_session, mut photos_service) = init_photos_service(
        auth_result,
        &cookie_directory,
        &username,
        domain.as_str(),
        &password_provider,
        api_retry_config,
    )
    .await?;

    match what {
        cli::ListCommand::Libraries => {
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
        }
        cli::ListCommand::Albums => {
            let selection =
                config::resolve_library_selection(library, toml.and_then(|t| t.filters.as_ref()));
            let libraries = resolve_libraries(&selection, &mut photos_service).await?;
            for library in &libraries {
                println!("Library: {}", library.zone_name());
                let albums = library.albums().await?;
                for name in albums.keys() {
                    println!("  {name}");
                }
            }
        }
    }
    Ok(())
}

/// Run the config show command: dump resolved config as TOML.
fn run_config_show(globals: &config::GlobalArgs, toml: Option<&TomlConfig>) -> anyhow::Result<()> {
    let cfg = config::Config::build(
        globals,
        cli::PasswordArgs::default(),
        cli::SyncArgs::default(),
        toml.cloned(),
    )?;
    let toml_config = cfg.to_toml();
    let output = toml::to_string_pretty(&toml_config)
        .map_err(|e| anyhow::anyhow!("failed to serialize config: {e}"))?;
    print!("{output}");
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
    globals: &config::GlobalArgs,
    toml: Option<&TomlConfig>,
) -> anyhow::Result<()> {
    use chrono::Local;
    use futures_util::StreamExt;
    use icloud::photos::AssetVersionSize;

    let db_path = get_db_path(globals, toml)?;
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

    // Resolve auth from globals + TOML
    let (username, password, domain, cookie_directory) =
        config::resolve_auth(globals, &args.password, toml);

    // Authenticate
    let password_provider =
        make_provider_from_auth(&args.password, password, &username, &cookie_directory, toml);

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

    let (_shared_session, mut photos_service) = init_photos_service(
        auth_result,
        &cookie_directory,
        &username,
        domain.as_str(),
        &password_provider,
        retry::RetryConfig::default(),
    )
    .await?;

    // Resolve library selection (CLI > TOML > default PrimarySync)
    let toml_filters = toml.and_then(|t| t.filters.as_ref());
    let selection = config::resolve_library_selection(args.library, toml_filters);
    let libraries = resolve_libraries(&selection, &mut photos_service).await?;

    if !args.no_progress_bar {
        println!("Scanning iCloud assets and matching with local files...");
    }

    let mut matched = 0u64;
    let mut unmatched = 0u64;
    let mut total = 0u64;

    for library in &libraries {
        tracing::info!(zone = %library.zone_name(), "Scanning library");
        let all_album = library.all();
        let stream = all_album.photo_stream(args.recent, None, 1);
        tokio::pin!(stream);

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
                    None, // import-existing doesn't have album context
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

                            let local_checksum = match download::file::compute_sha256(
                                &expected_path,
                            )
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
    }

    println!();
    println!("Import complete:");
    println!("  Total assets scanned: {total}");
    println!("  Files matched:        {matched}");
    println!("  Unmatched versions:   {unmatched}");

    Ok(())
}

/// Resolve a `LibrarySelection` into concrete `PhotoLibrary` instances.
async fn resolve_libraries(
    selection: &config::LibrarySelection,
    photos_service: &mut icloud::photos::PhotosService,
) -> anyhow::Result<Vec<icloud::photos::PhotoLibrary>> {
    match selection {
        config::LibrarySelection::All => {
            tracing::info!("Using all available libraries");
            photos_service.all_libraries().await
        }
        config::LibrarySelection::Single(name) => {
            if name != "PrimarySync" {
                tracing::info!(library = %name, "Using non-default library");
            }
            Ok(vec![photos_service.get_library(name).await?.clone()])
        }
    }
}

/// Resolve which albums to download from, plus any asset IDs to exclude.
///
/// When no `--album` names are specified, returns `library.all()` (a cheap
/// in-memory construction, no API call). When names are given, calls
/// `library.albums().await` to discover user-created albums from iCloud.
///
/// The returned `FxHashSet<String>` contains asset IDs from excluded albums
/// that should be filtered out at download time. This is only populated when
/// `--exclude-album` is set without `--album`, because the all-photos stream
/// doesn't carry album membership per asset.
async fn resolve_albums(
    library: &icloud::photos::PhotoLibrary,
    album_names: &[String],
    exclude_albums: &[String],
) -> anyhow::Result<(
    Vec<icloud::photos::PhotoAlbum>,
    rustc_hash::FxHashSet<String>,
)> {
    use futures_util::StreamExt;

    let empty_ids = rustc_hash::FxHashSet::default();

    if album_names.is_empty() && exclude_albums.is_empty() {
        return Ok((vec![library.all()], empty_ids));
    }

    if album_names.is_empty() {
        // No --album but --exclude-album is set: use library.all() as the
        // base (all photos) and pre-collect asset IDs from excluded albums
        // so they can be filtered at download time. This avoids silently
        // dropping photos that aren't in any named album.
        let album_map = library.albums().await?;
        let mut exclude_ids = rustc_hash::FxHashSet::default();
        for name in exclude_albums {
            if let Some(album) = album_map.get(name.as_str()) {
                let count = album.len().await.unwrap_or(0);
                tracing::info!(album = name, count, "Pre-fetching excluded album asset IDs");
                let (stream, _token_rx) = album.photo_stream_with_token(None, Some(count), 1);
                tokio::pin!(stream);
                while let Some(Ok(asset)) = stream.next().await {
                    exclude_ids.insert(asset.id().to_string());
                }
            } else {
                tracing::warn!(album = name, "Excluded album not found, ignoring");
            }
        }
        tracing::info!(count = exclude_ids.len(), "Collected excluded asset IDs");
        return Ok((vec![library.all()], exclude_ids));
    }

    // Explicit --album list: resolve and exclude.
    let mut album_map = library.albums().await?;
    let mut matched = Vec::new();
    for name in album_names {
        if exclude_albums.iter().any(|e| e == name) {
            tracing::info!(album = name, "Album excluded by --exclude-album");
            continue;
        }
        if let Some(album) = album_map.remove(name.as_str()) {
            matched.push(album);
        } else {
            let available: Vec<&String> = album_map.keys().collect();
            anyhow::bail!("Album '{name}' not found. Available albums: {available:?}");
        }
    }
    Ok((matched, empty_ids))
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
    /// Asset IDs from excluded albums, used to filter out assets when
    /// `--exclude-album` is set without explicit `--album`.
    exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
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
    // When --config is explicitly set but the file doesn't exist, allow it
    // if the parent directory exists (auto-config will create the file).
    // Otherwise require the file to exist so typos in --config paths error.
    // When --config is explicit but the file doesn't exist and the parent
    // dir does exist, allow it (auto-config will create the file).
    let can_auto_create =
        !config_path.exists() && config_path.parent().is_some_and(std::path::Path::is_dir);
    let config_required = config_explicitly_set && !can_auto_create;
    let mut toml_config = config::load_toml_config(&config_path, config_required)?;

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

    // Build globals from CLI early (username, domain, data_dir, cookie_directory).
    let mut globals = config::GlobalArgs::from_cli(&cli);

    // Dispatch based on command
    let mut command = cli.effective_command();
    // Inject the password captured from env before the runtime started,
    // since we cleared ICLOUD_PASSWORD before Cli::parse() could see it.
    // Must happen before command dispatch so all subcommands (login,
    // list, etc.) receive the password, not just sync.
    command.inject_env_password(env_password);
    let (is_one_shot, pw, sync) = match command {
        Command::Status(args) => {
            return run_status(args, &globals, toml_config.as_ref()).await;
        }
        Command::Reset { what } => match what {
            cli::ResetCommand::State { yes } => {
                return run_reset_state(yes, &globals, toml_config.as_ref()).await;
            }
            cli::ResetCommand::SyncToken => {
                return run_reset_sync_token(&globals, toml_config.as_ref()).await;
            }
        },
        Command::Verify(args) => {
            return run_verify(args, &globals, toml_config.as_ref()).await;
        }
        Command::ImportExisting(args) => {
            return run_import_existing(args, &globals, toml_config.as_ref()).await;
        }
        Command::Login {
            password,
            subcommand,
        } => {
            return run_login(subcommand, &password, &globals, toml_config.as_ref()).await;
        }
        Command::Password { password, action } => {
            return run_password(action, &globals, &password, toml_config.as_ref());
        }
        Command::List {
            password,
            library,
            what,
        } => {
            return run_list(what, &password, library, &globals, toml_config.as_ref()).await;
        }
        Command::Config { action } => match action {
            cli::ConfigAction::Show => {
                return run_config_show(&globals, toml_config.as_ref());
            }
            cli::ConfigAction::Setup { output } => {
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
                                            value
                                                .strip_prefix('"')
                                                .and_then(|v| v.strip_suffix('"'))
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
                        // Override globals with env values from setup
                        if let Some(u) = env_username {
                            globals.username = Some(u);
                        }
                        let sync_pw = cli::PasswordArgs {
                            password: env_password,
                            ..cli::PasswordArgs::default()
                        };
                        // Setup "sync now" is a one-shot initial sync, not a daemon.
                        (true, sync_pw, cli::SyncArgs::default())
                    }
                    setup::SetupResult::Done => return Ok(()),
                }
            }
        },
        Command::Sync { password, sync } => (sync.retry_failed, password, sync),
        // Legacy variants should never reach here (effective_command maps them)
        _ => unreachable!("legacy command variants should be mapped by effective_command()"),
    };
    let is_retry_failed = sync.retry_failed;
    let max_download_attempts = sync.max_download_attempts.unwrap_or(10);
    let reset_sync_token = sync.reset_sync_token;
    let toml_existed = toml_config.is_some();
    let cli_data_dir = globals
        .data_dir
        .clone()
        .or_else(|| globals.cookie_directory.clone());
    let mut config = config::Config::build(&globals, pw, sync, toml_config)?;

    // On first run (no config file), persist CLI-provided values so
    // subsequent runs don't need the same flags again. Only when the
    // user explicitly chose a config path (--config), to avoid surprise
    // writes at the default location during tests or one-off runs.
    if !toml_existed && config_explicitly_set {
        if let Err(e) =
            config::persist_first_run_config(&config_path, &config, cli_data_dir.as_deref())
        {
            tracing::warn!(error = %e, "Failed to save first-run config");
        }
    }

    // One-shot operations — never inherit watch mode from TOML config,
    // which would cause the process to loop forever instead of exiting.
    // retry-failed: one-shot by definition.
    // setup → "sync now": initial test sync, not a daemon.
    if is_one_shot {
        config.watch_with_interval = None;
    }

    // Install password redaction now that we know the password
    if let Some(pw) = &config.password {
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

    // Validate --directory early (before auth) to avoid wasting a 2FA code
    // when the user simply forgot --directory.
    if config.directory.as_os_str().is_empty() {
        anyhow::bail!(
            "--directory is required for downloading \
             (pass --directory on the CLI or set [download] directory in the config file)"
        );
    }

    // Validate download directory is writable before spending time on authentication.
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

    // Abort if available disk space is too low.
    if let Some(avail) = available_disk_space(&config.directory) {
        const MIN_FREE_BYTES: u64 = 1_073_741_824; // 1 GiB
        if avail < MIN_FREE_BYTES {
            let avail_mb = avail / (1024 * 1024);
            anyhow::bail!(
                "Insufficient disk space: only {avail_mb} MiB available in {} (minimum 1 GiB)",
                config.directory.display()
            );
        }
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
                "2FA required for {u}. Run: kei login get-code",
                u = config.username
            );
            tracing::warn!(message = %msg, "2FA required");
            notifier.notify(notifications::Event::TwoFaRequired, &msg, &config.username);

            wait_and_retry_2fa(&config.cookie_directory, &config.username, || {
                auth::authenticate(
                    &config.cookie_directory,
                    &config.username,
                    &password_provider,
                    config.domain.as_str(),
                    None,
                    None,
                    None,
                )
            })
            .await?
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

    let api_retry_config = retry::RetryConfig {
        max_retries: config.max_retries,
        base_delay_secs: config.retry_delay_secs,
        max_delay_secs: 60,
    };

    let (shared_session, mut photos_service) = init_photos_service(
        auth_result,
        &config.cookie_directory,
        &config.username,
        config.domain.as_str(),
        &password_provider,
        api_retry_config,
    )
    .await?;

    // Resolve the selected library/libraries
    let libraries = resolve_libraries(&config.library, &mut photos_service).await?;
    tracing::info!(
        count = libraries.len(),
        zones = %libraries.iter().map(|l| l.zone_name().to_string()).collect::<Vec<_>>().join(", "),
        "Resolved libraries"
    );

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

    // Handle --reset-sync-token (hidden compat flag): clear stored tokens before the sync loop
    if reset_sync_token {
        if let Some(db) = &state_db {
            let mut cleared_ok = true;
            if let Err(e) = db.set_metadata("db_sync_token", "").await {
                tracing::warn!(error = %e, "Failed to clear db_sync_token");
                cleared_ok = false;
            }
            for library in &libraries {
                let key = format!("sync_token:{}", library.zone_name());
                if let Err(e) = db.set_metadata(&key, "").await {
                    tracing::warn!(error = %e, key = %key, "Failed to clear sync token");
                    cleared_ok = false;
                }
            }
            if cleared_ok {
                tracing::info!("Cleared stored sync tokens");
            }
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
    let build_download_config = |sync_mode: download::SyncMode,
                                 exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>|
     -> Arc<download::DownloadConfig> {
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
            live_photo_mode: config.live_photo_mode,
            live_photo_size,
            live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
            align_raw: config.align_raw,
            no_progress_bar: config.no_progress_bar,
            only_print_filenames: config.only_print_filenames,
            file_match_policy: config.file_match_policy,
            force_size: config.force_size,
            keep_unicode_in_filenames: config.keep_unicode_in_filenames,
            filename_exclude: config.filename_exclude.clone(),
            temp_suffix: config.temp_suffix.clone(),
            state_db: state_db.clone(),
            retry_only: is_retry_failed,
            max_download_attempts,
            sync_mode,
            album_name: None,
            exclude_asset_ids,
        })
    };

    let shutdown_token = shutdown::install_signal_handler(sd_notifier)?;

    let is_watch_mode = config.watch_with_interval.is_some();
    let mut reauth_attempts = 0u32;
    let mut last_cycle_failed_count = 0usize;

    let mut library_states: Vec<LibraryState> = Vec::with_capacity(libraries.len());
    for library in &libraries {
        let zone_name = library.zone_name().to_string();
        let sync_token_key = format!("sync_token:{zone_name}");
        let (albums, exclude_ids) =
            resolve_albums(library, &config.albums, &config.exclude_albums).await?;
        library_states.push(LibraryState {
            library: library.clone(),
            zone_name,
            sync_token_key,
            albums,
            exclude_asset_ids: Arc::new(exclude_ids),
        });
    }
    sd_notifier.notify_ready();

    let mut health = health::HealthStatus::new();
    let mut consecutive_album_refresh_failures = 0u32;

    loop {
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown requested, exiting...");
            break;
        }

        // In watch mode with incremental sync, use changes/database as a
        // cheap pre-check to skip cycles when nothing has changed.
        // Only used for single-library mode; multi-library skips this optimization.
        let skip_cycle = if is_watch_mode && !config.no_incremental && library_states.len() == 1 {
            if let Some(db) = &state_db {
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

        if skip_cycle {
            // Skipped cycle (no changes detected) — still update health so
            // Docker HEALTHCHECK doesn't mark the container unhealthy after
            // the 2-hour staleness window when no new photos are uploaded.
            health.record_success();
            health.write(&config.cookie_directory);
        } else {
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
                    if let Some(db) = &state_db {
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
                            if let Err(e) = db.set_metadata("enum_config_hash", &config_hash).await
                            {
                                tracing::warn!(error = %e, "Failed to persist enum_config_hash");
                            }
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
                } else if let Some(db) = &state_db {
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

                let download_config =
                    build_download_config(sync_mode, Arc::clone(&lib_state.exclude_asset_ids));
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
                    if let Some(token) = &sync_result.sync_token {
                        if let Some(db) = &state_db {
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
                            "2FA required for {u}. Run: kei login get-code",
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

                        wait_and_retry_2fa(&config.cookie_directory, &config.username, || {
                            attempt_reauth(
                                &shared_session,
                                &config.cookie_directory,
                                &config.username,
                                config.domain.as_str(),
                                &password_provider,
                            )
                        })
                        .await?;
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
                    last_cycle_failed_count = cycle_failed_count;
                } else {
                    return Err(PartialSyncError(cycle_failed_count).into());
                }
            } else {
                reauth_attempts = 0;
                last_cycle_failed_count = 0;
                notifier.notify(
                    notifications::Event::SyncComplete,
                    "Sync completed successfully",
                    &config.username,
                );
            }
        }

        if let Some(interval) = config.watch_with_interval {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, exiting...");
                break;
            }

            // Release the file lock during idle sleep so that docker exec
            // commands (login get-code, login submit-code) can acquire it.
            {
                let session = shared_session.read().await;
                if let Err(e) = session.release_lock() {
                    tracing::warn!(error = %e, "Failed to release lock before idle sleep");
                }
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

            // Validate session before next cycle; re-authenticate if expired.
            // If the session is still valid, attempt_reauth returns without
            // re-locking, so we re-acquire the lock ourselves. If the session
            // is invalid, attempt_reauth creates a new Session with its own lock.
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
                    // Re-acquire the lock. If attempt_reauth performed a full
                    // re-auth, the new Session already holds its own lock, so
                    // LockContention here is expected and harmless.
                    let session = shared_session.read().await;
                    if let Err(e) = session.reacquire_lock() {
                        if e.downcast_ref::<auth::error::AuthError>()
                            .is_some_and(auth::error::AuthError::is_lock_contention)
                        {
                            tracing::debug!("Lock held by new session after reauth");
                        } else {
                            tracing::warn!(error = %e, "Failed to reacquire lock after idle");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Pre-cycle reauth failed, will retry mid-sync");
                }
            }

            // Re-resolve albums per-library to discover newly created iCloud albums.
            // TODO: When --exclude-album is set without --album, this re-fetches the
            // entire excluded album(s) to collect asset IDs. For large excluded albums
            // this is expensive -- consider caching exclude_asset_ids across watch
            // cycles and only refreshing when the album's sync token changes.
            for lib_state in &mut library_states {
                match resolve_albums(&lib_state.library, &config.albums, &config.exclude_albums)
                    .await
                {
                    Ok((refreshed, exclude_ids)) => {
                        lib_state.albums = refreshed;
                        lib_state.exclude_asset_ids = Arc::new(exclude_ids);
                        consecutive_album_refresh_failures = 0;
                    }
                    Err(e) => {
                        consecutive_album_refresh_failures += 1;
                        if consecutive_album_refresh_failures >= 3 {
                            tracing::error!(
                                zone = %lib_state.zone_name,
                                error = %e,
                                consecutive_failures = consecutive_album_refresh_failures,
                                "Repeated album refresh failures, reusing previous set"
                            );
                        } else {
                            tracing::warn!(
                                zone = %lib_state.zone_name,
                                error = %e,
                                "Failed to refresh albums, reusing previous set"
                            );
                        }
                    }
                }
            }
        } else {
            break;
        }
    }

    if last_cycle_failed_count > 0 {
        Err(PartialSyncError(last_cycle_failed_count).into())
    } else {
        Ok(())
    }
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

    // ── resolve_albums tests ──────────────────────────────────────────

    use crate::icloud::photos::PhotoLibrary;
    use crate::test_helpers::MockPhotosSession;

    /// Build a `PhotoLibrary` stub with a preconfigured mock session.
    fn stub_library(mock: MockPhotosSession) -> PhotoLibrary {
        PhotoLibrary::new_stub(Box::new(mock))
    }

    /// CloudKit folder record for a user album. The albumNameEnc field is
    /// base64-encoded.
    fn folder_record(record_name: &str, album_name: &str) -> serde_json::Value {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(album_name);
        serde_json::json!({
            "recordName": record_name,
            "recordType": "CPLAlbumByPositionLive",
            "fields": {
                "albumNameEnc": {"value": encoded},
                "isDeleted": {"value": false}
            }
        })
    }

    /// A single paired CPLMaster+CPLAsset page for photo streaming.
    fn asset_page(record_name: &str) -> serde_json::Value {
        serde_json::json!({
            "records": [
                {
                    "recordName": record_name,
                    "recordType": "CPLMaster",
                    "fields": {
                        "filenameEnc": {"value": "dGVzdC5qcGc=", "type": "STRING"},
                        "resOriginalRes": {"value": {
                            "downloadURL": "https://example.com/photo.jpg",
                            "size": 1024,
                            "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                        }},
                        "resOriginalFileType": {"value": "public.jpeg"},
                        "itemType": {"value": "public.jpeg"},
                        "adjustmentRenderType": {"value": 0, "type": "INT64"}
                    }
                },
                {
                    "recordName": format!("asset-{record_name}"),
                    "recordType": "CPLAsset",
                    "fields": {
                        "masterRef": {
                            "value": {"recordName": record_name, "zoneID": {"zoneName": "PrimarySync"}},
                            "type": "REFERENCE"
                        },
                        "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                        "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                    }
                }
            ]
        })
    }

    /// Batch album count response.
    fn album_count_response(count: u64) -> serde_json::Value {
        serde_json::json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": count}}}]}]
        })
    }

    #[tokio::test]
    async fn resolve_albums_no_album_no_exclude() {
        let mock = MockPhotosSession::new();
        let library = stub_library(mock);
        let (albums, exclude_ids) = resolve_albums(&library, &[], &[]).await.unwrap();
        assert_eq!(albums.len(), 1, "should return library.all()");
        assert!(exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_albums_exclude_not_found_warns() {
        // fetch_folders returns one album "Vacation", but we exclude "Nonexistent"
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []})); // fetch_folders: no user albums
        let library = stub_library(mock);

        let (albums, exclude_ids) = resolve_albums(&library, &[], &["Nonexistent".to_string()])
            .await
            .unwrap();
        assert_eq!(albums.len(), 1, "should return library.all()");
        assert!(exclude_ids.is_empty(), "non-existent album produces no IDs");
    }

    #[tokio::test]
    async fn resolve_albums_explicit_album_found() {
        // fetch_folders returns "Vacation" album
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);

        let (albums, exclude_ids) = resolve_albums(&library, &["Vacation".to_string()], &[])
            .await
            .unwrap();
        assert_eq!(albums.len(), 1);
        assert!(exclude_ids.is_empty());
    }

    #[tokio::test]
    async fn resolve_albums_explicit_album_not_found_errors() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": []})); // no user albums
        let library = stub_library(mock);

        let result = resolve_albums(&library, &["DoesNotExist".to_string()], &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn resolve_albums_explicit_album_with_exclusion() {
        // Two albums: Vacation and Hidden. Exclude Hidden.
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation"),
            folder_record("FOLDER_2", "Hidden")
        ]}));
        let library = stub_library(mock);

        let (albums, exclude_ids) = resolve_albums(
            &library,
            &["Vacation".to_string(), "Hidden".to_string()],
            &["Hidden".to_string()],
        )
        .await
        .unwrap();
        assert_eq!(
            albums.len(),
            1,
            "Hidden should be excluded from matched albums"
        );
        assert!(
            exclude_ids.is_empty(),
            "explicit album path doesn't populate exclude IDs"
        );
    }

    #[tokio::test]
    async fn resolve_albums_exclude_without_album_collects_ids() {
        // The mock session needs to handle:
        // 1. fetch_folders (original session) → returns album "Hidden"
        // 2. album.len() (cloned session) → returns count
        // 3. photo_stream fetcher (re-cloned session) → returns one asset page
        // 4. photo_stream fetcher 2nd call → returns empty (end of stream)
        let mock = MockPhotosSession::new()
            // 1. fetch_folders
            .ok(serde_json::json!({"records": [
                folder_record("FOLDER_1", "Hidden")
            ]}))
            // Remaining responses are cloned into the album's session:
            // 2. album.len() batch query
            .ok(album_count_response(1))
            // 3. photo_stream fetcher: first page with one asset
            .ok(asset_page("MASTER_1"))
            // 4. photo_stream fetcher: empty page (end)
            .ok(serde_json::json!({"records": []}));
        let library = stub_library(mock);

        let (albums, exclude_ids) = resolve_albums(&library, &[], &["Hidden".to_string()])
            .await
            .unwrap();
        assert_eq!(albums.len(), 1, "should return library.all()");
        assert!(
            exclude_ids.contains("MASTER_1"),
            "should contain the excluded asset ID"
        );
    }

    #[tokio::test]
    async fn resolve_albums_same_album_in_both_yields_empty() {
        let mock = MockPhotosSession::new().ok(serde_json::json!({"records": [
            folder_record("FOLDER_1", "Vacation")
        ]}));
        let library = stub_library(mock);

        let (albums, _) = resolve_albums(
            &library,
            &["Vacation".to_string()],
            &["Vacation".to_string()],
        )
        .await
        .unwrap();
        assert!(
            albums.is_empty(),
            "album present in both --album and --exclude-album should yield zero albums"
        );
    }
}
