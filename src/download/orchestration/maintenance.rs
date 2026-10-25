//! Explicit metadata refresh drains the existing durable rewrite queue.
use super::models::DownloadStore;
use crate::download::metadata_rewrite::{self, CaptureTimestampRepair};
use crate::download::pipeline::MetadataFlags;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Drain pending metadata-rewrite markers in bounded batches so a
/// `--refresh-metadata` migration finishes in its own run. Returns the number
/// of markers left failing when the drain stops.
pub(crate) async fn drain_pending_metadata_rewrites(
    db: &dyn DownloadStore,
    metadata: &crate::config::MetadataConfig,
    capture_timestamp_repair: CaptureTimestampRepair,
    library_scope: &[&str],
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> usize {
    let flags = MetadataFlags::from(metadata);
    if !flags.has_any_write() {
        return 0;
    }
    let mut offset = 0usize;
    let mut failed = 0usize;
    loop {
        let pass = metadata_rewrite::run_pending_page(
            db,
            flags,
            capture_timestamp_repair,
            Arc::clone(&temp_suffix),
            shutdown_token,
            Some(library_scope),
            offset,
        )
        .await;
        failed = failed.saturating_add(pass.failed);
        if pass.fetched == 0 {
            return failed;
        }
        tracing::debug!(
            applied = pass.applied,
            retired = pass.retired_from_selected_queue,
            failed = pass.failed,
            "Metadata rewrite batch completed"
        );
        if shutdown_token.is_cancelled() {
            return failed.max(1);
        }
        offset = offset.saturating_add(
            pass.fetched
                .saturating_sub(pass.retired_from_selected_queue),
        );
    }
}

#[cfg(test)]
mod tests;
