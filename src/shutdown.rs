//! Graceful shutdown coordinator.
//!
//! Listens for SIGINT (Ctrl+C), SIGTERM, and SIGHUP, then cancels a
//! [`tokio_util::sync::CancellationToken`] so the download pipeline can
//! drain in-flight work before exiting. A second signal force-exits.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[cfg(unix)]
use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::personality::Mode;
use crate::systemd::SystemdNotifier;

/// How long to wait for graceful shutdown before force-exiting.
/// Aligned with `stop_grace_period` in docker-compose.yml.
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// The exit code emitted when graceful shutdown drains exceed the
/// timeout. 130 matches the conventional "killed by SIGINT" code so
/// shell users see a familiar number instead of a kei-specific one.
const FORCE_EXIT_CODE: i32 = 130;

/// Wait `timeout`, then invoke `on_timeout` with [`FORCE_EXIT_CODE`].
/// The warn line is part of the contract: an operator chasing a
/// SIGKILL'd run greps for it.
pub(crate) async fn wait_then_force_exit<F>(timeout: Duration, on_timeout: F)
where
    F: FnOnce(i32),
{
    tokio::time::sleep(timeout).await;
    tracing::warn!("Graceful shutdown timed out, forcing exit");
    on_timeout(FORCE_EXIT_CODE);
}

/// Install signal handlers and return a [`CancellationToken`] that is
/// cancelled on the first SIGINT / SIGTERM / SIGHUP.  A second signal
/// force-exits the process.
///
/// The signal-handler task and the force-exit watchdog are deliberately
/// fire-and-forget: they must live for the lifetime of the process and
/// must not block any caller. No `JoinHandle` is returned — a panic inside
/// the handler would be surfaced by tokio's panic logger, and there is no
/// meaningful "shutdown" for these tasks short of the process exiting.
pub(crate) fn install_signal_handler(
    notifier: SystemdNotifier,
    mode: Mode,
) -> anyhow::Result<CancellationToken> {
    let token = CancellationToken::new();
    let count = Arc::new(AtomicU32::new(0));

    #[cfg(unix)]
    let (mut sigterm, mut sighup) = {
        use tokio::signal::unix::{SignalKind, signal};
        (
            signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?,
            signal(SignalKind::hangup()).context("failed to register SIGHUP handler")?,
        )
    };

    let handler_token = token.clone();
    let handler_notifier = notifier;
    tokio::spawn(async move {
        loop {
            #[cfg(unix)]
            {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm.recv() => {}
                    _ = sighup.recv() => {}
                }
            }

            #[cfg(not(unix))]
            {
                if tokio::signal::ctrl_c().await.is_err() {
                    tracing::error!("Failed to listen for Ctrl+C");
                    return;
                }
            }

            let prev = count.fetch_add(1, Ordering::SeqCst);
            if prev == 0 {
                handler_notifier.notify_stopping();
                // Friendly mode filters tracing to WARN+, so the info!
                // lines below are dropped there. Emit a curated line via
                // narration so the user sees that Ctrl+C was acknowledged
                // instead of "nothing happened" (no-op in off mode).
                crate::personality::narration::stop_signal_to_stderr(mode);
                tracing::info!("Received shutdown signal, finishing current downloads...");
                tracing::info!("Press Ctrl+C again to force exit");
                handler_token.cancel();
                // Force exit if graceful shutdown hangs (e.g. NFS stall,
                // dead CDN connection). Matches docker-compose
                // stop_grace_period so the app exits cleanly before Docker
                // sends SIGKILL.
                tokio::spawn(wait_then_force_exit(GRACEFUL_SHUTDOWN_TIMEOUT, |code| {
                    std::process::exit(code)
                }));
            } else {
                tracing::warn!("Force exit requested");
                std::process::exit(FORCE_EXIT_CODE);
            }
        }
    });

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_uncancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn child_tokens_observe_parent_cancel() {
        let parent = CancellationToken::new();
        let child = parent.child_token();
        parent.cancel();
        assert!(child.is_cancelled());
    }

    /// Verify that `install_signal_handler` returns a live, uncancelled token
    /// (signal delivery can't be safely tested in a shared test binary).
    #[tokio::test]
    async fn install_returns_live_token() {
        let notifier = SystemdNotifier::new(false);
        let token = install_signal_handler(notifier, Mode::Off).unwrap();
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn install_accepts_friendly_mode() {
        // Friendly mode threads through to the narration call inside the
        // handler; the install path itself must not depend on it.
        let notifier = SystemdNotifier::new(false);
        let token = install_signal_handler(notifier, Mode::Friendly).unwrap();
        assert!(!token.is_cancelled());
    }

    /// `start_paused = true` lets tokio auto-advance the clock when
    /// nothing else can make progress, so awaiting the helper runs the
    /// sleep to completion without wall-clock delay.
    #[tokio::test(start_paused = true)]
    async fn shutdown_grace_period_elapses_invokes_exit_callback() {
        let (capture, _guard) = crate::test_helpers::TracingCapture::install();
        let exited: Arc<std::sync::Mutex<Option<i32>>> = Arc::new(std::sync::Mutex::new(None));
        let exited_clone = Arc::clone(&exited);

        wait_then_force_exit(Duration::from_secs(30), move |code| {
            *exited_clone.lock().unwrap() = Some(code);
        })
        .await;

        assert_eq!(*exited.lock().unwrap(), Some(FORCE_EXIT_CODE));
        assert!(
            capture.contains_event(|event| {
                event.level == tracing::Level::WARN
                    && event.message() == Some("Graceful shutdown timed out, forcing exit")
            }),
            "warn line must accompany the timeout fire so operators can grep for it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_then_force_exit_does_not_fire_before_timeout() {
        let exited: Arc<std::sync::Mutex<Option<i32>>> = Arc::new(std::sync::Mutex::new(None));
        let exited_clone = Arc::clone(&exited);

        let handle = tokio::spawn(wait_then_force_exit(Duration::from_secs(30), move |code| {
            *exited_clone.lock().unwrap() = Some(code);
        }));

        tokio::time::advance(Duration::from_secs(15)).await;
        assert_eq!(
            *exited.lock().unwrap(),
            None,
            "callback must not fire before the configured timeout elapses"
        );

        // Drain the rest so the spawned task completes cleanly.
        tokio::time::advance(Duration::from_secs(20)).await;
        handle.await.unwrap();
        assert_eq!(*exited.lock().unwrap(), Some(FORCE_EXIT_CODE));
    }
}
