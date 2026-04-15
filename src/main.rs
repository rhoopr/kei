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
mod retry;
mod setup;
mod shutdown;
mod state;
mod sync_loop;
mod systemd;
mod types;

#[cfg(test)]
mod test_helpers;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

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
    run_config_show, run_import_existing, run_list, run_login, run_password, run_reset_state,
    run_reset_sync_token, run_status, run_verify,
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

    // Resolve log level: CLI > TOML > default (warn)
    let effective_log_level = cli
        .log_level
        .or_else(|| toml_config.as_ref().and_then(|t| t.log_level))
        .unwrap_or(types::LogLevel::Warn);

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
    sync_loop::run_sync(
        &globals,
        sync_loop::SyncArgs {
            is_one_shot,
            pw,
            sync,
            toml_config,
            config_explicitly_set,
            config_path: config_path.clone(),
            redact_password: Arc::clone(&redact_password),
        },
    )
    .await
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
