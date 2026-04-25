//! Password handling: secret types, lazy password sources, and helpers.
//!
//! Passwords are never held as plain `String` values. The [`secrecy`] crate
//! provides [`SecretString`] which auto-zeroizes on drop and prevents
//! accidental exposure via `Debug` / `Display`.
//!
//! [`PasswordSource`] captures *where* a password comes from (CLI flag, file,
//! command, credential store, interactive prompt) and evaluates lazily — the
//! password is only fetched at auth time and released immediately after.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
// Permissive-mode warnings are unix-only (Windows has no POSIX mode bits).
// The dedup cache (HashSet / Mutex / OnceLock) is scoped to that path.
#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};

use anyhow::Context;
pub use secrecy::{ExposeSecret, SecretString};

use crate::credential::CredentialStore;

/// Describes where to obtain a password, evaluated lazily on each auth attempt.
///
/// Between auth cycles (e.g., watch mode re-auth), the closure holds only the
/// source descriptor — no password remains in memory.
#[derive(Debug)]
pub enum PasswordSource {
    /// Password already in memory (from `--password` flag, env var, or TOML).
    Direct(Arc<SecretString>),
    /// Shell command to execute on each auth attempt.
    Command(String),
    /// File path to read on each auth attempt.
    File(PathBuf),
    /// OS keyring or encrypted file credential store.
    Store(CredentialStore),
    /// Interactive terminal prompt via `rpassword`.
    Interactive,
}

/// Source-kind tag without the attached payload. Copy-safe so callers can
/// record "where did the password come from" without holding the actual
/// secret or store reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordSourceKind {
    Direct,
    Command,
    File,
    Store,
    Interactive,
}

/// Shared handle to a password-resolving closure.
///
/// Behind `Arc` so the async call site can refcount-bump a handle
/// into `tokio::task::spawn_blocking` without borrowing across
/// `.await`. The closure itself stays synchronous — it's the
/// blocking pool that keeps slow `--password-command` invocations
/// from pinning an async runtime worker.
pub type PasswordProvider =
    std::sync::Arc<dyn Fn() -> Option<SecretString> + Send + Sync + 'static>;

/// Run a [`PasswordProvider`] on the blocking pool.
///
/// The provider's `resolve()` path may include a 30-second subprocess
/// wait (`--password-command`) or a file read; running it on an async
/// worker would block the runtime. Dispatch to `spawn_blocking` instead
/// so the worker stays free for concurrent download/state-DB work.
pub async fn invoke_password_provider(provider: &PasswordProvider) -> Option<SecretString> {
    let provider = std::sync::Arc::clone(provider);
    tokio::task::spawn_blocking(move || provider())
        .await
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "password provider panicked in spawn_blocking");
            None
        })
}

impl PasswordSource {
    /// Return the source kind, discarding the attached payload.
    pub const fn kind(&self) -> PasswordSourceKind {
        match self {
            Self::Direct(_) => PasswordSourceKind::Direct,
            Self::Command(_) => PasswordSourceKind::Command,
            Self::File(_) => PasswordSourceKind::File,
            Self::Store(_) => PasswordSourceKind::Store,
            Self::Interactive => PasswordSourceKind::Interactive,
        }
    }
}

/// Decision returned by [`decide_save_password_action`] to the sync loop:
/// either perform the save, or skip it with a user-visible warning
/// explaining why `--save-password` was a no-op for this source.
#[derive(Debug, PartialEq, Eq)]
pub enum SavePasswordAction {
    /// Password is CLI/env-sourced; write it to the credential store.
    Save,
    /// Source already persists the secret elsewhere. `reason` is the warning
    /// text the sync loop should emit at `tracing::warn!` level.
    SkipWithWarning(&'static str),
}

/// Decide how the sync loop should handle `--save-password` given the
/// resolved password source. Pure function for testability — the sync loop
/// wraps this with the actual `store.store()` call and log emission.
#[must_use]
pub fn decide_save_password_action(kind: PasswordSourceKind) -> SavePasswordAction {
    match kind {
        PasswordSourceKind::Direct => SavePasswordAction::Save,
        PasswordSourceKind::Interactive => SavePasswordAction::SkipWithWarning(
            "--save-password with an interactive password prompt is not supported. \
             Use `kei password set` to save a prompted password to the credential \
             store, then re-run without --save-password.",
        ),
        PasswordSourceKind::File => SavePasswordAction::SkipWithWarning(
            "--save-password skipped: the password is already persistent in a file \
             (`--password-file` / `[auth] password_file`). Remove --save-password, \
             or use `kei password set` to bootstrap the credential store from the file.",
        ),
        PasswordSourceKind::Command => SavePasswordAction::SkipWithWarning(
            "--save-password skipped: the password comes from an external command \
             (`--password-command` / `[auth] password_command`). Remove --save-password \
             to avoid storing a stale copy of a rotating secret.",
        ),
        PasswordSourceKind::Store => SavePasswordAction::SkipWithWarning(
            "--save-password skipped: the password is already in the credential store. \
             Remove --save-password from your invocation.",
        ),
    }
}

impl PasswordSource {
    /// Evaluate the source, returning the password.
    ///
    /// Called once per auth attempt — the result is not cached between attempts.
    /// For [`Command`](Self::Command) and [`File`](Self::File) sources, this
    /// re-executes the command or re-reads the file each time, supporting
    /// password rotation and external secret managers.
    pub fn resolve(&self) -> anyhow::Result<Option<SecretString>> {
        match self {
            Self::Direct(s) => Ok(Some(SecretString::from(s.expose_secret().to_owned()))),
            Self::Command(cmd) => run_password_command(cmd).map(Some),
            Self::File(path) => read_password_file(path).map(Some),
            Self::Store(store) => store.retrieve(),
            Self::Interactive => {
                if !std::io::stdin().is_terminal() {
                    anyhow::bail!(
                        "No password configured and stdin is not a terminal. \
                         Set a password with one of:\n  \
                         - ICLOUD_PASSWORD environment variable\n  \
                         - kei password set (OS keyring or encrypted file)\n  \
                         - --password-command or [auth] password_command (external secret manager)\n  \
                         - --password-file or [auth] password_file (Docker secret, etc.)"
                    );
                }
                Ok(prompt_password())
            }
        }
    }
}

/// Build a [`PasswordSource`] from resolved configuration, following the priority chain:
///
/// direct password > password_command > password_file > credential store > interactive
///
/// Direct password can come from `--password` or `ICLOUD_PASSWORD`. The command
/// and file sources can come from CLI flags or their TOML counterparts; CLI
/// wins per-field. `[auth] password` in TOML is rejected upstream in
/// `Config::build()`.
pub fn build_password_source(
    password: Option<&SecretString>,
    password_command: Option<&str>,
    password_file: Option<&Path>,
    credential_store: CredentialStore,
) -> PasswordSource {
    if let Some(pw) = password {
        PasswordSource::Direct(Arc::new(SecretString::from(pw.expose_secret().to_owned())))
    } else if let Some(cmd) = password_command {
        PasswordSource::Command(cmd.to_string())
    } else if let Some(path) = password_file {
        PasswordSource::File(path.to_path_buf())
    } else if credential_store.has_credential() {
        PasswordSource::Store(credential_store)
    } else {
        PasswordSource::Interactive
    }
}

/// Read a password from a file, stripping a single trailing newline.
///
/// Designed for Docker secrets (`/run/secrets/...`) and similar file-based
/// credential stores. The file is re-read on each call to support rotation.
///
/// On Unix, warns once per path if the file is readable by group or other
/// users (see [`check_password_file_mode`]).
pub(crate) fn read_password_file(path: &Path) -> anyhow::Result<SecretString> {
    warn_if_permissive_mode(path);
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read password file: {}", path.display()))?;
    let trimmed = strip_trailing_newline(&contents);
    anyhow::ensure!(
        !trimmed.is_empty(),
        "Password file is empty: {}",
        path.display()
    );
    Ok(SecretString::from(trimmed.to_string()))
}

/// Inspect a password file's mode and return a warning message if any
/// group/other permission bits are set, or `None` if the file is secure.
///
/// Paths under the conventional container-secret mount points
/// (`/run/secrets/` and `/var/run/secrets/`) are exempted because Docker and
/// Kubernetes publish those files as world-readable by design; isolation is
/// enforced at the mount layer, not the file mode.
///
/// Pure function so the policy can be unit-tested without touching the
/// filesystem or the tracing subscriber.
#[cfg(unix)]
pub(crate) fn check_password_file_mode(path: &Path, mode: u32) -> Option<String> {
    let path_str = path.to_string_lossy();
    if path_str.starts_with("/run/secrets/") || path_str.starts_with("/var/run/secrets/") {
        return None;
    }
    let mode = mode & 0o777;
    if mode & 0o077 == 0 {
        return None;
    }
    Some(format!(
        "password file {} is readable by other users (mode {mode:04o}); \
         run `chmod 600 {}` to restrict it",
        path.display(),
        path.display(),
    ))
}

/// Warn once per path when a password file has permissive group/other
/// permissions. No-op on non-Unix platforms (Windows uses a different ACL
/// model that's out of scope here).
#[cfg(unix)]
fn warn_if_permissive_mode(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    static WARNED_PATHS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let warned = WARNED_PATHS.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut guard) = warned.lock() else { return };
    if !guard.insert(path.to_path_buf()) {
        return;
    }
    drop(guard);

    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if let Some(msg) = check_password_file_mode(path, meta.permissions().mode()) {
        tracing::warn!("{msg}");
    }
}

#[cfg(not(unix))]
fn warn_if_permissive_mode(_path: &Path) {}

/// Default kill deadline for a password command. Watch-mode re-auth is the
/// scenario this guards: a hung secret manager (network-stalled Vault, sleepy
/// gpg-agent, etc.) would otherwise freeze the whole sync indefinitely.
pub(crate) const PASSWORD_COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Execute a shell command and capture stdout as a password.
///
/// The command runs via `sh -c` with stdin as `/dev/null` (so it can't hang
/// waiting on input) and stderr inherited (command errors visible to the
/// user). Re-executed on each auth attempt to support dynamic secret managers
/// (1Password, Vault, pass, etc.).
///
/// Not supported on Windows: there's no `sh` on a stock Windows PATH. Use
/// `--password-file` instead, or run kei under WSL.
pub(crate) fn run_password_command(cmd: &str) -> anyhow::Result<SecretString> {
    run_password_command_with_timeout(cmd, PASSWORD_COMMAND_TIMEOUT)
}

/// Shared implementation of [`run_password_command`] with an injectable
/// timeout so tests can exercise the kill path without waiting 30 seconds.
fn run_password_command_with_timeout(
    cmd: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<SecretString> {
    #[cfg(not(unix))]
    {
        let _ = (cmd, timeout);
        anyhow::bail!(
            "`--password-command` / `[auth] password_command` is not supported on Windows: \
             kei runs commands via `sh -c`, which isn't on the stock Windows PATH. \
             Use `--password-file` / `[auth] password_file`, or run kei under WSL."
        );
    }

    #[cfg(unix)]
    {
        use std::io::Read;

        let mut child = std::process::Command::new("sh")
            .args(["-c", cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .with_context(|| format!("Failed to execute password command: {cmd}"))?;

        // Poll with 5ms → 50ms backoff; clamp each sleep to the remaining
        // deadline so we never overshoot the timeout budget.
        let start = std::time::Instant::now();
        let mut poll = std::time::Duration::from_millis(5);
        let cap = std::time::Duration::from_millis(50);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    let remaining = timeout.saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        let _ = child.kill();
                        let _ = child.wait();
                        anyhow::bail!(
                            "password command timed out after {}s: {cmd}",
                            timeout.as_secs()
                        );
                    }
                    std::thread::sleep(poll.min(remaining));
                    poll = (poll * 2).min(cap);
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e)
                        .context(format!("waiting on password command: {cmd}")));
                }
            }
        };

        anyhow::ensure!(
            status.success(),
            "Password command exited with status {status}: {cmd}"
        );

        let mut buf = Vec::new();
        if let Some(mut stdout) = child.stdout.take() {
            stdout
                .read_to_end(&mut buf)
                .with_context(|| format!("reading password command stdout: {cmd}"))?;
        }
        let stdout =
            String::from_utf8(buf).context("Password command output is not valid UTF-8")?;
        let trimmed = strip_trailing_newline(&stdout);
        anyhow::ensure!(
            !trimmed.is_empty(),
            "Password command produced empty output: {cmd}"
        );
        Ok(SecretString::from(trimmed.to_string()))
    }
}

/// Prompt for a password on stdin using `rpassword` (masked input).
///
/// Returns `None` if stdin is not a terminal or the prompt fails.
pub fn prompt_password() -> Option<SecretString> {
    tokio::task::block_in_place(|| {
        rpassword::prompt_password("iCloud Password: ")
            .ok()
            .map(SecretString::from)
    })
}

/// Strip a single trailing newline (LF or CRLF) from a string.
fn strip_trailing_newline(s: &str) -> &str {
    s.strip_suffix("\r\n")
        .or_else(|| s.strip_suffix('\n'))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_test_file(dir: &std::path::Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    // ── strip_trailing_newline ──────────────────────────────────────

    #[test]
    fn strip_trailing_newline_lf() {
        assert_eq!(strip_trailing_newline("password\n"), "password");
    }

    #[test]
    fn strip_trailing_newline_crlf() {
        assert_eq!(strip_trailing_newline("password\r\n"), "password");
    }

    #[test]
    fn strip_trailing_newline_none() {
        assert_eq!(strip_trailing_newline("password"), "password");
    }

    #[test]
    fn strip_trailing_newline_only_one() {
        assert_eq!(strip_trailing_newline("password\n\n"), "password\n");
    }

    // ── read_password_file ──────────────────────────────────────────

    #[test]
    fn read_password_file_normal() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_file(dir.path(), "pw.txt", "my_secret\n");
        assert_eq!(
            read_password_file(&path).unwrap().expose_secret(),
            "my_secret"
        );
    }

    #[test]
    fn read_password_file_no_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_file(dir.path(), "pw.txt", "my_secret");
        assert_eq!(
            read_password_file(&path).unwrap().expose_secret(),
            "my_secret"
        );
    }

    #[test]
    fn read_password_file_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_file(dir.path(), "pw.txt", "my_secret\r\n");
        assert_eq!(
            read_password_file(&path).unwrap().expose_secret(),
            "my_secret"
        );
    }

    #[test]
    fn read_password_file_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_file(dir.path(), "pw.txt", "");
        let err = read_password_file(&path).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn read_password_file_only_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_file(dir.path(), "pw.txt", "\n");
        let err = read_password_file(&path).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn read_password_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.txt");
        let err = read_password_file(&path).unwrap_err();
        assert!(err.to_string().contains("Failed to read"), "{err}");
    }

    // ── run_password_command ────────────────────────────────────────
    //
    // All happy-path tests here shell out via `sh -c`, which is unix-only.
    // Windows coverage is `run_password_command_errors_on_non_unix` below.

    #[cfg(unix)]
    #[test]
    fn run_password_command_echo() {
        assert_eq!(
            run_password_command("echo hunter2")
                .unwrap()
                .expose_secret(),
            "hunter2"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_password_command_failure() {
        let err = run_password_command("false").unwrap_err();
        assert!(err.to_string().contains("exited with status"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_password_command_empty() {
        let err = run_password_command("printf ''").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn run_password_command_strips_newline() {
        assert_eq!(
            run_password_command("echo secret_value")
                .unwrap()
                .expose_secret(),
            "secret_value"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_password_command_times_out_and_kills_hung_process() {
        // `sleep 30` with a 100ms timeout must return within a fraction of a
        // second; anything close to 30s means the kill path is broken.
        let start = std::time::Instant::now();
        let err =
            run_password_command_with_timeout("sleep 30", std::time::Duration::from_millis(100))
                .unwrap_err();
        let elapsed = start.elapsed();
        assert!(
            err.to_string().contains("timed out"),
            "expected timeout error; got: {err}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "timeout path took {elapsed:?}; kill didn't work"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_password_command_completes_before_timeout() {
        // A command that finishes well under the timeout returns its output,
        // not a timeout error. Guards the happy path from a future regression
        // where the poll loop accidentally short-circuits.
        let pw = run_password_command_with_timeout("echo fast", std::time::Duration::from_secs(5))
            .unwrap();
        assert_eq!(pw.expose_secret(), "fast");
    }

    #[cfg(not(unix))]
    #[test]
    fn run_password_command_errors_on_non_unix() {
        let err = run_password_command("echo anything").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not supported on Windows"), "{msg}");
        assert!(msg.contains("--password-file"), "{msg}");
    }

    // ── PasswordSource::resolve ─────────────────────────────────────

    #[test]
    fn password_source_direct_resolve() {
        let source = PasswordSource::Direct(Arc::new(SecretString::from("direct_pw")));
        assert_eq!(
            source.resolve().unwrap().unwrap().expose_secret(),
            "direct_pw"
        );
    }

    // Same reasoning as run_password_command_echo: `sh -c` is unix-only.
    #[cfg(unix)]
    #[test]
    fn password_source_command_resolve() {
        let source = PasswordSource::Command("echo cmd_pw".to_string());
        assert_eq!(source.resolve().unwrap().unwrap().expose_secret(), "cmd_pw");
    }

    #[test]
    fn password_source_file_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_file(dir.path(), "pw_source.txt", "file_pw\n");
        let source = PasswordSource::File(path);
        assert_eq!(
            source.resolve().unwrap().unwrap().expose_secret(),
            "file_pw"
        );
    }

    // ── PasswordSource::kind + decide_save_password_action ──────────

    #[test]
    fn kind_maps_each_variant() {
        assert_eq!(
            PasswordSource::Direct(Arc::new(SecretString::from("x"))).kind(),
            PasswordSourceKind::Direct
        );
        assert_eq!(
            PasswordSource::Command("echo x".to_string()).kind(),
            PasswordSourceKind::Command
        );
        assert_eq!(
            PasswordSource::File(PathBuf::from("/x")).kind(),
            PasswordSourceKind::File
        );
        assert_eq!(
            PasswordSource::Interactive.kind(),
            PasswordSourceKind::Interactive
        );
        // Store variant covered only by source-kind round-trip elsewhere
        // because constructing a CredentialStore here requires filesystem
        // setup.
    }

    #[test]
    fn decide_save_direct_saves() {
        assert_eq!(
            decide_save_password_action(PasswordSourceKind::Direct),
            SavePasswordAction::Save
        );
    }

    #[test]
    fn decide_save_file_skips_with_bootstrap_hint() {
        match decide_save_password_action(PasswordSourceKind::File) {
            SavePasswordAction::SkipWithWarning(msg) => {
                assert!(msg.contains("--password-file"), "{msg}");
                assert!(msg.contains("kei password set"), "{msg}");
            }
            other => panic!("expected SkipWithWarning, got {other:?}"),
        }
    }

    #[test]
    fn decide_save_command_skips_with_rotation_hint() {
        match decide_save_password_action(PasswordSourceKind::Command) {
            SavePasswordAction::SkipWithWarning(msg) => {
                assert!(msg.contains("--password-command"), "{msg}");
                assert!(msg.contains("rotating"), "{msg}");
            }
            other => panic!("expected SkipWithWarning, got {other:?}"),
        }
    }

    #[test]
    fn decide_save_store_skips_as_redundant() {
        match decide_save_password_action(PasswordSourceKind::Store) {
            SavePasswordAction::SkipWithWarning(msg) => {
                assert!(msg.contains("already in the credential store"), "{msg}");
            }
            other => panic!("expected SkipWithWarning, got {other:?}"),
        }
    }

    #[test]
    fn decide_save_interactive_points_at_password_set() {
        match decide_save_password_action(PasswordSourceKind::Interactive) {
            SavePasswordAction::SkipWithWarning(msg) => {
                assert!(msg.contains("kei password set"), "{msg}");
                assert!(msg.contains("interactive"), "{msg}");
            }
            other => panic!("expected SkipWithWarning, got {other:?}"),
        }
    }

    // ── check_password_file_mode (unix-only) ────────────────────────
    //
    // These tests exercise the pure policy function without touching the
    // filesystem or the tracing subscriber.

    #[cfg(unix)]
    #[test]
    fn check_mode_owner_read_only_ok() {
        let path = Path::new("/home/user/icloud_pw");
        assert!(check_password_file_mode(path, 0o600).is_none());
        assert!(check_password_file_mode(path, 0o400).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_group_readable_warns() {
        let path = Path::new("/home/user/icloud_pw");
        let msg = check_password_file_mode(path, 0o640).unwrap();
        assert!(msg.contains("0640"), "mode should appear in message: {msg}");
        assert!(msg.contains("chmod 600"), "fix hint missing: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_world_readable_warns() {
        let path = Path::new("/home/user/icloud_pw");
        let msg = check_password_file_mode(path, 0o644).unwrap();
        assert!(msg.contains("0644"));
        assert!(msg.contains(path.to_str().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_group_exec_only_warns() {
        // Any bit in the lower six — including exec — means someone other
        // than the owner has access to the file metadata chain.
        let path = Path::new("/home/user/icloud_pw");
        assert!(check_password_file_mode(path, 0o610).is_some());
        assert!(check_password_file_mode(path, 0o601).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_masks_setuid_setgid_sticky() {
        // A sticky/setuid bit shouldn't mask a permissive low-bit mode.
        let path = Path::new("/home/user/icloud_pw");
        assert!(check_password_file_mode(path, 0o4644).is_some());
        assert!(check_password_file_mode(path, 0o2600).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_docker_secrets_exempted() {
        // Docker secrets land in /run/secrets/ with mode 0o444 by default;
        // the isolation is at the mount layer, not the file mode.
        let path = Path::new("/run/secrets/icloud_password");
        assert!(check_password_file_mode(path, 0o444).is_none());
        assert!(check_password_file_mode(path, 0o644).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_k8s_secrets_exempted() {
        let path = Path::new("/var/run/secrets/icloud/password");
        assert!(check_password_file_mode(path, 0o644).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn check_mode_secrets_prefix_must_match_path_segment() {
        // A user directory that merely starts with "/run/secrets" but isn't
        // the canonical container mount should still be checked.
        let path = Path::new("/run/secretsharing/pw");
        assert!(check_password_file_mode(path, 0o644).is_some());
    }

    // ── read_password_file with a permissive file ──────────────────
    //
    // Integration sanity check: the warn path must not break the read.

    #[cfg(unix)]
    #[test]
    fn read_password_file_still_works_on_permissive_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = write_test_file(dir.path(), "permissive_pw.txt", "leaky\n");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(read_password_file(&path).unwrap().expose_secret(), "leaky");
    }

    // ── invoke_password_provider ────────────────────────────────────
    //
    // A provider whose resolve() does blocking I/O (subprocess wait,
    // file read) must not run on the async worker. The canary here
    // is that the async caller stays responsive while the sync
    // closure is in flight, which spawn_blocking guarantees.

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn invoke_password_provider_yields_runtime_for_blocking_closure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc as StdArc;
        use std::time::Duration;

        let interleaved = StdArc::new(AtomicBool::new(false));
        let seen = StdArc::clone(&interleaved);

        // Sync provider that blocks for longer than the other future's delay.
        // If spawn_blocking pins the single async worker, the concurrent
        // task below will be starved past its intended wake time.
        let provider: PasswordProvider = StdArc::new(move || {
            std::thread::sleep(Duration::from_millis(200));
            Some(SecretString::from("ok".to_string()))
        });

        let provider_fut = super::invoke_password_provider(&provider);
        let canary_fut = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            seen.store(true, Ordering::SeqCst);
        };

        tokio::join!(provider_fut, canary_fut);
        assert!(
            interleaved.load(Ordering::SeqCst),
            "the async canary must have run while the sync provider was sleeping"
        );
    }
}
