//! kei: photo sync engine — Rust rewrite of icloud-photos-downloader.
//!
//! Downloads photos and videos from iCloud via Apple's private CloudKit APIs.
//! Authentication uses SRP-6a with Apple's custom variant, followed by optional
//! 2FA. Photos are streamed with exponential-backoff retries on transient
//! failures.

#![warn(clippy::all)]

mod auth;
mod cli;
mod config;
mod download;
mod icloud;
mod migration;
mod notifications;
pub mod retry;
mod setup;
mod shutdown;
mod state;
mod systemd;
mod types;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

/// A writer wrapper that redacts a password string from log output.
///
/// Wraps any `io::Write` implementor and replaces occurrences of the
/// configured password with `********` in each `write()` call.
struct RedactingWriter<W> {
    inner: W,
    password: Arc<std::sync::Mutex<Option<String>>>,
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let password = self.password.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pw) = password.as_deref() {
            if !pw.is_empty() {
                // A buffer shorter than the password can't contain it,
                // avoiding the lossy UTF-8 conversion for short log fragments.
                if buf.len() >= pw.len() {
                    let s = String::from_utf8_lossy(buf);
                    if s.contains(pw) {
                        let redacted = s.replace(pw, "********");
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
    password: Arc<std::sync::Mutex<Option<String>>>,
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

/// Build a password provider closure that returns the given password or
/// falls back to prompting on stdin.
fn make_password_provider(password: Option<String>) -> impl Fn() -> Option<String> {
    move || -> Option<String> {
        password.clone().or_else(|| {
            tokio::task::block_in_place(|| rpassword::prompt_password("iCloud Password: ").ok())
        })
    }
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
    F: Fn() -> Option<String>,
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
fn get_db_path(auth: &cli::AuthArgs, toml: &Option<TomlConfig>) -> PathBuf {
    let (username, _, _, cookie_dir) = config::resolve_auth(auth, toml);
    cookie_dir.join(format!(
        "{}.db",
        auth::session::sanitize_username(&username)
    ))
}

/// Run the status command.
async fn run_status(args: cli::StatusArgs, toml: &Option<TomlConfig>) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth, toml);

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
    toml: &Option<TomlConfig>,
) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth, toml);

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        return Ok(());
    }

    if !args.yes {
        println!("This will delete the state database at:");
        println!("  {}", db_path.display());
        println!();
        print!("Are you sure? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    std::fs::remove_file(&db_path)?;
    println!("State database deleted.");

    // Also remove WAL and SHM files if they exist
    let wal_path = db_path.with_extension("db-wal");
    let shm_path = db_path.with_extension("db-shm");
    let _ = std::fs::remove_file(&wal_path);
    let _ = std::fs::remove_file(&shm_path);

    Ok(())
}

/// Run the verify command.
async fn run_verify(args: cli::VerifyArgs, toml: &Option<TomlConfig>) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth, toml);

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    let downloaded = db.get_all_downloaded().await?;

    println!("Verifying {} downloaded assets...", downloaded.len());
    println!();

    let mut missing = 0;
    let mut corrupted = 0;
    let mut verified = 0;

    for asset in &downloaded {
        // Sanity check: all assets from get_all_downloaded should have Downloaded status
        debug_assert_eq!(asset.status, state::AssetStatus::Downloaded);

        if let Some(local_path) = &asset.local_path {
            if !local_path.exists() {
                let downloaded_at = asset
                    .downloaded_at
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_string());
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
                    // Verify against locally-computed SHA-256
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
                    // Pre-v3 asset without local checksum — skip verification
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

/// Run the submit-code command: authenticate with a pre-provided 2FA code.
async fn run_submit_code(
    args: cli::SubmitCodeArgs,
    toml: &Option<TomlConfig>,
) -> anyhow::Result<()> {
    let (username, password, domain, cookie_directory) = config::resolve_auth(&args.auth, toml);

    if username.is_empty() {
        anyhow::bail!("--username is required for submit-code");
    }

    let password_provider = make_password_provider(password);

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

/// Build the query parameters HashMap for the iCloud Photos CloudKit API.
///
/// Must match Python's `PyiCloudService.params` for API compatibility.
fn build_photos_params(
    client_id: &str,
    dsid: Option<&str>,
) -> std::collections::HashMap<String, serde_json::Value> {
    let mut params = std::collections::HashMap::new();
    params.insert(
        "clientBuildNumber".to_string(),
        serde_json::Value::String("2522Project44".to_string()),
    );
    params.insert(
        "clientMasteringNumber".to_string(),
        serde_json::Value::String("2522B2".to_string()),
    );
    params.insert(
        "clientId".to_string(),
        serde_json::Value::String(client_id.to_string()),
    );
    if let Some(dsid) = dsid {
        params.insert(
            "dsid".to_string(),
            serde_json::Value::String(dsid.to_string()),
        );
    }
    params
}

/// This imports existing local files into the state database by:
/// 1. Enumerating all iCloud assets via the photos API
/// 2. Computing the expected local path for each asset
/// 3. If the file exists and size matches, marking it as downloaded in the DB
async fn run_import_existing(
    args: cli::ImportArgs,
    toml: &Option<TomlConfig>,
) -> anyhow::Result<()> {
    use chrono::Local;
    use futures_util::StreamExt;
    use icloud::photos::AssetVersionSize;

    let db_path = get_db_path(&args.auth, toml);
    let directory = config::expand_tilde(&args.directory);

    if !directory.exists() {
        anyhow::bail!("Directory does not exist: {}", directory.display());
    }

    // Create or open the state database
    let db = Arc::new(state::SqliteStateDb::open(&db_path).await?);
    tracing::info!(path = %db_path.display(), "State database opened");

    // Resolve auth from CLI + TOML
    let (username, password, domain, cookie_directory) = config::resolve_auth(&args.auth, toml);

    // Authenticate
    let password_provider = make_password_provider(password);

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

    let ckdatabasews_url = auth_result
        .data
        .webservices
        .as_ref()
        .and_then(|ws| ws.ckdatabasews.as_ref())
        .map(|ep| ep.url.as_str())
        .ok_or_else(|| anyhow::anyhow!("No ckdatabasews URL"))?;

    let client_id = auth_result.session.client_id().cloned().unwrap_or_default();
    let dsid = auth_result
        .data
        .ds_info
        .as_ref()
        .and_then(|ds| ds.dsid.clone());
    let params = build_photos_params(&client_id, dsid.as_deref());

    let shared_session: auth::SharedSession =
        Arc::new(tokio::sync::RwLock::new(auth_result.session));
    let session_box: Box<dyn icloud::photos::PhotosSession> = Box::new(shared_session.clone());

    tracing::info!("Initializing photos service...");
    let photos_service =
        icloud::photos::PhotosService::new(ckdatabasews_url.to_string(), session_box, params)
            .await?;

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

        // Get filename from the asset
        let filename = match asset.filename() {
            Some(f) => f.to_string(),
            None => {
                tracing::debug!(id = %asset.id(), "Skipping asset with no filename");
                continue;
            }
        };

        // Get versions
        if asset.versions().is_empty() {
            tracing::debug!(id = %asset.id(), "Skipping asset with no versions");
            continue;
        }

        // Get the created date in local time for path computation
        let created_local = asset.created().with_timezone(&Local);

        // Check each version (we only check "original" for import since that's
        // what the normal sync would download)
        if let Some(version) = asset.get_version(&AssetVersionSize::Original) {
            let expected_path = download::paths::local_download_path(
                &directory,
                &args.folder_structure,
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

    if !args.no_progress_bar {
        println!();
        println!("Import complete:");
        println!("  Total assets scanned: {total}");
        println!("  Files matched:        {matched}");
        println!("  Unmatched versions:   {unmatched}");
    }

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
            match album_map.remove(name.as_str()) {
                Some(album) => matched.push(album),
                None => {
                    let available: Vec<&String> = album_map.keys().collect();
                    anyhow::bail!("Album '{name}' not found. Available albums: {available:?}");
                }
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

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // 2FA required is not a failure — kei checked auth, told the user
            // what to do, and is done. Exit 0 so `restart: on-failure` won't
            // restart into a loop that hammers Apple's auth endpoints.
            if e.downcast_ref::<auth::error::AuthError>()
                .is_some_and(|ae| ae.is_two_factor_required())
            {
                ExitCode::SUCCESS
            } else {
                eprintln!("Error: {e:#}");
                ExitCode::FAILURE
            }
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // Migrate legacy icloudpd-rs paths before loading config, so the
    // copied config.toml is found at the new location.
    let migration_report = migration::migrate_legacy_paths();

    // Load TOML config early so it can influence log level.
    // If the user explicitly set --config, the file must exist.
    let config_path = config::expand_tilde(&cli.config);
    let config_explicitly_set = cli.config != "~/.config/kei/config.toml";
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
    let redact_password: Arc<std::sync::Mutex<Option<String>>> =
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

    // Dispatch based on command
    let command = cli.effective_command();
    let is_retry_failed = matches!(command, Command::RetryFailed(_));
    let (auth, sync) = match command {
        Command::Status(args) => return run_status(args, &toml_config).await,
        Command::ResetState(args) => return run_reset_state(args, &toml_config).await,
        Command::Verify(args) => return run_verify(args, &toml_config).await,
        Command::ImportExisting(args) => return run_import_existing(args, &toml_config).await,
        Command::SubmitCode(args) => return run_submit_code(args, &toml_config).await,
        Command::Setup { output } => {
            let path = output
                .map(|o| config::expand_tilde(&o))
                .unwrap_or_else(|| config_path.clone());
            match setup::run_setup(&path)? {
                setup::SetupResult::SyncNow {
                    config_path: cfg_path,
                    env_path,
                } => {
                    // Load .env into process environment for this session
                    let mut env_username = None;
                    let mut env_password = None;
                    if let Ok(contents) = std::fs::read_to_string(&env_path) {
                        for line in contents.lines() {
                            if let Some((key, value)) = line.split_once('=') {
                                let key = key.trim();
                                let value = value.trim();
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
                        domain: None,
                        cookie_directory: None,
                    };
                    (sync_auth, cli::SyncArgs::default())
                }
                setup::SetupResult::Done => return Ok(()),
            }
        }
        Command::Sync { auth, sync } => (auth, sync),
        Command::RetryFailed(args) => (args.auth, args.sync),
    };
    let config = config::Config::build(auth, sync, cli.log_level, toml_config)?;

    // Install password redaction now that we know the password
    if let Some(pw) = &config.password {
        if let Ok(mut guard) = redact_password.lock() {
            *guard = Some(pw.clone());
        }
    }

    // Write PID file if requested (before auth so the PID is visible immediately)
    let _pid_guard = config
        .pid_file
        .as_ref()
        .map(|p| PidFileGuard::new(p.clone()))
        .transpose()?;

    let sd_notifier = SystemdNotifier::new(config.notify_systemd);
    let notifier = Notifier::new(config.notification_script);

    tracing::info!(concurrency = config.threads_num, "Starting kei");

    let password_provider = make_password_provider(config.password);

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
                .is_some_and(|ae| ae.is_two_factor_required()) =>
        {
            let msg = format!(
                "2FA required for {}. Run: kei submit-code <CODE> --username {}",
                config.username, config.username
            );
            tracing::warn!(message = %msg, "2FA required");
            notifier.notify(notifications::Event::TwoFaRequired, &msg, &config.username);

            // Wait for submit-code to update the session file, then retry
            // auth. No Apple API calls while waiting.
            wait_for_2fa_submit(&config.cookie_directory, &config.username).await;

            auth::authenticate(
                &config.cookie_directory,
                &config.username,
                &password_provider,
                config.domain.as_str(),
                None,
                None,
                None,
            )
            .await?
        }
        Err(e) => return Err(e),
    };

    if config.auth_only {
        tracing::info!("Authentication completed successfully");
        return Ok(());
    }

    let ckdatabasews_url = auth_result
        .data
        .webservices
        .as_ref()
        .and_then(|ws| ws.ckdatabasews.as_ref())
        .map(|ep| ep.url.as_str())
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
    let photos_service =
        icloud::photos::PhotosService::new(ckdatabasews_url.to_string(), session_box, params)
            .await?;

    let mut photos_service = photos_service;

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
        anyhow::bail!("--directory is required for downloading");
    }

    // Initialize state database
    let state_db: Option<Arc<dyn state::StateDb>> = {
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
                tracing::warn!(
                    path = %db_path.display(),
                    error = %e,
                    "Failed to open state database, continuing without state tracking"
                );
                None
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
    let retry_config = retry::RetryConfig {
        max_retries: config.max_retries,
        base_delay_secs: config.retry_delay_secs,
        max_delay_secs: 60,
    };
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

    let shutdown_token = shutdown::install_signal_handler(&sd_notifier)?;

    let is_watch_mode = config.watch_with_interval.is_some();
    let mut reauth_attempts = 0u32;

    // Build per-library state: zone name, sync token key, and resolved albums.
    struct LibraryState {
        library: icloud::photos::PhotoLibrary,
        zone_name: String,
        sync_token_key: String,
        albums: Vec<icloud::photos::PhotoAlbum>,
    }

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

                // Determine sync mode per-library
                let sync_mode = if config.no_incremental {
                    if library_states.len() == 1 {
                        tracing::info!("Incremental sync disabled via --no-incremental, performing full enumeration");
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
                let should_store_token =
                    matches!(sync_result.outcome, download::DownloadOutcome::Success);
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
                            .is_some_and(|ae| ae.is_two_factor_required()) =>
                    {
                        let msg = format!(
                            "2FA required for {}. Run: kei submit-code <CODE> --username {}",
                            config.username, config.username
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
                    anyhow::bail!("{cycle_failed_count} downloads failed");
                }
            } else {
                reauth_attempts = 0;
                notifier.notify(
                    notifications::Event::SyncComplete,
                    "Sync completed successfully",
                    &config.username,
                );
            }
        } // !skip_cycle

        if let Some(interval) = config.watch_with_interval {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, exiting...");
                break;
            }
            sd_notifier.notify_status(&format!("Waiting {interval} seconds..."));
            tracing::info!(interval_secs = interval, "Waiting before next cycle");
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                _ = shutdown_token.cancelled() => {
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
        let dir = PathBuf::from("/tmp/claude/checksum_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("local_match.bin");
        let content = b"hello world";
        std::fs::write(&file_path, content).unwrap();

        let hash = download::file::compute_sha256(&file_path).await.unwrap();
        assert!(verify_local_checksum(&file_path, &hash).await.unwrap());
    }

    #[tokio::test]
    async fn verify_local_checksum_mismatch() {
        let dir = PathBuf::from("/tmp/claude/checksum_tests");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("local_mismatch.bin");
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
        let result =
            verify_local_checksum(Path::new("/tmp/claude/nonexistent_file_abc.bin"), "abcd").await;
        assert!(result.is_err());
    }

    #[test]
    fn redacting_writer_replaces_password() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some("s3cret".to_string())));
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

        let password = Arc::new(std::sync::Mutex::new(None));
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

        let password = Arc::new(std::sync::Mutex::new(Some(String::new())));
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
        let password = Arc::new(std::sync::Mutex::new(Some("longpassword".to_string())));
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

        let password = Arc::new(std::sync::Mutex::new(None));
        let mut buf = Vec::new();
        let mut writer = RedactingWriter {
            inner: &mut buf,
            password,
        };
        writer.flush().unwrap();
    }

    #[test]
    fn make_password_provider_with_some() {
        let provider = make_password_provider(Some("mypass".to_string()));
        assert_eq!(provider(), Some("mypass".to_string()));
        // Can be called multiple times
        assert_eq!(provider(), Some("mypass".to_string()));
    }

    // ── build_photos_params tests ───────────────────────────────────────

    #[test]
    fn build_photos_params_includes_client_id_and_dsid() {
        let params = build_photos_params("test-client-id-123", Some("99999"));

        assert_eq!(
            params.get("clientBuildNumber"),
            Some(&serde_json::Value::String("2522Project44".to_string()))
        );
        assert_eq!(
            params.get("clientMasteringNumber"),
            Some(&serde_json::Value::String("2522B2".to_string()))
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
}
