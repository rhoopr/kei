//! Command startup and the top-level sync/watch cycle.

use std::sync::Arc;

use anyhow::Context;

use crate::notifications::Notifier;
use crate::password::{ExposeSecret, SecretString};
use crate::sync_cycle::{LibraryState, run_cycle};
use crate::sync_loop::planning::{maybe_notify_shared_libraries, refresh_needed_library_plans};
use crate::sync_loop::precheck::{
    DbPrecheckScope, WatchPrecheck, check_changes_database, include_pending_metadata_work,
    store_scoped_db_sync_token,
};
use crate::sync_loop::reconcile::{run_bounded_local_drift_probe, run_periodic_reconcile};
#[cfg(debug_assertions)]
use crate::sync_loop::reporting::maybe_write_offline_fake_sync_report;
use crate::sync_loop::reporting::merge_refresh_tail_outcome;
use crate::sync_loop::session::reacquire_session;
use crate::sync_loop::watch::{
    metadata_capture_watch_interval, refresh_metadata_forces_one_shot,
    repair_truncated_forces_one_shot, validate_capture_timestamp_repair,
};
use crate::sync_loop::{SyncArgs, service_mode_default_interval, should_reconcile_this_cycle};
use crate::systemd::SystemdNotifier;
use crate::{
    PartialSyncError, PidFileGuard, auth, available_disk_space, check_min_disk_space, config,
    credential, download, health, make_password_provider, notifications, password, retry, shutdown,
    state,
};

/// Run the sync command: authenticate, enumerate photos, download, and
/// optionally loop in watch mode.
pub(crate) async fn run_sync(globals: &config::GlobalArgs, args: SyncArgs) -> anyhow::Result<()> {
    let SyncArgs {
        is_one_shot,
        service_mode,
        pw,
        sync,
        toml_config,
        config_explicitly_set,
        config_path,
        redact_password,
        personality_mode,
        friendly_request,
        input_mode,
    } = args;

    let is_retry_failed = sync.retry_failed;
    let toml_existed = toml_config.is_some();
    let cli_data_dir = globals.data_dir.clone();
    let mut config = config::Config::build_inner(
        globals,
        &pw,
        sync,
        toml_config.as_ref(),
        personality_mode,
        friendly_request,
    )?;
    let capture_timestamp_repair = if config.runtime.repair_capture_timestamps {
        download::CaptureTimestampRepair::ReplaceWithCaptureLocal
    } else {
        download::CaptureTimestampRepair::Preserve
    };

    // On first run (no config file), persist bootstrap values so subsequent
    // runs don't need the same env again. Only when the user explicitly chose
    // a config path (--config), to avoid surprise writes at the default
    // location during tests or one-off runs.
    if !toml_existed
        && config_explicitly_set
        && let Err(e) =
            config::persist_first_run_config(&config_path, &config, cli_data_dir.as_deref())
    {
        tracing::warn!(error = %e, "Failed to save first-run config");
    }

    // One-shot operations — never inherit watch mode from TOML config,
    // which would cause the process to loop forever instead of exiting.
    // retry-failed: one-shot by definition.
    // setup → "sync now": initial test sync, not a daemon.
    if is_one_shot {
        config.watch.interval = None;
    }

    // --refresh-metadata is a one-shot recovery pass; reject watch/service and incomplete library sweeps, then force a single cycle.
    if refresh_metadata_forces_one_shot(
        config.runtime.refresh_metadata,
        service_mode,
        config.filters.narrows_enumeration(),
    )? {
        config.watch.interval = None;
    }
    let capture_timestamp_repair =
        validate_capture_timestamp_repair(capture_timestamp_repair, &config.metadata)?;
    if repair_truncated_forces_one_shot(config.runtime.repair_truncated, service_mode)? {
        config.watch.interval = None;
    }

    // Service-mode contract: the daemon must poll. If neither CLI nor TOML
    // supplied an interval, fall through to the canonical 24h default so a
    // launchd/systemd/SCM unit still has a heartbeat.
    if let Some(applied) = service_mode_default_interval(config.watch.interval, service_mode) {
        config.watch.interval = Some(applied);
        tracing::info!(
            interval_secs = applied,
            "service mode: applied default watch interval"
        );
    }
    let is_watch_mode = config.watch.interval.is_some();

    // Install password redaction now that we know the password
    if let Some(pw) = &config.auth.password
        && let Ok(mut guard) = redact_password.lock()
    {
        *guard = Some(SecretString::from(pw.expose_secret().to_owned()));
    }

    // Prevent core dumps from leaking in-memory credentials
    crate::harden_process();

    // Write PID file if requested (before auth so the PID is visible immediately)
    let _pid_guard = config
        .watch
        .pid_file
        .as_ref()
        .map(|p| PidFileGuard::new(p.clone()))
        .transpose()?;

    let sd_notifier = SystemdNotifier::new(config.watch.notify_systemd);
    let notifier = Notifier::new(config.notifications.script.clone());

    tracing::info!(concurrency = config.download.threads_num, "Starting kei");

    if config.auth.username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
    }

    // retry-failed + dry-run is unsupported: dry-run skips the state DB,
    // but retry-failed needs it to know which assets failed.
    if is_retry_failed && config.runtime.dry_run {
        anyhow::bail!(
            "`--dry-run` cannot be used with `--retry-failed` because retrying failed downloads needs to update the state database."
        );
    }

    // Validate download directory early (before auth) to avoid wasting a 2FA code
    // when the user simply forgot the destination.
    if config.download.directory.as_os_str().is_empty() {
        let message = crate::upgrade_hints::with_stale_env_hint(String::from(
            "Set [download].directory in the config file before syncing.",
        ));
        anyhow::bail!(message);
    }

    // Validate download directory is writable before spending time on authentication.
    tokio::fs::create_dir_all(&config.download.directory)
        .await
        .with_context(|| {
            format!(
                "Could not create download directory {}",
                config.download.directory.display()
            )
        })?;
    let probe = config.download.directory.join(".kei_probe");
    tokio::fs::write(&probe, b"").await.with_context(|| {
        format!(
            "Cannot write to download directory {}",
            config.download.directory.display()
        )
    })?;
    if let Err(e) = tokio::fs::remove_file(&probe).await {
        tracing::trace!(
            probe = %probe.display(),
            error = %e,
            "Could not clean up writability-probe file; harmless leakage"
        );
    }

    // Abort if available disk space is too low. See `check_min_disk_space`
    // for the pure inner check.
    if let Some(avail) = available_disk_space(&config.download.directory) {
        check_min_disk_space(avail, &config.download.directory)?;
    }

    #[cfg(debug_assertions)]
    if maybe_write_offline_fake_sync_report(&config, &notifier).await? {
        return Ok(());
    }

    let cred_store =
        credential::CredentialStore::new(&config.auth.username, &config.auth.cookie_directory);
    let source = password::build_password_source(
        config.auth.password.as_ref(),
        config.auth.password_command.as_deref(),
        config.auth.password_file.as_deref(),
        cred_store,
    );
    // Compute the save-password decision while `source` is still owned here.
    // The resulting action carries no password payload and survives moving the
    // source into the provider closure below.
    let save_password_action = config
        .auth
        .save_password
        .then(|| password::decide_save_password_action(&source));
    let password_provider = make_password_provider(source, input_mode);

    let auth_result = super::session::authenticate_initial_session(
        &config,
        &password_provider,
        &notifier,
        input_mode,
    )
    .await?;
    // Post-auth narration. Lands above any future bar; no-op in off mode.
    crate::personality::narration::auth_ok_to_stderr(
        config.ui.personality_mode,
        &config.auth.username,
    );

    // Save password to credential store if requested. Only the ephemeral
    // `Direct` source (CLI flag / env var) persists; File / Command /
    // Store / Interactive each emit a warning explaining why the flag is
    // a no-op for that source.
    if let Some(action) = save_password_action {
        match action {
            password::SavePasswordAction::Save => {
                if let Some(ref pw) = config.auth.password {
                    let store = credential::CredentialStore::new(
                        &config.auth.username,
                        &config.auth.cookie_directory,
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
        max_retries: config.retry.max_retries,
        base_delay_secs: config.retry.retry_delay_secs,
        max_delay_secs: 60,
    };
    api_retry_config.validate()?;

    let (shared_session, mut photos_service, libraries) = super::session::initialize_libraries(
        auth_result,
        api_retry_config,
        &config,
        &password_provider,
        &notifier,
        input_mode,
    )
    .await?;
    tracing::debug!(
        count = libraries.len(),
        zones = %libraries.iter().map(|l| l.zone_name().to_string()).collect::<Vec<_>>().join(", "),
        "Resolved libraries"
    );
    // Post-library-resolve narration. Friendly-mode-only.
    crate::personality::narration::libraries_resolved_to_stderr(
        config.ui.personality_mode,
        libraries.len(),
    );

    // Initialize state database.
    // Skip for --dry-run so a preview doesn't create the DB or poison
    // sync tokens, which would cause a subsequent real sync to believe
    // nothing has changed and download 0 photos.
    let state_db: Option<Arc<dyn download::DownloadStore>> = if config.runtime.dry_run {
        None
    } else {
        let db_path = config.auth.cookie_directory.join(format!(
            "{}.db",
            auth::session::sanitize_username(&config.auth.username)
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

                Some(db as Arc<dyn download::DownloadStore>)
            }
            Err(e) => {
                anyhow::bail!("Could not open state database {}: {e}", db_path.display());
            }
        }
    };

    // First-sync notice: tell users on the `PrimarySync` default about any
    // shared libraries they could be syncing. Runs once per data dir,
    // gated by state DB metadata.
    maybe_notify_shared_libraries(
        &config.filters.selection.libraries,
        &mut photos_service,
        state_db
            .as_deref()
            .map(|db| db as &dyn state::SyncTokenStore),
    )
    .await;

    // Pre-compute config values used each cycle to build DownloadConfig.
    // DownloadConfig is rebuilt per-cycle so sync_mode can vary.
    let skip_created_before = config.filters.skip_created_before;
    let skip_created_after = config.filters.skip_created_after;
    let retry_config = api_retry_config;
    let live_resolution = config.photos.live_resolution.to_asset_version_size();
    // One shared limiter per sync run so the configured cap applies to
    // aggregate throughput across every concurrent download.
    let bandwidth_limiter = config
        .download
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
    // Arc-sharing win is half-defeated: each build_download_config
    // call would re-allocate directory / filename_exclude / temp_suffix
    // from scratch instead of refcount-bumping.
    let cfg_directory: Arc<std::path::Path> = Arc::from(config.download.directory.as_path());
    let cfg_filename_exclude: Arc<[glob::Pattern]> =
        Arc::from(config.download.filename_exclude.clone());
    let cfg_temp_suffix: Arc<str> = Arc::from(config.download.temp_suffix.as_str());
    let cfg_folder_structure_albums: Arc<str> =
        Arc::from(config.download.folder_structure_albums.as_str());
    let cfg_folder_structure_smart_folders: Arc<str> =
        Arc::from(config.download.folder_structure_smart_folders.as_str());
    let enum_config_hash: Arc<str> = download::compute_config_hash(&config).into();

    let build_download_config = |sync_mode: download::SyncMode,
                                 exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
                                 asset_groupings: Arc<download::AssetGroupings>,
                                 library: Arc<str>|
     -> Arc<download::DownloadConfig> {
        Arc::new(download::DownloadConfig {
            directory: Arc::clone(&cfg_directory),
            folder_structure: config.download.folder_structure.clone(),
            folder_structure_albums: Arc::clone(&cfg_folder_structure_albums),
            folder_structure_smart_folders: Arc::clone(&cfg_folder_structure_smart_folders),
            library,
            resolution: config.photos.resolution,
            media: config.filters.media,
            skip_created_before,
            skip_created_after,
            metadata: config.metadata,
            refresh_metadata: config.runtime.refresh_metadata,
            capture_timestamp_repair,
            repair_truncated: config.runtime.repair_truncated,
            concurrent_downloads: config.download.threads_num as usize,
            recent: config.filters.recent,
            recent_scope: config.filters.recent_scope,
            retry: retry_config,
            live_photo_mode: config.photos.live_photo_mode,
            live_resolution,
            live_photo_mov_filename_policy: config.photos.live_photo_mov_filename_policy,
            edited: config.photos.edited,
            alternative: config.photos.alternative,
            raw_policy: config.photos.raw_policy,
            file_match_policy: config.photos.file_match_policy,
            force_resolution: config.photos.force_resolution,
            keep_unicode_in_filenames: config.photos.keep_unicode_in_filenames,
            filename_exclude: Arc::clone(&cfg_filename_exclude),
            temp_suffix: Arc::clone(&cfg_temp_suffix),
            state_db: state_db.clone(),
            retry_only: is_retry_failed,
            max_download_attempts: config.retry.max_download_attempts,
            sync_mode,
            enum_config_hash: Some(Arc::clone(&enum_config_hash)),
            album_name: None,
            exclude_asset_ids,
            asset_groupings,
            bandwidth_limiter: bandwidth_limiter.clone(),
        })
    };
    let run_mode = if config.runtime.only_print_filenames {
        download::DownloadRunMode::PrintFilenames
    } else if config.runtime.dry_run {
        download::DownloadRunMode::DryRun
    } else {
        download::DownloadRunMode::Download
    };
    let download_controls = download::DownloadControls::new(
        run_mode,
        download::DownloadReporting::new(
            config.download.no_progress_bar,
            config.ui.personality_mode,
        ),
    );

    let shutdown_token = shutdown::install_signal_handler(sd_notifier, config.ui.personality_mode)?;

    // Suppress the tty driver's `^C` echo for the lifetime of this sync run.
    // Without this, the echoed `^C` overflows the bar's right-edge filler
    // and pushes the cursor down one line, making indicatif's next redraw
    // leave a stale top rule above the live bar. Friendly + tty only;
    // restored on Drop and on the second-Ctrl+C force-exit path. See
    // `personality::tty_echo` for the full context.
    let _echo_guard = if config.ui.personality_mode.is_friendly() {
        crate::personality::tty_echo::EchoGuard::install()
    } else {
        None
    };

    let mut reauth_attempts = 0u32;
    // Sum of per-cycle failed_counts across the lifetime of this process.
    // Surfaced at exit so watch-mode daemons don't mask earlier-cycle
    // failures behind a clean final cycle.
    let mut cumulative_failed_count = 0usize;

    let (mut library_states, collection_context) =
        super::planning::resolve_library_plans(&mut photos_service, &libraries, &config).await?;
    let db_precheck_scope = DbPrecheckScope::from_config(
        &config,
        &library_states,
        &build_download_config,
        enum_config_hash.as_ref(),
    )?;
    sd_notifier.notify_ready();
    let _systemd_watchdog_task = sd_notifier.start_watchdog_heartbeat(shutdown_token.clone());
    // Friendly-mode greeting. Lands above any future bar via
    // active_bar::with_suspended; no-op in off mode. Once per process.
    crate::personality::narration::greet_to_stderr(config.ui.personality_mode, is_watch_mode);

    // Spawn the HTTP server (/healthz + /metrics) only in watch mode.
    // A one-shot sync exits before anything could scrape /healthz, so there
    // is no value in binding the port. In watch mode, flag /healthz as stale
    // after two missed intervals so a single slow cycle doesn't flip to 503
    // but a stuck main loop does.
    // Binds synchronously so a misconfigured port fails at startup.
    let staleness_threshold = config
        .watch
        .interval
        .map(|secs| chrono::Duration::seconds((secs * 2) as i64));
    let (metrics_handle, metrics_task) = if config.watch.interval.is_some() {
        let (h, t, _addr) = crate::metrics::spawn_server(
            config.server.bind,
            config.server.port,
            shutdown_token.clone(),
            staleness_threshold,
        )?;
        (Some(h), Some(t))
    } else {
        (None, None)
    };

    let mut health = health::HealthStatus::new();
    let cycle_reporter =
        crate::cycle_reporter::CycleReporter::new(crate::cycle_reporter::CycleReporterConfig {
            watch_mode: is_watch_mode,
            report_path: config.report.json.as_deref(),
            run_options: crate::report::RunOptions::from_config(&config),
            health_dir: &config.auth.cookie_directory,
            personality_mode: config.ui.personality_mode,
            state_db: state_db.as_deref(),
            metrics_handle: metrics_handle.as_ref(),
            notifier: &notifier,
        });
    let mut consecutive_album_refresh_failures = 0u32;
    // 1-based cycle counter for periodic-reconcile cadence. Logged at
    // cycle start so an operator chasing missed reconciliation runs has a
    // breadcrumb. Cycle 1 is the first iteration of this loop, cycle 2 is the
    // first re-entry under `--watch`, etc.
    let mut cycle_index: u64 = 0;
    if is_watch_mode {
        match config.watch.reconcile_every_n_cycles {
            Some(n) if n > 0 => tracing::info!(
                every_n_cycles = n,
                "Periodic local-vs-state reconciliation enabled"
            ),
            _ => tracing::debug!(
                "Periodic local-vs-state reconciliation disabled \
                 (set [watch] reconcile_every_n_cycles in TOML to enable)"
            ),
        }
    }

    loop {
        if shutdown_token.is_cancelled() {
            tracing::info!("Shutdown requested, exiting...");
            break;
        }
        cycle_index = cycle_index.saturating_add(1);
        let mut next_watch_interval = config.watch.interval;

        // In watch mode with incremental sync, use changes/database as a
        // cheap pre-check before refreshing album plans or running a sync.
        // No-change cycles should cost one CloudKit request, not a full
        // album/pass refresh per selected library.
        let mut watch_precheck = if is_watch_mode {
            check_changes_database(
                state_db
                    .as_deref()
                    .map(|db| db as &dyn state::SyncTokenStore),
                &library_states,
                &mut photos_service,
                &db_precheck_scope,
            )
            .await
        } else {
            WatchPrecheck::proceed_all()
        };

        if !config.runtime.dry_run
            && !config.runtime.only_print_filenames
            && let Some(db) = state_db.as_deref()
        {
            let drift = run_bounded_local_drift_probe(db, cycle_index).await;
            if drift.marked_failed > 0 {
                tracing::warn!(
                    marked_failed = drift.marked_failed,
                    "Local drift probe found missing or damaged files; forcing this cycle to retry them"
                );
                watch_precheck = WatchPrecheck::proceed_all();
            }
        }

        if is_watch_mode
            && !config.runtime.dry_run
            && !config.runtime.only_print_filenames
            && let Some(db) = state_db.as_deref()
        {
            include_pending_metadata_work(
                &mut watch_precheck,
                db,
                &config.metadata,
                &library_states,
            )
            .await;
        }

        if matches!(watch_precheck, WatchPrecheck::SkipAll) {
            cycle_reporter.report_skipped_watch_cycle(&mut health).await;
        } else {
            refresh_needed_library_plans(
                &mut library_states,
                &config.filters.selection,
                &collection_context,
                watch_precheck.changed_zones(),
                &mut consecutive_album_refresh_failures,
            )
            .await;
            let cycle_library_states: Vec<&LibraryState> = library_states
                .iter()
                .filter(|s| watch_precheck.should_sync_zone(&s.zone_name))
                .collect();
            debug_assert!(!cycle_library_states.is_empty());

            sd_notifier.notify_status("Syncing...");
            sd_notifier.notify_watchdog();
            notifier.notify(
                notifications::Event::SyncStarted,
                "Sync cycle starting",
                &config.auth.username,
                None,
                None,
            );

            let cycle_started_at = std::time::Instant::now();
            let cycle_wall_started_at = chrono::Utc::now();
            let mut cycle_result = run_cycle(
                &cycle_library_states,
                &config,
                state_db.as_deref(),
                is_retry_failed,
                &build_download_config,
                download_controls,
                &shared_session,
                &shutdown_token,
            )
            .await?;
            // Drain tagged rewrites now so the one-shot --refresh-metadata repair finishes in this run.
            let refresh_tail_failures = if config.runtime.refresh_metadata {
                if let Some(db) = state_db.as_deref() {
                    let library_scope: Vec<&str> = libraries
                        .iter()
                        .map(|library| library.zone_name())
                        .collect();
                    download::drain_pending_metadata_rewrites(
                        db,
                        &config.metadata,
                        capture_timestamp_repair,
                        &library_scope,
                        Arc::clone(&cfg_temp_suffix),
                        &shutdown_token,
                    )
                    .await
                } else {
                    0
                }
            } else {
                0
            };
            if config.runtime.refresh_metadata {
                merge_refresh_tail_outcome(
                    &mut cycle_result,
                    refresh_tail_failures,
                    shutdown_token.is_cancelled(),
                );
            }
            if refresh_tail_failures > 0 {
                tracing::warn!(
                    failed = refresh_tail_failures,
                    "Some --refresh-metadata rewrites did not complete; sync will exit non-zero"
                );
            }
            super::reporting::record_cycle_run(
                state_db.as_deref(),
                &cycle_result,
                cycle_wall_started_at,
            )
            .await;

            if let Some(token) = watch_precheck.db_sync_token_after_success() {
                if !cycle_result.session_expired
                    && cycle_result.failed_count == 0
                    && !cycle_result.stats.interrupted
                    && cycle_result.db_sync_token_advance_safe
                {
                    if let Some(db) = state_db.as_deref() {
                        store_scoped_db_sync_token(
                            db as &dyn state::SyncTokenStore,
                            &db_precheck_scope,
                            token,
                        )
                        .await;
                    }
                } else {
                    tracing::debug!(
                        "changes/database token not advanced because the sync cycle did not complete cleanly"
                    );
                }
            }

            cycle_reporter
                .report_completed_cycle(
                    &mut health,
                    crate::cycle_reporter::CycleFacts::new(
                        &cycle_result.stats,
                        cycle_result.failed_count,
                        cycle_result.session_expired,
                        cycle_started_at.elapsed(),
                    ),
                )
                .await;

            next_watch_interval = metadata_capture_watch_interval(
                config.watch.interval,
                &cycle_result.stats,
                cycle_result.failed_count,
            );
            if let (Some(configured_interval), Some(follow_up_interval)) =
                (config.watch.interval, next_watch_interval)
                && follow_up_interval < configured_interval
            {
                tracing::info!(
                    refreshed = cycle_result.stats.metadata_capture_refreshed,
                    remaining = cycle_result.stats.metadata_capture_remaining,
                    interval_secs = follow_up_interval,
                    "Metadata-capture repair scheduled a bounded follow-up cycle"
                );
            }

            // Handle aggregate outcome across all libraries
            if cycle_result.session_expired {
                super::session::recover_expired_cycle(
                    &shared_session,
                    &config,
                    &password_provider,
                    &notifier,
                    &mut reauth_attempts,
                    input_mode,
                )
                .await?;
                continue;
            } else if cycle_result.failed_count > 0 {
                cumulative_failed_count =
                    cumulative_failed_count.saturating_add(cycle_result.failed_count);
                if is_watch_mode {
                    tracing::warn!(
                        failed_count = cycle_result.failed_count,
                        cumulative = cumulative_failed_count,
                        "Some sync failures occurred this cycle, will retry next cycle"
                    );
                } else {
                    return Err(PartialSyncError(cycle_result.failed_count).into());
                }
            } else {
                reauth_attempts = 0;
            }
        }

        // Periodic local-vs-state reconciliation. This full-catalog walk is
        // read-only and surfaces missing or damaged files via `tracing::warn!`.
        // The bounded pre-cycle probe owns automatic requeue for the rows it
        // samples; `kei reconcile` remains the explicit full repair command
        // for operators who want to sweep the whole state DB immediately.
        if is_watch_mode
            && should_reconcile_this_cycle(cycle_index, config.watch.reconcile_every_n_cycles)
            && let Some(db) = state_db.as_ref()
        {
            run_periodic_reconcile(db.as_ref() as &dyn state::ReportStateStore, cycle_index).await;
        }

        if let Some(interval) = next_watch_interval {
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
            reacquire_session(&shared_session, &config, &password_provider, input_mode).await?;

            // Mark album/pass plans stale after an idle sleep, but defer the
            // CloudKit refresh until `changes/database` says a selected
            // library actually has work. Quiet watch cycles can then go back
            // to sleep without listing albums for every selected library.
            for lib_state in &mut library_states {
                lib_state.plan_needs_refresh = true;
            }
        } else {
            break;
        }
    }

    // Friendly farewell line. Only on graceful Ctrl+C-driven exit: if the
    // loop fell through without `shutdown_token` ever being cancelled (e.g.
    // a one-shot run completing normally), the per-cycle signoff already
    // covered the closing sentiment and a second "Done." line would be noise.
    if shutdown_token.is_cancelled() {
        crate::personality::narration::farewell_to_stderr(config.ui.personality_mode);
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

#[cfg(test)]
mod tests;
