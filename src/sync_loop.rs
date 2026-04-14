//! Sync loop: the watch-mode cycle that enumerates and downloads photos.
//!
//! Extracted from `main.rs` to keep the entry point focused on CLI dispatch.
//! The public entry point is [`run_sync`], which handles config resolution,
//! authentication, the download loop, and watch-mode re-sync.

use std::sync::Arc;

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::auth;
use crate::cli;
use crate::commands::{
    attempt_reauth, init_photos_service, resolve_albums, resolve_libraries, wait_and_retry_2fa,
    MAX_REAUTH_ATTEMPTS,
};
use crate::config;
use crate::credential;
use crate::download;
use crate::health;
use crate::notifications::{self, Notifier};
use crate::password::{self, ExposeSecret, SecretString};
use crate::retry;
use crate::shutdown;
use crate::state::{self, StateDb};
use crate::systemd::SystemdNotifier;
use crate::{available_disk_space, make_password_provider, PartialSyncError, PidFileGuard};

/// Per-library state: zone name, sync token key, and resolved albums.
struct LibraryState {
    library: crate::icloud::photos::PhotoLibrary,
    zone_name: String,
    sync_token_key: String,
    albums: Vec<crate::icloud::photos::PhotoAlbum>,
    /// Asset IDs from excluded albums, used to filter out assets when
    /// `--exclude-album` is set without explicit `--album`.
    exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
}

/// Arguments that [`run_sync`] needs from the CLI dispatch layer.
pub(crate) struct SyncArgs {
    pub is_one_shot: bool,
    pub pw: cli::PasswordArgs,
    pub sync: cli::SyncArgs,
    pub toml_config: Option<config::TomlConfig>,
    pub config_explicitly_set: bool,
    pub config_path: std::path::PathBuf,
    pub redact_password: Arc<std::sync::Mutex<Option<SecretString>>>,
}

/// Run the sync command: authenticate, enumerate photos, download, and
/// optionally loop in watch mode.
pub(crate) async fn run_sync(globals: &config::GlobalArgs, args: SyncArgs) -> anyhow::Result<()> {
    let SyncArgs {
        is_one_shot,
        pw,
        sync,
        toml_config,
        config_explicitly_set,
        config_path,
        redact_password,
    } = args;

    let is_retry_failed = sync.retry_failed;
    let max_download_attempts = sync.max_download_attempts.unwrap_or(10);
    let reset_sync_token = sync.reset_sync_token;
    let toml_existed = toml_config.is_some();
    let cli_data_dir = globals
        .data_dir
        .clone()
        .or_else(|| globals.cookie_directory.clone());
    let mut config = config::Config::build(globals, pw, sync, toml_config)?;

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
    crate::harden_process();

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
            check_changes_database(&state_db, &library_states[0], &mut photos_service).await
        } else {
            false
        };

        if skip_cycle {
            // Skipped cycle (no changes detected) -- still update health so
            // Docker HEALTHCHECK doesn't mark the container unhealthy after
            // the 2-hour staleness window when no new photos are uploaded.
            health.record_success();
            health.write(&config.cookie_directory);
        } else {
            sd_notifier.notify_status("Syncing...");
            sd_notifier.notify_watchdog();

            let cycle_result = run_cycle(
                &library_states,
                &config,
                &state_db,
                is_retry_failed,
                &build_download_config,
                &shared_session,
                &shutdown_token,
            )
            .await?;

            // Update health status for Docker HEALTHCHECK observability.
            if cycle_result.session_expired {
                health.record_failure("session expired");
            } else if cycle_result.failed_count > 0 {
                health.record_failure(&format!("{} downloads failed", cycle_result.failed_count));
            } else {
                health.record_success();
            }
            health.write(&config.cookie_directory);

            // Handle aggregate outcome across all libraries
            if cycle_result.session_expired {
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
                        // 2FA is user action, not a failed attempt -- don't
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
            } else if cycle_result.failed_count > 0 {
                notifier.notify(
                    notifications::Event::SyncFailed,
                    &format!("{} downloads failed", cycle_result.failed_count),
                    &config.username,
                );
                if is_watch_mode {
                    tracing::warn!(
                        failed_count = cycle_result.failed_count,
                        "Some downloads failed this cycle, will retry next cycle"
                    );
                    last_cycle_failed_count = cycle_result.failed_count;
                } else {
                    return Err(PartialSyncError(cycle_result.failed_count).into());
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
            reacquire_session(&shared_session, &config, &password_provider).await;

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

/// Outcome of a single sync cycle across all libraries.
struct CycleResult {
    failed_count: usize,
    session_expired: bool,
}

/// Run one sync cycle: iterate all libraries, download photos, store sync tokens.
async fn run_cycle(
    library_states: &[LibraryState],
    config: &config::Config,
    state_db: &Option<Arc<dyn state::StateDb>>,
    is_retry_failed: bool,
    build_download_config: &dyn Fn(
        download::SyncMode,
        Arc<rustc_hash::FxHashSet<String>>,
    ) -> Arc<download::DownloadConfig>,
    shared_session: &auth::SharedSession,
    shutdown_token: &CancellationToken,
) -> anyhow::Result<CycleResult> {
    let mut cycle_failed_count = 0usize;
    let mut cycle_session_expired = false;

    for lib_state in library_states {
        if shutdown_token.is_cancelled() {
            break;
        }

        // Check if the download config changed since last sync. If so,
        // clear sync tokens so the subsequent lookup falls back to full
        // enumeration -- the stored incremental token would miss assets
        // that are newly eligible under the changed config (e.g. a
        // user switching --size or adding --skip-videos).
        if !config.dry_run {
            if let Some(db) = state_db {
                // Use a separate key from the download-path's "config_hash"
                // (which tracks path-affecting fields only). This hash is a
                // superset that also includes enumeration filters (albums,
                // library, skip_live_photos). Using the same key would cause
                // the two hashes to overwrite each other every cycle,
                // permanently preventing incremental sync.
                let config_hash = download::compute_config_hash(config);
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
                    if let Err(e) = db.set_metadata("enum_config_hash", &config_hash).await {
                        tracing::warn!(error = %e, "Failed to persist enum_config_hash");
                    }
                }
            }
        }

        // Determine sync mode per-library
        // retry-failed must always use full enumeration: incremental
        // sync only returns NEW iCloud changes, missing previously-
        // failed assets that were already enumerated but not downloaded.
        let sync_mode = determine_sync_mode(
            is_retry_failed,
            config.no_incremental,
            library_states.len(),
            state_db,
            &lib_state.sync_token_key,
            &lib_state.zone_name,
        )
        .await;

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
        // change events for failed assets -- they'd never appear in the next delta.
        // Note: the token is stored after download_photos_with_sync returns, which
        // means all batch flushes are complete. A crash here means the token is
        // NOT advanced, so assets will replay on next sync (safe, not data loss).
        let should_store_token =
            matches!(sync_result.outcome, download::DownloadOutcome::Success) && !config.dry_run;
        if should_store_token {
            if let Some(token) = &sync_result.sync_token {
                if let Some(db) = state_db {
                    if let Err(e) = db.set_metadata(&lib_state.sync_token_key, token).await {
                        tracing::warn!(error = %e, "Failed to store sync token");
                    } else {
                        tracing::info!(zone = %lib_state.zone_name, "Stored sync token for next incremental sync");
                    }
                }
            }
        } else if sync_result.sync_token.is_some() {
            tracing::info!(
                zone = %lib_state.zone_name,
                "Sync token NOT advanced (incomplete sync -- will replay changes next cycle)"
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
                break; // Stop iterating libraries -- need re-auth
            }
            download::DownloadOutcome::PartialFailure { failed_count } => {
                cycle_failed_count += failed_count;
            }
        }
    }

    Ok(CycleResult {
        failed_count: cycle_failed_count,
        session_expired: cycle_session_expired,
    })
}

/// Check `changes/database` to determine if this watch cycle can be skipped.
///
/// Returns `true` when no zones report changes and `moreComing` is false.
async fn check_changes_database(
    state_db: &Option<Arc<dyn state::StateDb>>,
    lib_state: &LibraryState,
    photos_service: &mut crate::icloud::photos::PhotosService,
) -> bool {
    let Some(db) = state_db else {
        return false;
    };
    let has_token = db
        .get_metadata(&lib_state.sync_token_key)
        .await
        .ok()
        .flatten()
        .is_some_and(|t| !t.is_empty());
    if !has_token {
        return false;
    }
    let db_token = db
        .get_metadata("db_sync_token")
        .await
        .ok()
        .flatten()
        .filter(|t| !t.is_empty());
    match photos_service.changes_database(db_token.as_deref()).await {
        Ok(db_resp) => {
            if let Err(e) = db.set_metadata("db_sync_token", &db_resp.sync_token).await {
                tracing::warn!(error = %e, "Failed to store db_sync_token");
            }
            if db_resp.more_coming {
                tracing::debug!("changes/database has more pages (moreComing=true)");
            }
            if db_resp.zones.is_empty() && !db_resp.more_coming {
                tracing::info!("No changes detected (changes/database), skipping cycle");
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
}

/// Determine the sync mode for a library: full enumeration or incremental.
async fn determine_sync_mode(
    is_retry_failed: bool,
    no_incremental: bool,
    library_count: usize,
    state_db: &Option<Arc<dyn state::StateDb>>,
    sync_token_key: &str,
    zone_name: &str,
) -> download::SyncMode {
    if is_retry_failed || no_incremental {
        if no_incremental && library_count == 1 {
            tracing::info!(
                "Incremental sync disabled via --no-incremental, performing full enumeration"
            );
        }
        if is_retry_failed {
            tracing::info!(
                "Retry-failed requires full enumeration to find previously-failed assets"
            );
        }
        download::SyncMode::Full
    } else if let Some(db) = state_db {
        match db.get_metadata(sync_token_key).await {
            Ok(Some(ref token)) if !token.is_empty() => {
                tracing::info!(zone = %zone_name, "Stored sync token found, using incremental sync");
                download::SyncMode::Incremental {
                    zone_sync_token: token.clone(),
                }
            }
            Ok(_) => {
                tracing::info!(zone = %zone_name, "No sync token found, performing full enumeration");
                download::SyncMode::Full
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to load sync token, falling back to full enumeration");
                download::SyncMode::Full
            }
        }
    } else {
        download::SyncMode::Full
    }
}

/// Re-validate the session after an idle sleep and re-acquire the lock.
async fn reacquire_session<F>(
    shared_session: &auth::SharedSession,
    config: &config::Config,
    password_provider: &F,
) where
    F: Fn() -> Option<SecretString>,
{
    match attempt_reauth(
        shared_session,
        &config.cookie_directory,
        &config.username,
        config.domain.as_str(),
        password_provider,
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
}
