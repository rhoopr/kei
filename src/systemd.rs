//! Thin wrapper around systemd sd_notify integration.
//!
//! All functions are no-ops when `enabled` is false or on non-Linux platforms.
//! This keeps the rest of the codebase free from `#[cfg]` conditionals.

/// Holds the runtime flag controlling whether sd-notify messages are sent.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SystemdNotifier {
    enabled: bool,
}

impl SystemdNotifier {
    /// Create a new notifier. When `enabled` is false, all methods are no-ops.
    pub(crate) fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Send `READY=1` to systemd (service startup complete).
    pub(crate) fn notify_ready(&self) {
        if !self.enabled {
            return;
        }
        self.send_impl_ready();
    }

    /// Send `STOPPING=1` to systemd (service shutting down).
    pub(crate) fn notify_stopping(&self) {
        if !self.enabled {
            return;
        }
        self.send_impl_stopping();
    }

    /// Send `STATUS=<msg>` to systemd (human-readable status).
    pub(crate) fn notify_status(&self, msg: &str) {
        if !self.enabled {
            return;
        }
        self.send_impl_status(msg);
    }

    /// Send `WATCHDOG=1` to systemd (keepalive ping).
    pub(crate) fn notify_watchdog(&self) {
        if !self.enabled {
            return;
        }
        self.send_impl_watchdog();
    }

    #[cfg(target_os = "linux")]
    fn send_impl_ready(&self) {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
            tracing::debug!(error = %e, "sd_notify READY failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_ready(&self) {}

    #[cfg(target_os = "linux")]
    fn send_impl_stopping(&self) {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
            tracing::debug!(error = %e, "sd_notify STOPPING failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_stopping(&self) {}

    #[cfg(target_os = "linux")]
    fn send_impl_status(&self, msg: &str) {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Status(msg)]) {
            tracing::debug!(error = %e, "sd_notify STATUS failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_status(&self, _msg: &str) {}

    #[cfg(target_os = "linux")]
    fn send_impl_watchdog(&self) {
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Watchdog]) {
            tracing::debug!(error = %e, "sd_notify WATCHDOG failed");
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn send_impl_watchdog(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
