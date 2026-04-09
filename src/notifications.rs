//! Notification script support for unattended operation.
//!
//! Fires a user-provided script with event information as environment variables.
//! Used to notify users of 2FA expiry, sync completion, failures, etc.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Events that trigger notification scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// 2FA code is needed (session expired in headless mode)
    TwoFaRequired,
    /// Sync cycle completed successfully
    SyncComplete,
    /// Sync cycle had failures
    SyncFailed,
    /// Session expired and re-authentication failed
    SessionExpired,
}

impl Event {
    fn as_str(self) -> &'static str {
        match self {
            Self::TwoFaRequired => "2fa_required",
            Self::SyncComplete => "sync_complete",
            Self::SyncFailed => "sync_failed",
            Self::SessionExpired => "session_expired",
        }
    }
}

/// Notification dispatcher. Holds an optional script path.
/// When no script is configured, all methods are no-ops.
#[derive(Debug, Clone)]
pub struct Notifier {
    script: Option<PathBuf>,
}

/// Timeout for notification scripts.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

impl Notifier {
    pub fn new(script: Option<PathBuf>) -> Self {
        Self { script }
    }

    /// Fire the notification script with the given event.
    /// Fire-and-forget: spawns the script in a background task so it never blocks sync.
    pub fn notify(&self, event: Event, message: &str, username: &str) {
        let Some(script) = self.script.clone() else {
            return;
        };

        if !script.exists() {
            tracing::warn!(
                path = %script.display(),
                "Notification script does not exist"
            );
            return;
        }

        let event_str = event.as_str();
        let message = message.to_owned();
        let username = username.to_owned();

        tracing::debug!(event = event_str, "Firing notification script");

        tokio::spawn(async move {
            match run_script(&script, event_str, &message, &username).await {
                Ok(status) if status.success() => {
                    tracing::debug!(event = event_str, "Notification script completed");
                }
                Ok(status) => {
                    tracing::warn!(
                        event = event_str,
                        code = status.code(),
                        "Notification script exited with non-zero status"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        event = event_str,
                        error = %e,
                        "Notification script failed"
                    );
                }
            }
        });
    }
}

async fn run_script(
    script: &Path,
    event: &str,
    message: &str,
    username: &str,
) -> anyhow::Result<std::process::ExitStatus> {
    let mut child = tokio::process::Command::new(script)
        .env("KEI_EVENT", event)
        .env("KEI_MESSAGE", message)
        .env("KEI_ICLOUD_USERNAME", username)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    if let Ok(result) = tokio::time::timeout(SCRIPT_TIMEOUT, child.wait()).await {
        Ok(result?)
    } else {
        tracing::warn!("Notification script timed out, killing");
        child.kill().await.ok();
        anyhow::bail!(
            "notification script timed out after {}s",
            SCRIPT_TIMEOUT.as_secs()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_as_str() {
        assert_eq!(Event::TwoFaRequired.as_str(), "2fa_required");
        assert_eq!(Event::SyncComplete.as_str(), "sync_complete");
        assert_eq!(Event::SyncFailed.as_str(), "sync_failed");
        assert_eq!(Event::SessionExpired.as_str(), "session_expired");
    }

    #[test]
    fn notifier_none_is_noop() {
        let notifier = Notifier::new(None);
        assert!(notifier.script.is_none());
    }

    #[test]
    fn notify_with_nonexistent_script() {
        let notifier = Notifier::new(Some(PathBuf::from("/tmp/claude/nonexistent_notify.sh")));
        // Should not panic, just log a warning (script existence checked synchronously)
        notifier.notify(Event::SyncComplete, "test message", "user@example.com");
    }

    /// Write a shell script to a temp dir, avoiding ETXTBSY ("Text file busy")
    /// races on CI. Writes to a staging file first, then renames so the final
    /// path was never opened for writing by this process.
    #[cfg(unix)]
    fn write_test_script(dir: &std::path::Path, name: &str, body: &[u8]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let staging = dir.join(format!("{name}.tmp"));
        let final_path = dir.join(name);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&staging).unwrap();
            f.write_all(body).unwrap();
            f.sync_all().unwrap();
        }
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&staging, &final_path).unwrap();
        final_path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_success() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_test_script(dir.path(), "success.sh", b"#!/bin/sh\nexit 0\n");

        let status = run_script(&script, "test_event", "msg", "user")
            .await
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_test_script(dir.path(), "fail.sh", b"#!/bin/sh\nexit 1\n");

        let status = run_script(&script, "test_event", "msg", "user")
            .await
            .unwrap();
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_runs_script_with_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("test_notify_output.txt");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_EVENT|$KEI_MESSAGE|$KEI_ICLOUD_USERNAME\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_notify.sh", body.as_bytes());

        let notifier = Notifier::new(Some(script_path.clone()));
        notifier.notify(Event::TwoFaRequired, "Need 2FA code", "test@example.com");

        // Wait for the spawned background task to complete (poll instead of fixed sleep)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if output_path.exists() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "notification script did not produce output within timeout"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let output = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(output.trim(), "2fa_required|Need 2FA code|test@example.com");
    }
}
