//! kei: iCloud Photos sync engine.
//!
//! Moves photos and videos from iCloud Photos to local storage. Authentication
//! uses SRP-6a with Apple's custom variant followed by optional 2FA, and assets
//! are streamed from `CloudKit` with exponential-backoff retries on transient
//! failures.
//!
//! Lint configuration lives in `[lints.clippy]` in `Cargo.toml`.

// Test code is exempt from the panic-footgun, logging-hygiene, and
// numeric-cast lints that prod code enforces: unwrap/expect/panic are
// idiomatic in tests, a few tests write to stderr for failure diagnostics,
// and test fixtures commonly use `as` casts on values known to fit.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::print_stderr,
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::indexing_slicing,
    )
)]

mod auth;
mod cli;
mod commands;
mod config;
mod credential;
mod cycle_reporter;
mod download;
mod fs_util;
mod health;
mod icloud;
mod metrics;
mod notifications;
mod password;
mod personality;
mod report;
mod retry;
mod selection;
mod service;
mod setup;
mod shutdown;
mod state;
mod sync_cycle;
mod sync_loop;
mod systemd;
mod types;
mod upgrade_hints;

#[cfg(test)]
mod test_helpers;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use password::{ExposeSecret, SecretString};

/// Whether this process can ask the operator for input.
///
/// Detect this once before the async runtime starts, then pass the result to
/// command owners. This prevents prompt and wait policy from drifting when a
/// command runs under cron, CI, Docker, or a service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputMode {
    Interactive,
    NoInput,
}

impl InputMode {
    #[must_use]
    fn detect() -> Self {
        use std::io::IsTerminal;

        if std::io::stdin().is_terminal() {
            Self::Interactive
        } else {
            Self::NoInput
        }
    }

    #[must_use]
    pub(crate) const fn can_prompt(self) -> bool {
        matches!(self, Self::Interactive)
    }
}

/// A writer wrapper that redacts a password string from log output.
///
/// Wraps any `io::Write` implementor and replaces occurrences of the
/// configured password with `********`, including secrets split across
/// adjacent `write()` calls.
struct RedactingWriter<W: std::io::Write> {
    inner: W,
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
    pending: Vec<u8>,
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let password = self.current_password();

        // Fast path: short-circuit before allocating a `String`. Under
        // trace-level logging the redaction path runs on every event,
        // and `String::from_utf8_lossy` per event dominates the heap churn.
        let Some(pw) = password.as_deref() else {
            self.flush_pending_without_redaction()?;
            self.inner.write_all(buf)?;
            return Ok(buf.len());
        };
        let pw_bytes = pw.as_bytes();
        if pw_bytes.is_empty() || buf.len() < pw_bytes.len() {
            self.pending.extend_from_slice(buf);
            self.flush_redacted_pending(pw_bytes, false)?;
            return Ok(buf.len());
        }
        self.pending.extend_from_slice(buf);
        self.flush_redacted_pending(pw_bytes, false)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let password = self.current_password();
        if let Some(pw) = password.as_deref() {
            self.flush_redacted_pending(pw.as_bytes(), true)?;
        } else {
            self.flush_pending_without_redaction()?;
        }
        self.inner.flush()
    }
}

impl<W: std::io::Write> RedactingWriter<W> {
    fn current_password(&self) -> Option<String> {
        self.password
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|pw| pw.expose_secret().to_string())
    }

    fn flush_pending_without_redaction(&mut self) -> std::io::Result<()> {
        if !self.pending.is_empty() {
            self.inner.write_all(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }

    fn flush_redacted_pending(&mut self, pw_bytes: &[u8], flush_all: bool) -> std::io::Result<()> {
        if pw_bytes.is_empty() {
            return self.flush_pending_without_redaction();
        }

        let hold_back = if flush_all {
            0
        } else {
            pw_bytes.len().saturating_sub(1)
        };
        let mut emit_cutoff = self.pending.len().saturating_sub(hold_back);
        if emit_cutoff == 0 {
            return Ok(());
        }

        let mut output = Vec::with_capacity(emit_cutoff);
        let mut consumed = 0usize;
        let mut search_from = 0usize;
        while let Some(search) = self.pending.get(search_from..) {
            let Some(pos) = search
                .windows(pw_bytes.len())
                .position(|candidate| candidate == pw_bytes)
            else {
                break;
            };
            let match_start = search_from + pos;
            let match_end = match_start + pw_bytes.len();
            if match_start >= emit_cutoff {
                break;
            }
            if match_end > emit_cutoff {
                emit_cutoff = match_start;
                break;
            }
            if let Some(prefix) = self.pending.get(consumed..match_start) {
                output.extend_from_slice(prefix);
            }
            output.extend_from_slice(b"********");
            consumed = match_end;
            search_from = match_end;
        }

        if let Some(rest) = self.pending.get(consumed..emit_cutoff) {
            output.extend_from_slice(rest);
        }
        if !output.is_empty() {
            self.inner.write_all(&output)?;
        }
        self.pending.drain(..emit_cutoff);
        Ok(())
    }
}

impl<W: std::io::Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        let _ = <Self as std::io::Write>::flush(self);
    }
}

/// A `MakeWriter` implementation that produces `RedactingWriter` instances
/// fronting the non-blocking channel that wraps stderr.
struct RedactingMakeWriter {
    password: Arc<std::sync::Mutex<Option<SecretString>>>,
    inner: tracing_appender::non_blocking::NonBlocking,
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RedactingMakeWriter {
    type Writer = RedactingWriter<tracing_appender::non_blocking::NonBlocking>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.clone(),
            password: Arc::clone(&self.password),
            pending: Vec::new(),
        }
    }
}

/// `Write` impl that funnels stderr through `MultiProgress::suspend` so a
/// tracing event mid-redraw doesn't trample the active progress bar's ANSI
/// cursor positioning. Cheap when no bars are registered (suspend is a
/// passthrough); essential when a multi-line friendly bar is on screen.
///
/// Stays on `suspend` (rather than `println`) because the pipeline calls
/// `pb.suspend(|| tracing::warn!(...))` while already holding indicatif's
/// `MultiProgress` write lock - a `println` call from inside the closure
/// would re-enter that same `RwLock` and deadlock. Narration outside that
/// context (greeting, final summary, stop-signal) routes through `println`
/// directly via `personality::active_bar::println_above_bars`.
pub(crate) struct BarSuspendingStderr;

impl std::io::Write for BarSuspendingStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        personality::active_bar::with_suspended(|| std::io::stderr().write(buf))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        personality::active_bar::with_suspended(|| std::io::stderr().flush())
    }
}

/// Build the redacting log-writer pipeline `run` installs.
///
/// `lossy(true)` so producers never park on the background writer; the
/// returned `WorkerGuard` must outlive every `tracing::*` call so its
/// `Drop` can join the worker and drain the channel before teardown.
/// The password slot starts empty and is populated once the password
/// is known.
pub(crate) fn build_redacting_writer<W>(
    sink: W,
) -> (
    impl for<'a> tracing_subscriber::fmt::MakeWriter<'a> + 'static,
    tracing_appender::non_blocking::WorkerGuard,
    Arc<std::sync::Mutex<Option<SecretString>>>,
)
where
    W: std::io::Write + Send + 'static,
{
    let password: Arc<std::sync::Mutex<Option<SecretString>>> =
        Arc::new(std::sync::Mutex::new(None));
    let (non_blocking, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .finish(sink);
    let make_writer = RedactingMakeWriter {
        password: Arc::clone(&password),
        inner: non_blocking,
    };
    (make_writer, guard, password)
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

/// Exit code for partial sync (some sync failures occurred, but sync was not a total failure).
const EXIT_PARTIAL: u8 = 2;
/// Exit code for authentication failures.
const EXIT_AUTH: u8 = 3;
/// Exit code for terminal Apple authentication states that need operator action.
const EXIT_TERMINAL_AUTH: u8 = 4;

/// Returned when some (but not all) sync failures occurred during a cycle.
#[derive(Debug, thiserror::Error)]
#[error("{0} sync failures")]
struct PartialSyncError(usize);

/// Maps a fatal `Err` from `run` to an exit code and decides whether to log it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitClassification {
    Partial,
    Auth,
    TerminalAuth,
    Other,
}

impl ExitClassification {
    const fn exit_code(self) -> u8 {
        match self {
            Self::Partial => EXIT_PARTIAL,
            Self::Auth => EXIT_AUTH,
            Self::TerminalAuth => EXIT_TERMINAL_AUTH,
            Self::Other => 1,
        }
    }

    const fn should_log(self) -> bool {
        true
    }

    const fn should_suggest_diagnostics(self) -> bool {
        matches!(self, Self::Other)
    }
}

fn classify_exit_error(e: &anyhow::Error) -> ExitClassification {
    if e.downcast_ref::<auth::error::AuthError>()
        .is_some_and(auth::error::AuthError::is_two_factor_required)
    {
        ExitClassification::Auth
    } else if e
        .downcast_ref::<auth::error::AuthError>()
        .is_some_and(auth::error::AuthError::is_terminal_apple_auth)
    {
        ExitClassification::TerminalAuth
    } else if e.downcast_ref::<PartialSyncError>().is_some() {
        ExitClassification::Partial
    } else if e.downcast_ref::<auth::error::AuthError>().is_some() {
        ExitClassification::Auth
    } else {
        ExitClassification::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliParseRenderStream {
    Stdout,
    Stderr,
}

#[derive(Debug, PartialEq, Eq)]
struct CliParseExit<'a> {
    rendered: &'a str,
    code: u8,
    stream: CliParseRenderStream,
}

/// Classify clap/CLI parser exits before normal runtime error handling.
///
/// Help/version are successful parse exits that must print to stdout. Invalid
/// arguments must preserve clap's stderr routing and concrete exit code. Keep
/// this separate from [`classify_exit_error`] so CLI parse behavior does not
/// accidentally fall through to the generic `anyhow` exit classifier.
fn classify_cli_parse_exit(e: &anyhow::Error) -> Option<CliParseExit<'_>> {
    let parse_err = e.downcast_ref::<cli::ParseCliError>()?;
    Some(CliParseExit {
        rendered: parse_err.rendered(),
        code: u8::try_from(parse_err.exit_code()).unwrap_or(1),
        stream: if parse_err.use_stderr() {
            CliParseRenderStream::Stderr
        } else {
            CliParseRenderStream::Stdout
        },
    })
}

#[expect(
    clippy::string_slice,
    reason = "floor_char_boundary guarantees a valid char boundary"
)]
pub(crate) fn truncate_str(s: &str, max_bytes: usize) -> &str {
    &s[..s.floor_char_boundary(max_bytes)]
}

/// Query available disk space on the filesystem containing `path`.
///
/// Returns `None` if the statvfs call fails (e.g. path doesn't exist yet).
#[cfg(unix)]
pub(crate) fn available_disk_space(path: &Path) -> Option<u64> {
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
    // SAFETY: zeroed is valid for libc::statvfs (all-zero bit pattern is a
    // valid struct — every field is an integer type).
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated C string that outlives the
    // call. statvfs writes into the provided buffer and does not retain the
    // pointer.
    if unsafe { libc::statvfs(c_path.as_ptr(), &raw mut stat) } != 0 {
        return None;
    }
    Some(widen(stat.f_bavail) * widen(stat.f_frsize))
}

#[cfg(windows)]
pub(crate) fn available_disk_space(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide_path: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free_bytes_available = 0u64;
    // SAFETY: `wide_path` is a NUL-terminated UTF-16 string that outlives
    // the call. The output pointers are valid for writes for the duration of
    // the call, and null total-byte pointers are allowed by the Win32 API.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &raw mut free_bytes_available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free_bytes_available)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn available_disk_space(_path: &Path) -> Option<u64> {
    None
}

/// Minimum free disk space (1 GiB) required before kei is allowed to start a
/// sync. Below this, even a moderate-size video could push the filesystem
/// past full mid-write (which is a kei-data-sacred risk: torn writes,
/// truncated `.part` files, no clean rename target).
pub(crate) const MIN_FREE_BYTES: u64 = 1_073_741_824;

/// Bail when `available_bytes` is below [`MIN_FREE_BYTES`]. Pure / synchronous
/// so it can be unit-tested without statvfs or a real filesystem.
///
/// Production callers compute `available_bytes` from
/// [`available_disk_space`] (or any future probe) and forward it here so the
/// abort message is identical regardless of platform.
pub(crate) fn check_min_disk_space(available_bytes: u64, directory: &Path) -> anyhow::Result<()> {
    if available_bytes < MIN_FREE_BYTES {
        let avail_mb = available_bytes / (1024 * 1024);
        anyhow::bail!(
            "Not enough disk space: only {avail_mb} MiB is available in {}. kei needs at least 1 GiB.",
            directory.display()
        );
    }
    Ok(())
}

/// Build a password provider closure from a [`password::PasswordSource`].
///
/// The source is evaluated lazily on each call — for `Command` and `File`
/// sources, this re-executes/re-reads each time, supporting password rotation
/// and keeping no password in memory between auth cycles.
///
/// The closure is wrapped in `Arc<dyn Fn + Send + Sync>` so the async auth
/// path can dispatch invocations through `spawn_blocking` (see
/// [`password::invoke_password_provider`]) instead of calling the
/// blocking `resolve()` directly on a tokio worker.
fn make_password_provider(
    source: password::PasswordSource,
    input_mode: InputMode,
) -> password::PasswordProvider {
    std::sync::Arc::new(move || match source.resolve_in_mode(input_mode) {
        Ok(pw) => pw,
        Err(e) => {
            tracing::error!(error = %e, "Password source resolution failed");
            None
        }
    })
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
    input_mode: InputMode,
) -> password::PasswordProvider {
    let toml_auth = toml.and_then(|t| t.auth.as_ref());
    let password_command = config::resolve_password_command(pw, toml_auth);
    let password_file = config::resolve_password_file(pw, toml_auth);
    let source = password::build_password_source(
        password.map(SecretString::from).as_ref(),
        password_command.as_deref(),
        password_file.as_deref(),
        credential::CredentialStore::new(username, cookie_directory),
    );
    make_password_provider(source, input_mode)
}

use commands::{
    run_config_show, run_doctor, run_import_existing, run_list, run_login, run_manifest,
    run_password, run_reconcile, run_reset_session, run_reset_state, run_reset_sync_token,
    run_status, run_verify,
};

/// Get the database path for a given auth config, merging with TOML defaults.
///
/// Returns an error if the resolved username is empty, since an empty username
/// produces a `.db` filename that silently operates on the wrong database.
fn get_db_path(globals: &config::GlobalArgs, toml: Option<&TomlConfig>) -> anyhow::Result<PathBuf> {
    let (username, _, _, cookie_dir) =
        config::resolve_auth(globals, &cli::PasswordArgs::default(), toml);
    if username.is_empty() {
        anyhow::bail!("Set your iCloud username with ICLOUD_USERNAME or [auth].username.");
    }
    Ok(cookie_dir.join(format!(
        "{}.db",
        auth::session::sanitize_username(&username)
    )))
}

/// RAII guard that writes the current PID to a file on creation and removes
/// it when dropped.
#[derive(Debug)]
struct PidFileGuard {
    path: PathBuf,
}

impl PidFileGuard {
    fn new(path: PathBuf) -> std::io::Result<Self> {
        // If a prior PID file exists, validate whether the recorded process
        // is still alive. Alive → bail; dead/unparsable → treat as stale
        // and overwrite.
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Some(existing) = contents.trim().parse::<i32>().ok().filter(|p| *p > 0) {
                if pid_is_alive(existing) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        format!(
                            "PID file {} refers to running process {existing}; refusing to start a second instance",
                            path.display()
                        ),
                    ));
                }
                tracing::warn!(
                    path = %path.display(),
                    stale_pid = existing,
                    "PID file references a non-running process; overwriting as stale"
                );
            } else {
                tracing::warn!(
                    path = %path.display(),
                    "PID file contents unparsable; overwriting as stale"
                );
            }
        }
        std::fs::write(&path, std::process::id().to_string())?;
        tracing::debug!(path = %path.display(), "PID file created");
        Ok(Self { path })
    }
}

/// Return whether the PID corresponds to a running process.
///
/// Uses `kill(pid, 0)` on Unix: 0 = alive; ESRCH = dead; EPERM = alive
/// (exists but outside our signalling permissions — still a live process).
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 performs permission / existence checks only
    // and never delivers a signal.
    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

#[cfg(not(unix))]
fn pid_is_alive(_pid: i32) -> bool {
    // Windows PID reuse happens fast enough that a cheap "is PID alive" check
    // without a process-handle lookup can return false-alives for totally
    // unrelated processes. Report dead so stale PID files are overwritten,
    // trading duplicate-run protection (which the OS filesystem lock in
    // PidFileGuard::new still backstops) for avoiding spurious refusals.
    false
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::debug!(path = %self.path.display(), error = %e, "Failed to remove PID file");
        }
    }
}

/// Binary entry point. Lives here so the binary at `src/main.rs` is a
/// no-logic shim; everything else - module tree, helpers, run() - is part
/// of the lib so it's reachable from integration tests, fuzz harnesses,
/// and any future companion binaries.
pub fn main_inner() -> ExitCode {
    let input_mode = InputMode::detect();

    // Snapshot and scrub the password env var while truly single-threaded,
    // before the tokio runtime creates worker threads.
    let env_password = std::env::var("ICLOUD_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty());
    // SAFETY: no other threads exist yet — the tokio runtime has not been built.
    unsafe { std::env::remove_var("ICLOUD_PASSWORD") };

    #[allow(
        clippy::expect_used,
        reason = "startup failure: no runtime means nothing can run"
    )]
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    match rt.block_on(run(env_password, input_mode)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(parse_exit) = classify_cli_parse_exit(&e) {
                #[allow(clippy::print_stdout, reason = "clap routes help/version to stdout")]
                #[allow(clippy::print_stderr, reason = "clap routes parse failures to stderr")]
                match parse_exit.stream {
                    CliParseRenderStream::Stdout => print!("{}", parse_exit.rendered),
                    CliParseRenderStream::Stderr => eprint!("{}", parse_exit.rendered),
                }
                return ExitCode::from(parse_exit.code);
            }

            let classification = classify_exit_error(&e);
            if classification.should_log() {
                // Route the final error through tracing so it carries the same
                // timestamp + level prefix as the rest of the logs; makes
                // `docker logs` / `journalctl` output correlate cleanly.
                // Also echo to stderr unconditionally as a fallback for early
                // failures before `tracing_subscriber::fmt().init()` runs.
                tracing::error!(error = format!("{e:#}"), "kei exited with error");
                #[allow(
                    clippy::print_stderr,
                    reason = "fallback for failures that happen before tracing subscriber is installed"
                )]
                {
                    eprintln!("Error: {e:#}");
                    if classification.should_suggest_diagnostics() {
                        eprintln!();
                        eprintln!("Run `kei doctor --json` for a redacted diagnostic report.");
                        eprintln!("Report bugs: https://github.com/rhoopr/kei/issues");
                    }
                }
            }
            ExitCode::from(classification.exit_code())
        }
    }
}

async fn run(env_password: Option<String>, input_mode: InputMode) -> anyhow::Result<()> {
    let cli = cli::parse_cli_with_sources(std::env::args_os()).map_err(|e| anyhow::anyhow!(e))?;
    let config = load_startup_config(&cli)?;
    let output = resolve_startup_output(&cli, config.toml.as_ref());

    // Keep the guard until dispatch returns so fast commands drain their logs.
    // BarSuspendingStderr pauses progress rendering around each log write.
    let (make_writer, _writer_guard, redact_password) = build_redacting_writer(BarSuspendingStderr);
    initialize_logging(&output, make_writer);
    if config.used_docker_fallback {
        tracing::debug!(
            path = %config.path.display(),
            "Using Docker fallback config (default path not found)"
        );
    }

    let globals = config::GlobalArgs::from_bootstrap_env();
    let mut command = cli.command;
    // Restore the password scrubbed before runtime creation for every command.
    command.inject_env_password(env_password);
    dispatch_command(
        command,
        &globals,
        config,
        output,
        redact_password,
        input_mode,
    )
    .await
}

/// Config discovery facts and the load result, including recoverable doctor errors.
struct StartupConfig {
    path: PathBuf,
    explicitly_set: bool,
    used_docker_fallback: bool,
    toml: Option<TomlConfig>,
    load_error: Option<String>,
}

const DOCKER_FALLBACK_CONFIG: &str = "/config/config.toml";

fn load_startup_config(cli: &cli::Cli) -> anyhow::Result<StartupConfig> {
    // Load TOML config early so it can influence log level.
    //
    // Docker fallback: when no --config is passed, the default
    // ~/.config/kei/config.toml may not exist inside a container (it
    // resolves to /root/.config/kei/config.toml). Try the Docker
    // convention /config/config.toml as a fallback so that `docker exec`
    // subcommands (get-code, submit-code, credential, etc.) automatically
    // find the same config the Docker CMD uses.
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
    // When --config is explicit but the file doesn't exist and the parent
    // dir does exist, allow it (auto-config will create the file).
    // Otherwise require the file to exist so typos in --config paths error.
    let can_auto_create =
        !config_path.exists() && config_path.parent().is_some_and(std::path::Path::is_dir);
    let config_required = config_explicitly_set && !can_auto_create;
    let is_doctor_command = matches!(cli.command, cli::Command::Doctor(_));
    let (toml_config, toml_config_error) =
        match config::load_toml_config(&config_path, config_required) {
            Ok(config) => (config, None),
            Err(e) if is_doctor_command => (None, Some(e.to_string())),
            Err(e) => return Err(e),
        };

    Ok(StartupConfig {
        path: config_path,
        explicitly_set: config_explicitly_set,
        used_docker_fallback,
        toml: toml_config,
        load_error: toml_config_error,
    })
}

/// Output policy resolved before logging and command dispatch.
struct StartupOutput {
    personality_mode: personality::Mode,
    friendly_request: Option<bool>,
    default_filter: String,
}

#[must_use]
fn resolve_log_filter(cli: &cli::Cli, toml_config: Option<&TomlConfig>) -> (&'static str, bool) {
    // Resolve log level: --log-level > --verbose > TOML > default (info).
    // `--verbose` is a friendlier alias for `--log-level info` and is
    // overridden if `--log-level` is also explicitly set.
    let cli_log_level = cli.log_level.or(if cli.verbose {
        Some(types::LogLevel::Info)
    } else {
        None
    });
    let log_level_explicit = cli_log_level.is_some();
    let effective_log_level = cli_log_level
        .or_else(|| toml_config.and_then(|t| t.log_level))
        .unwrap_or(types::LogLevel::Info);

    // Scope debug/info to the app crate so dependency crates stay quieter.
    // Users can override with RUST_LOG env var for full control.
    let off_filter = match effective_log_level {
        types::LogLevel::Debug => "kei=debug,info",
        types::LogLevel::Info => "kei=info",
        types::LogLevel::Warn => "warn",
        types::LogLevel::Error => "error",
    };

    (off_filter, log_level_explicit)
}

#[must_use]
fn resolve_startup_output(cli: &cli::Cli, toml_config: Option<&TomlConfig>) -> StartupOutput {
    let (off_filter, log_level_explicit) = resolve_log_filter(cli, toml_config);
    // Resolve friendly mode. The gate has multiple short-circuits (service
    // context, non-TTY, RUST_LOG, machine-output mode, ...) so the user-stated
    // preference is a request, not a guarantee.
    //
    // Resolution chain: CLI > TOML > default-on-for-TTY. The gate then
    // clamps to Off in any environment that can't render or shouldn't
    // (non-TTY, journals, machine-output flags). Default-on means the
    // setup wizard's question and the TOML key are opt-out levers; first
    // contact with kei on a plain terminal already gets the friendly UX.
    //
    let toml_report_json = toml_config
        .and_then(|t| t.report.as_ref())
        .and_then(|r| r.json.as_ref())
        .is_some();
    let (cmd_no_progress_bar, cmd_only_print_filenames, cmd_report_json, cmd_service_run) =
        match &cli.command {
            cli::Command::Sync { sync, .. } => (
                sync.no_progress_bar,
                sync.only_print_filenames,
                toml_report_json,
                false,
            ),
            cli::Command::Service { .. } => (false, false, toml_report_json, true),
            _ => (false, false, false, false),
        };
    let personality_ctx = personality::Context::detect(
        cmd_no_progress_bar,
        cmd_only_print_filenames,
        cmd_report_json,
        log_level_explicit,
        cmd_service_run,
    );
    let toml_friendly = toml_config
        .and_then(|t| t.ui.as_ref())
        .and_then(|u| u.friendly);
    let cli_friendly = cli.friendly_request();
    let friendly_request = cli_friendly.or(toml_friendly);
    let personality_mode =
        personality::resolve_with_request(cli_friendly, toml_friendly, &personality_ctx);
    let default_filter = personality::tracing::default_filter_for(personality_mode, off_filter);

    StartupOutput {
        personality_mode,
        friendly_request,
        default_filter,
    }
}

fn initialize_logging(
    output: &StartupOutput,
    make_writer: impl for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
) {
    let env_filter = personality::tracing::env_filter(&output.default_filter);
    if output.personality_mode.is_friendly() {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(make_writer)
            .with_target(false)
            .with_level(false)
            .without_time()
            .event_format(personality::tracing::FriendlyFormat)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(make_writer)
            .init();
    }
}

async fn dispatch_command(
    command: Command,
    globals: &config::GlobalArgs,
    config: StartupConfig,
    output: StartupOutput,
    redact_password: Arc<std::sync::Mutex<Option<SecretString>>>,
    input_mode: InputMode,
) -> anyhow::Result<()> {
    let StartupConfig {
        path: config_path,
        explicitly_set: config_explicitly_set,
        toml: mut toml_config,
        load_error: toml_config_error,
        ..
    } = config;
    let StartupOutput {
        personality_mode,
        friendly_request,
        ..
    } = output;
    let (is_one_shot, pw, sync) = match command {
        Command::Status(args) => {
            return run_status(args, globals, toml_config.as_ref()).await;
        }
        Command::Doctor(args) => {
            return run_doctor(
                args,
                globals,
                toml_config.as_ref(),
                &config_path,
                toml_config_error,
            )
            .await;
        }
        Command::Manifest(args) => {
            return run_manifest(args, globals, toml_config.as_ref()).await;
        }
        Command::Reset { what } => match what {
            cli::ResetCommand::State { yes } => {
                return run_reset_state(yes, globals, toml_config.as_ref()).await;
            }
            cli::ResetCommand::SyncToken { yes } => {
                return run_reset_sync_token(yes, globals, toml_config.as_ref(), input_mode).await;
            }
            cli::ResetCommand::Session { yes } => {
                return run_reset_session(yes, globals, toml_config.as_ref()).await;
            }
        },
        Command::Verify(args) => {
            return run_verify(args, globals, toml_config.as_ref()).await;
        }
        Command::Reconcile(args) => {
            return run_reconcile(args, globals, toml_config.as_ref()).await;
        }
        Command::ImportExisting(args) => {
            return run_import_existing(args, globals, toml_config.as_ref(), input_mode).await;
        }
        Command::Login {
            password,
            subcommand,
        } => {
            return run_login(
                subcommand,
                &password,
                globals,
                toml_config.as_ref(),
                input_mode,
            )
            .await;
        }
        Command::Password { password, action } => {
            return run_password(action, globals, &password, toml_config.as_ref(), input_mode);
        }
        Command::List {
            password,
            libraries,
            what,
        } => {
            return run_list(
                what,
                &password,
                libraries,
                globals,
                toml_config.as_ref(),
                input_mode,
            )
            .await;
        }
        Command::Config { action } => match action {
            cli::ConfigAction::Show => {
                return run_config_show(globals, toml_config.as_ref());
            }
            cli::ConfigAction::Setup { output } => {
                let path = output.map_or_else(|| config_path.clone(), |o| config::expand_tilde(&o));
                match setup::run_setup(&path, input_mode)? {
                    setup::SetupResult::SyncNow {
                        config_path: cfg_path,
                        one_shot_password,
                    } => {
                        // Reload TOML from the newly written config
                        toml_config = config::load_toml_config(&cfg_path, true)?;
                        let sync_pw = cli::PasswordArgs {
                            password: one_shot_password.map(|p| p.expose_secret().to_string()),
                            ..cli::PasswordArgs::default()
                        };
                        // Setup "sync now" is a one-shot initial sync, not a daemon.
                        (true, sync_pw, cli::SyncArgs::default())
                    }
                    setup::SetupResult::Done => return Ok(()),
                }
            }
        },
        Command::Install(args) => {
            return service::install::run(args, &config_path).await;
        }
        Command::Uninstall(args) => {
            return service::uninstall::run(args).await;
        }
        Command::Service { action } => match action {
            cli::ServiceAction::Status => return service::status::run().await,
            cli::ServiceAction::Run(args) => {
                let cli::ServiceRunArgs { password, sync } = *args;
                return Box::pin(service::run::run(
                    globals,
                    sync_loop::SyncArgs {
                        is_one_shot: false,
                        service_mode: true,
                        pw: password,
                        sync,
                        toml_config,
                        config_explicitly_set,
                        config_path,
                        redact_password,
                        // service run is hard-off per gate; resolved mode is
                        // already Off here, but pass through for symmetry.
                        personality_mode,
                        friendly_request,
                        input_mode,
                    },
                ))
                .await;
            }
        },
        Command::Sync { password, sync, .. } => (sync.retry_failed, password, sync),
    };
    Box::pin(sync_loop::run_sync(
        globals,
        sync_loop::SyncArgs {
            is_one_shot,
            service_mode: false,
            pw,
            sync,
            toml_config,
            config_explicitly_set,
            config_path,
            redact_password,
            personality_mode,
            friendly_request,
            input_mode,
        },
    ))
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::EnvFilter;

    fn startup_cli() -> cli::Cli {
        cli::Cli {
            config: String::new(),
            log_level: None,
            verbose: false,
            command: Command::Config {
                action: cli::ConfigAction::Show,
            },
        }
    }

    #[test]
    fn startup_config_loads_explicit_file_before_output_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "log_level = 'warn'\n[ui]\nfriendly = false\n").unwrap();
        let cli = cli::Cli {
            config: path.to_string_lossy().into_owned(),
            ..startup_cli()
        };
        let config = load_startup_config(&cli).unwrap();
        assert_eq!(config.path, path);
        assert!(config.explicitly_set);
        assert!(!config.used_docker_fallback);
        assert!(config.load_error.is_none());
        let output = resolve_startup_output(&cli, config.toml.as_ref());
        assert_eq!(output.default_filter, "warn");
        assert_eq!(output.personality_mode, personality::Mode::Off);
        assert_eq!(output.friendly_request, Some(false));
    }

    #[test]
    fn startup_config_allows_missing_file_only_when_parent_exists() {
        let dir = tempfile::tempdir().unwrap();
        let mut cli = startup_cli();
        cli.config = dir
            .path()
            .join("config.toml")
            .to_string_lossy()
            .into_owned();
        let config = load_startup_config(&cli).unwrap();
        assert!(config.explicitly_set);
        assert!(config.toml.is_none());
        assert!(config.load_error.is_none());
        assert!(!config.path.exists(), "discovery must not create config");
        cli.config = dir
            .path()
            .join("missing/config.toml")
            .to_string_lossy()
            .into_owned();
        let err = load_startup_config(&cli).err().unwrap();
        assert!(err.to_string().contains("Could not read config file"));
    }

    #[test]
    fn startup_config_retains_parse_error_only_for_doctor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "broken = [").unwrap();
        let mut cli = cli::Cli {
            config: path.to_string_lossy().into_owned(),
            ..startup_cli()
        };
        let err = load_startup_config(&cli).err().unwrap().to_string();
        assert!(err.contains("Failed to parse config file"));
        cli.command = Command::Doctor(cli::DoctorArgs {
            json: true,
            live: false,
        });
        let config = load_startup_config(&cli).unwrap();
        assert_eq!(config.path, path);
        assert!(config.toml.is_none());
        assert_eq!(config.load_error.as_deref(), Some(err.as_str()));
    }

    #[test]
    fn startup_log_filter_preserves_precedence_and_explicitness() {
        use types::LogLevel::{Debug, Error, Info, Warn};

        for (cli_level, verbose, toml_level, expected, explicit) in [
            (None, false, None, "kei=info", false),
            (None, false, Some(Debug), "kei=debug,info", false),
            (None, false, Some(Warn), "warn", false),
            (None, false, Some(Error), "error", false),
            (None, true, Some(Error), "kei=info", true),
            (Some(Info), false, Some(Error), "kei=info", true),
            (Some(Debug), true, Some(Error), "kei=debug,info", true),
        ] {
            let cli = cli::Cli {
                log_level: cli_level,
                verbose,
                ..startup_cli()
            };
            let mut toml: TomlConfig = toml::from_str("").unwrap();
            toml.log_level = toml_level;
            assert_eq!(resolve_log_filter(&cli, Some(&toml)), (expected, explicit));
        }
        assert_eq!(
            resolve_log_filter(&startup_cli(), None),
            ("kei=info", false)
        );
    }

    #[test]
    fn startup_output_keeps_request_separate_from_hard_off_mode() {
        let toml: TomlConfig = toml::from_str("[ui]\nfriendly = false\n").unwrap();
        let mut cli = cli::Cli {
            log_level: Some(types::LogLevel::Debug),
            command: Command::Sync {
                friendly: cli::FriendlyArgs {
                    friendly: true,
                    no_friendly: false,
                },
                password: cli::PasswordArgs::default(),
                sync: cli::SyncArgs::default(),
            },
            ..startup_cli()
        };
        let output = resolve_startup_output(&cli, Some(&toml));
        assert_eq!(output.friendly_request, Some(true));
        assert_eq!(output.personality_mode, personality::Mode::Off);
        assert_eq!(output.default_filter, "kei=debug,info");

        cli.log_level = None;
        cli.command = Command::Service {
            action: cli::ServiceAction::Status,
        };
        let toml: TomlConfig = toml::from_str("[ui]\nfriendly = true\n").unwrap();
        let output = resolve_startup_output(&cli, Some(&toml));
        assert_eq!(output.friendly_request, Some(true));
        assert_eq!(output.personality_mode, personality::Mode::Off);
        assert_eq!(output.default_filter, "kei=info");
    }

    #[test]
    fn input_mode_only_allows_interactive_prompts() {
        assert!(InputMode::Interactive.can_prompt());
        assert!(!InputMode::NoInput.can_prompt());
    }

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
    #[cfg(unix)]
    fn pid_file_guard_refuses_when_existing_pid_alive() {
        // Windows' pid_is_alive stub deliberately always returns false to
        // avoid PID-reuse false positives, so this guard behavior is
        // Unix-only.
        let path = std::env::temp_dir().join("icloudpd_test_pid_guard_alive.pid");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, std::process::id().to_string()).unwrap();

        let err = PidFileGuard::new(path.clone()).expect_err("should refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    #[cfg(unix)]
    fn pid_file_guard_overwrites_when_existing_pid_dead() {
        // PID 2^31-2 is not allocatable on Linux (max_pid is much smaller),
        // so kill(pid, 0) returns ESRCH deterministically.
        let dead_pid = i32::MAX - 1;
        assert!(!pid_is_alive(dead_pid));

        let path = std::env::temp_dir().join("icloudpd_test_pid_guard_dead.pid");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, dead_pid.to_string()).unwrap();

        let guard = PidFileGuard::new(path.clone()).expect("should overwrite stale PID file");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, std::process::id().to_string());
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn pid_file_guard_overwrites_when_existing_contents_garbage() {
        let path = std::env::temp_dir().join("icloudpd_test_pid_guard_garbage.pid");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "not a pid").unwrap();

        let guard = PidFileGuard::new(path.clone()).expect("should overwrite garbage PID file");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, std::process::id().to_string());
        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn pid_is_alive_self() {
        assert!(pid_is_alive(std::process::id().cast_signed()));
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
                pending: Vec::new(),
            };
            writer.write_all(b"Login with s3cret ok").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert!(!output.contains("s3cret"));
        assert!(output.contains("********"));
    }

    #[test]
    fn redacting_writer_redacts_secret_split_across_writes() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
                pending: Vec::new(),
            };
            writer.write_all(b"Login with s3").unwrap();
            writer.write_all(b"cret ok").unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert!(
            !output.contains("s3cret"),
            "split writes must not leak the configured password: {output}"
        );
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
                pending: Vec::new(),
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
                pending: Vec::new(),
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
                pending: Vec::new(),
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
            pending: Vec::new(),
        };
        writer.flush().unwrap();
    }

    /// Password set but the buffer doesn't contain it. The pre-redaction
    /// byte-level scan must short-circuit and pass the buffer through
    /// unchanged, without allocating a `String` for UTF-8 lossy
    /// conversion -- under heavy trace-level logging most events do NOT
    /// contain the password, so the no-allocation path is the hot one.
    #[test]
    fn redacting_writer_password_absent_passthrough() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
                pending: Vec::new(),
            };
            writer
                .write_all(b"long line of trace output without any sensitive value")
                .unwrap();
        }
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(
            output,
            "long line of trace output without any sensitive value"
        );
    }

    /// A buffer containing arbitrary non-UTF-8 bytes (e.g. binary protocol
    /// trace output from `hyper`) and no password match must pass through
    /// byte-for-byte. The original implementation forced `from_utf8_lossy`
    /// on every event, which would have replaced invalid sequences with
    /// U+FFFD even when no redaction was needed.
    #[test]
    fn redacting_writer_non_utf8_passthrough_preserves_bytes() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let bytes: Vec<u8> = vec![0xff, 0xfe, 0xfd, b'o', b'k', 0x00, 0x80];
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
                pending: Vec::new(),
            };
            writer.write_all(&bytes).unwrap();
        }
        assert_eq!(buf, bytes);
    }

    /// Password match in a non-UTF-8 buffer: the slow path is taken,
    /// `from_utf8_lossy` runs, and the password substring is redacted.
    /// Trailing invalid bytes get the U+FFFD replacement, but that is
    /// the same behavior as the original implementation when redaction
    /// fires.
    #[test]
    fn redacting_writer_non_utf8_with_password_redacts() {
        use std::io::Write;

        let password = Arc::new(std::sync::Mutex::new(Some(SecretString::from("s3cret"))));
        let mut bytes: Vec<u8> = b"prefix s3cret suffix ".to_vec();
        bytes.push(0xff);
        let mut buf = Vec::new();
        {
            let mut writer = RedactingWriter {
                inner: &mut buf,
                password: Arc::clone(&password),
                pending: Vec::new(),
            };
            writer.write_all(&bytes).unwrap();
        }
        let output = String::from_utf8_lossy(&buf).into_owned();
        assert!(!output.contains("s3cret"));
        assert!(output.contains("********"));
    }

    /// 200 events at 50 ms per write would synchronously block the
    /// producer for ~10 s without the lossy non-blocking channel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_pipeline_does_not_back_pressure_producer() {
        use std::io::Write;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        struct SlowSink {
            delay: Duration,
            bytes: Arc<AtomicUsize>,
        }
        impl Write for SlowSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                std::thread::sleep(self.delay);
                self.bytes.fetch_add(buf.len(), Ordering::Relaxed);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let bytes = Arc::new(AtomicUsize::new(0));
        let sink = SlowSink {
            delay: Duration::from_millis(50),
            bytes: Arc::clone(&bytes),
        };
        let (make_writer, _guard, _pw) = build_redacting_writer(sink);

        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(make_writer)
            .finish();
        let _g = tracing::subscriber::set_default(subscriber);

        let start = Instant::now();
        for i in 0..200 {
            tracing::info!(i, "back-pressure test event");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "producer blocked {elapsed:?} emitting 200 events through a 50ms-per-write sink; \
             expected the lossy non-blocking channel to absorb the saturation",
        );
    }

    /// `WorkerGuard::drop` must flush every emitted event before
    /// returning. Hoisting the guard into a `static` (whose destructor
    /// never runs) silently re-introduces the race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn writer_pipeline_flushes_all_events_before_guard_drops() {
        use std::io::Write;

        struct CollectingSink {
            buf: Arc<std::sync::Mutex<Vec<u8>>>,
        }
        impl Write for CollectingSink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let buf = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = CollectingSink {
            buf: Arc::clone(&buf),
        };
        let (make_writer, guard, _pw) = build_redacting_writer(sink);

        // Disable ANSI so the assertion can match `seq=N` against a stable
        // byte boundary. Default `tracing_subscriber::fmt()` emits color
        // codes between fields, which would defeat a literal-substring
        // assertion.
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();
        {
            let _g = tracing::subscriber::set_default(subscriber);
            for i in 0..50 {
                tracing::info!(seq = i, "completeness test event");
            }
        }
        // Drop the guard explicitly (mirrors `run` returning) so the
        // background flush thread drains before we read the sink.
        drop(guard);

        let captured = buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(
            !captured.is_empty(),
            "guard drop completed but sink received no bytes; \
             the non-blocking worker did not flush before drop returned",
        );
        let captured_str = String::from_utf8_lossy(&captured);
        for i in 0..50 {
            // `seq=N\n` anchors at end-of-line so seq=5 doesn't match seq=50.
            assert!(
                captured_str.contains(&format!("seq={i}\n")),
                "event seq={i} missing from captured output after guard drop; \
                 captured was: {captured_str}",
            );
        }
    }

    #[test]
    fn make_password_provider_with_direct_source() {
        let source = password::PasswordSource::Direct(Arc::new(SecretString::from("mypass")));
        let provider = make_password_provider(source, InputMode::Interactive);
        let result = provider().unwrap();
        assert_eq!(result.expose_secret(), "mypass");
        // Can be called multiple times
        let result2 = provider().unwrap();
        assert_eq!(result2.expose_secret(), "mypass");
    }

    // `PasswordSource::Command::resolve()` shells out via `sh -c`, which
    // is unix-only. Windows coverage lives in
    // `password::tests::run_password_command_errors_on_non_unix`.
    #[cfg(unix)]
    #[test]
    fn make_password_provider_with_command_source() {
        let source = password::PasswordSource::Command("echo cmd_test".to_string());
        let provider = make_password_provider(source, InputMode::Interactive);
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
                    () = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                    () = shutdown_token.cancelled() => { break; }
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

    /// kei must abort BEFORE auth (and well before any download) when the
    /// data directory has < 1 GiB free. A sync that fills the disk mid-write
    /// leaves orphan `.part` files and a half-truncated final file -- the
    /// worst-case for the "atomic writes" invariant.
    ///
    /// Parameterised over the entire span around the threshold:
    ///   0 bytes               -> bail (extreme)
    ///   1023 MiB              -> bail (just below 1 GiB)
    ///   1 GiB exactly         -> ok (boundary is inclusive on the OK side)
    ///   10 GiB                -> ok (well above)
    #[test]
    fn run_sync_low_disk_space_aborts_before_auth() {
        let dir = std::path::Path::new("/tmp/codex/kei/cg-20-disk");

        // Below threshold: bail.
        for &low in &[0u64, 1023 * 1024 * 1024] {
            let r = check_min_disk_space(low, dir);
            assert!(
                r.is_err(),
                "{low} bytes is below 1 GiB; check_min_disk_space must bail"
            );
            let msg = format!("{:#}", r.expect_err("expected Err"));
            assert!(
                msg.contains("Not enough disk space"),
                "error message must call out the disk-space reason; got: {msg}"
            );
            assert!(
                msg.contains("at least 1 GiB"),
                "error message must state the minimum so operators know what to free; got: {msg}"
            );
        }

        // At and above threshold: ok.
        for &ok in &[MIN_FREE_BYTES, 10 * 1024 * 1024 * 1024] {
            assert!(
                check_min_disk_space(ok, dir).is_ok(),
                "{ok} bytes is at or above 1 GiB; check_min_disk_space must succeed"
            );
        }
    }

    /// Confirm `MIN_FREE_BYTES` is exactly 1 GiB. A future
    /// edit that nudges this constant changes operator-visible behavior;
    /// pin the value so the change is intentional.
    #[test]
    fn check_min_disk_space_threshold_is_one_gib() {
        assert_eq!(MIN_FREE_BYTES, 1_073_741_824);
        assert_eq!(MIN_FREE_BYTES, 1024 * 1024 * 1024);
    }

    #[test]
    fn classify_exit_error_two_factor_required_uses_auth_failure() {
        let e: anyhow::Error = auth::error::AuthError::TwoFactorRequired.into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::Auth);
        assert_eq!(c.exit_code(), EXIT_AUTH);
        assert!(c.should_log());
    }

    #[test]
    fn classify_cli_parse_exit_routes_help_to_stdout_success() {
        let parse_err = cli::parse_cli_with_sources(["kei", "--help"])
            .expect_err("help exits through ParseCliError");
        let e = anyhow::Error::from(parse_err);

        let c = classify_cli_parse_exit(&e).expect("parse error should classify");

        assert_eq!(c.code, 0);
        assert_eq!(c.stream, CliParseRenderStream::Stdout);
        assert!(c.rendered.contains("Usage:"));
    }

    #[test]
    fn classify_cli_parse_exit_routes_invalid_args_to_stderr() {
        let parse_err = cli::parse_cli_with_sources(["kei", "--definitely-not-a-real-flag"])
            .expect_err("invalid arg exits through ParseCliError");
        let e = anyhow::Error::from(parse_err).context("while parsing startup args");

        let c = classify_cli_parse_exit(&e).expect("context-wrapped parse error should classify");

        assert_eq!(c.code, 2);
        assert_eq!(c.stream, CliParseRenderStream::Stderr);
        assert!(c.rendered.contains("definitely-not-a-real-flag"));
    }

    #[test]
    fn classify_cli_parse_exit_ignores_non_parse_errors() {
        let e = anyhow::anyhow!("not a cli parse error");

        assert_eq!(classify_cli_parse_exit(&e), None);
    }

    #[test]
    fn classify_exit_error_partial_sync_uses_exit_partial() {
        let e: anyhow::Error = PartialSyncError(7).into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::Partial);
        assert_eq!(c.exit_code(), EXIT_PARTIAL);
        assert_eq!(c.exit_code(), 2);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_auth_non_2fa_uses_exit_auth() {
        let e: anyhow::Error = auth::error::AuthError::FailedLogin("bad password".into()).into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::Auth);
        assert_eq!(c.exit_code(), EXIT_AUTH);
        assert_eq!(c.exit_code(), 3);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_terminal_auth_uses_exit_terminal_auth() {
        let e: anyhow::Error = auth::error::AuthError::terminal_apple_auth(
            auth::error::APPLE_ACCOUNT_LOCKED_CODE,
            "Account locked",
        )
        .into();
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::TerminalAuth);
        assert_eq!(c.exit_code(), EXIT_TERMINAL_AUTH);
        assert_eq!(c.exit_code(), 4);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_generic_uses_failure() {
        let e = anyhow::anyhow!("disk on fire");
        let c = classify_exit_error(&e);
        assert_eq!(c, ExitClassification::Other);
        assert_eq!(c.exit_code(), 1);
        assert!(c.should_log());
    }

    #[test]
    fn classify_exit_error_walks_anyhow_context() {
        let e: anyhow::Error = anyhow::Error::from(auth::error::AuthError::TwoFactorRequired)
            .context("while validating session")
            .context("during startup");
        assert_eq!(
            classify_exit_error(&e),
            ExitClassification::Auth,
            "context-wrapped 2FA-required must keep the documented auth exit code"
        );

        let e: anyhow::Error =
            anyhow::Error::from(PartialSyncError(3)).context("after final retry pass");
        assert_eq!(classify_exit_error(&e), ExitClassification::Partial);

        let e: anyhow::Error = anyhow::Error::from(auth::error::AuthError::terminal_apple_auth(
            auth::error::APPLE_ACCOUNT_LOCKED_CODE,
            "Account locked",
        ))
        .context("while completing SRP login")
        .context("during startup");
        assert_eq!(classify_exit_error(&e), ExitClassification::TerminalAuth);
    }

    #[test]
    fn classify_exit_error_codes_are_distinct() {
        let codes = [
            ExitClassification::Partial.exit_code(),
            ExitClassification::Auth.exit_code(),
            ExitClassification::TerminalAuth.exit_code(),
            ExitClassification::Other.exit_code(),
        ];
        let mut sorted: Vec<u8> = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            codes.len(),
            "every exit classification must map to a distinct code; got {codes:?}"
        );
    }
}

/// Wrappers around `pub(crate)` parser entry points so cargo-fuzz harnesses
/// in `fuzz/` can drive them through the lib instead of inlining source
/// files via `#[path]`. Gated on the `__fuzz_internals` feature; absent in
/// production builds.
///
/// Every entry point takes/returns only externally-nameable types
/// (`&[u8]`, `&str`, `serde_json::Value`) and discards typed results
/// internally so the lib's internal types stay `pub(crate)`. Don't add
/// anything here that isn't strictly needed by a fuzz target.
#[cfg(feature = "__fuzz_internals")]
#[doc(hidden)]
pub mod __fuzz {
    use serde_json::Value;

    /// Try every CloudKit response struct on the same byte slice. Each call
    /// is `serde_json::from_slice::<T>(data)` with the result discarded.
    pub fn cloudkit_try_all(data: &[u8]) {
        use crate::icloud::photos::cloudkit::{
            BatchQueryResponse, ChangesDatabaseResponse, ChangesZoneResponse, ChangesZoneResult,
            QueryResponse, Record, ZoneId, ZoneListResponse,
        };
        let _ = serde_json::from_slice::<ZoneListResponse>(data);
        let _ = serde_json::from_slice::<QueryResponse>(data);
        let _ = serde_json::from_slice::<BatchQueryResponse>(data);
        let _ = serde_json::from_slice::<Record>(data);
        let _ = serde_json::from_slice::<ChangesDatabaseResponse>(data);
        let _ = serde_json::from_slice::<ChangesZoneResponse>(data);
        let _ = serde_json::from_slice::<ChangesZoneResult>(data);
        let _ = serde_json::from_slice::<ZoneId>(data);
    }

    /// Try every iCloud auth response struct on the same byte slice.
    pub fn auth_responses_try_all(data: &[u8]) {
        use crate::auth::responses::{AccountLoginResponse, SrpInitResponse, TwoFactorChallenge};
        let _ = serde_json::from_slice::<SrpInitResponse>(data);
        let _ = serde_json::from_slice::<AccountLoginResponse>(data);
        let _ = serde_json::from_slice::<TwoFactorChallenge>(data);
    }

    /// Parse a TOML config from arbitrary bytes; discards the typed result.
    pub fn parse_toml_config(s: &str) {
        let _ = toml::from_str::<crate::config::TomlConfig>(s);
    }

    /// Run every `*Enc` decoder against a JSON value, both as the JSON shape
    /// path and as the bplist-via-base64 path. The fuzz harness handles the
    /// base64 wrapping itself; this function takes the prepared `Value`.
    pub fn enc_decoders(fields: &Value) {
        use crate::icloud::photos::enc;
        let _ = enc::decode_string(fields, "captionEnc");
        let _ = enc::decode_string(fields, "extendedDescEnc");
        let _ = enc::decode_keywords(fields);
        let _ = enc::decode_location(fields, "locationEnc");
        let _ = enc::decode_location(fields, "locationV2Enc");
        let _ = enc::decode_location_with_fallback(fields);
    }

    /// Build a `PhotoAsset` from two CloudKit `Record` JSON values. Returns
    /// `()` on success / discard so the harness doesn't name internal types.
    /// Inputs that don't deserialize as a `Record` are skipped.
    pub fn photo_asset_from_record_json(master: Value, asset: Value) {
        use crate::icloud::photos::asset::{PhotoAsset, RequiredAssetFields};
        use crate::icloud::photos::cloudkit::Record;
        let Ok(master) = serde_json::from_value::<Record>(master) else {
            return;
        };
        let Ok(asset) = serde_json::from_value::<Record>(asset) else {
            return;
        };
        let _ = PhotoAsset::try_from_records(master, &asset, RequiredAssetFields::Downloadable);
    }

    /// Run the path-component sanitizers over an arbitrary `&str`. Splits
    /// the input on the first NUL into (template, album) so the fuzzer can
    /// reach the `{album}` substitution path.
    pub fn paths_sanitization(s: &str) {
        use crate::download::paths;
        let _ = paths::clean_filename(s);
        let _ = paths::sanitize_path_component(s);
        let _ = paths::strip_python_wrapper(s);
        let _ = paths::remove_unicode_chars(s);

        let (template, album) = match s.split_once('\0') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        let _ = paths::expand_named_token(template, paths::TOKEN_ALBUM, album);

        for size in [0u64, 1, u64::MAX] {
            let _ = paths::add_dedup_suffix(s, size);
        }
    }

    /// Walk an HEIC byte buffer for the embedded XMP packet. Defense-in-depth
    /// against the upstream mp4-atom OOM class that hit `parse_vorbis_comment`
    /// (kixelated/mp4-atom#154, fixed upstream in #157) and any sibling
    /// decoders that might regress in the same shape.
    pub fn heif_extract_xmp(bytes: &[u8]) -> Option<Vec<u8>> {
        crate::download::heif::extract_xmp_bytes(bytes)
    }

    /// Locate a HEIF EXIF TIFF range through the file-backed parser.
    pub fn heif_locate_exif(bytes: &[u8]) {
        let mut source = std::io::Cursor::new(bytes);
        let _ = crate::download::heif::locate_exif_tiff(&mut source, bytes.len() as u64);
    }

    /// Parse arbitrary bytes through the fixed-buffer TIFF GPS reader.
    pub fn tiff_source_gps(bytes: &[u8]) {
        crate::download::metadata::fuzz_tiff_source_gps(bytes);
    }

    /// Cheap content-sniff check for HEIC/HEIF/AVIF magic bytes.
    pub fn heif_is_heif_content(bytes: &[u8]) -> bool {
        crate::download::heif::is_heif_content(bytes)
    }

    /// Drive the byte-preserving HEIC XMP writer over arbitrary bytes and
    /// assert its safety contract: a rejected rewrite emits nothing, and an
    /// accepted rewrite keeps the container HEIF, round-trips the written
    /// packet, and preserves every non-XMP item's payload.
    pub fn heif_rewrite_xmp_preserves(bytes: &[u8]) {
        crate::download::heif::fuzz_rewrite_xmp_preserves(bytes);
    }

    /// Run the three `state` enum string parsers on the same input. They're
    /// inherent `from_str` methods, not the `FromStr` trait, returning
    /// `Option<Self>`.
    pub fn state_enums_from_str(s: &str) {
        let _ = crate::state::VersionSizeKey::from_str(s);
        let _ = crate::state::AssetStatus::from_str(s);
        let _ = crate::state::MediaType::from_str(s);
    }
}
