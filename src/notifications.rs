//! Notification script support for unattended operation.
//!
//! Fires a user-provided script with event information as environment variables.
//! Used to notify users of 2FA expiry, sync completion, failures, etc.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Events that trigger notification scripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    /// 2FA code is needed (session expired in headless mode)
    TwoFaRequired,
    /// A sync cycle is about to run (fires before run_cycle, after skip-check)
    SyncStarted,
    /// Sync cycle completed successfully
    SyncComplete,
    /// Sync cycle had failures
    SyncFailed,
    /// Session expired and re-authentication failed
    SessionExpired,
}

impl Event {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TwoFaRequired => "2fa_required",
            Self::SyncStarted => "sync_started",
            Self::SyncComplete => "sync_complete",
            Self::SyncFailed => "sync_failed",
            Self::SessionExpired => "session_expired",
        }
    }
}

/// Sync statistics passed to notification scripts as environment variables.
#[derive(Debug, Clone, Default)]
pub(crate) struct SyncNotificationData {
    pub assets_seen: u64,
    pub downloaded: usize,
    pub failed: usize,
    pub skipped: usize,
    pub bytes_downloaded: u64,
    pub disk_bytes_written: u64,
    pub elapsed_secs: f64,
    pub interrupted: bool,
    pub exif_failures: usize,
    pub state_write_failures: usize,
    pub enumeration_errors: usize,
    pub pagination_shortfall_warnings: usize,
    pub pagination_shortfall_assets: u64,
    pub sync_token_blocked: bool,
    pub sync_token_blocked_reason: Option<&'static str>,
    pub skipped_by_state: usize,
    pub skipped_on_disk: usize,
    pub skipped_by_media_type: usize,
    pub skipped_by_date_range: usize,
    pub skipped_by_live_photo: usize,
    pub skipped_by_filename: usize,
    pub skipped_by_excluded_album: usize,
    pub skipped_live_photo_variant: usize,
    pub skipped_duplicates: usize,
    pub skipped_retry_exhausted: usize,
    pub skipped_retry_only: usize,
}

impl From<&crate::download::SyncStats> for SyncNotificationData {
    fn from(s: &crate::download::SyncStats) -> Self {
        Self {
            assets_seen: s.assets_seen,
            downloaded: s.downloaded,
            failed: s.failed,
            skipped: s.skipped.total(),
            bytes_downloaded: s.bytes_downloaded,
            disk_bytes_written: s.disk_bytes_written,
            elapsed_secs: s.elapsed_secs,
            interrupted: s.interrupted,
            exif_failures: s.exif_failures,
            state_write_failures: s.state_write_failures,
            enumeration_errors: s.enumeration_errors,
            pagination_shortfall_warnings: s.pagination_shortfall_warnings,
            pagination_shortfall_assets: s.pagination_shortfall_assets,
            sync_token_blocked: s.sync_token_blocked,
            sync_token_blocked_reason: s.sync_token_blocked_reason,
            skipped_by_state: s.skipped.by_state,
            skipped_on_disk: s.skipped.on_disk,
            skipped_by_media_type: s.skipped.by_media_type,
            skipped_by_date_range: s.skipped.by_date_range,
            skipped_by_live_photo: s.skipped.by_live_photo,
            skipped_by_filename: s.skipped.by_filename,
            skipped_by_excluded_album: s.skipped.by_excluded_album,
            skipped_live_photo_variant: s.skipped.ampm_variant,
            skipped_duplicates: s.skipped.duplicates,
            skipped_retry_exhausted: s.skipped.retry_exhausted,
            skipped_retry_only: s.skipped.retry_only,
        }
    }
}

/// Notification dispatcher. Holds an optional script path.
/// When no script is configured, all methods are no-ops.
#[derive(Debug, Clone)]
pub(crate) struct Notifier {
    script: Option<PathBuf>,
    /// Bounds how many notification scripts can run concurrently. A
    /// misbehaving or long-running script can't queue an unbounded
    /// number of spawned tasks behind itself under load.
    concurrency: Arc<tokio::sync::Semaphore>,
}

/// Timeout for notification scripts.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on concurrent notification-script invocations. Events fire at
/// sync-cycle boundaries (start/complete/failure/token-required), so
/// 8 is plenty of headroom in watch mode while still bounding leaks.
const NOTIFIER_MAX_INFLIGHT: usize = 8;

impl Notifier {
    pub fn new(script: Option<PathBuf>) -> Self {
        // kei invokes scripts via `/bin/sh`, which isn't available on Windows.
        // Rather than let spawn fail silently on every event, drop the script
        // and warn once at construction time.
        if script.is_some() && cfg!(windows) {
            tracing::warn!(
                "[notifications].script is unix-only (kei invokes scripts via /bin/sh). \
                 Ignoring the configured script on Windows."
            );
            return Self {
                script: None,
                concurrency: Arc::new(tokio::sync::Semaphore::new(NOTIFIER_MAX_INFLIGHT)),
            };
        }
        Self {
            script,
            concurrency: Arc::new(tokio::sync::Semaphore::new(NOTIFIER_MAX_INFLIGHT)),
        }
    }

    /// Fire the notification script with the given event.
    /// Fire-and-forget: spawns the script in a background task so it never blocks sync.
    pub fn notify(
        &self,
        event: Event,
        message: &str,
        username: &str,
        data: Option<&SyncNotificationData>,
        report_path: Option<&Path>,
    ) {
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
        let data = data.cloned();
        let report_path = report_path.map(Path::to_path_buf);

        tracing::debug!(event = event_str, "Firing notification script");

        // Drop on saturation rather than queue: spawning a task that then
        // parks on `acquire_owned().await` is a softer version of the
        // unbounded-spawn behavior the semaphore exists to prevent. With
        // `try_acquire_owned` we also keep the saturation path observable
        // via the `notifier saturated` warning.
        let permit = match Arc::clone(&self.concurrency).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => {
                tracing::warn!(
                    event = event_str,
                    in_flight = NOTIFIER_MAX_INFLIGHT,
                    "Notifier saturated, dropping event"
                );
                return;
            }
            Err(tokio::sync::TryAcquireError::Closed) => {
                // Only reachable if the underlying semaphore is closed,
                // which kei never does. Treat as a process-exit no-op.
                return;
            }
        };
        tokio::spawn(async move {
            let _permit = permit;
            match run_script(
                &script,
                event_str,
                &message,
                &username,
                data.as_ref(),
                report_path.as_deref(),
            )
            .await
            {
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
    data: Option<&SyncNotificationData>,
    report_path: Option<&Path>,
) -> anyhow::Result<std::process::ExitStatus> {
    // Execute via /bin/sh to avoid ETXTBSY ("Text file busy") races when
    // the script file was recently written or replaced (e.g. config reload,
    // `kei setup`, parallel tests). Scripts with shebangs work fine via sh.
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg(script)
        .env("KEI_EVENT", event)
        .env("KEI_MESSAGE", message)
        .env("KEI_ICLOUD_USERNAME", username)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());

    if let Some(d) = data {
        cmd.env("KEI_ASSETS_SEEN", d.assets_seen.to_string())
            .env("KEI_DOWNLOADED", d.downloaded.to_string())
            .env("KEI_FAILED", d.failed.to_string())
            .env("KEI_SKIPPED", d.skipped.to_string())
            .env("KEI_INTERRUPTED", d.interrupted.to_string())
            .env("KEI_BYTES_DOWNLOADED", d.bytes_downloaded.to_string())
            .env("KEI_DISK_BYTES", d.disk_bytes_written.to_string())
            .env("KEI_ELAPSED_SECS", format!("{:.1}", d.elapsed_secs))
            .env("KEI_EXIF_FAILURES", d.exif_failures.to_string())
            .env(
                "KEI_STATE_WRITE_FAILURES",
                d.state_write_failures.to_string(),
            )
            .env("KEI_ENUMERATION_ERRORS", d.enumeration_errors.to_string())
            .env(
                "KEI_PAGINATION_SHORTFALL_WARNINGS",
                d.pagination_shortfall_warnings.to_string(),
            )
            .env(
                "KEI_PAGINATION_SHORTFALL_ASSETS",
                d.pagination_shortfall_assets.to_string(),
            )
            .env("KEI_SYNC_TOKEN_BLOCKED", d.sync_token_blocked.to_string())
            .env("KEI_SKIPPED_BY_STATE", d.skipped_by_state.to_string())
            .env("KEI_SKIPPED_ON_DISK", d.skipped_on_disk.to_string())
            .env(
                "KEI_SKIPPED_BY_MEDIA_TYPE",
                d.skipped_by_media_type.to_string(),
            )
            .env(
                "KEI_SKIPPED_BY_DATE_RANGE",
                d.skipped_by_date_range.to_string(),
            )
            .env(
                "KEI_SKIPPED_BY_LIVE_PHOTO",
                d.skipped_by_live_photo.to_string(),
            )
            .env("KEI_SKIPPED_BY_FILENAME", d.skipped_by_filename.to_string())
            .env(
                "KEI_SKIPPED_BY_EXCLUDED_ALBUM",
                d.skipped_by_excluded_album.to_string(),
            )
            .env(
                "KEI_SKIPPED_LIVE_PHOTO_VARIANT",
                d.skipped_live_photo_variant.to_string(),
            )
            .env("KEI_SKIPPED_DUPLICATES", d.skipped_duplicates.to_string())
            .env(
                "KEI_SKIPPED_RETRY_EXHAUSTED",
                d.skipped_retry_exhausted.to_string(),
            )
            .env("KEI_SKIPPED_RETRY_ONLY", d.skipped_retry_only.to_string());
        if let Some(reason) = &d.sync_token_blocked_reason {
            cmd.env("KEI_SYNC_TOKEN_BLOCK_REASON", reason);
        }
    }

    if let Some(path) = report_path {
        cmd.env("KEI_REPORT_JSON", path);
    }

    let mut child = cmd.spawn()?;

    if let Ok(result) = tokio::time::timeout(SCRIPT_TIMEOUT, child.wait()).await {
        Ok(result?)
    } else {
        tracing::warn!("Notification script timed out, killing");
        let _ = child.kill().await;
        anyhow::bail!(
            "Notification script timed out after {} seconds.",
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
        assert_eq!(Event::SyncStarted.as_str(), "sync_started");
        assert_eq!(Event::SyncComplete.as_str(), "sync_complete");
        assert_eq!(Event::SyncFailed.as_str(), "sync_failed");
        assert_eq!(Event::SessionExpired.as_str(), "session_expired");
    }

    #[cfg(windows)]
    #[test]
    fn notifier_drops_script_on_windows() {
        let notifier = Notifier::new(Some(PathBuf::from("C:/does/not/matter.sh")));
        assert!(notifier.script.is_none());
    }

    #[test]
    fn notifier_none_is_noop() {
        let notifier = Notifier::new(None);
        assert!(notifier.script.is_none());
    }

    #[test]
    fn notify_with_nonexistent_script() {
        let notifier = Notifier::new(Some(PathBuf::from("/tmp/codex/kei/nonexistent_notify.sh")));
        // Should not panic, just log a warning (script existence checked synchronously)
        notifier.notify(Event::SyncComplete, "test message", "user", None, None);
    }

    /// Write a shell script to a temp dir. No executable permission needed
    /// since `run_script` invokes scripts via `/bin/sh`.
    #[cfg(unix)]
    fn write_test_script(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap_or_else(|err| {
            panic!("write notification test script {}: {err}", path.display())
        });
        path
    }

    #[cfg(unix)]
    fn notification_test_dir(context: &str) -> tempfile::TempDir {
        tempfile::tempdir().unwrap_or_else(|err| panic!("create temp dir for {context}: {err}"))
    }

    #[cfg(unix)]
    fn read_script_output(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|err| {
            panic!("read notification script output {}: {err}", path.display())
        })
    }

    #[cfg(unix)]
    async fn wait_until_file_contains(path: &Path, expected: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let last_observed = match std::fs::read_to_string(path) {
                Ok(contents) if contents.contains(expected) => return,
                Ok(contents) => contents,
                Err(err) => format!("<read failed: {err}>"),
            };

            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {} to contain {expected:?}; last observed: {last_observed:?}",
                path.display()
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    async fn wait_until_available_permits(
        semaphore: &tokio::sync::Semaphore,
        expected_permits: usize,
    ) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let available = semaphore.available_permits();
            if available == expected_permits {
                return;
            }

            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for notifier permits; expected {expected_permits}, observed {available}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_success() {
        let dir = notification_test_dir("run_script_success");
        let script = write_test_script(dir.path(), "success.sh", b"#!/bin/sh\nexit 0\n");

        let status = run_script(&script, "test_event", "msg", "user", None, None)
            .await
            .unwrap_or_else(|err| panic!("run notification script {}: {err}", script.display()));
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_nonzero_exit() {
        let dir = notification_test_dir("run_script_nonzero_exit");
        let script = write_test_script(dir.path(), "fail.sh", b"#!/bin/sh\nexit 1\n");

        let status = run_script(&script, "test_event", "msg", "user", None, None)
            .await
            .unwrap_or_else(|err| panic!("run notification script {}: {err}", script.display()));
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_runs_script_with_env_vars() {
        let dir = notification_test_dir("notify_runs_script_with_env_vars");
        let output_path = dir.path().join("test_notify_output.txt");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_EVENT|$KEI_MESSAGE|$KEI_ICLOUD_USERNAME\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_notify.sh", body.as_bytes());

        let status = run_script(
            &script_path,
            Event::TwoFaRequired.as_str(),
            "Need 2FA code",
            "icloud@example.com",
            None,
            None,
        )
        .await
        .expect("run notification script");
        assert!(status.success());

        let output = read_script_output(&output_path);
        assert_eq!(
            output.trim(),
            "2fa_required|Need 2FA code|icloud@example.com"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_with_report_path_keeps_legacy_stats_env_vars() {
        let dir = notification_test_dir("notify_with_report_path_keeps_legacy_stats_env_vars");
        let output_path = dir.path().join("test_data_output.txt");
        let report_path = dir.path().join("sync_report.json");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_REPORT_JSON|$KEI_ICLOUD_USERNAME|$KEI_DOWNLOADED|$KEI_FAILED|$KEI_SKIPPED|$KEI_BYTES_DOWNLOADED|$KEI_SKIPPED_BY_STATE|$KEI_SYNC_TOKEN_BLOCK_REASON\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_data.sh", body.as_bytes());
        let data = SyncNotificationData {
            downloaded: 2,
            failed: 3,
            skipped: 5,
            bytes_downloaded: 4096,
            skipped_by_state: 4,
            sync_token_blocked_reason: Some("pagination_shortfall"),
            ..SyncNotificationData::default()
        };

        let status = run_script(
            &script_path,
            Event::SyncComplete.as_str(),
            "test",
            "icloud@example.com",
            Some(&data),
            Some(&report_path),
        )
        .await
        .expect("run notification script with report path");
        assert!(status.success());

        let output = read_script_output(&output_path);
        assert_eq!(
            output.trim(),
            format!(
                "{}|icloud@example.com|2|3|5|4096|4|pagination_shortfall",
                report_path.display()
            )
        );
    }

    /// The semaphore itself is the concurrency contract. Acquire every
    /// permit explicitly instead of coordinating shell scripts with marker
    /// files and sleep loops.
    #[cfg(unix)]
    #[tokio::test]
    async fn notifier_semaphore_caps_concurrent_inflight() {
        let notifier = Notifier::new(Some(PathBuf::from("/tmp/codex/kei/unused-notify.sh")));
        let held: Vec<_> = (0..NOTIFIER_MAX_INFLIGHT)
            .map(|_| {
                Arc::clone(&notifier.concurrency)
                    .try_acquire_owned()
                    .expect("permit should be available")
            })
            .collect();
        assert_eq!(
            held.len(),
            NOTIFIER_MAX_INFLIGHT,
            "test must hold every notifier permit"
        );
        assert_eq!(
            notifier.concurrency.available_permits(),
            0,
            "all notification permits should be held"
        );
        assert!(
            Arc::clone(&notifier.concurrency)
                .try_acquire_owned()
                .is_err(),
            "semaphore must reject the next concurrent invocation"
        );
        drop(held);
        assert_eq!(
            notifier.concurrency.available_permits(),
            NOTIFIER_MAX_INFLIGHT,
            "dropping held permits must restore full capacity"
        );
    }

    /// When more than `NOTIFIER_MAX_INFLIGHT` events are fired while every
    /// permit is held, the surplus events must be **dropped**, not queued.
    /// With the old `acquire_owned().await` we'd spawn a task per event and
    /// the surplus would run as permits became free; with `try_acquire_owned`
    /// the surplus saturates and we drop on the floor. After permits are
    /// released, fresh events must still be able to acquire (no permit leak).
    #[cfg(unix)]
    #[tokio::test]
    async fn notifier_drops_events_when_saturated() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let dir = notification_test_dir("saturated notifier");
        let output_path = dir.path().join("fresh-notify.txt");
        let body = format!("#!/bin/sh\necho fresh > {}\n", output_path.display());
        let script_path = write_test_script(dir.path(), "notify.sh", body.as_bytes());
        let notifier = Notifier::new(Some(script_path));
        let held: Vec<_> = (0..NOTIFIER_MAX_INFLIGHT)
            .map(|_| {
                Arc::clone(&notifier.concurrency)
                    .try_acquire_owned()
                    .expect("permit should be available")
            })
            .collect();
        assert_eq!(
            notifier.concurrency.available_permits(),
            0,
            "test must hold every permit before calling notify"
        );

        notifier.notify(Event::SyncStarted, "msg", "user", None, None);

        let events = capture.events();
        let saturated = events
            .iter()
            .find(|event| {
                event.level == tracing::Level::WARN
                    && event.message() == Some("Notifier saturated, dropping event")
            })
            .unwrap_or_else(|| panic!("missing notifier saturation warning: {events:?}"));
        assert_eq!(saturated.field("event"), Some(Event::SyncStarted.as_str()));
        let expected_in_flight = NOTIFIER_MAX_INFLIGHT.to_string();
        assert_eq!(
            saturated.field("in_flight"),
            Some(expected_in_flight.as_str())
        );
        assert_eq!(
            notifier.concurrency.available_permits(),
            0,
            "dropped notification must not consume or leak a permit"
        );
        assert!(
            !output_path.exists(),
            "saturated notification should be dropped instead of queued"
        );

        drop(held);
        notifier.notify(Event::SyncStarted, "msg", "user", None, None);
        wait_until_file_contains(&output_path, "fresh").await;
        wait_until_available_permits(&notifier.concurrency, NOTIFIER_MAX_INFLIGHT).await;
        assert_eq!(
            read_script_output(&output_path).trim(),
            "fresh",
            "fresh notification should run through Notifier::notify after saturation"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_without_report_path_omits_report_env() {
        let dir = notification_test_dir("notify_without_data_omits_extended_vars");
        let output_path = dir.path().join("test_no_data.txt");
        let body = format!(
            "#!/bin/sh\necho \"${{KEI_REPORT_JSON:-unset}}\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_no_data.sh", body.as_bytes());

        let status = run_script(
            &script_path,
            Event::SyncComplete.as_str(),
            "test",
            "user",
            None,
            None,
        )
        .await
        .expect("run notification script without sync data");
        assert!(status.success());

        let output = read_script_output(&output_path);
        assert_eq!(output.trim(), "unset");
    }
}
