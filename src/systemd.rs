//! Thin wrapper around systemd `sd_notify` integration.
//!
//! All functions are no-ops when `enabled` is false or on non-Linux platforms.
//! This keeps the rest of the codebase free from `#[cfg]` conditionals.

use std::time::Duration;

use tokio_util::sync::CancellationToken;

/// Holds the runtime flag controlling whether sd-notify messages are sent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SystemdNotifier {
    enabled: bool,
}

#[derive(Debug)]
pub(crate) struct SystemdWatchdogTask {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SystemdWatchdogTask {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

impl SystemdNotifier {
    /// Create a new notifier. When `enabled` is false, all methods are no-ops.
    pub(crate) const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Send `READY=1` to systemd (service startup complete).
    pub(crate) fn notify_ready(self) {
        if !self.enabled {
            return;
        }
        self.send_impl_ready();
    }

    /// The worker is ready to run or retry. This doesn't certify a sync plan.
    #[must_use]
    pub(crate) fn start_worker(self, shutdown: CancellationToken) -> SystemdWatchdogTask {
        self.notify_ready();
        self.start_watchdog_heartbeat(shutdown)
    }

    /// Send `STOPPING=1` to systemd (service shutting down).
    pub(crate) fn notify_stopping(self) {
        if !self.enabled {
            return;
        }
        self.send_impl_stopping();
    }

    /// Send `STATUS=<msg>` to systemd (human-readable status).
    pub(crate) fn notify_status(self, msg: &str) {
        if !self.enabled {
            return;
        }
        self.send_impl_status(msg);
    }

    /// Send `WATCHDOG=1` to systemd (keepalive ping).
    pub(crate) fn notify_watchdog(self) {
        if !self.enabled {
            return;
        }
        self.send_impl_watchdog();
    }

    /// Start a background watchdog heartbeat when systemd requested one.
    ///
    /// systemd passes `WATCHDOG_USEC` to notify services when `WatchdogSec=`
    /// is active. A long watch-mode sleep can be much larger than that
    /// timeout, so a cycle-bound ping is not enough; keep pinging until the
    /// sync run exits or shutdown starts.
    pub(crate) fn start_watchdog_heartbeat(
        self,
        shutdown_token: CancellationToken,
    ) -> SystemdWatchdogTask {
        let Some(timeout) = self.watchdog_timeout() else {
            return SystemdWatchdogTask { handle: None };
        };
        let interval = watchdog_heartbeat_interval(timeout);
        let handle = tokio::spawn(watchdog_heartbeat_loop(
            interval,
            shutdown_token,
            move || self.notify_watchdog(),
        ));
        SystemdWatchdogTask {
            handle: Some(handle),
        }
    }

    fn watchdog_timeout(self) -> Option<Duration> {
        if !self.enabled {
            return None;
        }
        self.watchdog_timeout_impl()
    }

    #[cfg(target_os = "linux")]
    fn send_impl_ready(self) {
        if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Ready]) {
            tracing::debug!(error = %e, "sd_notify READY failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_ready(self) {
        let _ = self;
    }

    #[cfg(target_os = "linux")]
    fn send_impl_stopping(self) {
        if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Stopping]) {
            tracing::debug!(error = %e, "sd_notify STOPPING failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_stopping(self) {
        let _ = self;
    }

    #[cfg(target_os = "linux")]
    fn send_impl_status(self, msg: &str) {
        if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Status(msg)]) {
            tracing::debug!(error = %e, "sd_notify STATUS failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_status(self, _msg: &str) {
        let _ = self;
    }

    #[cfg(target_os = "linux")]
    fn send_impl_watchdog(self) {
        if let Err(e) = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]) {
            tracing::debug!(error = %e, "sd_notify WATCHDOG failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_watchdog(self) {
        let _ = self;
    }

    #[cfg(target_os = "linux")]
    fn watchdog_timeout_impl(self) -> Option<Duration> {
        sd_notify::watchdog_enabled()
    }

    #[cfg(not(target_os = "linux"))]
    fn watchdog_timeout_impl(self) -> Option<Duration> {
        let _ = self;
        None
    }
}

fn watchdog_heartbeat_interval(timeout: Duration) -> Duration {
    if timeout.is_zero() {
        return Duration::from_secs(1);
    }
    timeout
        .checked_div(2)
        .filter(|d| !d.is_zero())
        .unwrap_or(timeout)
}

async fn watchdog_heartbeat_loop<F>(
    interval: Duration,
    shutdown_token: CancellationToken,
    mut notify_watchdog: F,
) where
    F: FnMut(),
{
    notify_watchdog();
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => notify_watchdog(),
            () = shutdown_token.cancelled() => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn disabled_notifier_is_noop() {
        let n = SystemdNotifier::new(false);
        n.notify_ready();
        n.notify_stopping();
        n.notify_status("test");
        n.notify_watchdog();
    }

    #[test]
    fn enabled_notifier_does_not_panic() {
        // On non-Linux this is still a no-op; on Linux without a socket it logs debug
        let n = SystemdNotifier::new(true);
        n.notify_ready();
        n.notify_stopping();
        n.notify_status("test");
        n.notify_watchdog();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(start_paused = true)]
    async fn systemd_worker_is_ready_and_pings_while_discovery_waits() {
        use std::os::unix::net::UnixDatagram;
        use std::process::Command;

        const CHILD: &str = "KEI_TEST_SYSTEMD_DISCOVERY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let notifier = SystemdNotifier::new(true);
            let shutdown = CancellationToken::new();
            let _watchdog = notifier.start_worker(shutdown.clone());
            notifier.notify_status("Waiting for iCloud indexing readiness");
            tokio::time::sleep(Duration::from_millis(2500)).await;
            shutdown.cancel();
            notifier.notify_stopping();
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("notify");
        let socket = UnixDatagram::bind(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "systemd::tests::systemd_worker_is_ready_and_pings_while_discovery_waits",
            ])
            .env(CHILD, "1")
            .env("NOTIFY_SOCKET", &path)
            .env("WATCHDOG_USEC", "2000000")
            .env_remove("WATCHDOG_PID")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stdout)
        );

        let mut messages = Vec::new();
        loop {
            let mut buffer = [0; 256];
            let size = socket.recv(&mut buffer).unwrap();
            let message = std::str::from_utf8(&buffer[..size]).unwrap().trim();
            messages.push(message.to_owned());
            if message == "STOPPING=1" {
                break;
            }
        }
        assert_eq!(messages[0], "READY=1");
        assert_eq!(messages[1], "STATUS=Waiting for iCloud indexing readiness");
        assert!(messages.iter().filter(|m| *m == "WATCHDOG=1").count() >= 2);
        assert!(!messages.iter().any(|m| m.starts_with("EXTEND_TIMEOUT")));
    }

    #[test]
    fn watchdog_heartbeat_interval_is_half_timeout() {
        assert_eq!(
            watchdog_heartbeat_interval(Duration::from_secs(120)),
            Duration::from_secs(60)
        );
        assert_eq!(
            watchdog_heartbeat_interval(Duration::from_micros(1)),
            Duration::from_nanos(500)
        );
        assert_eq!(
            watchdog_heartbeat_interval(Duration::ZERO),
            Duration::from_secs(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_heartbeat_loop_pings_until_shutdown() {
        let shutdown_token = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let notify_calls = Arc::clone(&calls);
        let task = tokio::spawn(watchdog_heartbeat_loop(
            Duration::from_secs(60),
            shutdown_token.clone(),
            move || {
                notify_calls.fetch_add(1, Ordering::SeqCst);
            },
        ));

        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        tokio::time::sleep(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        shutdown_token.cancel();
        task.await.unwrap();

        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
