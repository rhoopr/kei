//! Explicit metadata refresh drains the existing durable rewrite queue.
use super::models::DownloadStore;
use crate::download::metadata_rewrite::{self, CaptureTimestampRepair, MetadataWriterWitness};
use crate::download::pipeline::MetadataFlags;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// New physical failures and outstanding witnessed blocked debt are separate:
/// only the former is a failure count, while the latter keeps completion and
/// source revalidation closed without retrying a witnessed duplicate.
#[must_use]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RefreshTailOutcome {
    pub(crate) new_failures: usize,
    pub(crate) witnessed_blocked: usize,
}

/// Drain pending metadata-rewrite markers in bounded batches so a
/// `--refresh-metadata` migration finishes in its own run. Returns the number
/// of markers left failing when the drain stops.
pub(crate) async fn drain_pending_metadata_rewrites(
    db: &dyn DownloadStore,
    root: &std::path::Path,
    metadata: &crate::config::MetadataConfig,
    capture_timestamp_repair: CaptureTimestampRepair,
    library_scope: &[&str],
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> usize {
    drain_pending_metadata_rewrites_witnessed(
        db,
        root,
        metadata,
        capture_timestamp_repair,
        library_scope,
        temp_suffix,
        shutdown_token,
        None,
    )
    .await
    .new_failures
}

pub(crate) async fn drain_pending_metadata_rewrites_witnessed(
    db: &dyn DownloadStore,
    root: &std::path::Path,
    metadata: &crate::config::MetadataConfig,
    capture_timestamp_repair: CaptureTimestampRepair,
    library_scope: &[&str],
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    witness: Option<Arc<MetadataWriterWitness>>,
) -> RefreshTailOutcome {
    let flags = MetadataFlags::from(metadata);
    if !flags.has_any_write() {
        return RefreshTailOutcome::default();
    }
    let mut offset = 0usize;
    let mut outcome = RefreshTailOutcome::default();
    let repair_root = matches!(
        capture_timestamp_repair,
        CaptureTimestampRepair::ReplaceWithCaptureLocal
    )
    .then_some(root);
    loop {
        let pass = metadata_rewrite::run_pending_page_witnessed(
            db,
            flags,
            capture_timestamp_repair,
            Arc::clone(&temp_suffix),
            shutdown_token,
            repair_root,
            Some(library_scope),
            offset,
            witness.clone(),
        )
        .await;
        outcome.new_failures = outcome.new_failures.saturating_add(pass.failed);
        outcome.witnessed_blocked = outcome
            .witnessed_blocked
            .saturating_add(pass.witnessed_blocked);
        if pass.fetched == 0 {
            return outcome;
        }
        tracing::debug!(
            applied = pass.applied,
            retired = pass.retired_from_selected_queue,
            failed = pass.failed,
            witnessed_blocked = pass.witnessed_blocked,
            "Metadata rewrite batch completed"
        );
        if shutdown_token.is_cancelled() {
            outcome.new_failures = outcome.new_failures.max(1);
            return outcome;
        }
        offset = offset.saturating_add(
            pass.fetched
                .saturating_sub(pass.retired_from_selected_queue),
        );
    }
}

#[cfg(test)]
mod tests;
