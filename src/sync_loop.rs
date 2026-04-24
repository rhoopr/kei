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

/// Per-library state: zone name, sync token key, and resolved album plan.
struct LibraryState {
    library: crate::icloud::photos::PhotoLibrary,
    zone_name: String,
    sync_token_key: String,
    /// Ordered list of download passes. Each pass carries its own
    /// exclude-asset-ids set. See [`crate::commands::AlbumPlan`].
    plan: crate::commands::AlbumPlan,
}

/// State-DB metadata key for the first-sync shared-library notice. Bumping
/// the version suffix (e.g. `_v2`) re-fires the notice for every existing
/// data dir the next time it's used.
const SHARED_LIBRARY_NOTICE_KEY: &str = "shared_library_notice_shown_v1";

/// Classify whether an error from `init_photos_service` or
/// `resolve_libraries` indicates a stale session / routing state that
/// an SRP re-auth would fix.
///
/// Returns `true` for `ICloudError::SessionExpired` (CloudKit 401/403)
/// and `ICloudError::MisdirectedRequest` (persistent 421 after pool
/// reset) — the two classes that invalidate the cached session and
/// trigger the reauth retry branch. Extracted as a free function so
/// the classification is independently testable without spinning up
/// a full sync cycle.
fn session_error_signature(err: &anyhow::Error) -> bool {
    err.downcast_ref::<crate::icloud::error::ICloudError>()
        .is_some_and(crate::icloud::error::ICloudError::is_session_error)
}

/// Given the user's library selection, the count of iCloud shared libraries
/// on the account, and whether the notice has already fired, return the
/// warning message to emit, or `None` if no notice is warranted.
///
/// Pure function so the policy is unit-testable without mocking the
/// `PhotosService` or the state DB. The I/O wrapper lives in
/// [`maybe_notify_shared_libraries`].
fn should_notify_shared_libraries(
    selection: &config::LibrarySelection,
    shared_count: usize,
    already_notified: bool,
) -> Option<String> {
    if already_notified || shared_count == 0 {
        return None;
    }
    // Only users on the `PrimarySync` default see the notice. Anyone who
    // explicitly picked `--library all` or a specific shared zone has
    // already made a deliberate choice.
    if !matches!(selection, config::LibrarySelection::Single(s) if s == "PrimarySync") {
        return None;
    }
    let (word, verb) = if shared_count == 1 {
        ("library", "is")
    } else {
        ("libraries", "are")
    };
    Some(format!(
        "Detected {shared_count} iCloud shared {word} on this account; only the primary \
         library {verb} being synced. To include shared libraries too, set \
         `[filters] library = \"all\"` in config.toml (or pass `--library all`). \
         Run `kei list libraries` to enumerate every zone."
    ))
}

/// Probe + warning for users on the `PrimarySync` default who also have
/// shared libraries. The marker (stored in the state DB's `metadata` table
/// under [`SHARED_LIBRARY_NOTICE_KEY`]) is only set after the notice fires,
/// so accounts with no shared libraries re-probe on every sync and catch a
/// later-added library. The probe and marker write are best-effort: failures
/// degrade to `tracing::debug!` and skip without breaking the sync.
async fn maybe_notify_shared_libraries(
    selection: &config::LibrarySelection,
    photos_service: &mut crate::icloud::photos::PhotosService,
    state_db: Option<&Arc<dyn state::StateDb>>,
) {
    let already_notified = match state_db {
        Some(db) => match db.get_metadata(SHARED_LIBRARY_NOTICE_KEY).await {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "shared-library notice: metadata read failed; skipping"
                );
                return;
            }
        },
        None => false,
    };
    if already_notified {
        return;
    }

    // Skip the probe when the user explicitly picked a non-default library;
    // they've already opted in or out. `should_notify_shared_libraries`
    // repeats this check defensively.
    if !matches!(selection, config::LibrarySelection::Single(s) if s == "PrimarySync") {
        return;
    }

    let shared_count = match photos_service.fetch_shared_libraries().await {
        Ok(map) => map.len(),
        Err(e) => {
            tracing::debug!(
                error = %e,
                "shared-library notice: enumeration failed; skipping"
            );
            return;
        }
    };

    let Some(msg) = should_notify_shared_libraries(selection, shared_count, already_notified)
    else {
        return;
    };
    tracing::warn!("{msg}");

    if let Some(db) = state_db {
        if let Err(e) = db.set_metadata(SHARED_LIBRARY_NOTICE_KEY, "1").await {
            tracing::debug!(
                error = %e,
                "shared-library notice: failed to persist marker"
            );
        }
    }
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
    if reset_sync_token {
        crate::cli::deprecation_warning("--reset-sync-token", "kei reset sync-token");
    }
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
    // Snapshot the source kind before moving `source` into the provider
    // closure — used by the --save-password hook after auth succeeds.
    let password_source_kind = source.kind();
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
            notifier.notify(
                notifications::Event::TwoFaRequired,
                &msg,
                &config.username,
                None,
            );

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

    // Save password to credential store if requested. Only the ephemeral
    // `Direct` source (CLI flag / env var) persists; File / Command /
    // Store / Interactive each emit a warning explaining why the flag is
    // a no-op for that source.
    if config.save_password {
        match password::decide_save_password_action(password_source_kind) {
            password::SavePasswordAction::Save => {
                if let Some(ref pw) = config.password {
                    let store = credential::CredentialStore::new(
                        &config.username,
                        &config.cookie_directory,
                    );
                    if let Err(e) = store.store(pw.expose_secret()) {
                        tracing::warn!(error = %e, "Failed to save password to credential store");
                    } else {
                        tracing::info!(
                            backend = store.backend_name(),
                            "Password saved to credential store"
                        );
                    }
                }
            }
            password::SavePasswordAction::SkipWithWarning(reason) => {
                tracing::warn!("{reason}");
            }
        }
    }

    let api_retry_config = retry::RetryConfig {
        max_retries: config.max_retries,
        base_delay_secs: config.retry_delay_secs,
        max_delay_secs: 60,
    };
    api_retry_config.validate()?;

    // CloudKit session/routing recovery: if init or the first CloudKit query
    // surfaces a session-error signature (401 stale session, or 421 persisting
    // after a pool reset), strip routing state and force SRP re-auth. A second
    // failure bails cleanly instead of looping under Docker's restart policy.
    let mut pending_auth = Some(auth_result);
    let mut retried_after_session_error = false;
    let (shared_session, mut photos_service, libraries) = loop {
        #[allow(
            clippy::expect_used,
            reason = "pending_auth is re-populated at the end of every retry branch before looping"
        )]
        let this_auth = pending_auth
            .take()
            .expect("auth_result present at start of attempt");
        let init_result = init_photos_service(this_auth, api_retry_config).await;
        let (ss, mut ps) = match init_result {
            Ok(pair) => pair,
            Err(e) if !retried_after_session_error && session_error_signature(&e) => {
                tracing::warn!(
                    error = %e,
                    "CloudKit init failed with stale-session signature; forcing SRP re-authentication"
                );
                retried_after_session_error = true;
                pending_auth =
                    Some(reauth_with_srp(&config, &password_provider, &notifier, None).await?);
                continue;
            }
            Err(e) => return Err(e),
        };
        match resolve_libraries(&config.library, &mut ps).await {
            Ok(libs) => break (ss, ps, libs),
            Err(e) if !retried_after_session_error && session_error_signature(&e) => {
                tracing::warn!(
                    error = %e,
                    "CloudKit returned stale-session signature; forcing SRP re-authentication"
                );
                retried_after_session_error = true;
                pending_auth = Some(
                    reauth_with_srp(&config, &password_provider, &notifier, Some((ss, ps))).await?,
                );
            }
            Err(e) => return Err(e),
        }
    };
    tracing::debug!(
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

                // Promote any sync_runs rows left in status='running' from a
                // prior SIGKILL'd or crashed process. Runs once per process,
                // before any new sync starts.
                match db.promote_orphaned_sync_runs().await {
                    Ok(0) => {}
                    Ok(count) => {
                        tracing::warn!(
                            count,
                            "Promoted orphaned sync_runs rows to 'interrupted' \
                             (prior process exited uncleanly)"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to promote orphaned sync_runs rows");
                    }
                }

                // Surface enum_in_progress:<zone> markers left by a prior
                // interrupted full enumeration so the operator understands
                // why the next full sync will re-enumerate from scratch.
                match db.list_interrupted_enumerations().await {
                    Ok(zones) if !zones.is_empty() => {
                        tracing::warn!(
                            zones = zones.join(","),
                            "Prior full enumeration was interrupted; next sync will re-enumerate \
                             the affected zones from offset 0"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(
                            error = %e,
                            "Failed to list interrupted enumerations"
                        );
                    }
                }

                // For retry-failed, reset failed assets to pending
                if is_retry_failed {
                    match db.reset_failed().await {
                        Ok(0) => {
                            tracing::info!("No failed assets to retry");
                            return Ok(());
                        }
                        Ok(count) => {
                            tracing::debug!(count, "Reset failed assets to pending");
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

    // First-sync notice: tell users on the `PrimarySync` default about any
    // shared libraries they could be syncing. Runs once per data dir,
    // gated by state DB metadata.
    maybe_notify_shared_libraries(&config.library, &mut photos_service, state_db.as_ref()).await;

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
                tracing::debug!("Cleared stored sync tokens");
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
    // One shared limiter per sync run so the configured cap applies to
    // aggregate throughput across every concurrent download.
    let bandwidth_limiter = config
        .bandwidth_limit
        .map(download::limiter::BandwidthLimiter::new);
    if let Some(limiter) = &bandwidth_limiter {
        tracing::info!(
            bytes_per_sec = limiter.bytes_per_sec(),
            "Bandwidth limit enabled"
        );
    }
    let build_download_config = |sync_mode: download::SyncMode,
                                 exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
                                 asset_groupings: Arc<download::AssetGroupings>|
     -> Arc<download::DownloadConfig> {
        Arc::new(download::DownloadConfig {
            directory: Arc::from(config.directory.as_path()),
            folder_structure: config.folder_structure.clone(),
            size: config.size.into(),
            skip_videos: config.skip_videos,
            skip_photos: config.skip_photos,
            skip_created_before,
            skip_created_after,
            #[cfg(feature = "xmp")]
            set_exif_datetime: config.set_exif_datetime,
            #[cfg(feature = "xmp")]
            set_exif_rating: config.set_exif_rating,
            #[cfg(feature = "xmp")]
            set_exif_gps: config.set_exif_gps,
            #[cfg(feature = "xmp")]
            set_exif_description: config.set_exif_description,
            #[cfg(feature = "xmp")]
            embed_xmp: config.embed_xmp,
            #[cfg(feature = "xmp")]
            xmp_sidecar: config.xmp_sidecar,
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
            filename_exclude: Arc::from(config.filename_exclude.clone()),
            temp_suffix: Arc::from(config.temp_suffix.as_str()),
            state_db: state_db.clone(),
            retry_only: is_retry_failed,
            max_download_attempts,
            sync_mode,
            album_name: None,
            exclude_asset_ids,
            asset_groupings,
            bandwidth_limiter: bandwidth_limiter.clone(),
        })
    };

    let shutdown_token = shutdown::install_signal_handler(sd_notifier)?;

    let is_watch_mode = config.watch_with_interval.is_some();
    let mut reauth_attempts = 0u32;
    // Sum of per-cycle failed_counts across the lifetime of this process.
    // Surfaced at exit so watch-mode daemons don't mask earlier-cycle
    // failures behind a clean final cycle.
    let mut cumulative_failed_count = 0usize;

    let mut library_states: Vec<LibraryState> = Vec::with_capacity(libraries.len());
    for library in &libraries {
        let zone_name = library.zone_name().to_string();
        let sync_token_key = format!("sync_token:{zone_name}");
        let plan = resolve_albums(
            library,
            &config.albums,
            &config.exclude_albums,
            &config.folder_structure,
        )
        .await?;
        library_states.push(LibraryState {
            library: library.clone(),
            zone_name,
            sync_token_key,
            plan,
        });
    }
    sd_notifier.notify_ready();

    // Spawn the HTTP server (/healthz + /metrics) only in watch mode.
    // A one-shot sync exits before anything could scrape /healthz, so there
    // is no value in binding the port. In watch mode, flag /healthz as stale
    // after two missed intervals so a single slow cycle doesn't flip to 503
    // but a stuck main loop does.
    // Binds synchronously so a misconfigured port fails at startup.
    let staleness_threshold = config
        .watch_with_interval
        .map(|secs| chrono::Duration::seconds((secs * 2) as i64));
    let (metrics_handle, metrics_task) = if config.watch_with_interval.is_some() {
        let (h, t, _addr) = crate::metrics::spawn_server(
            config.http_bind,
            config.http_port,
            shutdown_token.clone(),
            staleness_threshold,
        )?;
        (Some(h), Some(t))
    } else {
        (None, None)
    };

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
        let skip_cycle = match library_states.as_slice() {
            [only] if is_watch_mode && !config.no_incremental => {
                check_changes_database(&state_db, only, &mut photos_service).await
            }
            _ => false,
        };

        if skip_cycle {
            // Skipped cycle (no changes detected) -- still update health so
            // Docker HEALTHCHECK doesn't mark the container unhealthy after
            // the 2-hour staleness window when no new photos are uploaded.
            health.record_success();
            health.write(&config.cookie_directory);
            // Refresh health gauges only -- do not reset cycle_duration_seconds.
            if let Some(ref handle) = metrics_handle {
                handle.update_health_only(&health).await;
            }
        } else {
            sd_notifier.notify_status("Syncing...");
            sd_notifier.notify_watchdog();
            notifier.notify(
                notifications::Event::SyncStarted,
                "Sync cycle starting",
                &config.username,
                None,
            );

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

            // Update Prometheus metrics if the server is running.
            if let Some(ref handle) = metrics_handle {
                if cycle_result.session_expired {
                    handle.record_session_expiration();
                }
                handle.update(&cycle_result.stats, &health).await;

                // Update DB-backed gauges from the state database.
                if let Some(ref db) = state_db {
                    match db.get_summary().await {
                        Ok(summary) => {
                            handle.update_db_stats(&summary, cycle_result.stats.assets_seen);
                        }
                        Err(e) => {
                            handle.record_db_summary_failure();
                            tracing::warn!(error = %e, "Failed to fetch DB summary for metrics; skipping DB gauge update");
                        }
                    }
                }
            }

            // Write JSON report if configured
            if let Some(report_path) = &config.report_json {
                let status = crate::report::sync_status_str(
                    cycle_result.session_expired,
                    cycle_result.stats.interrupted,
                    cycle_result.failed_count,
                );
                // Populate failed_assets from the state DB so the report
                // reflects the final committed set, not mid-sync churn.
                // get_failed_sample pushes the LIMIT into SQL so an account
                // with thousands of failures doesn't load every row here.
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "FAILED_ASSETS_CAP is a small compile-time constant well under u32::MAX"
                )]
                let cap_u32 = crate::report::FAILED_ASSETS_CAP as u32;
                let (failed_assets, failed_assets_truncated) = match state_db.as_ref() {
                    Some(db) => match db.get_failed_sample(cap_u32).await {
                        Ok((records, total)) => {
                            #[allow(
                                clippy::cast_possible_truncation,
                                reason = "failed-asset totals are persisted counts of per-sync failures, comfortably below usize::MAX on 64-bit"
                            )]
                            let total_usize = total as usize;
                            let truncated =
                                total_usize.saturating_sub(crate::report::FAILED_ASSETS_CAP);
                            let entries: Vec<_> = records
                                .iter()
                                .map(crate::report::FailedAssetEntry::from_record)
                                .collect();
                            (entries, truncated)
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "Failed to load failed_assets for sync_report.json"
                            );
                            (Vec::new(), 0)
                        }
                    },
                    None => (Vec::new(), 0),
                };
                let report = crate::report::SyncReport {
                    version: "1",
                    kei_version: env!("CARGO_PKG_VERSION"),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    status: status.to_string(),
                    options: crate::report::RunOptions::from_config(&config),
                    stats: cycle_result.stats.clone(),
                    failed_assets,
                    failed_assets_truncated,
                };
                if let Err(e) = crate::report::write_report(report_path, &report) {
                    tracing::warn!(error = %e, path = %report_path.display(), "Failed to write JSON report");
                }
            }

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
                            None,
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
                            None,
                        );
                        return Err(e);
                    }
                }
            } else if cycle_result.failed_count > 0 {
                let data = notifications::SyncNotificationData::from(&cycle_result.stats);
                notifier.notify(
                    notifications::Event::SyncFailed,
                    &format!("{} downloads failed", cycle_result.failed_count),
                    &config.username,
                    Some(&data),
                );
                cumulative_failed_count =
                    cumulative_failed_count.saturating_add(cycle_result.failed_count);
                if is_watch_mode {
                    tracing::warn!(
                        failed_count = cycle_result.failed_count,
                        cumulative = cumulative_failed_count,
                        "Some downloads failed this cycle, will retry next cycle"
                    );
                } else {
                    return Err(PartialSyncError(cycle_result.failed_count).into());
                }
            } else {
                reauth_attempts = 0;
                let data = notifications::SyncNotificationData::from(&cycle_result.stats);
                notifier.notify(
                    notifications::Event::SyncComplete,
                    "Sync completed successfully",
                    &config.username,
                    Some(&data),
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
                match resolve_albums(
                    &lib_state.library,
                    &config.albums,
                    &config.exclude_albums,
                    &config.folder_structure,
                )
                .await
                {
                    Ok(refreshed) => {
                        lib_state.plan = refreshed;
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

    // Signal the metrics server to shut down (idempotent if SIGINT already
    // fired) and await its graceful drain so the binary doesn't exit while
    // an in-flight /metrics scrape is still flushing.
    if let Some(task) = metrics_task {
        shutdown_token.cancel();
        if let Err(e) = task.await {
            tracing::warn!(error = %e, "metrics server task panicked");
        }
    }

    // Exit non-zero if any cycle in this watch session had failures, not
    // just the last one. A single successful final cycle must not mask a
    // multi-cycle failure backlog in Docker / systemd exit-code signalling.
    if cumulative_failed_count > 0 {
        Err(PartialSyncError(cumulative_failed_count).into())
    } else {
        Ok(())
    }
}

/// Outcome of a single sync cycle across all libraries.
struct CycleResult {
    failed_count: usize,
    session_expired: bool,
    stats: download::SyncStats,
}

/// Re-authenticate via SRP after a session-error signature from CloudKit.
///
/// Drops any live session + service (releasing the file lock), strips routing
/// state from the session file so `auth::authenticate` is forced onto SRP,
/// then runs authentication — handling the 2FA-required case by notifying and
/// waiting for `kei login submit-code`.
async fn reauth_with_srp(
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
    notifier: &Notifier,
    live: Option<(auth::SharedSession, crate::icloud::photos::PhotosService)>,
) -> anyhow::Result<auth::AuthResult> {
    if let Some((ss, ps)) = live {
        ss.read().await.release_lock()?;
        drop(ps);
        drop(ss);
    }
    let session_file = auth::session_file_path(&config.cookie_directory, &config.username);
    auth::strip_session_routing_state(&session_file).await;

    match auth::authenticate(
        &config.cookie_directory,
        &config.username,
        password_provider,
        config.domain.as_str(),
        None,
        None,
        None,
    )
    .await
    {
        Ok(result) => Ok(result),
        Err(e)
            if e.downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_two_factor_required) =>
        {
            let msg = format!(
                "2FA required for {u}. Run: kei login get-code",
                u = config.username
            );
            tracing::warn!(message = %msg, "2FA required");
            notifier.notify(
                notifications::Event::TwoFaRequired,
                &msg,
                &config.username,
                None,
            );
            wait_and_retry_2fa(&config.cookie_directory, &config.username, || {
                auth::authenticate(
                    &config.cookie_directory,
                    &config.username,
                    password_provider,
                    config.domain.as_str(),
                    None,
                    None,
                    None,
                )
            })
            .await
        }
        Err(e) => Err(e),
    }
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
        Arc<download::AssetGroupings>,
    ) -> Arc<download::DownloadConfig>,
    shared_session: &auth::SharedSession,
    shutdown_token: &CancellationToken,
) -> anyhow::Result<CycleResult> {
    let mut cycle_failed_count = 0usize;
    let mut cycle_session_expired = false;
    let mut cycle_stats = download::SyncStats::default();

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
                                tracing::debug!(cleared = n, "Cleared stale sync tokens");
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

        // Skip the DB scan entirely when nothing downstream will read it.
        #[cfg(feature = "xmp")]
        let asset_groupings = if config.embed_xmp || config.xmp_sidecar {
            preload_asset_groupings(state_db).await
        } else {
            Arc::new(download::AssetGroupings::default())
        };
        #[cfg(not(feature = "xmp"))]
        let asset_groupings = Arc::new(download::AssetGroupings::default());
        // Each pass carries its own exclude-asset-ids, so the config built
        // here starts with an empty set; download_photos_with_sync derives
        // per-pass configs internally via `with_exclude_ids`.
        let download_config = build_download_config(
            sync_mode,
            Arc::new(rustc_hash::FxHashSet::default()),
            asset_groupings,
        );
        let download_client = shared_session.read().await.download_client();
        let sync_result = download::download_photos_with_sync(
            &download_client,
            &lib_state.plan.passes,
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
                        tracing::debug!(zone = %lib_state.zone_name, "Stored sync token for next incremental sync");
                    }
                }
            }
        } else if sync_result.sync_token.is_some() {
            tracing::info!(
                zone = %lib_state.zone_name,
                "Sync token NOT advanced (incomplete sync -- will replay changes next cycle)"
            );
        }

        // Accumulate stats across libraries
        cycle_stats.assets_seen += sync_result.stats.assets_seen;
        cycle_stats.downloaded += sync_result.stats.downloaded;
        cycle_stats.failed += sync_result.stats.failed;
        cycle_stats.bytes_downloaded += sync_result.stats.bytes_downloaded;
        cycle_stats.disk_bytes_written += sync_result.stats.disk_bytes_written;
        cycle_stats.exif_failures += sync_result.stats.exif_failures;
        cycle_stats.state_write_failures += sync_result.stats.state_write_failures;
        cycle_stats.enumeration_errors += sync_result.stats.enumeration_errors;
        cycle_stats.elapsed_secs += sync_result.stats.elapsed_secs;
        cycle_stats.interrupted = cycle_stats.interrupted || sync_result.stats.interrupted;
        cycle_stats.skipped.by_state += sync_result.stats.skipped.by_state;
        cycle_stats.skipped.on_disk += sync_result.stats.skipped.on_disk;
        cycle_stats.skipped.by_media_type += sync_result.stats.skipped.by_media_type;
        cycle_stats.skipped.by_date_range += sync_result.stats.skipped.by_date_range;
        cycle_stats.skipped.by_live_photo += sync_result.stats.skipped.by_live_photo;
        cycle_stats.skipped.by_filename += sync_result.stats.skipped.by_filename;
        cycle_stats.skipped.by_excluded_album += sync_result.stats.skipped.by_excluded_album;
        cycle_stats.skipped.ampm_variant += sync_result.stats.skipped.ampm_variant;
        cycle_stats.skipped.duplicates += sync_result.stats.skipped.duplicates;
        cycle_stats.skipped.retry_exhausted += sync_result.stats.skipped.retry_exhausted;
        cycle_stats.skipped.retry_only += sync_result.stats.skipped.retry_only;

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
        stats: cycle_stats,
    })
}

/// Check `changes/database` to determine if this watch cycle can be skipped.
///
/// Returns `true` when no zones report changes and `moreComing` is false.
/// Bulk-load `asset_albums` + `asset_people` into an in-memory index so the
/// filter phase can enrich payloads without per-asset DB hits.
#[cfg(feature = "xmp")]
async fn preload_asset_groupings(
    state_db: &Option<Arc<dyn state::StateDb>>,
) -> Arc<download::AssetGroupings> {
    let Some(db) = state_db else {
        return Arc::new(download::AssetGroupings::default());
    };
    let albums = db.get_all_asset_albums().await;
    let people = db.get_all_asset_people().await;
    let mut groupings = download::AssetGroupings::default();
    match albums {
        Ok(rows) => {
            for (asset_id, album) in rows {
                groupings.albums.entry(asset_id).or_default().push(album);
            }
        }
        Err(e) => tracing::warn!(error = %e, "Failed to preload asset_albums"),
    }
    match people {
        Ok(rows) => {
            for (asset_id, person) in rows {
                groupings.people.entry(asset_id).or_default().push(person);
            }
        }
        Err(e) => tracing::warn!(error = %e, "Failed to preload asset_people"),
    }
    Arc::new(groupings)
}

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
            tracing::debug!(
                "Incremental sync disabled via --no-incremental, performing full enumeration"
            );
        }
        if is_retry_failed {
            tracing::debug!(
                "Retry-failed requires full enumeration to find previously-failed assets"
            );
        }
        download::SyncMode::Full
    } else if let Some(db) = state_db {
        match db.get_metadata(sync_token_key).await {
            Ok(Some(ref token)) if !token.is_empty() => {
                tracing::debug!(zone = %zone_name, "Stored sync token found, using incremental sync");
                download::SyncMode::Incremental {
                    zone_sync_token: token.clone(),
                }
            }
            Ok(_) => {
                tracing::debug!(zone = %zone_name, "No sync token found, performing full enumeration");
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
async fn reacquire_session(
    shared_session: &auth::SharedSession,
    config: &config::Config,
    password_provider: &crate::password::PasswordProvider,
) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn primary() -> config::LibrarySelection {
        config::LibrarySelection::Single("PrimarySync".to_string())
    }

    #[test]
    fn notice_suppressed_when_already_shown() {
        // The marker overrides everything: even a user with 5 shared libraries
        // and the default selection gets no notice on re-entry.
        assert!(should_notify_shared_libraries(&primary(), 5, true).is_none());
    }

    #[test]
    fn notice_suppressed_when_no_shared_libraries() {
        assert!(should_notify_shared_libraries(&primary(), 0, false).is_none());
    }

    #[test]
    fn notice_suppressed_when_user_picked_all() {
        // Anyone who explicitly set `--library all` has already opted in;
        // nothing to tell them.
        assert!(should_notify_shared_libraries(&config::LibrarySelection::All, 3, false).is_none());
    }

    #[test]
    fn notice_suppressed_when_user_picked_shared_zone_explicitly() {
        // A user who typed out `--library SharedSync-ABCD1234` has also made
        // a choice; don't second-guess them.
        let explicit = config::LibrarySelection::Single("SharedSync-ABCD1234".to_string());
        assert!(should_notify_shared_libraries(&explicit, 3, false).is_none());
    }

    #[test]
    fn notice_fires_with_singular_wording_for_one_library() {
        let msg = should_notify_shared_libraries(&primary(), 1, false).unwrap();
        assert!(
            msg.contains("1 iCloud shared library"),
            "singular 'library' wording expected; got: {msg}"
        );
        assert!(
            msg.contains("is being synced"),
            "singular verb 'is' expected; got: {msg}"
        );
        // The guidance is what the notice is for - it must name the config
        // key, the CLI flag, and the discovery subcommand.
        assert!(
            msg.contains("[filters] library = \"all\""),
            "TOML guidance missing: {msg}"
        );
        assert!(msg.contains("--library all"), "CLI guidance missing: {msg}");
        assert!(
            msg.contains("kei list libraries"),
            "discovery guidance missing: {msg}"
        );
    }

    #[test]
    fn notice_fires_with_plural_wording_for_multiple_libraries() {
        let msg = should_notify_shared_libraries(&primary(), 3, false).unwrap();
        assert!(
            msg.contains("3 iCloud shared libraries"),
            "plural 'libraries' wording expected; got: {msg}"
        );
        assert!(
            msg.contains("are being synced"),
            "plural verb 'are' expected; got: {msg}"
        );
    }

    #[test]
    fn notice_suppressed_when_both_user_opted_out_and_already_notified() {
        // Belt-and-braces: every suppression condition stacks correctly.
        assert!(should_notify_shared_libraries(&config::LibrarySelection::All, 0, true).is_none());
    }

    // ── session_error_signature (CF-10 regression coverage) ─────────
    //
    // The run_sync reauth retry branch (lines 370-408) keys on whether
    // an error coming back from init_photos_service or resolve_libraries
    // is a "session error". Misclassifying would either (a) retry SRP on
    // a non-session failure (wasting an Apple rate-limit slot) or
    // (b) fail to retry on a real 401/421 (visible to the operator as
    // an immediate Docker restart). Pin down every variant here so a
    // refactor of ICloudError can't silently regress the match arm.

    #[test]
    fn session_error_signature_true_for_cloudkit_401_403() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 401 }.into();
        assert!(session_error_signature(&e), "401 must trigger reauth");

        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 403 }.into();
        assert!(session_error_signature(&e), "403 must trigger reauth");
    }

    #[test]
    fn session_error_signature_true_for_cloudkit_421() {
        let e: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(
            session_error_signature(&e),
            "persistent 421 must trigger reauth (stale routing state needs fresh SRP)"
        );
    }

    #[test]
    fn session_error_signature_false_for_service_not_activated() {
        // ADP / ZONE_NOT_FOUND is a permanent failure, not a session issue.
        // Reauth would burn an Apple rate-limit slot for nothing.
        let e: anyhow::Error = crate::icloud::error::ICloudError::ServiceNotActivated {
            code: "ADP".into(),
            reason: "Advanced Data Protection".into(),
        }
        .into();
        assert!(!session_error_signature(&e));
    }

    #[test]
    fn session_error_signature_false_for_connection_and_io() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::Connection("DNS failure".into()).into();
        assert!(!session_error_signature(&e));

        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x");
        let e: anyhow::Error = crate::icloud::error::ICloudError::from(io).into();
        assert!(!session_error_signature(&e));
    }

    #[test]
    fn session_error_signature_false_for_non_icloud_error() {
        // Any other anyhow error (config parsing, state DB, etc.) must not
        // be classified as a session error — that would trigger an
        // inappropriate SRP cycle.
        let e = anyhow::anyhow!("unrelated top-level error");
        assert!(!session_error_signature(&e));
    }

    #[test]
    fn session_error_signature_peers_through_context() {
        // Real error chains are wrapped in .context() before hitting the
        // retry branch. The classifier downcasts on the root cause, which
        // anyhow exposes as downcast_ref — wrap here to pin the contract.
        let root = crate::icloud::error::ICloudError::SessionExpired { status: 401 };
        let e = anyhow::Error::from(root).context("while initializing photos service");
        assert!(
            session_error_signature(&e),
            "classifier must downcast through context wrappers"
        );
    }
}
