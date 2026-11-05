//! Windows backend for `kei service status` plus the SCM service-main
//! bridge for `kei service run`.
//!
//! `kei service status` queries an existing Service Control Manager (SCM)
//! registration. kei no longer creates or removes SCM entries; existing
//! services that run `kei service run --config ...` keep working.
//!
//! `kei service run` on Windows double-dispatches: when SCM launches
//! the binary, `StartServiceCtrlDispatcher` must be called within 30s
//! or SCM kills the process. [`run_under_scm_or_foreground`] tries the
//! dispatcher first; on `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` it
//! falls through to a foreground sync-loop run for `kei service run`
//! invoked from a terminal.

#![allow(
    clippy::print_stdout,
    reason = "kei service status renders human-readable output to stdout, matching kei status / kei verify."
)]

use anyhow::Result;

#[cfg(target_os = "windows")]
use crate::service::env::SERVICE_IDENTIFIER;
use crate::service::status::ServiceState;

/// SCM service name (matches `SERVICE_IDENTIFIER` so `sc.exe query
/// com.rhoopr.kei` works for ad-hoc inspection).
#[cfg(target_os = "windows")]
pub(crate) const SERVICE_NAME: &str = SERVICE_IDENTIFIER;

/// `kei service status` implementation.
pub(crate) async fn status() -> Result<()> {
    let line = render_status(scm_impl::probe().await?);
    println!("{line}");
    Ok(())
}

/// `service_state()` for the `Service:` section in `kei status`. SCM
/// does not expose a service-start timestamp via `QueryServiceStatusEx`,
/// so `since` is always `None`; the lifecycle label and PID are
/// sufficient signal for the status line.
pub(crate) async fn service_state() -> Result<ServiceState> {
    Ok(match scm_impl::probe().await? {
        StatusInputs::NotInstalled => ServiceState::NotInstalled,
        StatusInputs::ScmUnavailable => ServiceState::BackendUnavailable {
            backend: "windows scm",
            reason: "SCM unavailable",
        },
        StatusInputs::Probed { state, pid } => ServiceState::Installed {
            backend: "windows scm",
            state_label: state.label(),
            since: None,
            pid,
        },
    })
}

/// `kei service run` entry on Windows.
///
/// Tries SCM dispatcher first. When the binary is launched by SCM, the
/// dispatcher attaches and blocks here for the lifetime of the service;
/// when launched from a terminal, dispatcher returns
/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` immediately and we fall
/// through to a foreground `sync_loop::run_sync` so `kei service run`
/// stays useful for local testing.
#[cfg(target_os = "windows")]
pub(crate) async fn run_under_scm_or_foreground(
    globals: crate::config::GlobalArgs,
    args: crate::sync_loop::SyncArgs,
) -> Result<()> {
    scm_impl::run_or_foreground(globals, args).await
}

// ── Status rendering (testable on every target) ────────────────────────

/// Mirror of `windows_service::service::ServiceState` that compiles on
/// every target. The runtime arm in `scm_impl` maps the windows-service
/// enum into this view so the renderer and its tests stay reachable from
/// linux/macOS hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ServiceStateView {
    Running,
    Stopped,
    StartPending,
    StopPending,
    ContinuePending,
    PausePending,
    Paused,
}

impl ServiceStateView {
    fn label(self) -> &'static str {
        match self {
            ServiceStateView::Running => "running",
            ServiceStateView::Stopped => "stopped",
            ServiceStateView::StartPending => "start-pending",
            ServiceStateView::StopPending => "stop-pending",
            ServiceStateView::ContinuePending => "continue-pending",
            ServiceStateView::PausePending => "pause-pending",
            ServiceStateView::Paused => "paused",
        }
    }
}

/// Inputs the status renderer accepts. Decoupled from the
/// windows-service crate's `ServiceStatus` so the formatter stays
/// testable on linux/macOS hosts where that type does not compile.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StatusInputs {
    NotInstalled,
    ScmUnavailable,
    Probed {
        state: ServiceStateView,
        pid: Option<u32>,
    },
}

fn render_status(inputs: StatusInputs) -> String {
    match inputs {
        StatusInputs::NotInstalled => "Service: not installed".to_string(),
        // SCM is unavailable when called from a non-elevated shell or
        // (rare) when the Service Control Manager itself is down.
        // Surface the cause; "not installed" would lie.
        StatusInputs::ScmUnavailable => {
            "Service: SCM unavailable (run from an elevated PowerShell to query state)".to_string()
        }
        StatusInputs::Probed {
            state: ServiceStateView::Running,
            pid: Some(pid),
        } => format!("Service: running (windows scm, pid {pid})"),
        StatusInputs::Probed {
            state: ServiceStateView::Running,
            pid: None,
        } => "Service: running (windows scm)".to_string(),
        StatusInputs::Probed { state, .. } => format!("Service: {} (windows scm)", state.label()),
    }
}

// ── SCM glue ───────────────────────────────────────────────────────────
//
// The real impl is `#[cfg(target_os = "windows")]`. The stub on other
// targets exists so the renderer and its tests compile and run on
// linux/macOS; the status dispatcher in status.rs routes only Windows
// targets here.

#[cfg(target_os = "windows")]
mod scm_impl {
    use super::*;
    use anyhow::{Context, anyhow};
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;
    use windows_service::{
        define_windows_service,
        service::{
            ServiceAccess, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
            ServiceStatus, ServiceType,
        },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    use windows_sys::Win32::Foundation::{
        ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_DOES_NOT_EXIST,
    };

    /// Bridge between the async caller in `service::run::run` and the
    /// SCM service-main callback that runs on a thread spawned by the
    /// windows-service dispatcher. The OS thread that `kei_service_main`
    /// runs on cannot capture our async-context payload by closure (the
    /// callback signature is fixed by the FFI contract), so we stash it
    /// here, the callback takes it, and the foreground fall-through
    /// path takes it back when the dispatcher refuses.
    static SCM_PAYLOAD: Mutex<Option<ScmPayload>> = Mutex::new(None);

    /// SCM stop is delivered on a thread separate from the one running
    /// `service_main_inner`; the handler signals shutdown by sending
    /// on this oneshot which the sync-loop runtime observes via select!.
    static SCM_SHUTDOWN_TX: Mutex<Option<tokio::sync::oneshot::Sender<()>>> = Mutex::new(None);

    struct ScmPayload {
        globals: crate::config::GlobalArgs,
        sync: crate::sync_loop::SyncArgs,
    }

    fn lock_scm_payload() -> Result<MutexGuard<'static, Option<ScmPayload>>> {
        SCM_PAYLOAD
            .lock()
            .map_err(|_| anyhow!("internal: SCM payload mutex poisoned"))
    }

    fn lock_scm_shutdown_tx()
    -> Result<MutexGuard<'static, Option<tokio::sync::oneshot::Sender<()>>>> {
        SCM_SHUTDOWN_TX
            .lock()
            .map_err(|_| anyhow!("internal: SCM shutdown mutex poisoned"))
    }

    fn raw_os_error(e: &windows_service::Error) -> Option<u32> {
        match e {
            windows_service::Error::Winapi(io) => io.raw_os_error().map(|n| n as u32),
            _ => None,
        }
    }

    pub(super) async fn probe() -> Result<StatusInputs> {
        tokio::task::spawn_blocking(probe_blocking)
            .await
            .context("status probe task panicked")?
    }

    fn probe_blocking() -> Result<StatusInputs> {
        let manager = match open_manager(ServiceManagerAccess::CONNECT) {
            Ok(m) => m,
            Err(_) => return Ok(StatusInputs::ScmUnavailable),
        };
        let service = match manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) {
            Ok(s) => s,
            Err(ref e) if raw_os_error(e) == Some(ERROR_SERVICE_DOES_NOT_EXIST) => {
                return Ok(StatusInputs::NotInstalled);
            }
            Err(e) => return Err(anyhow!("Could not open the Windows service: {e}")),
        };
        let status = service
            .query_status()
            .context("Could not query the Windows service status")?;
        Ok(StatusInputs::Probed {
            state: service_state_view(status.current_state),
            pid: status.process_id,
        })
    }

    fn open_manager(access: ServiceManagerAccess) -> Result<ServiceManager> {
        ServiceManager::local_computer(None::<&str>, access)
            .context("Could not open the Windows Service Control Manager.")
    }

    fn service_state_view(state: ServiceState) -> ServiceStateView {
        match state {
            ServiceState::Running => ServiceStateView::Running,
            ServiceState::Stopped => ServiceStateView::Stopped,
            ServiceState::StartPending => ServiceStateView::StartPending,
            ServiceState::StopPending => ServiceStateView::StopPending,
            ServiceState::ContinuePending => ServiceStateView::ContinuePending,
            ServiceState::PausePending => ServiceStateView::PausePending,
            ServiceState::Paused => ServiceStateView::Paused,
        }
    }

    // -- Service main / SCM event handler --------------------------------

    define_windows_service!(ffi_service_main, kei_service_main);

    pub(super) async fn run_or_foreground(
        globals: crate::config::GlobalArgs,
        sync: crate::sync_loop::SyncArgs,
    ) -> Result<()> {
        // Stash payload before the dispatcher attempt so the SCM-spawned
        // service-main thread cannot observe an empty slot.
        *lock_scm_payload()? = Some(ScmPayload { globals, sync });

        let dispatcher_result = tokio::task::spawn_blocking(|| {
            service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        })
        .await
        .context("Windows service dispatcher task crashed")?;

        match dispatcher_result {
            Ok(()) => {
                tracing::info!(service = SERVICE_NAME, "SCM-managed service exited cleanly");
                Ok(())
            }
            Err(ref e) if raw_os_error(e) == Some(ERROR_FAILED_SERVICE_CONTROLLER_CONNECT) => {
                tracing::info!(
                    service = SERVICE_NAME,
                    "kei service run invoked outside SCM; running in foreground"
                );
                let payload = lock_scm_payload()?
                    .take()
                    .ok_or_else(|| anyhow!("internal: SCM payload missing on foreground path"))?;
                crate::sync_loop::run_sync(&payload.globals, payload.sync).await
            }
            Err(e) => Err(anyhow!(
                "Could not start the Windows service dispatcher: {e}"
            )),
        }
    }

    fn kei_service_main(_arguments: Vec<OsString>) {
        if let Err(e) = service_main_inner() {
            tracing::error!(error = %e, "kei SCM service main failed");
        }
    }

    fn service_main_inner() -> Result<()> {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        *lock_scm_shutdown_tx()? = Some(shutdown_tx);

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop | ServiceControl::Shutdown => {
                    match lock_scm_shutdown_tx() {
                        Ok(mut guard) => {
                            if let Some(tx) = guard.take() {
                                let _ = tx.send(());
                            }
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "failed to signal SCM shutdown");
                            return ServiceControlHandlerResult::Other(1);
                        }
                    }
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = service_control_handler::register(SERVICE_NAME, event_handler)
            .context("Could not register the Windows service control handler")?;

        status_handle
            .set_service_status(service_status(ServiceState::Running, 0))
            .context("Could not report the Windows service as running")?;

        let outcome = run_payload_under_scm(shutdown_rx);

        // Always report Stopped, even on error -- otherwise SCM keeps
        // the service in StartPending until wait_hint elapses and then
        // force-kills the process, which is a worse signal than a clean
        // stop with a non-zero exit code.
        let exit_code = if outcome.is_ok() { 0 } else { 1 };
        let report =
            status_handle.set_service_status(service_status(ServiceState::Stopped, exit_code));
        if let Err(e) = report {
            tracing::error!(error = %e, "failed to report Stopped to SCM");
        }
        outcome
    }

    fn run_payload_under_scm(mut shutdown_rx: tokio::sync::oneshot::Receiver<()>) -> Result<()> {
        let payload = lock_scm_payload()?.take().ok_or_else(|| {
            anyhow!("Internal error: kei service main started without service state.")
        })?;

        // Dedicated single-thread runtime for the sync loop. Lives on
        // the OS thread SCM spawned for service main; the foreground
        // caller's runtime, if any, is on a different thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("Could not start the Windows service runtime")?;

        runtime.block_on(async move {
            tokio::select! {
                result = crate::sync_loop::run_sync(&payload.globals, payload.sync) => result,
                _ = &mut shutdown_rx => {
                    tracing::info!(service = SERVICE_NAME, "SCM stop received; shutting down sync loop");
                    Ok(())
                }
            }
        })
    }

    /// Builds a `ServiceStatus` for the two states this backend reports.
    /// `Running` accepts STOP/SHUTDOWN; `Stopped` accepts nothing and
    /// carries the exit code. Other states (StartPending, etc.) would
    /// need additional shape (`checkpoint`, `wait_hint`) so they are
    /// not built here.
    fn service_status(state: ServiceState, exit_code: u32) -> ServiceStatus {
        let controls_accepted = match state {
            ServiceState::Running => ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN,
            _ => ServiceControlAccept::empty(),
        };
        ServiceStatus {
            service_type: ServiceType::OWN_PROCESS,
            current_state: state,
            controls_accepted,
            exit_code: ServiceExitCode::Win32(exit_code),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod scm_impl {
    use super::*;

    pub(super) async fn probe() -> Result<StatusInputs> {
        anyhow::bail!(
            "internal error: Windows status path reached on a non-Windows target; \
             this is a build configuration bug"
        )
    }
}

// ── Tests ──────────────────────────────────────────────────────────────
//
// Status formatter coverage. These run on every unix host as well as on
// Windows so a regression in shape is caught on linux CI before
// windows-latest sees the change.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_status_reports_not_installed() {
        assert_eq!(
            render_status(StatusInputs::NotInstalled),
            "Service: not installed"
        );
    }

    #[test]
    fn render_status_reports_scm_unavailable() {
        let line = render_status(StatusInputs::ScmUnavailable);
        assert!(line.starts_with("Service: SCM unavailable"));
        // Operator hint about elevation belongs in the same line so a
        // tail -f / log collector picks it up alongside the verdict.
        assert!(line.contains("elevated PowerShell"));
    }

    #[test]
    fn render_status_includes_pid_when_running() {
        let line = render_status(StatusInputs::Probed {
            state: ServiceStateView::Running,
            pid: Some(4321),
        });
        assert_eq!(line, "Service: running (windows scm, pid 4321)");
    }

    #[test]
    fn render_status_running_without_pid_is_still_running() {
        // SCM occasionally returns process_id = None during the
        // start-pending -> running transition; we should not lose the
        // "running" verdict just because the pid was racing.
        let line = render_status(StatusInputs::Probed {
            state: ServiceStateView::Running,
            pid: None,
        });
        assert_eq!(line, "Service: running (windows scm)");
    }

    #[test]
    fn render_status_renders_non_running_states() {
        let cases = [
            (ServiceStateView::Stopped, "stopped"),
            (ServiceStateView::StartPending, "start-pending"),
            (ServiceStateView::StopPending, "stop-pending"),
            (ServiceStateView::Paused, "paused"),
            (ServiceStateView::ContinuePending, "continue-pending"),
            (ServiceStateView::PausePending, "pause-pending"),
        ];
        for (state, expected) in cases {
            let line = render_status(StatusInputs::Probed {
                state,
                pid: Some(1),
            });
            assert_eq!(line, format!("Service: {expected} (windows scm)"));
        }
    }
}
