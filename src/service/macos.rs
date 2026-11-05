//! macOS backend for `kei service status`.
//!
//! Reads the per-user LaunchAgent registration written by an external
//! supervisor and queries launchd for lifecycle state. kei no longer writes
//! or removes plist files; existing LaunchAgents that run
//! `kei service run --config ...` keep working.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::service::env::{SERVICE_IDENTIFIER, effective_uid};
use crate::service::status::ServiceState;

const PLIST_FILE_NAME: &str = "com.rhoopr.kei.plist";

const LAUNCH_AGENTS_SUBDIR: &str = "Library/LaunchAgents";

/// State the `kei service status` line is rendered from.
const LAUNCHD_STATE_RUNNING: &str = "running";

/// `launchctl print` reports `pid = -` when a service is loaded but
/// hasn't (yet) spawned a process. Treat that as "no PID to surface".
const LAUNCHD_PID_NONE: &str = "-";

fn user_plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(LAUNCH_AGENTS_SUBDIR).join(PLIST_FILE_NAME))
}

/// `print` is the modern replacement for `launchctl list` and returns
/// enough structure to recover both running-state and the spawned PID
/// across recent macOS versions.
pub(crate) async fn status() -> Result<()> {
    let line = render_status(probe_status_inputs().await?);
    println!("{line}");
    Ok(())
}

#[derive(Debug)]
enum StatusInputs {
    NotInstalled,
    DomainUnavailable,
    Probed { state: String, pid: Option<String> },
}

async fn probe_status_inputs() -> Result<StatusInputs> {
    if !user_plist_path().is_some_and(|p| p.exists()) {
        return Ok(StatusInputs::NotInstalled);
    }
    launchctl_print().await
}

fn render_status(inputs: StatusInputs) -> String {
    match inputs {
        StatusInputs::NotInstalled => "Service: not installed".to_string(),
        // Plist exists but launchctl can't talk to the GUI domain
        // (typical of an SSH session into a headless mac without an
        // active console user). Same shape as the linux BusUnavailable
        // branch so consumers see a consistent "installed but
        // unprobeable" signal across platforms.
        StatusInputs::DomainUnavailable => {
            "Service: installed (launchd user, domain unavailable)".to_string()
        }
        StatusInputs::Probed { state, pid } => {
            let pid_suffix = pid
                .as_deref()
                .filter(|p| !p.is_empty() && *p != LAUNCHD_PID_NONE)
                .map(|p| format!(", pid {p}"))
                .unwrap_or_default();
            if state == LAUNCHD_STATE_RUNNING {
                format!("Service: running (launchd user{pid_suffix})")
            } else {
                format!("Service: {state} (launchd user{pid_suffix})")
            }
        }
    }
}

/// `service_state()` for the `Service:` section in `kei status`.
/// `launchctl print` exposes state and pid but no start time, so
/// `since` is always `None` on macOS.
pub(crate) async fn service_state() -> Result<ServiceState> {
    Ok(match probe_status_inputs().await? {
        StatusInputs::NotInstalled => ServiceState::NotInstalled,
        StatusInputs::DomainUnavailable => ServiceState::BackendUnavailable {
            backend: "launchd user",
            reason: "domain unavailable",
        },
        StatusInputs::Probed { state, pid } => probed_to_state(&state, pid.as_deref()),
    })
}

fn probed_to_state(state: &str, pid: Option<&str>) -> ServiceState {
    let state_label: &'static str = if state == LAUNCHD_STATE_RUNNING {
        crate::service::status::RUNNING_LABEL
    } else {
        "stopped"
    };
    let pid = pid
        .filter(|p| !p.is_empty() && *p != LAUNCHD_PID_NONE)
        .and_then(|p| p.parse::<u32>().ok());
    ServiceState::Installed {
        backend: "launchd user",
        state_label,
        since: None,
        pid,
    }
}

fn gui_domain() -> String {
    gui_domain_for(effective_uid())
}

fn gui_domain_for(uid: u32) -> String {
    format!("gui/{uid}")
}

fn gui_service_target() -> String {
    format!("{}/{SERVICE_IDENTIFIER}", gui_domain())
}

fn is_domain_unavailable(stderr: &str) -> bool {
    // launchctl emits a small set of fingerprints when the GUI domain
    // can't be reached (typical of CI runners or SSH sessions into a
    // mac without an active GUI login). The error matrix is documented
    // (loosely) in `launchctl(1)`; these are the strings observed in
    // the wild.
    stderr.contains("Could not find domain")
        || stderr.contains("Bootstrap failed: 5: Input/output error")
        || stderr.contains("Could not bootstrap")
        || stderr.contains("Operation not permitted")
}

fn is_not_loaded(stderr: &str) -> bool {
    stderr.contains("No such process") || stderr.contains("Could not find specified service")
}

async fn launchctl_print() -> Result<StatusInputs> {
    let target = gui_service_target();
    let output = Command::new("launchctl")
        .args(["print", &target])
        .output()
        .await
        .context("Could not run `launchctl print`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_domain_unavailable(&stderr) || is_not_loaded(&stderr) {
            return Ok(StatusInputs::DomainUnavailable);
        }
        bail!("`launchctl print {target}` failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed = parse_launchctl_print(&stdout);
    Ok(StatusInputs::Probed {
        state: parsed.state.unwrap_or_else(|| "unknown".to_string()),
        pid: parsed.pid,
    })
}

#[derive(Debug, Default)]
struct LaunchctlPrint {
    state: Option<String>,
    pid: Option<String>,
}

/// Whitespace and `=` alignment vary between macOS releases, so the
/// parser trims every line and only matches `key = value` after that.
fn parse_launchctl_print(stdout: &str) -> LaunchctlPrint {
    let mut out = LaunchctlPrint::default();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(value) = strip_kv(trimmed, "state") {
            out.state = Some(value);
        } else if let Some(value) = strip_kv(trimmed, "pid") {
            out.pid = Some(value);
        }
    }
    out
}

fn strip_kv(line: &str, key: &str) -> Option<String> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    Some(rest.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_status_reports_not_installed() {
        assert_eq!(
            render_status(StatusInputs::NotInstalled),
            "Service: not installed",
        );
    }

    #[test]
    fn render_status_reports_domain_unavailable() {
        assert_eq!(
            render_status(StatusInputs::DomainUnavailable),
            "Service: installed (launchd user, domain unavailable)",
        );
    }

    #[test]
    fn render_status_running_includes_pid_when_present() {
        assert_eq!(
            render_status(StatusInputs::Probed {
                state: "running".to_string(),
                pid: Some("12345".to_string()),
            }),
            "Service: running (launchd user, pid 12345)",
        );
    }

    #[test]
    fn render_status_running_omits_dash_pid() {
        // launchctl print emits `pid = -` for loaded-but-stopped
        // services in some macOS versions; that's not a real PID and
        // shouldn't pollute the status line.
        assert_eq!(
            render_status(StatusInputs::Probed {
                state: "running".to_string(),
                pid: Some(LAUNCHD_PID_NONE.to_string()),
            }),
            "Service: running (launchd user)",
        );
    }

    #[test]
    fn render_status_passes_through_non_running_state() {
        assert_eq!(
            render_status(StatusInputs::Probed {
                state: "not running".to_string(),
                pid: None,
            }),
            "Service: not running (launchd user)",
        );
    }

    #[test]
    fn parse_launchctl_print_extracts_state_and_pid() {
        let raw = "\
com.rhoopr.kei = {
    type = LaunchAgent
    handle = 12345
    state = running
    pid = 12345
    program = /usr/local/bin/kei
    arguments = {
        /usr/local/bin/kei
        service
        run
        --config
        /Users/alice/.config/kei/config.toml
    }
}
";
        let parsed = parse_launchctl_print(raw);
        assert_eq!(parsed.state.as_deref(), Some("running"));
        assert_eq!(parsed.pid.as_deref(), Some("12345"));
    }

    #[test]
    fn parse_launchctl_print_handles_loaded_but_stopped() {
        let raw = "\
com.rhoopr.kei = {
    state = not running
    pid = -
}
";
        let parsed = parse_launchctl_print(raw);
        assert_eq!(parsed.state.as_deref(), Some("not running"));
        assert_eq!(parsed.pid.as_deref(), Some("-"));
    }

    #[test]
    fn detects_domain_unavailable_strings() {
        assert!(is_domain_unavailable("Could not find domain for: gui/501"));
        assert!(is_domain_unavailable(
            "Bootstrap failed: 5: Input/output error"
        ));
        assert!(is_domain_unavailable(
            "Operation not permitted while System Integrity Protection is engaged"
        ));
        assert!(!is_domain_unavailable("service already loaded\n"));
    }

    #[test]
    fn detects_not_loaded_strings() {
        assert!(is_not_loaded("No such process"));
        assert!(is_not_loaded("Could not find specified service"));
        assert!(!is_not_loaded("Bootstrap failed"));
    }

    #[test]
    fn probed_to_state_running_with_pid() {
        match probed_to_state(LAUNCHD_STATE_RUNNING, Some("4321")) {
            ServiceState::Installed {
                backend,
                state_label,
                since,
                pid,
            } => {
                assert_eq!(backend, "launchd user");
                assert_eq!(state_label, "running");
                assert_eq!(since, None);
                assert_eq!(pid, Some(4321));
            }
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn probed_to_state_drops_dash_pid() {
        match probed_to_state(LAUNCHD_STATE_RUNNING, Some(LAUNCHD_PID_NONE)) {
            ServiceState::Installed { pid, .. } => assert_eq!(pid, None),
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn probed_to_state_non_running_label() {
        match probed_to_state("not running", None) {
            ServiceState::Installed { state_label, .. } => {
                assert_eq!(state_label, "stopped");
            }
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn user_plist_path_ends_at_launch_agents() {
        if let Some(p) = user_plist_path() {
            assert!(
                p.ends_with("Library/LaunchAgents/com.rhoopr.kei.plist"),
                "expected LaunchAgents path, got {}",
                p.display(),
            );
            assert!(p.is_absolute());
        }
    }

    #[test]
    fn gui_domain_for_renders_uid() {
        assert_eq!(gui_domain_for(0), "gui/0");
        assert_eq!(gui_domain_for(501), "gui/501");
        assert_eq!(gui_domain_for(u32::MAX), format!("gui/{}", u32::MAX));
    }
}
