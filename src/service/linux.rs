//! Linux backend for `kei service status`.
//!
//! Queries the per-user or system unit file written by an external
//! supervisor and reports its lifecycle state. kei no longer writes or
//! removes unit files; existing units that run
//! `kei service run --config ...` keep working.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use tokio::process::Command;

use crate::service::status::ServiceState;

const UNIT_FILE_NAME: &str = "kei.service";

const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";

/// Where the per-user unit lives. Honors `XDG_CONFIG_HOME`; falls back
/// to `~/.config/systemd/user/kei.service`. Returns `None` when neither
/// `XDG_CONFIG_HOME` nor `HOME` is set, which is the right answer
/// because there's no reasonable place to read the file in that case.
fn user_unit_path() -> Option<PathBuf> {
    let dir = dirs::config_dir()?;
    Some(dir.join("systemd/user").join(UNIT_FILE_NAME))
}

fn system_unit_path() -> PathBuf {
    Path::new(SYSTEM_UNIT_DIR).join(UNIT_FILE_NAME)
}

/// Implementation for `kei service status` on Linux.
///
/// Calls `systemctl --user show kei.service` and parses the resulting
/// key=value pairs. We do not read the full status output (`systemctl
/// status`) because it embeds escape codes and free-form journal lines
/// that complicate parsing; `show` returns a clean machine-readable
/// projection.
pub(crate) async fn status() -> Result<()> {
    let line = render_status(probe_status_inputs().await?);
    println!("{line}");
    Ok(())
}

enum StatusInputs {
    NotInstalled,
    BusUnavailable {
        scope: &'static str,
    },
    Probed {
        scope: &'static str,
        probe: std::collections::BTreeMap<String, String>,
    },
}

async fn probe_status_inputs() -> Result<StatusInputs> {
    let user_unit = user_unit_path().is_some_and(|p| p.exists());
    let system_unit = system_unit_path().exists();

    if !user_unit && !system_unit {
        return Ok(StatusInputs::NotInstalled);
    }

    let scope = if user_unit { "user" } else { "system" };
    let scope_args: &[&str] = if user_unit { &["--user"] } else { &[] };
    Ok(match show_unit(scope_args).await? {
        ProbeOutcome::BusUnavailable => StatusInputs::BusUnavailable { scope },
        ProbeOutcome::Properties(probe) => StatusInputs::Probed { scope, probe },
    })
}

fn render_status(inputs: StatusInputs) -> String {
    match inputs {
        StatusInputs::NotInstalled => "Service: not installed".to_string(),
        StatusInputs::BusUnavailable { scope } => {
            // Unit file exists but we can't talk to systemd (no active
            // session bus, e.g. SSH without `loginctl enable-linger`).
            // Saying "not installed" would be wrong; surface the cause.
            format!("Service: installed (systemd {scope}, bus unavailable)")
        }
        StatusInputs::Probed { scope, probe } => {
            let active = probe.get("ActiveState").map(String::as_str).unwrap_or("?");
            let sub = probe.get("SubState").map(String::as_str).unwrap_or("?");
            let since = probe
                .get("ActiveEnterTimestamp")
                .filter(|s| !s.is_empty())
                .map(String::as_str);

            if active == "active" {
                let when = since.map(|s| format!(" since {s}")).unwrap_or_default();
                format!("Service: running (systemd {scope}, {sub}{when})")
            } else {
                format!("Service: {active} (systemd {scope}, {sub})")
            }
        }
    }
}

/// `service_state()` for the `Service:` section in `kei status`. Reuses
/// [`probe_status_inputs`] and converts the raw systemctl key/value map
/// into the platform-agnostic [`ServiceState`].
pub(crate) async fn service_state() -> Result<ServiceState> {
    Ok(match probe_status_inputs().await? {
        StatusInputs::NotInstalled => ServiceState::NotInstalled,
        StatusInputs::BusUnavailable { scope } => ServiceState::BackendUnavailable {
            backend: backend_label(scope),
            reason: "bus unavailable",
        },
        StatusInputs::Probed { scope, probe } => probe_to_state(scope, &probe),
    })
}

// probe_status_inputs only ever passes "user" or "system"; the unreachable!
// arm is a refactor tripwire, not a runtime concern, hence the allow.
#[allow(
    clippy::unreachable,
    reason = "scope strings are produced by probe_status_inputs; \
              third value would be a refactor bug worth panicking on"
)]
fn backend_label(scope: &'static str) -> &'static str {
    match scope {
        "user" => "systemd user",
        "system" => "systemd system",
        other => unreachable!("unexpected systemd scope: {other:?}"),
    }
}

fn probe_to_state(
    scope: &'static str,
    probe: &std::collections::BTreeMap<String, String>,
) -> ServiceState {
    let active = probe.get("ActiveState").map(String::as_str).unwrap_or("?");
    let state_label: &'static str = match active {
        "active" => crate::service::status::RUNNING_LABEL,
        "inactive" => "stopped",
        "failed" => "failed",
        "activating" => "starting",
        "deactivating" => "stopping",
        "reloading" => "reloading",
        _ => "unknown",
    };
    let since = probe
        .get("ActiveEnterTimestamp")
        .and_then(|s| parse_systemd_timestamp(s));
    let pid = probe
        .get("MainPID")
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&p| p != 0);
    ServiceState::Installed {
        backend: backend_label(scope),
        state_label,
        since,
        pid,
    }
}

/// Parses systemd's `ActiveEnterTimestamp` into a UTC `DateTime`.
///
/// systemd formats the timestamp as `<weekday-abbrev> YYYY-MM-DD
/// HH:MM:SS <TZ>` where the timezone defaults to the host's local zone
/// unless `LC_ALL=C` or similar is in effect. Service units run with
/// the host's tzdata, so the value is usually `UTC` on servers and the
/// local zone on workstations. Only the UTC form is parsed here -- other
/// zones return `None` so the renderer omits the `since` clause rather
/// than guessing wrong. The returned timestamp is informational; missing
/// it is preferable to misleading the operator.
fn parse_systemd_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // `Thu 2026-05-07 14:32:01 UTC` -- weekday is decorative; chrono
    // matches `%a` against any 3-letter abbrev.
    let parsed = chrono::NaiveDateTime::parse_from_str(trimmed, "%a %Y-%m-%d %H:%M:%S UTC").ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(parsed, Utc))
}

enum ProbeOutcome {
    Properties(std::collections::BTreeMap<String, String>),
    BusUnavailable,
}

async fn show_unit(scope: &[&str]) -> Result<ProbeOutcome> {
    let mut argv: Vec<&str> = scope.to_vec();
    argv.extend([
        "show",
        UNIT_FILE_NAME,
        "--property=ActiveState,SubState,ActiveEnterTimestamp,MainPID",
    ]);
    let output = Command::new("systemctl")
        .args(&argv)
        .output()
        .await
        .context("Could not run `systemctl show`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if is_session_bus_unavailable(&stderr) {
            return Ok(ProbeOutcome::BusUnavailable);
        }
        bail!("`systemctl show` failed: {}", stderr.trim());
    }
    Ok(ProbeOutcome::Properties(parse_show_output(
        &String::from_utf8_lossy(&output.stdout),
    )))
}

fn is_session_bus_unavailable(stderr: &str) -> bool {
    // systemd / dbus emit one of these when there's no active user
    // session to talk to. We treat that as "can't probe state" rather
    // than "service is broken" so `kei service status` over SSH
    // reports something useful instead of a hard error.
    stderr.contains("Failed to connect to bus")
        || stderr.contains("Failed to connect to user scope bus")
        || stderr.contains("No medium found")
}

fn parse_show_output(stdout: &str) -> std::collections::BTreeMap<String, String> {
    stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_show_output_extracts_known_keys() {
        let raw = "ActiveState=active\nSubState=running\nActiveEnterTimestamp=Thu 2026-05-07 14:32:01 UTC\n";
        let parsed = parse_show_output(raw);
        assert_eq!(
            parsed.get("ActiveState").map(String::as_str),
            Some("active")
        );
        assert_eq!(parsed.get("SubState").map(String::as_str), Some("running"));
        assert_eq!(
            parsed.get("ActiveEnterTimestamp").map(String::as_str),
            Some("Thu 2026-05-07 14:32:01 UTC"),
        );
    }

    #[test]
    fn parse_show_output_handles_blank_timestamp() {
        let raw = "ActiveState=inactive\nSubState=dead\nActiveEnterTimestamp=\n";
        let parsed = parse_show_output(raw);
        assert_eq!(
            parsed.get("ActiveEnterTimestamp").map(String::as_str),
            Some(""),
        );
    }

    #[test]
    fn user_unit_path_is_under_config_dir() {
        // Result depends on $HOME / $XDG_CONFIG_HOME, but if either is
        // set the path must end with systemd/user/kei.service.
        if let Some(p) = user_unit_path() {
            assert!(
                p.ends_with("systemd/user/kei.service"),
                "expected systemd user path, got {}",
                p.display()
            );
            assert!(p.is_absolute());
        }
    }

    #[test]
    fn system_unit_path_is_etc_systemd_system() {
        let p = system_unit_path();
        assert_eq!(p, Path::new("/etc/systemd/system/kei.service"));
    }

    #[test]
    fn render_status_reports_not_installed() {
        assert_eq!(
            render_status(StatusInputs::NotInstalled),
            "Service: not installed"
        );
    }

    #[test]
    fn render_status_reports_bus_unavailable_when_no_session() {
        let line = render_status(StatusInputs::BusUnavailable { scope: "user" });
        assert_eq!(line, "Service: installed (systemd user, bus unavailable)");
    }

    #[test]
    fn render_status_includes_active_enter_timestamp() {
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "active".to_string());
        probe.insert("SubState".to_string(), "running".to_string());
        probe.insert(
            "ActiveEnterTimestamp".to_string(),
            "Thu 2026-05-07 14:32:01 UTC".to_string(),
        );
        let line = render_status(StatusInputs::Probed {
            scope: "user",
            probe,
        });
        assert_eq!(
            line,
            "Service: running (systemd user, running since Thu 2026-05-07 14:32:01 UTC)"
        );
    }

    #[test]
    fn render_status_handles_inactive_with_blank_timestamp() {
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "inactive".to_string());
        probe.insert("SubState".to_string(), "dead".to_string());
        probe.insert("ActiveEnterTimestamp".to_string(), String::new());
        let line = render_status(StatusInputs::Probed {
            scope: "user",
            probe,
        });
        assert_eq!(line, "Service: inactive (systemd user, dead)");
    }

    #[test]
    fn parse_systemd_timestamp_accepts_utc_form() {
        let parsed = parse_systemd_timestamp("Thu 2026-05-07 14:32:01 UTC")
            .expect("UTC-form timestamp must parse");
        assert_eq!(
            parsed,
            chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 5, 7, 14, 32, 1).unwrap(),
        );
    }

    #[test]
    fn parse_systemd_timestamp_rejects_local_tz_and_blank() {
        // Anything other than "UTC" returns None so we don't render a
        // misleading "since X UTC" against a local-zone wall clock.
        assert_eq!(parse_systemd_timestamp(""), None);
        assert_eq!(parse_systemd_timestamp("Thu 2026-05-07 14:32:01 EDT"), None);
        assert_eq!(parse_systemd_timestamp("not a timestamp"), None);
    }

    #[test]
    fn probe_to_state_running_with_main_pid() {
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "active".to_string());
        probe.insert("SubState".to_string(), "running".to_string());
        probe.insert(
            "ActiveEnterTimestamp".to_string(),
            "Thu 2026-05-07 14:32:01 UTC".to_string(),
        );
        probe.insert("MainPID".to_string(), "12345".to_string());
        match probe_to_state("user", &probe) {
            ServiceState::Installed {
                backend,
                state_label,
                since,
                pid,
            } => {
                assert_eq!(backend, "systemd user");
                assert_eq!(state_label, "running");
                assert!(since.is_some());
                assert_eq!(pid, Some(12345));
            }
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn probe_to_state_inactive_drops_zero_main_pid() {
        // systemd reports `MainPID=0` for inactive units; do not
        // surface that as a real PID in the rendered status line.
        let mut probe = std::collections::BTreeMap::new();
        probe.insert("ActiveState".to_string(), "inactive".to_string());
        probe.insert("SubState".to_string(), "dead".to_string());
        probe.insert("ActiveEnterTimestamp".to_string(), String::new());
        probe.insert("MainPID".to_string(), "0".to_string());
        match probe_to_state("user", &probe) {
            ServiceState::Installed {
                state_label,
                pid,
                since,
                ..
            } => {
                assert_eq!(state_label, "stopped");
                assert_eq!(pid, None);
                assert_eq!(since, None);
            }
            other => panic!("expected Installed, got {other:?}"),
        }
    }

    #[test]
    fn backend_label_distinguishes_user_and_system_scopes() {
        assert_eq!(backend_label("user"), "systemd user");
        assert_eq!(backend_label("system"), "systemd system");
    }

    #[test]
    #[should_panic(expected = "unexpected systemd scope")]
    fn backend_label_panics_on_unknown_scope() {
        // probe_status_inputs only emits "user" or "system" today; if a
        // future refactor adds a third scope without updating
        // backend_label, the panic surfaces it loud.
        let _ = backend_label("other");
    }

    #[test]
    fn detects_session_bus_unavailable_strings() {
        assert!(is_session_bus_unavailable(
            "Failed to connect to bus: No such file or directory\n"
        ));
        assert!(is_session_bus_unavailable(
            "Failed to connect to user scope bus via local transport\n"
        ));
        assert!(!is_session_bus_unavailable(
            "Unit kei.service could not be found.\n"
        ));
    }
}
