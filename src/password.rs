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

use anyhow::Context;
pub use secrecy::{ExposeSecret, SecretString};

use crate::credential::CredentialStore;

/// Describes where to obtain a password, evaluated lazily on each auth attempt.
///
/// Between auth cycles (e.g., watch mode re-auth), the closure holds only the
/// source descriptor — no password remains in memory.
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
                         - kei password set\n  \
                         - --password-command (external secret manager)\n  \
                         - --password-file or Docker secret\n  \
                         - [auth] password in config.toml"
                    );
                }
                Ok(prompt_password())
            }
        }
    }
}

/// Build a [`PasswordSource`] from resolved configuration, following the priority chain:
///
/// `--password` > `--password-command` > `--password-file` > credential store > TOML > interactive
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
pub fn read_password_file(path: &Path) -> anyhow::Result<SecretString> {
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

/// Execute a shell command and capture stdout as a password.
///
/// The command runs via `sh -c` with stdin as `/dev/null` (prevents hanging)
/// and stderr inherited (command errors visible to the user). Re-executed on
/// each auth attempt to support dynamic secret managers (1Password, Vault, etc.).
pub fn run_password_command(cmd: &str) -> anyhow::Result<SecretString> {
    let output = std::process::Command::new("sh")
        .args(["-c", cmd])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .output()
        .with_context(|| format!("Failed to execute password command: {cmd}"))?;

    anyhow::ensure!(
        output.status.success(),
        "Password command exited with status {}: {cmd}",
        output.status
    );

    let stdout =
        String::from_utf8(output.stdout).context("Password command output is not valid UTF-8")?;
    let trimmed = strip_trailing_newline(&stdout);
    anyhow::ensure!(
        !trimmed.is_empty(),
        "Password command produced empty output: {cmd}"
    );
    Ok(SecretString::from(trimmed.to_string()))
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

    #[test]
    fn run_password_command_echo() {
        assert_eq!(
            run_password_command("echo hunter2")
                .unwrap()
                .expose_secret(),
            "hunter2"
        );
    }

    #[test]
    fn run_password_command_failure() {
        let err = run_password_command("false").unwrap_err();
        assert!(err.to_string().contains("exited with status"), "{err}");
    }

    #[test]
    fn run_password_command_empty() {
        let err = run_password_command("printf ''").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn run_password_command_strips_newline() {
        assert_eq!(
            run_password_command("echo secret_value")
                .unwrap()
                .expose_secret(),
            "secret_value"
        );
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
}
