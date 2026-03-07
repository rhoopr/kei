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
        .env("ICLOUDPD_EVENT", event)
        .env("ICLOUDPD_MESSAGE", message)
        .env("ICLOUDPD_USERNAME", username)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .spawn()?;

    match tokio::time::timeout(SCRIPT_TIMEOUT, child.wait()).await {
        Ok(result) => Ok(result?),
        Err(_) => {
            tracing::warn!("Notification script timed out, killing");
            child.kill().await.ok();
            anyhow::bail!(
                "notification script timed out after {}s",
                SCRIPT_TIMEOUT.as_secs()
            )
        }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_runs_script_with_env_vars() {
        use std::os::unix::fs::PermissionsExt;

        let script_path = PathBuf::from("/tmp/claude/test_notify.sh");
        let output_path = PathBuf::from("/tmp/claude/test_notify_output.txt");

        // Clean up from previous runs
        let _ = std::fs::remove_file(&output_path);
        std::fs::create_dir_all("/tmp/claude").ok();

        std::fs::write(
            &script_path,
            format!(
                "#!/bin/sh\necho \"$ICLOUDPD_EVENT|$ICLOUDPD_MESSAGE|$ICLOUDPD_USERNAME\" > {}\n",
                output_path.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let notifier = Notifier::new(Some(script_path.clone()));
        notifier.notify(Event::TwoFaRequired, "Need 2FA code", "test@example.com");

        // Wait for the spawned background task to complete
        tokio::time::sleep(Duration::from_millis(500)).await;

        let output = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(output.trim(), "2fa_required|Need 2FA code|test@example.com");

        // Clean up
        let _ = std::fs::remove_file(&script_path);
        let _ = std::fs::remove_file(&output_path);
    }
}
