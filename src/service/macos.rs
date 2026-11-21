//! macOS backend for `kei install` / `kei uninstall` / `kei service status`.
//!
//! Per-user LaunchAgent only. kei does not ship a LaunchDaemon
//! (`/Library/LaunchDaemons/`) path because that would require root and
//! brings a different threat model than the keychain-protected per-user
//! flow. `--system` therefore errors with a pointer at `--user`.
//!
//! `kei install` writes
//! `~/Library/LaunchAgents/com.rhoopr.kei.plist`, creates the matching
//! log directory at `~/Library/Logs/kei/`, and runs
//! `launchctl bootstrap gui/$(id -u) <plist>`. `bootstrap` is the modern
//! API; on hosted CI runners and other headless macOS environments where
//! the GUI domain is unavailable we fall back to the legacy
//! `launchctl load -w <plist>` path so the install still succeeds.
//!
//! Uninstall mirrors the same fallback: `launchctl bootout` first,
//! `launchctl unload` if the GUI domain refuses. The plist file is
//! removed last; with `--purge`, `~/.config/kei/` and the credential
//! entry go too (matching the linux backend).
//!
//! Plist rendering is pulled out as a pure function so tests can assert
//! key shape without spawning launchctl. CI covers dry-run rendering and
//! plist syntax; real `launchctl bootstrap` / `bootout` handoff needs a
//! live launchd domain.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use plist::{Dictionary, Value as PlistValue};
use tokio::process::Command;

use crate::service::env::{
    SERVICE_DESCRIPTION, SERVICE_IDENTIFIER, current_executable, effective_uid, purge_kei_state,
};
use crate::service::plan::{self, InstallPlan};
use crate::service::status::ServiceState;

const PLIST_FILE_NAME: &str = "com.rhoopr.kei.plist";

const LAUNCH_AGENTS_SUBDIR: &str = "Library/LaunchAgents";
const LOG_SUBDIR: &str = "Library/Logs/kei";

/// State the `kei service status` line is rendered from.
const LAUNCHD_STATE_RUNNING: &str = "running";

/// `launchctl print` reports `pid = -` when a service is loaded but
/// hasn't (yet) spawned a process. Treat that as "no PID to surface".
const LAUNCHD_PID_NONE: &str = "-";

struct PlistInputs<'a> {
    exec: &'a Path,
    config: &'a Path,
    log_dir: &'a Path,
    home: &'a Path,
}

/// `KeepAlive` uses the `NetworkState` predicate so launchd brings the
/// daemon back when network connectivity returns (post sleep/wake, VPN
/// toggle, Wi-Fi handoff). `RunAtLoad=true` covers the boot path.
fn render_user_plist(inputs: PlistInputs<'_>) -> Dictionary {
    let mut dict = Dictionary::new();
    dict.insert(
        "Label".to_string(),
        PlistValue::String(SERVICE_IDENTIFIER.to_string()),
    );

    let program_args = vec![
        PlistValue::String(inputs.exec.display().to_string()),
        PlistValue::String("service".to_string()),
        PlistValue::String("run".to_string()),
        PlistValue::String("--config".to_string()),
        PlistValue::String(inputs.config.display().to_string()),
    ];
    dict.insert(
        "ProgramArguments".to_string(),
        PlistValue::Array(program_args),
    );

    dict.insert("RunAtLoad".to_string(), PlistValue::Boolean(true));

    let mut keep_alive = Dictionary::new();
    keep_alive.insert("NetworkState".to_string(), PlistValue::Boolean(true));
    dict.insert("KeepAlive".to_string(), PlistValue::Dictionary(keep_alive));

    dict.insert(
        "StandardOutPath".to_string(),
        PlistValue::String(inputs.log_dir.join("stdout.log").display().to_string()),
    );
    dict.insert(
        "StandardErrorPath".to_string(),
        PlistValue::String(inputs.log_dir.join("stderr.log").display().to_string()),
    );
    dict.insert(
        "WorkingDirectory".to_string(),
        PlistValue::String(inputs.home.display().to_string()),
    );

    // Description isn't part of launchd's documented schema, but several
    // GUI tools (LaunchControl, Lingon) surface it. Cheap to include and
    // keeps the human-readable label aligned across platforms.
    dict.insert(
        "ServiceDescription".to_string(),
        PlistValue::String(SERVICE_DESCRIPTION.to_string()),
    );

    // `ProcessType=Background` tells launchd this is a long-running
    // daemon rather than a UI app, so it is exempt from App Nap and
    // similar power-management throttling.
    dict.insert(
        "ProcessType".to_string(),
        PlistValue::String("Background".to_string()),
    );

    dict
}

fn user_plist_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(LAUNCH_AGENTS_SUBDIR).join(PLIST_FILE_NAME))
}

fn user_log_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(LOG_SUBDIR))
}

pub(crate) async fn install_user(plan: InstallPlan, config_path: &Path) -> Result<()> {
    let exe = current_executable()?;
    let home = dirs::home_dir().ok_or_else(|| {
        anyhow!("Could not find $HOME, which is needed for the LaunchAgents plist path.")
    })?;
    let plist_path = home.join(LAUNCH_AGENTS_SUBDIR).join(PLIST_FILE_NAME);
    let log_dir = home.join(LOG_SUBDIR);

    let dict = render_user_plist(PlistInputs {
        exec: &exe,
        config: config_path,
        log_dir: &log_dir,
        home: &home,
    });
    let xml = serialize_plist(&dict)?;
    if plan.is_preview() {
        print!("{xml}");
        tracing::info!("dry run: rendered per-user launchd plist; no files written");
        return Ok(());
    }

    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Could not create log directory {}", log_dir.display()))?;

    write_plist(&plist_path, &xml)?;
    tracing::info!(
        service = SERVICE_IDENTIFIER,
        path = %plist_path.display(),
        executable = %exe.display(),
        config = %config_path.display(),
        log_dir = %log_dir.display(),
        "wrote per-user launchd plist",
    );

    bootstrap_or_load(&plist_path).await?;

    tracing::info!(
        "kei is now running as a per-user launchd agent; \
         check `launchctl list {SERVICE_IDENTIFIER}` to verify. \
         Run `kei uninstall` to remove this service."
    );
    Ok(())
}

/// `--system` rejection. macOS LaunchDaemons require root and a different
/// security review (system-context FDA, keychain access constraints) that
/// kei does not currently support. Errors with a pointer at the supported
/// flag rather than silently downgrading.
pub(crate) fn install_system(_plan: InstallPlan, _config_path: &Path) -> Result<()> {
    plan::reject_macos_system_install()
}

pub(crate) async fn uninstall(root: Option<&Path>) -> Result<()> {
    let plist_path = user_plist_path().filter(|p| p.exists());

    if plist_path.is_none() {
        tracing::info!("kei service was already removed. Nothing to do.");
    }

    if let Some(path) = plist_path.as_ref() {
        // bootout / unload may legitimately fail in environments where
        // the GUI domain is unavailable (CI, SSH session into headless
        // mac, plist-not-loaded-but-present). The plist removal is the
        // load-bearing step; log+proceed.
        let _ = bootout_or_unload(path).await;
        remove_plist_file(path)?;
        tracing::info!(path = %path.display(), "removed per-user launchd plist");
    }

    if let Some(kei_dir) = root {
        let extras: Vec<PathBuf> = user_log_dir().into_iter().collect();
        purge_kei_state(kei_dir, &extras)?;
    }

    Ok(())
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

// ── Internals ───────────────────────────────────────────────────────────

fn write_plist(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Could not create LaunchAgents directory {}",
                parent.display()
            )
        })?;
    }
    std::fs::write(path, contents)
        .with_context(|| format!("Could not write launchd plist {}", path.display()))
}

fn remove_plist_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("Could not remove launchd plist {}", path.display()))
        }
    }
}

fn serialize_plist(dict: &Dictionary) -> Result<String> {
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, dict).context("Could not build launchd plist XML")?;
    String::from_utf8(buf).context("launchd plist XML was not valid UTF-8")
}

/// Tries `launchctl bootstrap gui/<uid>` first; falls back to the legacy
/// `launchctl load -w` path on hosts where the GUI domain is unavailable
/// (headless CI runners, screen-locked hosts without an active session).
async fn bootstrap_or_load(plist_path: &Path) -> Result<()> {
    let domain = gui_domain();
    let plist_arg = path_to_str(plist_path, "plist")?;
    match run_launchctl(&["bootstrap", &domain, plist_arg]).await {
        Ok(()) => Ok(()),
        Err(e) if is_domain_unavailable(&e.to_string()) => {
            tracing::warn!(
                error = %e,
                "launchctl bootstrap failed (no GUI domain); falling back to legacy `load -w`"
            );
            run_launchctl(&["load", "-w", plist_arg]).await
        }
        Err(e) => Err(e),
    }
}

async fn bootout_or_unload(plist_path: &Path) -> Result<()> {
    let target = gui_service_target();
    let plist_arg = path_to_str(plist_path, "plist")?;
    match run_launchctl(&["bootout", &target]).await {
        Ok(()) => Ok(()),
        Err(e) if is_domain_unavailable(&e.to_string()) || is_not_loaded(&e.to_string()) => {
            // Either no GUI domain, or already booted out. Try the
            // legacy unload as a belt-and-braces clean up.
            tracing::debug!(
                error = %e,
                "launchctl bootout fell through; running legacy `unload`"
            );
            run_launchctl(&["unload", plist_arg]).await
        }
        Err(e) => Err(e),
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

/// macOS paths are UTF-8 in practice (HFS+ / APFS enforce normalization
/// over UTF-8 code units). Bail loudly rather than silently corrupting
/// the launchctl invocation if a non-UTF-8 path slips through.
fn path_to_str<'a>(p: &'a Path, label: &str) -> Result<&'a str> {
    p.to_str()
        .ok_or_else(|| anyhow!("{label} path is not valid UTF-8: {}", p.display()))
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

async fn run_launchctl(args: &[&str]) -> Result<()> {
    let output = Command::new("launchctl")
        .args(args)
        .output()
        .await
        .context("Could not run `launchctl`. Is this macOS?")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        bail!("`launchctl {}` failed: {detail}", args.join(" "));
    }
    Ok(())
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
    use std::path::PathBuf;

    fn sample_inputs() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        (
            PathBuf::from("/usr/local/bin/kei"),
            PathBuf::from("/Users/alice/.config/kei/config.toml"),
            PathBuf::from("/Users/alice/Library/Logs/kei"),
            PathBuf::from("/Users/alice"),
        )
    }

    fn render_with(exec: &Path, config: &Path, log_dir: &Path, home: &Path) -> Dictionary {
        render_user_plist(PlistInputs {
            exec,
            config,
            log_dir,
            home,
        })
    }

    #[test]
    fn user_plist_contains_required_keys() {
        let (exec, config, log_dir, home) = sample_inputs();
        let dict = render_with(&exec, &config, &log_dir, &home);
        assert_eq!(
            dict.get("Label").and_then(|v| v.as_string()),
            Some(SERVICE_IDENTIFIER),
        );
        assert_eq!(
            dict.get("RunAtLoad").and_then(|v| v.as_boolean()),
            Some(true),
        );
        assert_eq!(
            dict.get("WorkingDirectory").and_then(|v| v.as_string()),
            Some("/Users/alice"),
        );
        assert_eq!(
            dict.get("StandardOutPath").and_then(|v| v.as_string()),
            Some("/Users/alice/Library/Logs/kei/stdout.log"),
        );
        assert_eq!(
            dict.get("StandardErrorPath").and_then(|v| v.as_string()),
            Some("/Users/alice/Library/Logs/kei/stderr.log"),
        );
        assert_eq!(
            dict.get("ProcessType").and_then(|v| v.as_string()),
            Some("Background"),
        );
    }

    #[test]
    fn program_arguments_are_absolute_and_carry_config_flag() {
        let dict = render_with(
            &PathBuf::from("/opt/homebrew/bin/kei"),
            &PathBuf::from("/Users/bob/.config/kei/config.toml"),
            &PathBuf::from("/Users/bob/Library/Logs/kei"),
            &PathBuf::from("/Users/bob"),
        );
        let args = dict
            .get("ProgramArguments")
            .and_then(|v| v.as_array())
            .expect("ProgramArguments must be an array");
        let strings: Vec<&str> = args.iter().filter_map(|v| v.as_string()).collect();
        assert_eq!(
            strings,
            vec![
                "/opt/homebrew/bin/kei",
                "service",
                "run",
                "--config",
                "/Users/bob/.config/kei/config.toml",
            ],
        );
    }

    #[test]
    fn keep_alive_uses_network_state_predicate() {
        let dict = render_with(
            &PathBuf::from("/usr/local/bin/kei"),
            &PathBuf::from("/tmp/config.toml"),
            &PathBuf::from("/tmp/logs"),
            &PathBuf::from("/tmp"),
        );
        let keep = dict
            .get("KeepAlive")
            .and_then(|v| v.as_dictionary())
            .expect("KeepAlive must be a dict");
        assert_eq!(
            keep.get("NetworkState").and_then(|v| v.as_boolean()),
            Some(true),
        );
    }

    #[test]
    fn rendered_plist_round_trips_through_serializer() {
        // Round-trip via plist::to_writer_xml + plist::from_bytes is the
        // closest local check to what launchctl will see at install
        // time. If the dict has an invalid type or unsupported value
        // shape, this test fails before the user does.
        let (exec, config, log_dir, home) = sample_inputs();
        let dict = render_with(&exec, &config, &log_dir, &home);
        let xml = serialize_plist(&dict).expect("serialize");
        let reparsed: Dictionary = plist::from_bytes(xml.as_bytes())
            .expect("plist must round-trip through XML serializer");
        assert_eq!(
            reparsed.get("Label").and_then(|v| v.as_string()),
            Some(SERVICE_IDENTIFIER),
        );
        // Sanity: the XML payload itself should look like a plist.
        assert!(
            xml.contains("<plist") && xml.contains("<dict>"),
            "expected plist XML, got:\n{xml}",
        );
    }

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

    #[test]
    fn path_to_str_accepts_utf8() {
        assert_eq!(
            path_to_str(Path::new("/Users/alice/file.txt"), "test").unwrap(),
            "/Users/alice/file.txt",
        );
    }
}
