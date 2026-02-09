//! icloudpd-rs — Rust rewrite of icloud-photos-downloader.
//!
//! Downloads photos and videos from iCloud via Apple's private CloudKit APIs.
//! Authentication uses SRP-6a with Apple's custom variant, followed by optional
//! 2FA. Photos are streamed with checksum verification and exponential-backoff
//! retries on transient failures.

#![warn(clippy::all)]

mod auth;
mod cli;
mod config;
mod download;
mod icloud;
pub mod retry;
mod shutdown;
mod state;
mod types;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
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
                let s = String::from_utf8_lossy(buf);
                if s.contains(pw) {
                    let redacted = s.replace(pw, "********");
                    self.inner.write_all(redacted.as_bytes())?;
                    return Ok(buf.len());
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

use cli::{AuthArgs, Command};
use state::StateDb;

/// Maximum number of re-authentication attempts before giving up.
const MAX_REAUTH_ATTEMPTS: u32 = 3;

/// Attempt to re-authenticate the session.
///
/// First validates the existing session; if invalid, performs full re-authentication.
/// In headless mode (non-interactive stdin), returns an error suggesting the user
/// run `--auth-only` interactively since 2FA prompts won't work.
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
    // Check if headless — 2FA won't work without a TTY
    if !std::io::stdin().is_terminal() {
        anyhow::bail!(
            "Session expired and re-authentication may require 2FA.\n\
             Run `icloudpd-rs --auth-only` interactively to re-authenticate,\n\
             then restart your sync."
        );
    }

    let mut session = shared_session.write().await;

    // Try validation first
    if auth::validate_session(&mut session, domain).await? {
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
    )
    .await?;

    let mut session = shared_session.write().await;
    *session = new_auth.session;
    tracing::info!("Re-authentication successful");
    Ok(())
}

/// Expand ~ to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    PathBuf::from(path)
}

/// Get the database path for a given auth config.
fn get_db_path(auth: &AuthArgs) -> PathBuf {
    let cookie_dir = expand_tilde(&auth.cookie_directory);
    cookie_dir.join(format!(
        "{}.db",
        auth::session::sanitize_username(&auth.username)
    ))
}

/// Run the status command.
async fn run_status(args: cli::StatusArgs) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth);

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
async fn run_reset_state(args: cli::ResetStateArgs) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth);

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
async fn run_verify(args: cli::VerifyArgs) -> anyhow::Result<()> {
    let db_path = get_db_path(&args.auth);

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
                // Verify checksum
                match verify_checksum(local_path, &asset.checksum).await {
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
                verified += 1;
            }
        } else {
            println!("NO PATH: {} - no local path recorded", asset.id);
            missing += 1;
        }
    }

    println!();
    println!("Results:");
    println!("  Verified:  {}", verified);
    println!("  Missing:   {}", missing);
    if args.checksums {
        println!("  Corrupted: {}", corrupted);
    }

    if missing > 0 || corrupted > 0 {
        std::process::exit(1);
    }

    Ok(())
}

/// Verify a file's SHA256 checksum.
async fn verify_checksum(path: &Path, expected: &str) -> anyhow::Result<bool> {
    use sha2::{Digest, Sha256};

    let path = path.to_path_buf();
    let expected = expected.to_string();

    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        std::io::copy(&mut file, &mut hasher)?;
        let hash = hasher.finalize();
        use std::fmt::Write;
        let computed = hash.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });

        // Apple sometimes uses a 33-byte format with a leading byte
        let expected_normalized = if expected.len() == 66 && expected.starts_with("01") {
            &expected[2..]
        } else {
            &expected
        };

        Ok(computed.eq_ignore_ascii_case(expected_normalized))
    })
    .await?
}

/// Run the import-existing command.
///
/// This imports existing local files into the state database by:
/// 1. Enumerating all iCloud assets via the photos API
/// 2. Computing the expected local path for each asset
/// 3. If the file exists and size matches, marking it as downloaded in the DB
async fn run_import_existing(args: cli::ImportArgs) -> anyhow::Result<()> {
    use chrono::Local;
    use futures_util::StreamExt;
    use icloud::photos::AssetVersionSize;

    let db_path = get_db_path(&args.auth);
    let directory = expand_tilde(&args.directory);

    if !directory.exists() {
        anyhow::bail!("Directory does not exist: {}", directory.display());
    }

    // Create or open the state database
    let db = Arc::new(state::SqliteStateDb::open(&db_path).await?);
    tracing::info!("State database at {}", db_path.display());

    // Authenticate
    let password_provider = {
        let pw = args.auth.password.clone();
        move || -> Option<String> {
            pw.clone().or_else(|| {
                tokio::task::block_in_place(|| rpassword::prompt_password("iCloud Password: ").ok())
            })
        }
    };

    let cookie_directory = expand_tilde(&args.auth.cookie_directory);
    let auth_result = auth::authenticate(
        &cookie_directory,
        &args.auth.username,
        &password_provider,
        args.auth.domain.as_str(),
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
        serde_json::Value::String(auth_result.session.client_id().cloned().unwrap_or_default()),
    );
    if let Some(dsid) = &auth_result
        .data
        .ds_info
        .as_ref()
        .and_then(|ds| ds.dsid.as_ref())
    {
        params.insert(
            "dsid".to_string(),
            serde_json::Value::String(dsid.to_string()),
        );
    }

    let shared_session: auth::SharedSession =
        Arc::new(tokio::sync::RwLock::new(auth_result.session));
    let session_box: Box<dyn icloud::photos::PhotosSession> = Box::new(shared_session.clone());

    tracing::info!("Initializing photos service...");
    let photos_service =
        icloud::photos::PhotosService::new(ckdatabasews_url.to_string(), session_box, params)
            .await?;

    let all_album = photos_service.all();
    let stream = all_album.photo_stream(args.recent);
    tokio::pin!(stream);

    println!("Scanning iCloud assets and matching with local files...");

    let mut matched = 0u64;
    let mut unmatched = 0u64;
    let mut total = 0u64;

    while let Some(result) = stream.next().await {
        let asset: icloud::photos::PhotoAsset = match result {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("Error fetching asset: {}", e);
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
                            tracing::warn!("Failed to record asset {}: {}", asset.id(), e);
                            continue;
                        }

                        if let Err(e) = db
                            .mark_downloaded(asset.id(), version_size.as_str(), &expected_path)
                            .await
                        {
                            tracing::warn!("Failed to mark {} as downloaded: {}", asset.id(), e);
                            continue;
                        }

                        matched += 1;
                        if matched.is_multiple_of(100) {
                            println!("  Matched {} files so far...", matched);
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
    println!("  Total assets scanned: {}", total);
    println!("  Files matched:        {}", matched);
    println!("  Unmatched versions:   {}", unmatched);

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // Scope debug/info to the app crate so dependency crates stay quieter.
    // Users can override with RUST_LOG env var for full control.
    let filter = match cli.log_level {
        types::LogLevel::Debug => "icloudpd_rs=debug,info",
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

    // Dispatch based on command
    let is_retry_failed = matches!(cli.effective_command(), Command::RetryFailed(_));
    match cli.effective_command() {
        Command::Status(args) => return run_status(args).await,
        Command::ResetState(args) => return run_reset_state(args).await,
        Command::Verify(args) => return run_verify(args).await,
        Command::ImportExisting(args) => return run_import_existing(args).await,
        Command::Sync { .. } | Command::RetryFailed(_) => {
            // Continue with sync logic below
        }
    }

    // For Sync and RetryFailed, use legacy config path
    let legacy_cli: cli::LegacyCli = cli.into();
    let config = config::Config::from_cli(legacy_cli)?;

    // Install password redaction now that we know the password
    if let Some(pw) = &config.password {
        if let Ok(mut guard) = redact_password.lock() {
            *guard = Some(pw.clone());
        }
    }

    tracing::info!(concurrency = config.threads_num, "Starting icloudpd-rs");

    let password_provider = {
        let pw = config.password;
        move || -> Option<String> {
            pw.clone().or_else(|| {
                tokio::task::block_in_place(|| rpassword::prompt_password("iCloud Password: ").ok())
            })
        }
    };

    let auth_result = auth::authenticate(
        &config.cookie_directory,
        &config.username,
        &password_provider,
        config.domain.as_str(),
        None,
        None,
    )
    .await?;

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

    // Must match Python's PyiCloudService.params for API compatibility
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
        serde_json::Value::String(auth_result.session.client_id().cloned().unwrap_or_default()),
    );
    if let Some(dsid) = &auth_result
        .data
        .ds_info
        .as_ref()
        .and_then(|ds| ds.dsid.as_ref())
    {
        params.insert(
            "dsid".to_string(),
            serde_json::Value::String(dsid.to_string()),
        );
    }

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
            println!("  {}", name);
        }
        println!("Shared libraries:");
        let shared = photos_service.fetch_shared_libraries().await?;
        for name in shared.keys() {
            println!("  {}", name);
        }
        return Ok(());
    }

    // Resolve the selected library (defaults to PrimarySync)
    if config.library != "PrimarySync" {
        tracing::info!(library = %config.library, "Using non-default library");
    }
    let library = photos_service.get_library(&config.library).await?;

    if config.list_albums {
        let albums = library.albums().await?;
        println!("Albums:");
        for name in albums.keys() {
            println!("  {}", name);
        }
        return Ok(());
    }

    if config.directory.as_os_str().is_empty() {
        anyhow::bail!("--directory is required for downloading");
    }

    let albums = if config.albums.is_empty() {
        vec![library.all()]
    } else {
        let mut album_map = library.albums().await?;
        let mut matched = Vec::new();
        for name in &config.albums {
            match album_map.remove(name.as_str()) {
                Some(album) => matched.push(album),
                None => {
                    let available: Vec<&String> = album_map.keys().collect();
                    anyhow::bail!(
                        "Album '{}' not found. Available albums: {:?}",
                        name,
                        available
                    );
                }
            }
        }
        matched
    };

    // Initialize state database
    let state_db: Option<Arc<dyn state::StateDb>> = {
        let db_path = config.cookie_directory.join(format!(
            "{}.db",
            auth::session::sanitize_username(&config.username)
        ));
        match state::SqliteStateDb::open(&db_path).await {
            Ok(db) => {
                tracing::debug!("State database opened at {}", db_path.display());
                let db = Arc::new(db);

                // For retry-failed, reset failed assets to pending
                if is_retry_failed {
                    match db.reset_failed().await {
                        Ok(count) if count > 0 => {
                            tracing::info!(count, "Reset failed assets to pending");
                        }
                        Ok(_) => {
                            tracing::info!("No failed assets to retry");
                        }
                        Err(e) => {
                            tracing::warn!("Failed to reset failed assets: {}", e);
                        }
                    }
                }

                Some(db as Arc<dyn state::StateDb>)
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to open state database at {}: {}. Continuing without state tracking.",
                    db_path.display(),
                    e
                );
                None
            }
        }
    };

    let download_config = download::DownloadConfig {
        directory: config.directory.clone(),
        folder_structure: config.folder_structure.clone(),
        size: config.size.into(),
        skip_videos: config.skip_videos,
        skip_photos: config.skip_photos,
        skip_created_before: config
            .skip_created_before
            .map(|d| d.with_timezone(&chrono::Utc)),
        skip_created_after: config
            .skip_created_after
            .map(|d| d.with_timezone(&chrono::Utc)),
        set_exif_datetime: config.set_exif_datetime,
        dry_run: config.dry_run,
        concurrent_downloads: config.threads_num as usize,
        recent: config.recent,
        retry: retry::RetryConfig {
            max_retries: config.max_retries,
            base_delay_secs: config.retry_delay_secs,
            max_delay_secs: 60,
        },
        skip_live_photos: config.skip_live_photos,
        live_photo_size: config.live_photo_size.to_asset_version_size(),
        live_photo_mov_filename_policy: config.live_photo_mov_filename_policy,
        align_raw: config.align_raw,
        no_progress_bar: config.no_progress_bar,
        file_match_policy: config.file_match_policy,
        force_size: config.force_size,
        keep_unicode_in_filenames: config.keep_unicode_in_filenames,
        temp_suffix: config.temp_suffix.clone(),
        state_db,
    };

    let shutdown_token = shutdown::install_signal_handler()?;

    let mut reauth_attempts = 0u32;

    loop {
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown requested, exiting...");
            break;
        }

        let download_client = shared_session.read().await.download_client();
        let outcome = download::download_photos(
            &download_client,
            &albums,
            &download_config,
            shutdown_token.clone(),
        )
        .await?;

        match outcome {
            download::DownloadOutcome::Success => {
                reauth_attempts = 0;
            }
            download::DownloadOutcome::SessionExpired { auth_error_count } => {
                reauth_attempts += 1;
                if reauth_attempts >= MAX_REAUTH_ATTEMPTS {
                    anyhow::bail!(
                        "Session expired {} times, giving up after {} re-auth attempts",
                        auth_error_count,
                        MAX_REAUTH_ATTEMPTS
                    );
                }
                tracing::warn!(
                    "Session expired ({} auth errors), attempting re-auth ({}/{})",
                    auth_error_count,
                    reauth_attempts,
                    MAX_REAUTH_ATTEMPTS
                );
                attempt_reauth(
                    &shared_session,
                    &config.cookie_directory,
                    &config.username,
                    config.domain.as_str(),
                    &password_provider,
                )
                .await?;
                tracing::info!("Re-auth successful, resuming download...");
                continue; // Restart download pass
            }
            download::DownloadOutcome::PartialFailure { failed_count } => {
                anyhow::bail!("{} downloads failed", failed_count);
            }
        }

        if let Some(interval) = config.watch_with_interval {
            if shutdown_token.is_cancelled() {
                tracing::info!("Shutdown requested, exiting...");
                break;
            }
            tracing::info!("Waiting {} seconds...", interval);
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
        } else {
            break;
        }
    }

    Ok(())
}
