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
use crate::{
    available_disk_space, check_min_disk_space, make_password_provider, PartialSyncError,
    PidFileGuard,
};

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
fn is_session_error(err: &anyhow::Error) -> bool {
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
    tracing::warn!(message = %msg, "Shared library notice");

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

    // Abort if available disk space is too low. CG-20: see
    // `check_min_disk_space` for the pure inner check.
    if let Some(avail) = available_disk_space(&config.directory) {
        check_min_disk_space(avail, &config.directory)?;
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
                tracing::warn!(reason = %reason, "Skipping save of password to credential store");
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
            Err(e) if !retried_after_session_error && is_session_error(&e) => {
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
            Err(e) if !retried_after_session_error && is_session_error(&e) => {
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
                            tracing::warn!(error = %e, "Failed to reset failed assets");
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
    // Promote the String / Vec / PathBuf config fields to their Arc
    // counterparts once, outside the per-library closure. Otherwise the
    // CF-12 Arc-sharing win is half-defeated: each build_download_config
    // call would re-allocate directory / filename_exclude / temp_suffix
    // from scratch instead of refcount-bumping.
    let cfg_directory: Arc<std::path::Path> = Arc::from(config.directory.as_path());
    let cfg_filename_exclude: Arc<[glob::Pattern]> = Arc::from(config.filename_exclude.clone());
    let cfg_temp_suffix: Arc<str> = Arc::from(config.temp_suffix.as_str());

    let build_download_config = |sync_mode: download::SyncMode,
                                 exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
                                 asset_groupings: Arc<download::AssetGroupings>|
     -> Arc<download::DownloadConfig> {
        Arc::new(download::DownloadConfig {
            directory: Arc::clone(&cfg_directory),
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
            filename_exclude: Arc::clone(&cfg_filename_exclude),
            temp_suffix: Arc::clone(&cfg_temp_suffix),
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
                if let Err(e) = crate::report::write_report(report_path, &report).await {
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
                        if !should_wait_for_2fa(is_watch_mode, &e) {
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
#[derive(Debug)]
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

/// Decide whether the per-zone `sync_token` should be persisted to the state
/// DB after a download pass.
///
/// The contract is "advance only on full success and not in dry-run":
/// - On `PartialFailure`, a stored token would skip the failed assets on the
///   next incremental sync (they'd never appear in the delta again -- silent
///   data loss). CG-9.
/// - On `SessionExpired`, the cycle aborts mid-stream; the token may be
///   stale or only reflect a subset of the work.
/// - In `--dry-run`, we promise to make no DB writes that survive the run
///   (apart from the `sync_runs` ledger). Advancing the token here would
///   silently break the next real sync. CG-21.
///
/// The returned bool is the gate: callers still check that `sync_token` is
/// `Some(_)` and that a state DB is configured before persisting.
pub(crate) fn should_store_sync_token(outcome: &download::DownloadOutcome, dry_run: bool) -> bool {
    matches!(outcome, download::DownloadOutcome::Success) && !dry_run
}

/// Decide whether the reauth path inside the sync loop should block on a
/// 2FA prompt or surface the error to the caller.
///
/// In **watch mode** a 2FA-required error is recoverable: the loop notifies
/// the user, parks `wait_and_retry_2fa`, and resumes once a code arrives.
///
/// In **one-shot mode** there is nothing to wait on -- the caller (a CI run,
/// a cron, the systemd unit's first start) needs the error so it can exit
/// non-zero and the operator can run `kei login get-code`. CG-19.
///
/// Note that the **entry-point** auth path (`run_sync`'s initial
/// `auth::authenticate` call) intentionally does NOT use this predicate --
/// it always parks on `wait_and_retry_2fa` because the user is presumed
/// present at a terminal during the initial command. This helper exists
/// because the *reauth* branch fires mid-cycle, by which point a one-shot
/// caller has long since detached and there is no operator to type a code.
pub(crate) fn should_wait_for_2fa(is_watch_mode: bool, err: &anyhow::Error) -> bool {
    is_watch_mode
        && err
            .downcast_ref::<auth::error::AuthError>()
            .is_some_and(auth::error::AuthError::is_two_factor_required)
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
        let download_client = shared_session.read().await.download_client().clone();
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
        let should_store_token = should_store_sync_token(&sync_result.outcome, config.dry_run);
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

        // Accumulate stats across libraries. CG-15.
        cycle_stats.accumulate(&sync_result.stats);

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

    // The run_sync reauth retry branch keys on whether an error from
    // init_photos_service or resolve_libraries is a session error.
    // Misclassifying either retries SRP on a non-session failure
    // (burning an Apple rate-limit slot) or fails to retry on a real
    // 401/421 (visible as an immediate Docker restart). Pin every
    // variant so a future ICloudError refactor can't silently regress.

    #[test]
    fn is_session_error_true_for_cloudkit_401_403() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 401 }.into();
        assert!(is_session_error(&e), "401 must trigger reauth");

        let e: anyhow::Error =
            crate::icloud::error::ICloudError::SessionExpired { status: 403 }.into();
        assert!(is_session_error(&e), "403 must trigger reauth");
    }

    #[test]
    fn is_session_error_true_for_cloudkit_421() {
        let e: anyhow::Error = crate::icloud::error::ICloudError::MisdirectedRequest.into();
        assert!(
            is_session_error(&e),
            "persistent 421 must trigger reauth (stale routing state needs fresh SRP)"
        );
    }

    #[test]
    fn is_session_error_false_for_service_not_activated() {
        // ADP / ZONE_NOT_FOUND is a permanent failure, not a session issue.
        // Reauth would burn an Apple rate-limit slot for nothing.
        let e: anyhow::Error = crate::icloud::error::ICloudError::ServiceNotActivated {
            code: "ADP".into(),
            reason: "Advanced Data Protection".into(),
        }
        .into();
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_false_for_connection_and_io() {
        let e: anyhow::Error =
            crate::icloud::error::ICloudError::Connection("DNS failure".into()).into();
        assert!(!is_session_error(&e));

        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "x");
        let e: anyhow::Error = crate::icloud::error::ICloudError::from(io).into();
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_false_for_non_icloud_error() {
        // Any other anyhow error (config parsing, state DB, etc.) must not
        // be classified as a session error — that would trigger an
        // inappropriate SRP cycle.
        let e = anyhow::anyhow!("unrelated top-level error");
        assert!(!is_session_error(&e));
    }

    #[test]
    fn is_session_error_peers_through_context() {
        // Real error chains are wrapped in .context() before hitting the
        // retry branch. The classifier downcasts on the root cause, which
        // anyhow exposes as downcast_ref — wrap here to pin the contract.
        let root = crate::icloud::error::ICloudError::SessionExpired { status: 401 };
        let e = anyhow::Error::from(root).context("while initializing photos service");
        assert!(
            is_session_error(&e),
            "classifier must downcast through context wrappers"
        );
    }

    /// Comprehensive classification table: every `ICloudError` variant plus
    /// a generic anyhow error. Prevents silent regressions from future enum
    /// additions — adding a new variant requires updating this table.
    #[test]
    fn is_session_error_classification_table() {
        use crate::icloud::error::ICloudError;

        // Variants that SHOULD trigger re-auth (session errors). Each entry
        // is a (label, anyhow::Error) pair so downcast_ref inside
        // is_session_error works correctly without needing Clone.
        let session_errors: Vec<(&str, anyhow::Error)> = vec![
            (
                "SessionExpired-401",
                ICloudError::SessionExpired { status: 401 }.into(),
            ),
            (
                "SessionExpired-403",
                ICloudError::SessionExpired { status: 403 }.into(),
            ),
            ("MisdirectedRequest", ICloudError::MisdirectedRequest.into()),
        ];
        for (label, e) in session_errors {
            assert!(
                is_session_error(&e),
                "expected {label} to be a session error"
            );
        }

        // Variants that must NOT trigger re-auth (non-session errors).
        let non_session: Vec<(&str, anyhow::Error)> = vec![
            (
                "Connection",
                ICloudError::Connection("DNS timeout".into()).into(),
            ),
            (
                "ServiceNotActivated",
                ICloudError::ServiceNotActivated {
                    code: "ZONE_NOT_FOUND".into(),
                    reason: "ADP".into(),
                }
                .into(),
            ),
            (
                "Io",
                ICloudError::from(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "denied",
                ))
                .into(),
            ),
            (
                "Json",
                ICloudError::from(
                    serde_json::from_str::<serde_json::Value>("not json").unwrap_err(),
                )
                .into(),
            ),
            ("non-ICloudError", anyhow::anyhow!("config parse error")),
        ];
        for (label, e) in non_session {
            assert!(
                !is_session_error(&e),
                "expected {label} to NOT be a session error"
            );
        }
    }

    // CG-18: classifier sees through `Box<dyn Error + Send + Sync>` wrappers.
    //
    // anyhow lets callers wrap raw boxed errors via `anyhow::Error::from`
    // when adapting third-party error returns. The downcast walk used by
    // `is_session_error` must conclude "not a session error" for any error
    // chain that was never an `ICloudError` — otherwise a future refactor
    // through a boxed-error adapter could silently flip every config /
    // network / state error into "burn an Apple SRP slot".
    #[test]
    fn is_session_error_through_boxed_error_returns_false() {
        // A boxed error of a foreign type wrapped via anyhow::Error::from.
        let boxed: Box<dyn std::error::Error + Send + Sync> =
            Box::new(std::io::Error::other("foreign"));
        let e: anyhow::Error = anyhow::Error::from_boxed(boxed);
        assert!(
            !is_session_error(&e),
            "boxed-error wrapper must not be classified as a session error"
        );

        // Also: anyhow::Error::msg(string) must be non-session — this is
        // the most common path through our error pipeline (a `bail!` from
        // a non-icloud module).
        let plain = anyhow::anyhow!("config parse failed at line 7");
        assert!(
            !is_session_error(&plain),
            "free-form anyhow::Error must not be classified as a session error"
        );
    }

    // ── determine_sync_mode ──────────────────────────────────────────
    //
    // Sync-mode decision is the gatekeeper for the kei "user data is sacred"
    // invariant: pick Full vs Incremental wrong and either (a) re-download
    // the world (waste) or (b) skip previously-failed assets (silent loss).
    // None of the four critical branches had a direct unit test before.

    fn make_state_db() -> Arc<dyn state::StateDb> {
        Arc::new(state::SqliteStateDb::open_in_memory().expect("open in-memory state DB"))
    }

    /// CG-1: `is_retry_failed=true` MUST force `SyncMode::Full` even when a
    /// sync token is stored. A regression that picked Incremental during
    /// retry-failed would silently skip the previously-failed assets the
    /// user explicitly asked to retry — silent data loss.
    #[tokio::test]
    async fn determine_sync_mode_retry_failed_with_token_returns_full() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        // Pre-populate a stored token so we can verify it is ignored.
        db.set_metadata(sync_token_key, "stored-token-abc")
            .await
            .expect("set token");

        let mode = determine_sync_mode(
            true,  // is_retry_failed
            false, // no_incremental
            1,
            &Some(Arc::clone(&db)),
            sync_token_key,
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "retry-failed must force Full, got {mode:?}"
        );
    }

    /// CG-2: `--no-incremental` MUST force `SyncMode::Full`, ignoring any
    /// stored token. The flag exists so a user can deliberately re-enumerate
    /// (e.g. after a known incremental drift); silently downgrading would
    /// betray that contract.
    #[tokio::test]
    async fn determine_sync_mode_no_incremental_overrides_stored_token() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        db.set_metadata(sync_token_key, "stored-token-xyz")
            .await
            .expect("set token");

        let mode = determine_sync_mode(
            false, // is_retry_failed
            true,  // no_incremental
            1,
            &Some(Arc::clone(&db)),
            sync_token_key,
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "--no-incremental must force Full, got {mode:?}"
        );
    }

    /// CG-3: an empty stored token must fall back to Full. Production
    /// guards on `!token.is_empty()`; if a refactor flipped that check the
    /// caller would request `changes/zone` with empty token and silently
    /// drop pending events.
    #[tokio::test]
    async fn determine_sync_mode_empty_stored_token_falls_back_to_full() {
        let db = make_state_db();
        let sync_token_key = "sync_token:PrimarySync";
        // Empty-string token is the malformed case — should be ignored.
        db.set_metadata(sync_token_key, "")
            .await
            .expect("set empty token");

        let mode = determine_sync_mode(
            false,
            false,
            1,
            &Some(Arc::clone(&db)),
            sync_token_key,
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "empty stored token must yield Full, got {mode:?}"
        );

        // And a present, non-empty token must trigger Incremental — pin the
        // happy path here too so the empty-string check can't be dropped
        // without breaking both assertions.
        db.set_metadata(sync_token_key, "real-token")
            .await
            .expect("set real token");
        let mode = determine_sync_mode(
            false,
            false,
            1,
            &Some(Arc::clone(&db)),
            sync_token_key,
            "PrimarySync",
        )
        .await;
        assert!(
            matches!(mode, download::SyncMode::Incremental { ref zone_sync_token } if zone_sync_token == "real-token"),
            "non-empty token must yield Incremental with that token, got {mode:?}"
        );
    }

    /// CG-4: when the state DB read fails, fall back to Full rather than
    /// propagating. The watch loop must keep going even if sqlite hiccups —
    /// silently biasing toward Incremental on errors would mask data loss.
    ///
    /// We reach the failure surface by closing the underlying connection
    /// out from under a real `SqliteStateDb`. Doing this against a plain
    /// fail-everything stub would require implementing every method in the
    /// (large) `StateDb` trait; instead we use the test-only failing impl
    /// the pipeline tests already maintain. See the inline `FailingDb`.
    #[tokio::test]
    async fn determine_sync_mode_state_db_error_falls_back_to_full() {
        use std::collections::{HashMap, HashSet};
        use std::path::PathBuf;

        // Minimal failing impl: only `get_metadata` is reachable from
        // `determine_sync_mode`; every other method is unimplemented so
        // any silent reroute lights up immediately.
        struct FailingDb;

        #[async_trait::async_trait]
        impl state::StateDb for FailingDb {
            #[cfg(test)]
            async fn should_download(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &std::path::Path,
            ) -> Result<bool, state::error::StateError> {
                unimplemented!()
            }
            async fn upsert_seen(
                &self,
                _: &state::types::AssetRecord,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_downloaded(
                &self,
                _: &str,
                _: &str,
                _: &std::path::Path,
                _: &str,
                _: Option<&str>,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn import_adopt(
                &self,
                _: &state::types::AssetRecord,
                _: &std::path::Path,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_failed(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_failed(
                &self,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_failed_sample(
                &self,
                _: u32,
            ) -> Result<(Vec<state::types::AssetRecord>, u64), state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_pending(
                &self,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_summary(
                &self,
            ) -> Result<state::types::SyncSummary, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_page(
                &self,
                _: u64,
                _: u32,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn start_sync_run(&self) -> Result<i64, state::error::StateError> {
                unimplemented!()
            }
            async fn complete_sync_run(
                &self,
                _: i64,
                _: &state::types::SyncRunStats,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn promote_orphaned_sync_runs(&self) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn begin_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn end_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn list_interrupted_enumerations(
                &self,
            ) -> Result<Vec<String>, state::error::StateError> {
                unimplemented!()
            }
            async fn reset_failed(&self) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), state::error::StateError> {
                unimplemented!()
            }
            async fn promote_pending_to_failed(
                &self,
                _: i64,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_ids(
                &self,
            ) -> Result<HashSet<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_all_known_ids(&self) -> Result<HashSet<String>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_checksums(
                &self,
            ) -> Result<HashMap<(String, String), String>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_attempt_counts(
                &self,
            ) -> Result<HashMap<String, u32>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_metadata(
                &self,
                _: &str,
            ) -> Result<Option<String>, state::error::StateError> {
                Err(state::error::StateError::LockPoisoned("simulated".into()))
            }
            async fn set_metadata(&self, _: &str, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn delete_metadata_by_prefix(
                &self,
                _: &str,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn touch_last_seen_many(
                &self,
                _: &[&str],
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn sample_downloaded_paths(
                &self,
                _: usize,
            ) -> Result<Vec<PathBuf>, state::error::StateError> {
                unimplemented!()
            }
            async fn add_asset_album(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_all_asset_albums(
                &self,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_all_asset_people(
                &self,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn mark_soft_deleted(
                &self,
                _: &str,
                _: Option<chrono::DateTime<chrono::Utc>>,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_hidden_at_source(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn record_metadata_write_failure(
                &self,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_metadata_hashes(
                &self,
            ) -> Result<HashMap<(String, String), String>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_metadata_retry_markers(
                &self,
            ) -> Result<HashSet<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_pending_metadata_rewrites(
                &self,
                _: usize,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn update_metadata_hash(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn clear_metadata_write_failure(
                &self,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn has_downloaded_without_metadata_hash(
                &self,
            ) -> Result<bool, state::error::StateError> {
                unimplemented!()
            }
        }

        let db: Arc<dyn state::StateDb> = Arc::new(FailingDb);

        let mode = determine_sync_mode(
            false,
            false,
            1,
            &Some(db),
            "sync_token:PrimarySync",
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "DB read error must fall back to Full, got {mode:?}"
        );
    }

    /// Sanity: `state_db = None` (e.g. legacy run with no state path) must
    /// still produce Full. The match-arm exists in production today; pin
    /// it so a future refactor that drops the `else` branch tells us.
    #[tokio::test]
    async fn determine_sync_mode_no_state_db_returns_full() {
        let mode = determine_sync_mode(
            false,
            false,
            1,
            &None,
            "sync_token:PrimarySync",
            "PrimarySync",
        )
        .await;

        assert!(
            matches!(mode, download::SyncMode::Full),
            "no state DB must yield Full, got {mode:?}"
        );
    }

    // ── check_changes_database ───────────────────────────────────────
    //
    // Watch-mode wakes the sync loop on a fixed interval. The first thing
    // each cycle does is hit the `changes/database` endpoint to ask Apple
    // "anything actually changed?" If we mis-classify the response we
    // either hammer Apple uselessly (no changes but proceeded) or silently
    // skip a real delta (changes pending but skipped). Pin every branch.

    /// Build a `LibraryState` that's just enough for `check_changes_database`.
    /// The `plan` and `library` fields are unused by that function, so an
    /// empty plan + a stub library is safe.
    fn make_library_state(zone: &str, sync_token_key: &str) -> LibraryState {
        let stub_session = Box::new(
            crate::test_helpers::MockPhotosSession::new().ok(serde_json::json!({"records": []})),
        );
        LibraryState {
            library: crate::icloud::photos::PhotoLibrary::new_stub(stub_session),
            zone_name: zone.to_string(),
            sync_token_key: sync_token_key.to_string(),
            plan: crate::commands::AlbumPlan { passes: Vec::new() },
        }
    }

    /// CG-5: `more_coming=true` with empty zones must NOT skip the cycle.
    /// Production logic: `if zones.is_empty() && !more_coming { skip }`.
    /// A regression that flipped the conjunction would silently skip every
    /// page-bearing wakeup — silent loss of pending changes.
    #[tokio::test]
    async fn check_changes_database_more_coming_does_not_skip() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-2",
            "moreComing": true,
            "zones": []
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        // Pre-populate a stored sync token so the function actually
        // makes the changes/database HTTP call (the `has_token` early
        // return otherwise short-circuits).
        db.set_metadata("sync_token:PrimarySync", "zone-tok-1")
            .await
            .expect("set token");

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let skip = check_changes_database(&Some(Arc::clone(&db)), &lib_state, &mut svc).await;

        assert!(
            !skip,
            "more_coming=true must not skip the cycle (more pages pending)"
        );
        // db_sync_token should have been persisted so the next cycle
        // continues paging from where we left off.
        let stored = db
            .get_metadata("db_sync_token")
            .await
            .expect("read db_sync_token")
            .expect("token persisted");
        assert_eq!(stored, "db-tok-2");
    }

    /// CG-6: empty zones + `more_coming=false` must return `skip=true`.
    /// This is the optimistic short-circuit: Apple confirmed there are no
    /// pending changes, so we save a full enumeration cycle. A regression
    /// that flipped this branch would either burn a CloudKit query per
    /// idle wakeup (cost) or silently skip a real cycle (loss).
    #[tokio::test]
    async fn check_changes_database_empty_zones_skips_cycle() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-3",
            "moreComing": false,
            "zones": []
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("set token");

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let skip = check_changes_database(&Some(Arc::clone(&db)), &lib_state, &mut svc).await;

        assert!(skip, "empty zones + more_coming=false must skip the cycle");
        // The new db_sync_token must still be persisted even on skip:
        // otherwise the next call re-asks from scratch and we'd get an
        // unbounded list of all zones.
        let stored = db
            .get_metadata("db_sync_token")
            .await
            .expect("read db_sync_token")
            .expect("token persisted on skip");
        assert_eq!(stored, "db-tok-3");
    }

    /// Companion to CG-6: a non-empty zones list MUST NOT skip — even
    /// when more_coming=false. This is the real-work path; pinning it
    /// alongside the skip path catches a flipped branch in either
    /// direction.
    #[tokio::test]
    async fn check_changes_database_zone_changes_present_does_not_skip() {
        use serde_json::json;
        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-4",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "ps-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let db: Arc<dyn state::StateDb> = make_state_db();
        db.set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("set token");

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let skip = check_changes_database(&Some(db), &lib_state, &mut svc).await;
        assert!(!skip, "zones-present response must not skip the cycle");
    }

    /// CG-6 corner: no stored sync token at all must return false (don't
    /// skip) without making the HTTP call. Pinning this prevents a future
    /// refactor that flipped the early return from silently consuming an
    /// Apple call slot on bootstrap.
    #[tokio::test]
    async fn check_changes_database_no_stored_token_does_not_skip() {
        let session = crate::test_helpers::MockPhotosSession::new();
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        // Empty DB — no `sync_token:PrimarySync` set.
        let db: Arc<dyn state::StateDb> = make_state_db();
        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        let skip = check_changes_database(&Some(db), &lib_state, &mut svc).await;
        assert!(!skip, "no stored token must skip-result false (continue)");
    }

    /// CG-7: a `set_metadata("db_sync_token", ...)` write failure must
    /// NOT break the cycle. The current implementation logs a warning and
    /// continues. A regression that propagated the error would crash watch
    /// mode whenever a sqlite hiccup hit that single write.
    #[tokio::test]
    async fn check_changes_database_token_persist_failure_does_not_skip() {
        use serde_json::json;
        // StateDb that succeeds on get_metadata("sync_token:...") but
        // fails on set_metadata("db_sync_token", ...) — the only write
        // path inside `check_changes_database`.
        struct PartiallyFailingDb {
            inner: Arc<dyn state::StateDb>,
        }

        #[async_trait::async_trait]
        impl state::StateDb for PartiallyFailingDb {
            #[cfg(test)]
            async fn should_download(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &std::path::Path,
            ) -> Result<bool, state::error::StateError> {
                unimplemented!()
            }
            async fn upsert_seen(
                &self,
                _: &state::types::AssetRecord,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_downloaded(
                &self,
                _: &str,
                _: &str,
                _: &std::path::Path,
                _: &str,
                _: Option<&str>,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn import_adopt(
                &self,
                _: &state::types::AssetRecord,
                _: &std::path::Path,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_failed(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_failed(
                &self,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_failed_sample(
                &self,
                _: u32,
            ) -> Result<(Vec<state::types::AssetRecord>, u64), state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_pending(
                &self,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_summary(
                &self,
            ) -> Result<state::types::SyncSummary, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_page(
                &self,
                _: u64,
                _: u32,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn start_sync_run(&self) -> Result<i64, state::error::StateError> {
                unimplemented!()
            }
            async fn complete_sync_run(
                &self,
                _: i64,
                _: &state::types::SyncRunStats,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn promote_orphaned_sync_runs(&self) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn begin_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn end_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn list_interrupted_enumerations(
                &self,
            ) -> Result<Vec<String>, state::error::StateError> {
                unimplemented!()
            }
            async fn reset_failed(&self) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), state::error::StateError> {
                unimplemented!()
            }
            async fn promote_pending_to_failed(
                &self,
                _: i64,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_ids(
                &self,
            ) -> Result<std::collections::HashSet<(String, String)>, state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_all_known_ids(
                &self,
            ) -> Result<std::collections::HashSet<String>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_checksums(
                &self,
            ) -> Result<std::collections::HashMap<(String, String), String>, state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_attempt_counts(
                &self,
            ) -> Result<std::collections::HashMap<String, u32>, state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_metadata(
                &self,
                key: &str,
            ) -> Result<Option<String>, state::error::StateError> {
                self.inner.get_metadata(key).await
            }
            async fn set_metadata(
                &self,
                key: &str,
                _value: &str,
            ) -> Result<(), state::error::StateError> {
                if key == "db_sync_token" {
                    Err(state::error::StateError::LockPoisoned(
                        "simulated db_sync_token write failure".into(),
                    ))
                } else {
                    Ok(())
                }
            }
            async fn delete_metadata_by_prefix(
                &self,
                _: &str,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn touch_last_seen_many(
                &self,
                _: &[&str],
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn sample_downloaded_paths(
                &self,
                _: usize,
            ) -> Result<Vec<std::path::PathBuf>, state::error::StateError> {
                unimplemented!()
            }
            async fn add_asset_album(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_all_asset_albums(
                &self,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_all_asset_people(
                &self,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn mark_soft_deleted(
                &self,
                _: &str,
                _: Option<chrono::DateTime<chrono::Utc>>,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_hidden_at_source(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn record_metadata_write_failure(
                &self,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_metadata_hashes(
                &self,
            ) -> Result<std::collections::HashMap<(String, String), String>, state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_metadata_retry_markers(
                &self,
            ) -> Result<std::collections::HashSet<(String, String)>, state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_pending_metadata_rewrites(
                &self,
                _: usize,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn update_metadata_hash(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn clear_metadata_write_failure(
                &self,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn has_downloaded_without_metadata_hash(
                &self,
            ) -> Result<bool, state::error::StateError> {
                unimplemented!()
            }
        }

        // Inner DB has the stored sync token so the changes/database call
        // is actually attempted.
        let inner = make_state_db();
        inner
            .set_metadata("sync_token:PrimarySync", "zone-tok-prev")
            .await
            .expect("seed token");
        let db: Arc<dyn state::StateDb> = Arc::new(PartiallyFailingDb { inner });

        let session = crate::test_helpers::MockPhotosSession::new().ok(json!({
            "syncToken": "db-tok-bad-write",
            "moreComing": false,
            "zones": [
                {"zoneID": {"zoneName": "PrimarySync"}, "syncToken": "ps-tok-new"}
            ]
        }));
        let mut svc = crate::icloud::photos::PhotosService::for_testing(
            Box::new(session),
            std::collections::HashMap::new(),
        );

        let lib_state = make_library_state("PrimarySync", "sync_token:PrimarySync");

        // The function logs the write failure and continues. zones non-empty
        // means it must return false (don't skip).
        let skip = check_changes_database(&Some(db), &lib_state, &mut svc).await;
        assert!(
            !skip,
            "db_sync_token write failure must not propagate as a skip"
        );
    }

    // ── preload_asset_groupings ──────────────────────────────────────
    //
    // CG-8: `preload_asset_groupings` must be best-effort: a hiccup
    // loading people must NOT empty the albums map, and vice versa.
    // XMP-sidecar runs read this struct; biasing the entire grouping
    // empty would silently strip metadata from every downloaded photo.

    /// CG-8: when `get_all_asset_albums` succeeds but
    /// `get_all_asset_people` fails, the result still includes albums.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn preload_asset_groupings_partial_people_failure_keeps_albums() {
        use std::collections::{HashMap, HashSet};

        struct PartialDb {
            inner: Arc<dyn state::StateDb>,
        }

        #[async_trait::async_trait]
        impl state::StateDb for PartialDb {
            #[cfg(test)]
            async fn should_download(
                &self,
                _: &str,
                _: &str,
                _: &str,
                _: &std::path::Path,
            ) -> Result<bool, state::error::StateError> {
                unimplemented!()
            }
            async fn upsert_seen(
                &self,
                _: &state::types::AssetRecord,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_downloaded(
                &self,
                _: &str,
                _: &str,
                _: &std::path::Path,
                _: &str,
                _: Option<&str>,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn import_adopt(
                &self,
                _: &state::types::AssetRecord,
                _: &std::path::Path,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_failed(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_failed(
                &self,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_failed_sample(
                &self,
                _: u32,
            ) -> Result<(Vec<state::types::AssetRecord>, u64), state::error::StateError>
            {
                unimplemented!()
            }
            async fn get_pending(
                &self,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_summary(
                &self,
            ) -> Result<state::types::SyncSummary, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_page(
                &self,
                _: u64,
                _: u32,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn start_sync_run(&self) -> Result<i64, state::error::StateError> {
                unimplemented!()
            }
            async fn complete_sync_run(
                &self,
                _: i64,
                _: &state::types::SyncRunStats,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn promote_orphaned_sync_runs(&self) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn begin_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn end_enum_progress(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn list_interrupted_enumerations(
                &self,
            ) -> Result<Vec<String>, state::error::StateError> {
                unimplemented!()
            }
            async fn reset_failed(&self) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn prepare_for_retry(&self) -> Result<(u64, u64, u64), state::error::StateError> {
                unimplemented!()
            }
            async fn promote_pending_to_failed(
                &self,
                _: i64,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_ids(
                &self,
            ) -> Result<HashSet<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_all_known_ids(&self) -> Result<HashSet<String>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_checksums(
                &self,
            ) -> Result<HashMap<(String, String), String>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_attempt_counts(
                &self,
            ) -> Result<HashMap<String, u32>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_metadata(
                &self,
                _: &str,
            ) -> Result<Option<String>, state::error::StateError> {
                unimplemented!()
            }
            async fn set_metadata(&self, _: &str, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn delete_metadata_by_prefix(
                &self,
                _: &str,
            ) -> Result<u64, state::error::StateError> {
                unimplemented!()
            }
            async fn touch_last_seen_many(
                &self,
                _: &[&str],
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn sample_downloaded_paths(
                &self,
                _: usize,
            ) -> Result<Vec<std::path::PathBuf>, state::error::StateError> {
                unimplemented!()
            }
            async fn add_asset_album(
                &self,
                asset_id: &str,
                album_name: &str,
                source: &str,
            ) -> Result<(), state::error::StateError> {
                self.inner
                    .add_asset_album(asset_id, album_name, source)
                    .await
            }
            async fn get_all_asset_albums(
                &self,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                self.inner.get_all_asset_albums().await
            }
            async fn get_all_asset_people(
                &self,
            ) -> Result<Vec<(String, String)>, state::error::StateError> {
                Err(state::error::StateError::LockPoisoned(
                    "simulated people-table read failure".into(),
                ))
            }
            async fn mark_soft_deleted(
                &self,
                _: &str,
                _: Option<chrono::DateTime<chrono::Utc>>,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn mark_hidden_at_source(&self, _: &str) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn record_metadata_write_failure(
                &self,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn get_downloaded_metadata_hashes(
                &self,
            ) -> Result<HashMap<(String, String), String>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_metadata_retry_markers(
                &self,
            ) -> Result<HashSet<(String, String)>, state::error::StateError> {
                unimplemented!()
            }
            async fn get_pending_metadata_rewrites(
                &self,
                _: usize,
            ) -> Result<Vec<state::types::AssetRecord>, state::error::StateError> {
                unimplemented!()
            }
            async fn update_metadata_hash(
                &self,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn clear_metadata_write_failure(
                &self,
                _: &str,
                _: &str,
            ) -> Result<(), state::error::StateError> {
                unimplemented!()
            }
            async fn has_downloaded_without_metadata_hash(
                &self,
            ) -> Result<bool, state::error::StateError> {
                unimplemented!()
            }
        }

        // Seed the inner DB with two album memberships across two assets,
        // so we can verify the surviving map is non-empty.
        let inner = make_state_db();
        inner
            .add_asset_album("ASSET_A", "Vacation", "icloud")
            .await
            .expect("add album A");
        inner
            .add_asset_album("ASSET_B", "Family", "icloud")
            .await
            .expect("add album B");

        let db: Option<Arc<dyn state::StateDb>> = Some(Arc::new(PartialDb { inner }));

        let groupings = preload_asset_groupings(&db).await;
        // Albums must survive intact.
        assert_eq!(
            groupings.albums.len(),
            2,
            "two assets with album memberships expected, got {}",
            groupings.albums.len()
        );
        assert!(groupings.albums.contains_key("ASSET_A"));
        assert!(groupings.albums.contains_key("ASSET_B"));
        // People map is empty (the read failed) — but the function still
        // returns Some groupings rather than panicking.
        assert!(
            groupings.people.is_empty(),
            "people map should be empty when its read failed; got {} entries",
            groupings.people.len()
        );
    }

    /// Companion: `state_db = None` returns an empty grouping struct.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn preload_asset_groupings_no_db_returns_empty() {
        let groupings = preload_asset_groupings(&None).await;
        assert!(groupings.albums.is_empty());
        assert!(groupings.people.is_empty());
    }

    // CG-9 / CG-21: `should_store_sync_token` is the single decision gate
    // protecting the sync-token from being advanced after a partial sync or
    // a dry run. Both situations would lose change events on the next
    // incremental cycle ("user data is sacred"). The matrix below pins every
    // (outcome, dry_run) combination so a future refactor can't relax the
    // contract without a failing test.

    /// CG-9: a partial download failure MUST NOT advance the stored sync
    /// token. Otherwise the next incremental sync would skip past the
    /// failed assets' change events and never retry them.
    #[test]
    fn sync_loop_partial_failure_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::PartialFailure { failed_count: 3 };
        assert!(
            !should_store_sync_token(&outcome, false),
            "PartialFailure must NOT advance the sync token, even outside dry-run"
        );
        // dry_run=true cannot rescue a partial failure either.
        assert!(
            !should_store_sync_token(&outcome, true),
            "PartialFailure + dry_run must still NOT advance the sync token"
        );
    }

    /// CG-9 companion: `SessionExpired` is also a non-success outcome and
    /// MUST NOT advance the token. The cycle aborts mid-stream; the captured
    /// token may only reflect a subset of the work.
    #[test]
    fn sync_loop_session_expired_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::SessionExpired {
            auth_error_count: 5,
        };
        assert!(!should_store_sync_token(&outcome, false));
        assert!(!should_store_sync_token(&outcome, true));
    }

    /// CG-21: in `--dry-run`, even a fully-successful pass MUST NOT advance
    /// the token. Dry-run promises no DB writes that affect the next real
    /// sync; advancing the token would silently break the next incremental.
    #[test]
    fn sync_loop_dry_run_does_not_advance_sync_token() {
        let outcome = download::DownloadOutcome::Success;
        assert!(
            !should_store_sync_token(&outcome, true),
            "dry_run must NOT advance the sync token even on Success"
        );
    }

    /// Positive control: only the (Success, dry_run=false) combination
    /// advances the token. Pinning this branch prevents a future refactor
    /// from accidentally inverting the predicate.
    #[test]
    fn sync_loop_full_success_outside_dry_run_advances_sync_token() {
        let outcome = download::DownloadOutcome::Success;
        assert!(
            should_store_sync_token(&outcome, false),
            "(Success, dry_run=false) is the ONLY combination that should advance the token"
        );
    }

    // CG-19: `should_wait_for_2fa` decides whether the reauth-time 2FA
    // branch parks the loop on a code prompt or surfaces the error. In
    // one-shot mode there is no operator at the keyboard; the error MUST
    // bubble up so cron / systemd / CI exits non-zero.

    /// CG-19: a 2FA-required error in one-shot (`is_watch_mode = false`)
    /// MUST NOT cause the helper to return `true`. The caller will then
    /// surface the error to the user instead of blocking forever on a
    /// 2FA prompt that no one is watching.
    #[test]
    fn run_sync_2fa_required_in_one_shot_returns_error() {
        let err: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        assert!(
            !should_wait_for_2fa(false, &err),
            "one-shot + 2FA-required must surface the error, not park on a prompt"
        );
    }

    /// CG-19 companion: in watch mode the same error is recoverable;
    /// the helper returns `true` so the loop can park.
    #[test]
    fn run_sync_2fa_required_in_watch_mode_waits() {
        let err: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        assert!(
            should_wait_for_2fa(true, &err),
            "watch + 2FA-required must wait on a code"
        );
    }

    /// Negative control: any non-2FA error MUST surface in both modes.
    /// Otherwise the loop could silently swallow a real failure (e.g.
    /// `FailedLogin`) by treating it as "wait for a code that won't come".
    #[test]
    fn run_sync_non_2fa_error_never_waits() {
        let err: anyhow::Error = auth::error::AuthError::FailedLogin("bad password".into()).into();
        assert!(
            !should_wait_for_2fa(false, &err),
            "one-shot + non-2FA must surface"
        );
        assert!(
            !should_wait_for_2fa(true, &err),
            "watch + non-2FA must surface; do not park on a code that will never arrive"
        );
    }

    /// Negative control: a non-AuthError (e.g. an arbitrary anyhow error
    /// with no downcast target) MUST NOT be treated as 2FA. Without this
    /// guard the predicate could mis-park on transport errors.
    #[test]
    fn run_sync_non_auth_error_never_waits() {
        let err: anyhow::Error = anyhow::anyhow!("network unreachable");
        assert!(!should_wait_for_2fa(false, &err));
        assert!(!should_wait_for_2fa(true, &err));
    }
}
