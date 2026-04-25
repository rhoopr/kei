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
    // Skip breakdown
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
                "--notification-script is unix-only (kei invokes scripts via /bin/sh). \
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
            match run_script(&script, event_str, &message, &username, data.as_ref()).await {
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
    }

    let mut child = cmd.spawn()?;

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
        let notifier = Notifier::new(Some(PathBuf::from("/tmp/claude/nonexistent_notify.sh")));
        // Should not panic, just log a warning (script existence checked synchronously)
        notifier.notify(
            Event::SyncComplete,
            "test message",
            "user@example.com",
            None,
        );
    }

    /// Write a shell script to a temp dir. No executable permission needed
    /// since `run_script` invokes scripts via `/bin/sh`.
    #[cfg(unix)]
    fn write_test_script(dir: &std::path::Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_success() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_test_script(dir.path(), "success.sh", b"#!/bin/sh\nexit 0\n");

        let status = run_script(&script, "test_event", "msg", "user", None)
            .await
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_script_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_test_script(dir.path(), "fail.sh", b"#!/bin/sh\nexit 1\n");

        let status = run_script(&script, "test_event", "msg", "user", None)
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
        notifier.notify(
            Event::TwoFaRequired,
            "Need 2FA code",
            "test@example.com",
            None,
        );

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

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_with_sync_data_sets_extended_env_vars() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("test_data_output.txt");
        let body = format!(
            "#!/bin/sh\necho \"$KEI_DOWNLOADED|$KEI_FAILED|$KEI_SKIPPED|$KEI_BYTES_DOWNLOADED|$KEI_SKIPPED_BY_STATE\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_data.sh", body.as_bytes());

        let data = SyncNotificationData {
            downloaded: 42,
            failed: 3,
            skipped: 100,
            bytes_downloaded: 1_500_000,
            skipped_by_state: 80,
            ..SyncNotificationData::default()
        };

        let notifier = Notifier::new(Some(script_path));
        notifier.notify(Event::SyncComplete, "test", "user@example.com", Some(&data));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if output_path.exists() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "notification script did not produce output"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let output = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(output.trim(), "42|3|100|1500000|80");
    }

    /// Test scaffold: a barrier-blocked sh script that tracks both
    /// concurrent in-flight invocations (per-pid marker files) and
    /// total invocations (single-byte appends). Each invocation:
    /// 1. Appends one byte to `invocations` (atomic on Linux).
    /// 2. Drops a marker file at `inflight/$pid`.
    /// 3. Polls until `release` exists.
    /// 4. Removes its marker on exit.
    #[cfg(unix)]
    struct BarrierFixture {
        _dir: tempfile::TempDir,
        counter_dir: PathBuf,
        release: PathBuf,
        invocations: PathBuf,
        script_path: PathBuf,
    }

    #[cfg(unix)]
    impl BarrierFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let counter_dir = dir.path().join("inflight");
            std::fs::create_dir_all(&counter_dir).unwrap();
            let release = dir.path().join("release");
            let invocations = dir.path().join("invocations");
            let body = format!(
                "#!/bin/sh\nprintf x >> \"{}\"\nmarker=\"{}/$$\"\n: > \"$marker\"\n\
                 while [ ! -f \"{}\" ]; do sleep 0.02; done\nrm -f \"$marker\"\n",
                invocations.display(),
                counter_dir.display(),
                release.display(),
            );
            let script_path = write_test_script(dir.path(), "barrier.sh", body.as_bytes());
            Self {
                _dir: dir,
                counter_dir,
                release,
                invocations,
                script_path,
            }
        }

        fn count_markers(&self) -> usize {
            std::fs::read_dir(&self.counter_dir)
                .map(|it| it.flatten().count())
                .unwrap_or(0)
        }

        fn count_invocations(&self) -> usize {
            std::fs::read(&self.invocations)
                .map(|b| b.len())
                .unwrap_or(0)
        }

        fn release_barrier(&self) {
            std::fs::write(&self.release, b"").unwrap();
        }

        async fn wait_until<F: FnMut() -> bool>(&self, timeout: Duration, mut pred: F) {
            let deadline = tokio::time::Instant::now() + timeout;
            while tokio::time::Instant::now() < deadline {
                if pred() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    /// Fire more events than `NOTIFIER_MAX_INFLIGHT` at a barrier script
    /// and confirm the semaphore caps concurrent in-flight invocations.
    /// Without the cap, every event would spawn `/bin/sh` concurrently
    /// and the marker count would exceed `NOTIFIER_MAX_INFLIGHT`.
    #[cfg(unix)]
    #[tokio::test]
    async fn notifier_semaphore_caps_concurrent_inflight() {
        let fixture = BarrierFixture::new();
        let notifier = Notifier::new(Some(fixture.script_path.clone()));
        for _ in 0..NOTIFIER_MAX_INFLIGHT * 2 {
            notifier.notify(Event::SyncStarted, "msg", "user@example.com", None);
        }

        let mut max_concurrent = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            max_concurrent = max_concurrent.max(fixture.count_markers());
            if max_concurrent >= NOTIFIER_MAX_INFLIGHT {
                for _ in 0..10 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    max_concurrent = max_concurrent.max(fixture.count_markers());
                }
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        fixture.release_barrier();
        fixture
            .wait_until(Duration::from_secs(5), || fixture.count_markers() == 0)
            .await;

        assert!(max_concurrent >= 1, "no scripts ever ran -- test setup bug");
        assert!(
            max_concurrent <= NOTIFIER_MAX_INFLIGHT,
            "semaphore did not cap concurrent scripts: max observed {max_concurrent}, cap is {NOTIFIER_MAX_INFLIGHT}",
        );
    }

    /// When more than `NOTIFIER_MAX_INFLIGHT` events are fired while every
    /// permit is held, the surplus events must be **dropped**, not queued.
    /// With the old `acquire_owned().await` we'd spawn a task per event and
    /// the surplus would run as permits became free; with `try_acquire_owned`
    /// the surplus saturates and we drop on the floor. After permits are
    /// released, fresh events must still be able to acquire (no permit leak).
    #[cfg(unix)]
    #[tracing_test::traced_test]
    #[tokio::test]
    async fn notifier_drops_events_when_saturated() {
        let fixture = BarrierFixture::new();
        let notifier = Notifier::new(Some(fixture.script_path.clone()));
        for _ in 0..NOTIFIER_MAX_INFLIGHT * 4 {
            notifier.notify(Event::SyncStarted, "msg", "user@example.com", None);
        }

        fixture
            .wait_until(Duration::from_secs(5), || {
                fixture.count_markers() >= NOTIFIER_MAX_INFLIGHT
            })
            .await;
        assert_eq!(
            fixture.count_markers(),
            NOTIFIER_MAX_INFLIGHT,
            "expected exactly {NOTIFIER_MAX_INFLIGHT} scripts holding permits"
        );

        fixture.release_barrier();
        fixture
            .wait_until(Duration::from_secs(5), || fixture.count_markers() == 0)
            .await;
        assert_eq!(
            fixture.count_markers(),
            0,
            "scripts did not drain after release"
        );

        // Dropped events must not retroactively run once permits are free.
        assert_eq!(
            fixture.count_invocations(),
            NOTIFIER_MAX_INFLIGHT,
            "saturation drop regressed: surplus events ran retroactively"
        );
        assert!(
            logs_contain("Notifier saturated"),
            "expected a 'Notifier saturated' warning during the flood"
        );

        // Permit-leak guard: after drain, fresh events should run.
        // With `release` in place, each new script exits immediately.
        const FRESH_BATCH: usize = 4;
        for _ in 0..FRESH_BATCH {
            notifier.notify(Event::SyncStarted, "msg", "user@example.com", None);
        }
        let expected_total = NOTIFIER_MAX_INFLIGHT + FRESH_BATCH;
        fixture
            .wait_until(Duration::from_secs(5), || {
                fixture.count_invocations() >= expected_total
            })
            .await;
        assert_eq!(
            fixture.count_invocations(),
            expected_total,
            "permit leak: post-release events failed to acquire"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn notify_without_data_omits_extended_vars() {
        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("test_no_data.txt");
        let body = format!(
            "#!/bin/sh\necho \"${{KEI_DOWNLOADED:-unset}}|${{KEI_FAILED:-unset}}\" > {}\n",
            output_path.display()
        );
        let script_path = write_test_script(dir.path(), "test_no_data.sh", body.as_bytes());

        let notifier = Notifier::new(Some(script_path));
        notifier.notify(Event::SyncComplete, "test", "user@example.com", None);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if output_path.exists() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "notification script did not produce output"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let output = std::fs::read_to_string(&output_path).unwrap();
        assert_eq!(output.trim(), "unset|unset");
    }
}
