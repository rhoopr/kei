//! Command startup and the top-level sync/watch cycle.

use std::sync::Arc;

use anyhow::Context;

use crate::notifications::Notifier;
use crate::password::{ExposeSecret, SecretString};
use crate::sync_cycle::{LibraryState, run_cycle};
use crate::sync_loop::planning::{maybe_notify_shared_libraries, refresh_needed_library_plans};
use crate::sync_loop::precheck::{
    DbPrecheckScope, WatchPrecheck, check_changes_database, include_pending_local_work,
    include_pending_provider_work, store_scoped_db_sync_token,
};
use crate::sync_loop::reconcile::{run_bounded_local_drift_probe, run_periodic_reconcile};
#[cfg(debug_assertions)]
use crate::sync_loop::reporting::maybe_write_offline_fake_sync_report;
use crate::sync_loop::reporting::merge_refresh_tail_outcome;
use crate::sync_loop::session::reacquire_session;
use crate::sync_loop::watch::{
    refresh_metadata_forces_one_shot, repair_truncated_forces_one_shot,
    validate_capture_timestamp_repair,
};
use crate::sync_loop::{SyncArgs, service_mode_default_interval, should_reconcile_this_cycle};
use crate::systemd::SystemdNotifier;
use crate::{
    PartialSyncError, PidFileGuard, available_disk_space, check_min_disk_space, config, credential,
    download, health, make_password_provider, notifications, password, retry, shutdown, state,
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
    config.runtime.dry_run |= config.runtime.only_print_filenames;
    let capture_timestamp_repair = if config.runtime.repair_capture_timestamps {
        download::CaptureTimestampRepair::ReplaceWithCaptureLocal
    } else {
        download::CaptureTimestampRepair::Preserve
    };

    // Keep fresh-install auto-config behavior, but never create a previously
    // absent config ahead of the recovery backup for retained state.
    let defer_first_run_config =
        if !config.runtime.dry_run && !toml_existed && config_explicitly_set {
            let path = state::db::account::state_path(
                &config.auth.cookie_directory,
                &config.auth.username,
                config.auth.domain.as_str(),
            )
            .await?;
            tokio::fs::try_exists(path).await?
        } else {
            false
        };
    if !config.runtime.dry_run
        && !toml_existed
        && config_explicitly_set
        && !defer_first_run_config
        && let Err(e) =
            config::persist_first_run_config(&config_path, &config, cli_data_dir.as_deref())
    {
        tracing::warn!(error = %e, "Failed to save first-run config");
    }

    // One-shot operations — never inherit watch mode from TOML config,
    // which would cause the process to loop forever instead of exiting.
    // retry-failed: one-shot by definition.
    // setup → "sync now": initial test sync, not a daemon.
    if is_one_shot || config.runtime.dry_run {
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
    if !config.runtime.dry_run
        && let Some(applied) = service_mode_default_interval(config.watch.interval, service_mode)
    {
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
        .filter(|_| !config.runtime.dry_run)
        .map(|p| PidFileGuard::new(p.clone()))
        .transpose()?;

    let sd_notifier = SystemdNotifier::new(config.watch.notify_systemd && !config.runtime.dry_run);
    let notifier = Notifier::new(if config.runtime.dry_run {
        None
    } else {
        config.notifications.script.clone()
    });

    tracing::info!(concurrency = config.download.threads_num, "Starting kei");

    if config.auth.username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
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
    if !config.runtime.dry_run {
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
    }

    #[cfg(debug_assertions)]
    if maybe_write_offline_fake_sync_report(&config, &notifier).await? {
        return Ok(());
    }

    if !config.runtime.dry_run {
        state::db::account::state_path(
            &config.auth.cookie_directory,
            &config.auth.username,
            config.auth.domain.as_str(),
        )
        .await?;
    }
    let cred_store = credential::CredentialStore::new(
        &config.auth.username,
        &config.auth.cookie_directory,
        config.auth.domain.as_str(),
    );
    let source = password::build_password_source(
        config.auth.password.as_ref(),
        config.auth.password_command.as_deref(),
        config.auth.password_file.as_deref(),
        cred_store,
    );
    // Compute the save-password decision while `source` is still owned here.
    // The resulting action carries no password payload and survives moving the
    // source into the provider closure below.
    let save_password_action = (config.auth.save_password && !config.runtime.dry_run)
        .then(|| password::decide_save_password_action(&source));
    let password_provider = make_password_provider(source, input_mode);

    let auth_result = super::session::authenticate_initial_session(
        &config,
        &password_provider,
        &notifier,
        input_mode,
    )
    .await?;
    let account_owner = if config.runtime.dry_run {
        None
    } else {
        Some(state::db::account::AccountOwner::authenticated(
            &config.auth.username,
            config.auth.domain.as_str(),
            &auth_result.data,
        )?)
    };
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
                        config.auth.domain.as_str(),
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

    let shutdown_token = shutdown::install_signal_handler(sd_notifier, config.ui.personality_mode)?;
    // Watch health must answer while authenticated discovery waits to retry.
    // Bind before signaling readiness; cancel the listener on any startup exit.
    let metrics_shutdown = shutdown_token.child_token();
    let _metrics_guard = metrics_shutdown.clone().drop_guard();
    let (metrics_handle, metrics_task) = if let Some(interval) = config.watch.interval {
        let (handle, task, _addr) = crate::metrics::spawn_server(
            config.server.bind,
            config.server.port,
            metrics_shutdown,
            Some(chrono::Duration::seconds((interval * 2) as i64)),
        )?;
        (Some(handle), Some(task))
    } else {
        (None, None)
    };
    // Library discovery can take a watch interval to recover. Release the
    // service manager's start job without exposing a partial sync plan.
    let _watch_start = is_watch_mode.then(|| sd_notifier.start_worker(shutdown_token.clone()));
    let (shared_session, mut photos_service, libraries) = tokio::select! {
        biased;
        () = shutdown_token.cancelled() => return Ok(()),
        result = super::session::initialize_libraries(
            auth_result,
            api_retry_config,
            &config,
            &password_provider,
            &notifier,
            input_mode,
        ) => result?,
    };
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

    // Real syncs run on the sync core. Previews stay on the previous engine
    // until the core supports them; it is removed with that engine.
    if !config.runtime.dry_run {
        let _one_shot_start =
            (!is_watch_mode).then(|| sd_notifier.start_worker(shutdown_token.clone()));
        return crate::sync_core::runner::run(crate::sync_core::runner::Run {
            config: &config,
            libraries,
            session: shared_session,
            owner: account_owner
                .as_ref()
                .ok_or(state::error::StateError::AccountIdentityUnavailable)?,
            retry: api_retry_config,
            shutdown: &shutdown_token,
        })
        .await;
    }

    // Initialize state database.
    // Skip for --dry-run so a preview doesn't create the DB or poison
    // sync tokens, which would cause a subsequent real sync to believe
    // nothing has changed and download 0 photos.
    let startup_db = if config.runtime.dry_run {
        None
    } else {
        match state::db::account::open_sync_state(
            &config.auth.cookie_directory,
            &config.auth.username,
            config.auth.domain.as_str(),
            account_owner
                .as_ref()
                .ok_or(state::error::StateError::AccountIdentityUnavailable)?,
            &globals.config_source,
        )
        .await
        {
            Ok(db) => {
                tracing::debug!("Account state database opened");
                let db = Arc::new(db);
                photos_service.set_shadow_capture(
                    crate::icloud::photos::inbox::ShadowCapture::new(
                        Arc::clone(&db),
                        account_owner
                            .as_ref()
                            .ok_or(state::error::StateError::AccountIdentityUnavailable)?
                            .clone(),
                        config.auth.domain.as_str(),
                    ),
                );

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

                Some(db)
            }
            Err(e) => {
                anyhow::bail!("Could not open account state database: {e}");
            }
        }
    };

    let state_db: Option<Arc<dyn download::DownloadStore>> = startup_db
        .as_ref()
        .map(|db| Arc::clone(db) as Arc<dyn download::DownloadStore>);

    // Persist first-run settings only after the state upgrade boundary: an old
    // owned database must be backed up before creating a previously absent config.
    if defer_first_run_config
        && let Err(e) =
            config::persist_first_run_config(&config_path, &config, cli_data_dir.as_deref())
    {
        tracing::warn!(error = %e, "Failed to save first-run config");
    }

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
    // call would re-allocate directory / temp_suffix
    // from scratch instead of refcount-bumping.
    let cfg_directory: Arc<std::path::Path> = Arc::from(config.download.directory.as_path());
    let cfg_temp_suffix: Arc<str> = Arc::from(config.download.temp_suffix.as_str());
    let cfg_folder_structure_albums: Arc<str> =
        Arc::from(config.download.folder_structure_albums.as_str());
    let cfg_folder_structure_smart_folders: Arc<str> =
        Arc::from(config.download.folder_structure_smart_folders.as_str());
    let enum_config_hash: Arc<str> = download::compute_config_hash(&config).into();
    let post_sync_metadata = !config.runtime.refresh_metadata
        && download::metadata_rewrite::writers_enabled(&config.metadata)
        && capture_timestamp_repair == download::CaptureTimestampRepair::Preserve
        && !config.runtime.dry_run
        && !config.runtime.only_print_filenames;

    let build_download_config = |sync_mode: download::SyncMode,
                                 exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
                                 asset_groupings: Arc<download::AssetGroupings>,
                                 library: Arc<str>|
     -> Arc<download::DownloadConfig> {
        Arc::new(download::DownloadConfig {
            post_sync_metadata,
            inventory_reconciliation: false,
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
            legacy_preservation_allow_hardlinks: config
                .download
                .legacy_preservation_allow_hardlinks,
            concurrent_downloads: config.download.threads_num as usize,
            retry: retry_config,
            live_photo_mode: config.photos.live_photo_mode,
            live_resolution,
            live_photo_mov_filename_policy: config.photos.live_photo_mov_filename_policy,
            edited: config.photos.edited,
            edited_naming: config.photos.edited_naming,
            primary_layout_active: false,
            primary_layout_preview: false,
            primary_layout_source: None,
            primary_layout_pass: None,
            primary_layout_pass_kind: None,
            alternative: config.photos.alternative,
            raw_policy: config.photos.raw_policy,
            file_match_policy: config.photos.file_match_policy,
            force_resolution: config.photos.force_resolution,
            keep_unicode_in_filenames: config.photos.keep_unicode_in_filenames,
            temp_suffix: Arc::clone(&cfg_temp_suffix),
            state_db: state_db.clone(),
            sparse_targets: Arc::from([]),
            sparse_membership_due: false,
            complete_path_selection: false,
            selection_context: None,
            selection_run: None,
            selection_pass: None,
            provisional: false,
            indexing_inventory_only: false,
            indexing_inventory_cursor: None,
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

    photos_service.replay_catalog(&shutdown_token).await?;
    if shutdown_token.is_cancelled() {
        return Ok(());
    }

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

    let (mut library_states, mut collection_context) =
        super::planning::resolve_library_plans_with_recovery(
            &mut photos_service,
            &libraries,
            &config,
            startup_db.as_deref().zip(account_owner.as_ref()),
        )
        .await?;
    if run_mode.downloads_files()
        && let Some(db) = startup_db.as_deref()
    {
        let plans: Vec<_> = library_states
            .iter()
            .map(|library| {
                (
                    build_download_config(
                        download::SyncMode::Full,
                        Arc::new(rustc_hash::FxHashSet::default()),
                        Arc::new(download::AssetGroupings::default()),
                        Arc::from(library.zone_name.as_str()),
                    ),
                    library.plan.passes.as_slice(),
                )
            })
            .collect();
        if let Err(error) =
            crate::commands::adopt_startup_archive(db, &plans, &shutdown_token).await
        {
            tracing::warn!(%error, "Startup archive adoption incomplete; continuing current sync");
        }
    }
    let db_precheck_scope = DbPrecheckScope::from_config(
        &config,
        &library_states,
        &build_download_config,
        enum_config_hash.as_ref(),
    )?;
    let _one_shot_start =
        (!is_watch_mode).then(|| sd_notifier.start_worker(shutdown_token.clone()));
    // Friendly-mode greeting. Lands above any future bar via
    // active_bar::with_suspended; no-op in off mode. Once per process.
    crate::personality::narration::greet_to_stderr(config.ui.personality_mode, is_watch_mode);

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
    let mut backfill_cursors = std::collections::HashMap::new();
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

        // Readiness may change without changes/database events. Persist holds
        // before any shortcut and rebuild collection/pass scope while held so
        // a provisional album list cannot certify FINISHED reconciliation.
        tokio::select! {
            biased;
            () = shutdown_token.cancelled() => break,
            result = super::planning::refresh_indexing_library_plans_with_recovery(
                &mut photos_service,
                &libraries,
                &config,
                state_db.as_deref().map(|db| db as &dyn download::DownloadStore),
                &mut library_states,
                &mut collection_context,
                startup_db.as_deref().zip(account_owner.as_ref()),
            ) => result?,
        }

        // In watch mode with incremental sync, use changes/database as a
        // cheap pre-check before refreshing album plans or running a sync.
        // After the readiness probes, quiet cycles avoid full album/pass
        // refreshes for every selected library.
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
                    "Local drift probe found missing files; forcing this cycle to retry them"
                );
                watch_precheck = WatchPrecheck::proceed_all();
            }
        }

        if is_watch_mode
            && !config.runtime.dry_run
            && !config.runtime.only_print_filenames
            && let Some(db) = state_db.as_deref()
        {
            include_pending_local_work(
                &mut watch_precheck,
                db,
                (!post_sync_metadata).then_some(&config.metadata),
                &library_states,
            )
            .await;
            include_pending_provider_work(
                &mut watch_precheck,
                &library_states,
                &build_download_config,
                download_controls,
            )
            .await;
        }

        let current_work_complete;
        let mut backfill_safe = true;
        if matches!(watch_precheck, WatchPrecheck::SkipAll) {
            current_work_complete = true;
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
            crate::support::begin(crate::support::runtime_configuration(&config));
            let mut cycle_result = run_cycle(
                &cycle_library_states,
                &config,
                state_db.as_deref(),
                &build_download_config,
                download_controls,
                &shared_session,
                &shutdown_token,
            )
            .await?;
            // Drain tagged rewrites now so the one-shot --refresh-metadata repair finishes in this run.
            let refresh_tail =
                if config.runtime.refresh_metadata && !cycle_result.stats.storage_failed {
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
                        download::MetadataDrainOutcome::default()
                    }
                } else {
                    download::MetadataDrainOutcome::default()
                };
            if config.runtime.refresh_metadata {
                merge_refresh_tail_outcome(
                    &mut cycle_result,
                    refresh_tail.failed,
                    refresh_tail.storage_failed,
                    shutdown_token.is_cancelled(),
                );
            }
            if refresh_tail.failed > 0 {
                tracing::warn!(
                    failed = refresh_tail.failed,
                    storage_failed = refresh_tail.storage_failed,
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
                if cycle_result.can_advance_database_checkpoint() {
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

            let facts = crate::cycle_reporter::CycleFacts::new(
                &cycle_result.stats,
                cycle_result.failed_count,
                cycle_result.session_expired,
                cycle_started_at.elapsed(),
            );
            backfill_safe = !cycle_result.stats.storage_failed
                && cycle_result.stats.state_write_failures == 0
                && !cycle_result.session_expired;
            current_work_complete = facts.status == crate::cycle_reporter::CycleStatus::Success;
            cycle_reporter
                .report_completed_cycle(&mut health, facts)
                .await;

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
                }
            } else {
                reauth_attempts = 0;
            }
        }

        // The zone decisions and (where eligible) the database precheck token
        // are already durable. Local failures cannot revoke those checkpoints.
        let backfill_started = std::time::Instant::now();
        let deadline = config
            .watch
            .interval
            .and_then(|secs| backfill_started.checked_add(std::time::Duration::from_secs(secs)));
        if post_sync_metadata
            && backfill_safe
            && let Some(db) = startup_db.as_deref()
        {
            // A large first library must not consume every watch interval.
            let start = usize::try_from(cycle_index - 1)
                .unwrap_or(0)
                .checked_rem(library_states.len())
                .unwrap_or(0);
            for library in library_states
                .iter()
                .cycle()
                .skip(start)
                .take(library_states.len())
            {
                if shutdown_token.is_cancelled()
                    || deadline.is_some_and(|due| std::time::Instant::now() >= due)
                {
                    break;
                }
                if library.plan_is_stale {
                    continue;
                }
                let local_config = build_download_config(
                    download::SyncMode::Full,
                    Arc::default(),
                    Arc::default(),
                    Arc::from(library.zone_name.as_str()),
                );
                let cursor = backfill_cursors
                    .entry(library.zone_name.clone())
                    .or_default();
                match Box::pin(download::drain_local_backfill(
                    &library.plan.passes,
                    &local_config,
                    db,
                    download_controls,
                    &shutdown_token,
                    deadline,
                    cursor,
                ))
                .await
                {
                    Ok(outcome) if outcome.storage_failed => {
                        tracing::warn!(
                            "Local metadata backfill stopped on a storage failure; pending work was retained"
                        );
                        cumulative_failed_count = cumulative_failed_count.saturating_add(1);
                        break;
                    }
                    Ok(outcome) if outcome.failed > 0 => {
                        tracing::warn!(
                            pending = outcome.failed,
                            "Local metadata remains pending; provider checkpoints are unchanged"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%error,"Local metadata backfill stopped; pending work was retained");
                        cumulative_failed_count = cumulative_failed_count.saturating_add(1);
                        break;
                    }
                }
            }
        }
        let backfill_elapsed = backfill_started.elapsed();
        if !is_watch_mode && cumulative_failed_count > 0 {
            return Err(PartialSyncError(cumulative_failed_count).into());
        }

        if current_work_complete
            && !shutdown_token.is_cancelled()
            && let Some(db) = startup_db.as_deref()
            && super::reconcile::recover_historical_sources(db, &library_states, &shutdown_token)
                .await
                .is_err()
        {
            tracing::warn!("Historical source checks deferred after a state error");
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

        if let Some(interval) = config.watch.interval {
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
                () = tokio::time::sleep(std::time::Duration::from_secs(interval).saturating_sub(backfill_elapsed)) => {}
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
