//! Password handling: secret types, lazy password sources, and helpers.
//!
//! Passwords are never held as plain `String` values. The [`secrecy`] crate
//! provides [`SecretString`] which auto-zeroizes on drop and prevents
//! accidental exposure via `Debug` / `Display`.
//!
//! [`PasswordSource`] captures *where* a password comes from (CLI flag, file,
//! command, credential store, interactive prompt) and evaluates lazily — the
//! password is only fetched at auth time and released immediately after.

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
            Self::Interactive => Ok(prompt_password()),
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
    use std::io::Write;

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

    #[test]
    fn read_password_file_normal() {
        let dir = PathBuf::from("/tmp/claude/test_pw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw_normal.txt");
        std::fs::write(&path, "my_secret\n").unwrap();
        let secret = read_password_file(&path).unwrap();
        assert_eq!(secret.expose_secret(), "my_secret");
    }

    #[test]
    fn read_password_file_no_newline() {
        let dir = PathBuf::from("/tmp/claude/test_pw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw_no_nl.txt");
        std::fs::write(&path, "my_secret").unwrap();
        let secret = read_password_file(&path).unwrap();
        assert_eq!(secret.expose_secret(), "my_secret");
    }

    #[test]
    fn read_password_file_crlf() {
        let dir = PathBuf::from("/tmp/claude/test_pw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw_crlf.txt");
        std::fs::write(&path, "my_secret\r\n").unwrap();
        let secret = read_password_file(&path).unwrap();
        assert_eq!(secret.expose_secret(), "my_secret");
    }

    #[test]
    fn read_password_file_empty() {
        let dir = PathBuf::from("/tmp/claude/test_pw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw_empty.txt");
        std::fs::write(&path, "").unwrap();
        let err = read_password_file(&path).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn read_password_file_only_newline() {
        let dir = PathBuf::from("/tmp/claude/test_pw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw_only_nl.txt");
        std::fs::write(&path, "\n").unwrap();
        let err = read_password_file(&path).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn read_password_file_missing() {
        let path = PathBuf::from("/tmp/claude/test_pw/nonexistent.txt");
        let err = read_password_file(&path).unwrap_err();
        assert!(err.to_string().contains("Failed to read"), "{err}");
    }

    #[test]
    fn run_password_command_echo() {
        let secret = run_password_command("echo hunter2").unwrap();
        assert_eq!(secret.expose_secret(), "hunter2");
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
        // `echo` adds a trailing newline by default
        let secret = run_password_command("echo secret_value").unwrap();
        assert_eq!(secret.expose_secret(), "secret_value");
    }

    #[test]
    fn password_source_direct_resolve() {
        let secret = SecretString::from("direct_pw".to_string());
        let source = PasswordSource::Direct(Arc::new(secret));
        let resolved = source.resolve().unwrap().unwrap();
        assert_eq!(resolved.expose_secret(), "direct_pw");
    }

    #[test]
    fn password_source_command_resolve() {
        let source = PasswordSource::Command("echo cmd_pw".to_string());
        let resolved = source.resolve().unwrap().unwrap();
        assert_eq!(resolved.expose_secret(), "cmd_pw");
    }

    #[test]
    fn password_source_file_resolve() {
        let dir = PathBuf::from("/tmp/claude/test_pw");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pw_source.txt");
        let mut f = std::fs::File::create(&path).unwrap();
        write!(f, "file_pw\n").unwrap();
        let source = PasswordSource::File(path);
        let resolved = source.resolve().unwrap().unwrap();
        assert_eq!(resolved.expose_secret(), "file_pw");
    }
}
