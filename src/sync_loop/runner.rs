//! Command startup and the top-level sync/watch cycle.

use anyhow::Context;

use crate::notifications::Notifier;
use crate::password::{ExposeSecret, SecretString};
use crate::sync_loop::watch::{
    refresh_metadata_forces_one_shot, repair_truncated_forces_one_shot,
    validate_capture_timestamp_repair,
};
use crate::sync_loop::{SyncArgs, service_mode_default_interval};
use crate::systemd::SystemdNotifier;
use crate::{
    PidFileGuard, available_disk_space, check_min_disk_space, config, credential, download,
    make_password_provider, password, retry, shutdown, state,
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
    let _capture_timestamp_repair =
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
    if crate::sync_core::runner::maybe_write_offline_fake_report(&config).await? {
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
    let account_owner = state::db::account::AccountOwner::authenticated(
        &config.auth.username,
        config.auth.domain.as_str(),
        &auth_result.data,
    )?;
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
    let (metrics_handle, _metrics_task) = if let Some(interval) = config.watch.interval {
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
    let (shared_session, _photos_service, libraries) = tokio::select! {
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

    let _one_shot_start =
        (!is_watch_mode).then(|| sd_notifier.start_worker(shutdown_token.clone()));
    crate::sync_core::runner::run(crate::sync_core::runner::Run {
        config: &config,
        libraries,
        session: shared_session,
        owner: &account_owner,
        retry: api_retry_config,
        shutdown: &shutdown_token,
        notifier: &notifier,
        metrics: metrics_handle.as_ref(),
        password: &password_provider,
        input_mode,
        systemd: sd_notifier,
    })
    .await
}
