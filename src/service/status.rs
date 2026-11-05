//! `kei service status` dispatcher and the cross-platform `ServiceState`
//! data type that backs the `Service:` section in `kei status`.
//!
//! `run()` powers `kei service status`: it delegates to the per-platform
//! `status()` function, which prints a single line tuned to that
//! platform's vocabulary (`systemctl --user`, `launchctl print`,
//! `sc.exe query`).
//!
//! [`service_state`] is the data-only counterpart used by `kei status`:
//! each backend reports back as a [`ServiceState`] so the renderer in
//! [`render_oneline`] is platform-agnostic and can be unit-tested without
//! shelling out. Container hosts short-circuit to
//! [`ServiceState::InContainer`] before any platform query, so docker /
//! kubernetes deployments never see a misleading `not installed`.

use anyhow::Result;
use chrono::{DateTime, Utc};

pub(crate) async fn run() -> Result<()> {
    dispatch().await
}

#[cfg(target_os = "linux")]
async fn dispatch() -> Result<()> {
    crate::service::linux::status().await
}

#[cfg(target_os = "macos")]
async fn dispatch() -> Result<()> {
    crate::service::macos::status().await
}

#[cfg(target_os = "windows")]
async fn dispatch() -> Result<()> {
    crate::service::windows::status().await
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn dispatch() -> Result<()> {
    Err(anyhow::anyhow!(
        "`kei service status` is not available on this platform."
    ))
}

/// Cross-platform view of "is kei registered as a service on this host".
///
/// Each backend exposes a `service_state()` that returns one of these
/// variants. The renderer in [`render_oneline`] is the single source of
/// truth for the `Service:` line in `kei status` output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServiceState {
    /// No service file / SCM entry / launchd plist registered for kei.
    NotInstalled,
    /// Service is registered. `state_label` carries the lifecycle
    /// verdict (`"running"`, `"stopped"`, `"failed"`, ...); `since`
    /// renders only when the label is `"running"` so a stale
    /// activation timestamp on a stopped unit cannot mislead.
    Installed {
        backend: &'static str,
        state_label: &'static str,
        since: Option<DateTime<Utc>>,
        pid: Option<u32>,
    },
    /// The service is registered but the platform's query surface is
    /// unavailable: no session bus on linux SSH-without-linger, no GUI
    /// domain on a headless macOS CI runner, non-elevated SCM on
    /// Windows.
    BackendUnavailable {
        backend: &'static str,
        reason: &'static str,
    },
    /// Running inside a container (Docker, Kubernetes, Podman, ...).
    /// kei's service-management surface is a no-op here: the container's
    /// process supervisor is what restarts kei.
    InContainer { supervisor: &'static str },
}

/// Lifecycle label used by [`ServiceState::Installed`] for a running
/// service. The renderer compares against this constant to decide
/// whether to attach the `since` clause; per-platform adapters set
/// `state_label` to this string when the service is actively running.
pub(crate) const RUNNING_LABEL: &str = "running";

/// Cross-platform `service_state()` dispatcher used by `kei status`.
///
/// Container detection is checked first so `kei status` inside Docker
/// reports the supervisor rather than running a per-platform probe that
/// would (correctly) say "not installed" and confuse the operator.
pub(crate) async fn service_state() -> Result<ServiceState> {
    if let Some(supervisor) = crate::service::env::container_supervisor() {
        return Ok(ServiceState::InContainer { supervisor });
    }
    platform_service_state().await
}

#[cfg(target_os = "linux")]
async fn platform_service_state() -> Result<ServiceState> {
    crate::service::linux::service_state().await
}

#[cfg(target_os = "macos")]
async fn platform_service_state() -> Result<ServiceState> {
    crate::service::macos::service_state().await
}

#[cfg(target_os = "windows")]
async fn platform_service_state() -> Result<ServiceState> {
    crate::service::windows::service_state().await
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
async fn platform_service_state() -> Result<ServiceState> {
    Ok(ServiceState::NotInstalled)
}

/// Renders a [`ServiceState`] as the single `Service: ...` line that
/// `kei status` emits. Pure function; tests cover every variant.
pub(crate) fn render_oneline(state: &ServiceState) -> String {
    match state {
        ServiceState::NotInstalled => {
            "Service: not installed (run `kei service run` under a supervisor for background sync)"
                .to_string()
        }
        ServiceState::InContainer { supervisor } => format!(
            "Service: container-managed (process supervisor: {supervisor}; start the worker with `kei service run`)"
        ),
        ServiceState::BackendUnavailable { backend, reason } => {
            format!("Service: installed ({backend}, {reason})")
        }
        ServiceState::Installed {
            backend,
            state_label,
            since,
            pid,
        } => {
            let mut detail = format!("Service: {state_label} ({backend}");
            if let Some(pid) = pid {
                detail.push_str(&format!(", pid {pid}"));
            }
            if *state_label == RUNNING_LABEL
                && let Some(since) = since
            {
                detail.push_str(&format!(", since {}", since.format("%Y-%m-%d %H:%M UTC")));
            }
            detail.push(')');
            detail
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn fixed_since() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, 7, 14, 32, 1).unwrap()
    }

    #[test]
    fn renders_not_installed() {
        assert_eq!(
            render_oneline(&ServiceState::NotInstalled),
            "Service: not installed (run `kei service run` under a supervisor for background sync)",
        );
    }

    #[test]
    fn renders_in_container() {
        assert_eq!(
            render_oneline(&ServiceState::InContainer {
                supervisor: "docker",
            }),
            "Service: container-managed (process supervisor: docker; start the worker with `kei service run`)",
        );
    }

    #[test]
    fn renders_backend_unavailable() {
        assert_eq!(
            render_oneline(&ServiceState::BackendUnavailable {
                backend: "systemd user",
                reason: "bus unavailable",
            }),
            "Service: installed (systemd user, bus unavailable)",
        );
    }

    #[test]
    fn renders_running_with_pid_and_since() {
        let line = render_oneline(&ServiceState::Installed {
            backend: "systemd user",
            state_label: RUNNING_LABEL,
            since: Some(fixed_since()),
            pid: Some(12345),
        });
        assert_eq!(
            line,
            "Service: running (systemd user, pid 12345, since 2026-05-07 14:32 UTC)",
        );
    }

    #[test]
    fn renders_running_without_since_or_pid() {
        let line = render_oneline(&ServiceState::Installed {
            backend: "windows scm",
            state_label: RUNNING_LABEL,
            since: None,
            pid: None,
        });
        assert_eq!(line, "Service: running (windows scm)");
    }

    #[test]
    fn renders_running_with_pid_only_when_since_missing() {
        // macOS path: launchctl print exposes the PID but no start time.
        let line = render_oneline(&ServiceState::Installed {
            backend: "launchd user",
            state_label: RUNNING_LABEL,
            since: None,
            pid: Some(4321),
        });
        assert_eq!(line, "Service: running (launchd user, pid 4321)");
    }

    #[test]
    fn renders_stopped_state_without_since() {
        // ActiveEnterTimestamp may carry over from an earlier activation
        // on a now-stopped unit; the renderer must not dangle "since X"
        // off the stopped line.
        let line = render_oneline(&ServiceState::Installed {
            backend: "systemd user",
            state_label: "stopped",
            since: Some(fixed_since()),
            pid: None,
        });
        assert_eq!(line, "Service: stopped (systemd user)");
    }
}
