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
mod types;

use clap::Parser;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    let filter = match cli.log_level {
        types::LogLevel::Debug => "debug",
        types::LogLevel::Info => "info",
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
        let pw = config.password.clone();
        move || -> Option<String> {
            pw.clone().or_else(|| {
                // Note: This closure is called from an async context but
                // rpassword blocks. The caller should wrap in spawn_blocking
                // if needed. For CLI startup this is acceptable.
                rpassword::prompt_password("iCloud Password: ").ok()
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
    };

    loop {
        // Check trust token expiry before each download cycle
        {
            let session = shared_session.read().await;
            if session.trust_token_expires_soon(7) {
                if let Some(age) = session.trust_token_age() {
                    tracing::warn!(
                        "Trust token is {} days old and may expire soon — \
                         consider re-authenticating with --auth-only",
                        age.as_secs() / 86400
                    );
                }
            }
        }

        let client = shared_session.read().await.http_client();
        download::download_photos(&client, &albums, &download_config).await?;

        if let Some(interval) = config.watch_with_interval {
            tracing::info!("Waiting {} seconds...", interval);
            tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

            // Validate session before next cycle; re-authenticate if expired
            {
                let mut session = shared_session.write().await;
                if !auth::validate_session(&mut session, config.domain.as_str()).await? {
                    tracing::warn!("Session expired, re-authenticating...");
                    session.release_lock()?; // release file lock before re-auth
                    drop(session); // release write lock before re-auth
                    let new_auth = auth::authenticate(
                        &config.cookie_directory,
                        &config.username,
                        &password_provider,
                        config.domain.as_str(),
                        None,
                        None,
                    )
                    .await?;
                    let mut session = shared_session.write().await;
                    *session = new_auth.session;
                    tracing::info!("Re-authentication successful");
                }
            }
        } else {
            break;
        }
    }

    Ok(())
}
