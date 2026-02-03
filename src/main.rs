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
mod types;

use std::io::IsTerminal;
use std::path::Path;

use clap::Parser;
use tracing_subscriber::EnvFilter;

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    let filter = match cli.log_level {
        types::LogLevel::Debug => "debug",
        types::LogLevel::Info => "info",
        types::LogLevel::Warn => "warn",
        types::LogLevel::Error => "error",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter)),
        )
        .init();

    let config = config::Config::from_cli(cli)?;
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

    if config.list_libraries {
        let mut photos_service = photos_service;
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

    if config.list_albums {
        let albums = photos_service.albums().await?;
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
        vec![photos_service.all()]
    } else {
        let mut album_map = photos_service.albums().await?;
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
