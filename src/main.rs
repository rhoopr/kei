//! kei: photo sync engine — Rust rewrite of icloud-photos-downloader.
//!
//! Downloads photos and videos from iCloud via Apple's private `CloudKit` APIs.
//! Authentication uses SRP-6a with Apple's custom variant, followed by optional
//! 2FA. Photos are streamed with exponential-backoff retries on transient
//! failures.

#![warn(clippy::all)]

mod auth;
mod cli;
mod commands;
mod config;
mod credential;
mod download;
mod health;
mod icloud;
mod migration;
mod notifications;
mod password;
pub mod retry;
mod setup;
mod shutdown;
mod state;
mod systemd;
mod types;

#[cfg(test)]
mod test_helpers;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use password::{ExposeSecret, SecretString};
use tracing_subscriber::EnvFilter;

/// A writer wrapper that redacts a password string from log output.
///
/// Wraps any `io::Write` implementor and replaces occurrences of the
/// configured password with `********` in each `write()` call.
struct RedactingWriter<W> {
    inner: W,
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let password = self
            .password
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pw) = &*password {
            let pw_str = pw.expose_secret();
            if !pw_str.is_empty() {
                // A buffer shorter than the password can't contain it,
                // avoiding the lossy UTF-8 conversion for short log fragments.
                if buf.len() >= pw_str.len() {
                    let s = String::from_utf8_lossy(buf);
                    if s.contains(pw_str) {
                        let redacted = s.replace(pw_str, "********");
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
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
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

/// Prevent core dumps from leaking in-memory credentials.
/// Best-effort: failures are logged but not fatal (Docker containers may
/// restrict these syscalls).
fn harden_process() {
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE with value 0 is a simple prctl flag toggle.
    // No pointer arguments; failure is non-fatal (logged and ignored).
    unsafe {
        if libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0 {
            tracing::debug!("prctl(PR_SET_DUMPABLE, 0) failed");
        }
    }
    #[cfg(unix)]
    // SAFETY: rlim is stack-allocated and fully initialized. setrlimit reads
    // from the pointer but does not store it. Failure is non-fatal.
    unsafe {
        let rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::setrlimit(libc::RLIMIT_CORE, &raw const rlim) != 0 {
            tracing::debug!("setrlimit(RLIMIT_CORE, 0) failed");
        }
    }
}

/// Exit code for partial sync (some downloads failed, but sync was not a total failure).
const EXIT_PARTIAL: u8 = 2;
/// Exit code for authentication failures.
const EXIT_AUTH: u8 = 3;

/// Returned when some (but not all) downloads failed during a sync.
#[derive(Debug, thiserror::Error)]
#[error("{0} downloads failed")]
struct PartialSyncError(usize);

/// Query available disk space on the filesystem containing `path`.
///
/// Returns `None` if the statvfs call fails (e.g. path doesn't exist yet).
#[cfg(unix)]
fn available_disk_space(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// Widen a platform-dependent statvfs field to u64. `as u64` is the only
    /// portable way since the underlying types (`c_ulong`, `fsblkcnt_t`) vary
    /// across targets.
    #[inline]
    fn widen(v: impl Into<u64>) -> u64 {
        v.into()
    }

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: statvfs is zeroed before the call. libc::statvfs writes into
    // the provided buffer and does not retain the pointer. c_path is valid
    // for the duration of the call.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &raw mut stat) != 0 {
            return None;
        }
        Some(widen(stat.f_bavail) * widen(stat.f_frsize))
    }
}

#[cfg(not(unix))]
fn available_disk_space(_path: &Path) -> Option<u64> {
    None
}

/// Build a password provider closure from a [`password::PasswordSource`].
///
/// The source is evaluated lazily on each call — for `Command` and `File`
/// sources, this re-executes/re-reads each time, supporting password rotation
/// and keeping no password in memory between auth cycles.
fn make_password_provider(source: password::PasswordSource) -> impl Fn() -> Option<SecretString> {
    move || match source.resolve() {
        Ok(pw) => pw,
        Err(e) => {
            tracing::error!(error = %e, "Password source resolution failed");
            None
        }
    }
}

/// Build a password provider from CLI password args, TOML config, and resolved auth fields.
///
/// Shared by `run_login`, `run_list`, and `run_import_existing`.
fn make_provider_from_auth(
    pw: &cli::PasswordArgs,
    password: Option<String>,
    username: &str,
    cookie_directory: &Path,
    toml: Option<&config::TomlConfig>,
) -> impl Fn() -> Option<SecretString> {
    let toml_auth = toml.and_then(|t| t.auth.as_ref());
    let password_command = config::resolve_password_command(pw, toml_auth);
    let password_file = config::resolve_password_file(pw, toml_auth);
    let source = password::build_password_source(
        password.map(SecretString::from).as_ref(),
        password_command.as_deref(),
        password_file.as_deref(),
        credential::CredentialStore::new(username, cookie_directory),
    );
    make_password_provider(source)
}

use commands::{
    attempt_reauth, init_photos_service, resolve_albums, resolve_libraries, run_config_show,
    run_import_existing, run_list, run_login, run_password, run_reset_state, run_reset_sync_token,
    run_status, run_verify, wait_and_retry_2fa, MAX_REAUTH_ATTEMPTS,
};

/// Get the database path for a given auth config, merging with TOML defaults.
///
/// Returns an error if the resolved username is empty, since an empty username
/// produces a `.db` filename that silently operates on the wrong database.
fn get_db_path(globals: &config::GlobalArgs, toml: Option<&TomlConfig>) -> anyhow::Result<PathBuf> {
    let (username, _, _, cookie_dir) =
        config::resolve_auth(globals, &cli::PasswordArgs::default(), toml);
    if username.is_empty() {
        anyhow::bail!(
            "--username is required (or set ICLOUD_USERNAME, or add username to config file)"
        );
    }
    Ok(cookie_dir.join(format!(
        "{}.db",
        auth::session::sanitize_username(&username)
    )))
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

/// Per-library state: zone name, sync token key, and resolved albums.
struct LibraryState {
    library: icloud::photos::PhotoLibrary,
    zone_name: String,
    sync_token_key: String,
    albums: Vec<icloud::photos::PhotoAlbum>,
    /// Asset IDs from excluded albums, used to filter out assets when
    /// `--exclude-album` is set without explicit `--album`.
    exclude_asset_ids: Arc<rustc_hash::FxHashSet<String>>,
}

fn main() -> ExitCode {
    // Snapshot and scrub the password env var while truly single-threaded,
    // before the tokio runtime creates worker threads.
    let env_password = std::env::var("ICLOUD_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    // SAFETY: no other threads exist yet — the tokio runtime has not been built.
    unsafe { std::env::remove_var("ICLOUD_PASSWORD") };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    match rt.block_on(run(env_password)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // 2FA required is not a failure — kei checked auth, told the user
            // what to do, and is done. Exit 0 so `restart: on-failure` won't
            // restart into a loop that hammers Apple's auth endpoints.
            if e.downcast_ref::<auth::error::AuthError>()
                .is_some_and(auth::error::AuthError::is_two_factor_required)
            {
                ExitCode::SUCCESS
            } else {
                eprintln!("Error: {e:#}");
                if e.downcast_ref::<PartialSyncError>().is_some() {
                    ExitCode::from(EXIT_PARTIAL)
                } else if e.downcast_ref::<auth::error::AuthError>().is_some() {
                    ExitCode::from(EXIT_AUTH)
                } else {
                    ExitCode::FAILURE
                }
            }
        }
    }
}

async fn run(env_password: Option<String>) -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // Migrate legacy icloudpd-rs paths before loading config, so the
    // copied config.toml is found at the new location.
    let migration_report = migration::migrate_legacy_paths();

    // Load TOML config early so it can influence log level.
    // If the user explicitly set --config, the file must exist.
    //
    // Docker fallback: when no --config is passed, the default
    // ~/.config/kei/config.toml may not exist inside a container (it
    // resolves to /root/.config/kei/config.toml). Try the Docker
    // convention /config/config.toml as a fallback so that `docker exec`
    // subcommands (get-code, submit-code, credential, etc.) automatically
    // find the same config the Docker CMD uses.
    const DOCKER_FALLBACK_CONFIG: &str = "/config/config.toml";
    let config_explicitly_set =
        cli.config != "~/.config/kei/config.toml" && cli.config != DOCKER_FALLBACK_CONFIG;
    let (config_path, used_docker_fallback) = {
        let expanded = config::expand_tilde(&cli.config);
        if !config_explicitly_set && !expanded.exists() {
            let docker = PathBuf::from(DOCKER_FALLBACK_CONFIG);
            if docker.exists() {
                (docker, true)
            } else {
                (expanded, false)
            }
        } else {
            (expanded, false)
        }
    };
    // When --config is explicitly set but the file doesn't exist, allow it
    // if the parent directory exists (auto-config will create the file).
    // Otherwise require the file to exist so typos in --config paths error.
    // When --config is explicit but the file doesn't exist and the parent
    // dir does exist, allow it (auto-config will create the file).
    let can_auto_create =
        !config_path.exists() && config_path.parent().is_some_and(std::path::Path::is_dir);
    let config_required = config_explicitly_set && !can_auto_create;
    let mut toml_config = config::load_toml_config(&config_path, config_required)?;

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
    let redact_password: Arc<std::sync::Mutex<Option<SecretString>>> =
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

    if used_docker_fallback {
        tracing::debug!(
            path = %config_path.display(),
            "Using Docker fallback config (default path not found)"
        );
    }

    // Build globals from CLI early (username, domain, data_dir, cookie_directory).
    let mut globals = config::GlobalArgs::from_cli(&cli);

    // Dispatch based on command
    let mut command = cli.effective_command();
    // Inject the password captured from env before the runtime started,
    // since we cleared ICLOUD_PASSWORD before Cli::parse() could see it.
    // Must happen before command dispatch so all subcommands (login,
    // list, etc.) receive the password, not just sync.
    command.inject_env_password(env_password);
    let (is_one_shot, pw, sync) = match command {
        Command::Status(args) => {
            return run_status(args, &globals, toml_config.as_ref()).await;
        }
        Command::Reset { what } => match what {
            cli::ResetCommand::State { yes } => {
                return run_reset_state(yes, &globals, toml_config.as_ref()).await;
            }
            cli::ResetCommand::SyncToken => {
                return run_reset_sync_token(&globals, toml_config.as_ref()).await;
            }
        },
        Command::Verify(args) => {
            return run_verify(args, &globals, toml_config.as_ref()).await;
        }
        Command::ImportExisting(args) => {
            return run_import_existing(args, &globals, toml_config.as_ref()).await;
        }
        Command::Login {
            password,
            subcommand,
        } => {
            return run_login(subcommand, &password, &globals, toml_config.as_ref()).await;
        }
        Command::Password { password, action } => {
            return run_password(action, &globals, &password, toml_config.as_ref());
        }
        Command::List {
            password,
            library,
            what,
        } => {
            return run_list(what, &password, library, &globals, toml_config.as_ref()).await;
        }
        Command::Config { action } => match action {
            cli::ConfigAction::Show => {
                return run_config_show(&globals, toml_config.as_ref());
            }
            cli::ConfigAction::Setup { output } => {
                let path = output.map_or_else(|| config_path.clone(), |o| config::expand_tilde(&o));
                match setup::run_setup(&path)? {
                    setup::SetupResult::SyncNow {
                        config_path: cfg_path,
                        env_path,
                    } => {
                        // Load .env into process environment for this session
                        let mut env_username = None;
                        let mut env_password = None;
                        if let Ok(contents) = tokio::fs::read_to_string(&env_path).await {
                            for line in contents.lines() {
                                if let Some((key, value)) = line.split_once('=') {
                                    let key = key.trim();
                                    // Strip surrounding single or double quotes
                                    // (the setup wizard single-quotes values to
                                    // prevent shell expansion when sourced).
                                    let value = value.trim();
                                    let value = value
                                        .strip_prefix('\'')
                                        .and_then(|v| v.strip_suffix('\''))
                                        .or_else(|| {
                                            value
                                                .strip_prefix('"')
                                                .and_then(|v| v.strip_suffix('"'))
                                        })
                                        .unwrap_or(value);
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
                        // Override globals with env values from setup
                        if let Some(u) = env_username {
                            globals.username = Some(u);
                        }
                        let sync_pw = cli::PasswordArgs {
                            password: env_password,
                            ..cli::PasswordArgs::default()
                        };
                        // Setup "sync now" is a one-shot initial sync, not a daemon.
                        (true, sync_pw, cli::SyncArgs::default())
                    }
                    setup::SetupResult::Done => return Ok(()),
                }
            }
        },
        Command::Sync { password, sync } => (sync.retry_failed, password, sync),
        // Legacy variants should never reach here (effective_command maps them)
        _ => unreachable!("legacy command variants should be mapped by effective_command()"),
    };
    let is_retry_failed = sync.retry_failed;
    let max_download_attempts = sync.max_download_attempts.unwrap_or(10);
    let reset_sync_token = sync.reset_sync_token;
    let toml_existed = toml_config.is_some();
    let cli_data_dir = globals
        .data_dir
        .clone()
        .or_else(|| globals.cookie_directory.clone());
    let mut config = config::Config::build(&globals, pw, sync, toml_config)?;

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
    harden_process();

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
            if let Some(db) = &state_db {
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

        if skip_cycle {
            // Skipped cycle (no changes detected) — still update health so
            // Docker HEALTHCHECK doesn't mark the container unhealthy after
            // the 2-hour staleness window when no new photos are uploaded.
            health.record_success();
            health.write(&config.cookie_directory);
        } else {
            sd_notifier.notify_status("Syncing...");
            sd_notifier.notify_watchdog();

            let mut cycle_failed_count = 0usize;
            let mut cycle_session_expired = false;

            for lib_state in &library_states {
                if shutdown_token.is_cancelled() {
                    break;
                }

                // Check if the download config changed since last sync. If so,
                // clear sync tokens so the subsequent lookup falls back to full
                // enumeration — the stored incremental token would miss assets
                // that are newly eligible under the changed config (e.g. a
                // user switching --size or adding --skip-videos).
                if !config.dry_run {
                    if let Some(db) = &state_db {
                        // Use a separate key from the download-path's "config_hash"
                        // (which tracks path-affecting fields only). This hash is a
                        // superset that also includes enumeration filters (albums,
                        // library, skip_live_photos). Using the same key would cause
                        // the two hashes to overwrite each other every cycle,
                        // permanently preventing incremental sync.
                        let config_hash = download::compute_config_hash(&config);
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
                            if let Err(e) = db.set_metadata("enum_config_hash", &config_hash).await
                            {
                                tracing::warn!(error = %e, "Failed to persist enum_config_hash");
                            }
                        }
                    }
                }

                // Determine sync mode per-library
                // retry-failed must always use full enumeration: incremental
                // sync only returns NEW iCloud changes, missing previously-
                // failed assets that were already enumerated but not downloaded.
                let sync_mode = if is_retry_failed || config.no_incremental {
                    if config.no_incremental && library_states.len() == 1 {
                        tracing::info!("Incremental sync disabled via --no-incremental, performing full enumeration");
                    }
                    if is_retry_failed {
                        tracing::info!("Retry-failed requires full enumeration to find previously-failed assets");
                    }
                    download::SyncMode::Full
                } else if let Some(db) = &state_db {
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
                // change events for failed assets — they'd never appear in the next delta.
                // Note: the token is stored after download_photos_with_sync returns, which
                // means all batch flushes are complete. A crash here means the token is
                // NOT advanced, so assets will replay on next sync (safe, not data loss).
                let should_store_token =
                    matches!(sync_result.outcome, download::DownloadOutcome::Success)
                        && !config.dry_run;
                if should_store_token {
                    if let Some(token) = &sync_result.sync_token {
                        if let Some(db) = &state_db {
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

            // Update health status for Docker HEALTHCHECK observability.
            if cycle_session_expired {
                health.record_failure("session expired");
            } else if cycle_failed_count > 0 {
                health.record_failure(&format!("{cycle_failed_count} downloads failed"));
            } else {
                health.record_success();
            }
            health.write(&config.cookie_directory);

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
                            .is_some_and(auth::error::AuthError::is_two_factor_required) =>
                    {
                        // 2FA is user action, not a failed attempt — don't
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
                    last_cycle_failed_count = cycle_failed_count;
                } else {
                    return Err(PartialSyncError(cycle_failed_count).into());
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
            // If the session is still valid, attempt_reauth returns without
            // re-locking, so we re-acquire the lock ourselves. If the session
            // is invalid, attempt_reauth creates a new Session with its own lock.
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

    #[test]
    fn redacting_writer_replaces_password() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
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

        let password: Arc<std::sync::Mutex<Option<SecretString>>> =
            Arc::new(std::sync::Mutex::new(None));
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

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from(
            String::new(),
        ))));
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
        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from(
            "longpassword",
        ))));
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

        let password: Arc<std::sync::Mutex<Option<SecretString>>> =
            Arc::new(std::sync::Mutex::new(None));
        let mut buf = Vec::new();
        let mut writer = RedactingWriter {
            inner: &mut buf,
            password,
        };
        writer.flush().unwrap();
    }

    #[test]
    fn make_password_provider_with_direct_source() {
        let source = password::PasswordSource::Direct(Arc::new(SecretString::from("mypass")));
        let provider = make_password_provider(source);
        let result = provider().unwrap();
        assert_eq!(result.expose_secret(), "mypass");
        // Can be called multiple times
        let result2 = provider().unwrap();
        assert_eq!(result2.expose_secret(), "mypass");
    }

    #[test]
    fn make_password_provider_with_command_source() {
        let source = password::PasswordSource::Command("echo cmd_test".to_string());
        let provider = make_password_provider(source);
        let result = provider().unwrap();
        assert_eq!(result.expose_secret(), "cmd_test");
    }

    // ── Watch-mode control flow tests ──────────────────────────────────

    use tokio_util::sync::CancellationToken;

    /// Run the watch-loop pattern and return how many cycles completed.
    async fn run_watch_loop(
        shutdown_token: &CancellationToken,
        watch_with_interval: Option<u64>,
    ) -> u32 {
        let mut cycles = 0u32;
        loop {
            if shutdown_token.is_cancelled() {
                break;
            }
            cycles += 1;
            if let Some(interval) = watch_with_interval {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                    _ = shutdown_token.cancelled() => { break; }
                }
            } else {
                break;
            }
        }
        cycles
    }

    /// The watch loop uses `tokio::select!` to make the inter-cycle sleep
    /// interruptible by a shutdown signal. Cancellation breaks out promptly
    /// despite a long interval.
    #[tokio::test]
    async fn watch_sleep_exits_promptly_on_shutdown() {
        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token_clone.cancel();
        });

        let start = std::time::Instant::now();
        let cycles = run_watch_loop(&shutdown_token, Some(3600)).await;

        assert_eq!(cycles, 1);
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
    }

    /// A pre-cancelled token prevents any cycle from starting.
    #[test]
    fn watch_loop_skips_cycle_when_already_cancelled() {
        let shutdown_token = CancellationToken::new();
        shutdown_token.cancel();

        let mut cycles_started = 0u32;
        loop {
            if shutdown_token.is_cancelled() {
                break;
            }
            cycles_started += 1;
        }
        assert_eq!(cycles_started, 0);
    }

    /// When `watch_with_interval` is None the loop executes exactly once.
    #[tokio::test]
    async fn watch_loop_runs_once_without_interval() {
        let shutdown_token = CancellationToken::new();
        assert_eq!(run_watch_loop(&shutdown_token, None).await, 1);
    }

    /// Shutdown during inter-cycle sleep completes exactly one cycle.
    #[tokio::test]
    async fn watch_loop_completes_one_cycle_then_exits_on_shutdown() {
        let shutdown_token = CancellationToken::new();
        let token_clone = shutdown_token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            token_clone.cancel();
        });
        assert_eq!(run_watch_loop(&shutdown_token, Some(3600)).await, 1);
    }
}
