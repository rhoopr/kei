//! Graceful shutdown coordinator.
//!
//! Listens for SIGINT (Ctrl+C), SIGTERM, and SIGHUP, then cancels a
//! [`tokio_util::sync::CancellationToken`] so the download pipeline can
//! drain in-flight work before exiting. A second signal force-exits.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

#[cfg(unix)]
use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::systemd::SystemdNotifier;

/// Install signal handlers and return a [`CancellationToken`] that is
/// cancelled on the first SIGINT / SIGTERM / SIGHUP.  A second signal
/// force-exits the process.
pub(crate) fn install_signal_handler(
    notifier: &SystemdNotifier,
) -> anyhow::Result<CancellationToken> {
    let token = CancellationToken::new();
    let count = Arc::new(AtomicU32::new(0));

    #[cfg(unix)]
    let (mut sigterm, mut sighup) = {
        use tokio::signal::unix::{signal, SignalKind};
        (
            signal(SignalKind::terminate()).context("failed to register SIGTERM handler")?,
            signal(SignalKind::hangup()).context("failed to register SIGHUP handler")?,
        )
    };

    let handler_token = token.clone();
    let handler_notifier = *notifier;
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
                tracing::info!("Received shutdown signal, finishing current downloads...");
                tracing::info!("Press Ctrl+C again to force exit");
                handler_token.cancel();
            } else {
                tracing::warn!("Force exit requested");
                std::process::exit(130);
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
        let token = install_signal_handler(&notifier).unwrap();
        assert!(!token.is_cancelled());
    }
}
