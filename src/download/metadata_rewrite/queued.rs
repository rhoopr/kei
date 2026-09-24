//! Durable retry tagging, bounded execution, and marker retirement.

use super::MetadataWriteOutcome;
use super::capture::{
    capture_repair_is_verified, fingerprint_checksum, receipt_matches_fingerprint,
};
use super::embedded::{
    EmbedPrepareResult, EmbedWriteResult, prepare_embed_metadata, publish_embed_metadata,
};
use super::grouping::load_pending_groupings;
use super::planning::{CaptureTimestampRepair, MetadataFlags};
#[cfg(feature = "xmp")]
use super::sidecar::write_sidecar_metadata;
use crate::download::filter::MetadataPayload;
use crate::download::{DownloadConfig, DownloadContext};
use crate::icloud::photos::PhotoAsset;
use crate::state::db::{CaptureRepairReceipt, MetadataRewriteCompletion, MetadataRewriteQueue};
use crate::state::{MembershipStore, MetadataRewriteStore, VersionSizeKey};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Persist a metadata-rewrite marker for each candidate version whose
/// metadata drifted from the stored hash, or that already carries a marker
/// from a prior sync. No-op when metadata writing is off or the state DB
/// is absent.
pub(in crate::download) async fn tag_if_needed<D>(
    state_db: Option<&D>,
    config: &DownloadConfig,
    asset: &PhotoAsset,
    candidates: &[(VersionSizeKey, &str)],
    ctx: &DownloadContext,
) where
    D: MetadataRewriteStore + ?Sized,
{
    if !MetadataFlags::from(config).has_any_write() {
        return;
    }
    let Some(db) = state_db else {
        return;
    };
    let library = asset.source_zone().unwrap_or(config.library.as_ref());
    let capture = crate::download::filter::metadata_capture(asset);
    for &(vs, _) in candidates {
        let checksum = ctx
            .downloaded_checksums
            .get(library)
            .and_then(|assets| assets.get(asset.state_id()))
            .and_then(|versions| versions.get(vs.as_str()))
            .map_or("", AsRef::as_ref);
        let metadata = capture.resolve(vs, checksum);
        let new_hash = metadata.metadata_hash.as_deref();
        if !ctx.needs_metadata_rewrite(library, asset.state_id(), vs, new_hash) {
            continue;
        }
        tracing::info!(
            target: "kei::download::metadata_rewrite",
            asset_id = %asset.id(),
            version_size = vs.as_str(),
            "Metadata-only change detected; tagging for rewrite"
        );
        if let Err(e) = db
            .record_metadata_write_failure(library, asset.state_id(), vs.as_str())
            .await
        {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                asset_id = %asset.id(),
                error = %e,
                "Failed to set metadata rewrite marker"
            );
        }
    }
}

/// Maximum assets processed per metadata-rewrite invocation. Bounds worst-case
/// tail work at sync end; anything beyond this rolls into the next sync.
const METADATA_REWRITE_BATCH: usize = 500;

/// Per-batch outcome of [`run_pending`]: fetched, applied, and still-failing counts.
#[derive(Default)]
pub(in crate::download) struct RewritePass {
    pub(in crate::download) fetched: usize,
    pub(in crate::download) applied: usize,
    pub(in crate::download) failed: usize,
    pub(in crate::download) retired_from_selected_queue: usize,
}

/// Process one bounded batch of persisted metadata-rewrite markers: for each
/// asset whose `metadata_write_failed_at` is set and whose local file is still
/// on disk, re-apply EXIF/XMP using the stored metadata. On success clears the
/// marker; on failure leaves it so the next pass retries. Returns the
/// per-batch counts.
pub(in crate::download) async fn run_pending<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + ?Sized,
{
    run_pending_page(
        db,
        metadata_flags,
        CaptureTimestampRepair::Preserve,
        temp_suffix,
        shutdown_token,
        None,
        0,
    )
    .await
}
pub(in crate::download) async fn run_pending_page<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    library_scope: Option<&[&str]>,
    offset: usize,
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + ?Sized,
{
    let selected_queue = if matches!(
        capture_timestamp_repair,
        CaptureTimestampRepair::ReplaceWithCaptureLocal
    ) {
        MetadataRewriteQueue::CaptureRepair
    } else {
        MetadataRewriteQueue::Ordinary
    };
    let pending = match db
        .get_pending_metadata_rewrites_page_for_queue(
            selected_queue,
            library_scope,
            offset,
            METADATA_REWRITE_BATCH,
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                error = %e, "Failed to load pending metadata rewrites");
            return RewritePass {
                failed: 1,
                ..RewritePass::default()
            };
        }
    };
    if pending.is_empty() {
        return RewritePass::default();
    }
    let (groupings_by_library, grouping_read_failures) = if metadata_flags.uses_xmp_groupings() {
        load_pending_groupings(db, &pending).await
    } else {
        (HashMap::new(), HashSet::new())
    };
    let pending_count = pending.len();
    tracing::info!(
        target: "kei::download::metadata_rewrite",
        count = pending_count,
        "Applying metadata rewrites to on-disk files"
    );
    let mut applied = 0usize;
    let mut skipped_missing = 0usize;
    let mut skipped_drifted = 0usize;
    let mut skipped_unverified = 0usize;
    let mut errored = 0usize;
    let mut deferred = 0usize;
    let mut retired_from_selected_queue = 0usize;
    for (idx, pending_rewrite) in pending.into_iter().enumerate() {
        let record = &pending_rewrite.asset;
        if shutdown_token.is_cancelled() {
            deferred += pending_count - idx;
            tracing::info!(
                target: "kei::download::metadata_rewrite",
                "Shutdown requested, deferring remaining metadata rewrites");
            break;
        }
        if grouping_read_failures.contains(record.library.as_ref()) {
            errored += 1;
            continue;
        }
        let Some(local_path) = record.local_path.as_deref() else {
            continue;
        };
        let path = PathBuf::from(local_path);
        // tokio::fs defers the stat to the blocking pool; raw
        // std::Path::exists() would block the async runtime thread.
        // Keep the marker on missing so a future sync that re-downloads the
        // asset re-drives the writer.
        match tokio::fs::try_exists(&path).await {
            Ok(true) => {}
            Ok(false) => {
                skipped_missing += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
                    path = %path.display(),
                    error = %e,
                    "Could not stat file for metadata rewrite; skipping"
                );
                skipped_missing += 1;
                continue;
            }
        }
        if matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ) && record.local_checksum.is_none()
        {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                path = %path.display(),
                "Capture timestamp repair requires a recorded local checksum; leaving the file and rewrite marker unchanged"
            );
            skipped_unverified += 1;
            continue;
        }

        let payload = Arc::new(
            groupings_by_library
                .get(record.library.as_ref())
                .map_or_else(
                    || MetadataPayload::from_metadata(&record.metadata),
                    |groupings| groupings.metadata_payload(&record.id, &record.metadata),
                ),
        );
        let created_local = record.metadata.capture_local(record.created_at);

        let embed_writable =
            metadata_flags.any_embed() && crate::download::metadata::is_embed_writable_path(&path);
        if matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ) && !embed_writable
        {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                path = %path.display(),
                "Capture timestamp repair does not support this embedded format; leaving the file and rewrite marker unchanged"
            );
            errored += 1;
            continue;
        }

        if selected_queue == MetadataRewriteQueue::CaptureRepair {
            let Some(receipt) = pending_rewrite.capture_repair_receipt.as_ref() else {
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
                    path = %path.display(),
                    "Capture-repair queue returned a row without durable repair state"
                );
                errored += 1;
                continue;
            };
            let receipt_metadata_hash = match receipt {
                CaptureRepairReceipt::Pending { metadata_hash }
                | CaptureRepairReceipt::Prepared { metadata_hash, .. } => metadata_hash,
            };
            if record.metadata.metadata_hash.as_deref() != Some(receipt_metadata_hash.as_str()) {
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
                    path = %path.display(),
                    "Capture-repair receipt does not match the current provider metadata"
                );
                errored += 1;
                continue;
            }
        }

        // Only an embedded write touches media bytes, so the drain needs the
        // pre-write hash to tell its own rewrite apart from damage that
        // arrived some other way.
        let pre_rewrite_fingerprint = if metadata_flags.any_embed() {
            match crate::download::file::fingerprint_regular_file(&path).await {
                Ok(fingerprint) => Some(fingerprint),
                Err(e) => {
                    tracing::warn!(
                        target: "kei::download::metadata_rewrite",
                        asset_id = %record.id,
                        path = %path.display(),
                        error = %e,
                        "Could not hash file before metadata rewrite; leaving marker for future retry"
                    );
                    errored += 1;
                    continue;
                }
            }
        } else {
            None
        };
        let pre_rewrite_checksum = pre_rewrite_fingerprint.map(fingerprint_checksum);

        let recovered_prepared_output = selected_queue == MetadataRewriteQueue::CaptureRepair
            && pending_rewrite
                .capture_repair_receipt
                .as_ref()
                .zip(pre_rewrite_fingerprint)
                .is_some_and(|(receipt, fingerprint)| {
                    receipt_matches_fingerprint(receipt, fingerprint)
                });

        // A file that no longer matches the recorded hash is not kei's to
        // rewrite: embedding would overwrite the evidence that `verify` and
        // `reconcile` rely on, and re-hashing would bless the damage. The
        // sidecar is a separate file, so it still runs.
        let drifted = matches!(
            (&pre_rewrite_checksum, &record.local_checksum),
            (Some(actual), Some(recorded)) if actual != recorded
        ) && !recovered_prepared_output;
        if drifted {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                asset_id = %record.id,
                path = %path.display(),
                "On-disk file does not match its recorded checksum; leaving the media and the \
                 marker alone. Run `kei verify --checksums` or `kei reconcile`"
            );
            skipped_drifted += 1;
        }

        let mut outcome = MetadataWriteOutcome::default();
        let mut pending_for_finish = pending_rewrite.clone();
        let mut final_fingerprint = pre_rewrite_fingerprint;
        let mut capture_embed_complete = false;
        let mut recovered_capture_only = false;

        if recovered_prepared_output {
            if capture_repair_is_verified(&path, Arc::clone(&payload), created_local).await {
                capture_embed_complete = true;
                recovered_capture_only = true;
                outcome.embed_no_write = true;
            } else {
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
                    path = %path.display(),
                    "Prepared capture-repair output no longer verifies the requested timestamp pair"
                );
                errored += 1;
                continue;
            }
        } else if !drifted && embed_writable {
            match prepare_embed_metadata(
                &path,
                pre_rewrite_fingerprint,
                Arc::clone(&payload),
                created_local,
                metadata_flags,
                capture_timestamp_repair,
                &temp_suffix,
            )
            .await
            {
                EmbedPrepareResult::Prepared(prepared) => {
                    if selected_queue == MetadataRewriteQueue::Ordinary
                        && pending_rewrite.capture_repair_receipt.is_some()
                    {
                        tracing::info!(
                            target: "kei::download::metadata_rewrite",
                            path = %path.display(),
                            "Deferring ordinary embedded metadata until explicit capture repair"
                        );
                        outcome.embed_failed = true;
                    } else {
                        if selected_queue == MetadataRewriteQueue::CaptureRepair {
                            let output = prepared.output_fingerprint();
                            let output_checksum = fingerprint_checksum(output);
                            let prepared_receipt = match db
                                .record_capture_repair_prepared(
                                    &pending_for_finish,
                                    &output_checksum,
                                    output.size,
                                )
                                .await
                            {
                                Ok(Some(receipt)) => receipt,
                                Ok(None) => {
                                    tracing::warn!(
                                        target: "kei::download::metadata_rewrite",
                                        path = %path.display(),
                                        "Capture-repair receipt changed before preparation completed"
                                    );
                                    errored += 1;
                                    continue;
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        target: "kei::download::metadata_rewrite",
                                        path = %path.display(),
                                        %error,
                                        "Could not record prepared capture-repair output"
                                    );
                                    errored += 1;
                                    continue;
                                }
                            };
                            pending_for_finish.capture_repair_receipt = Some(prepared_receipt);
                        }
                        match publish_embed_metadata(&path, prepared).await {
                            EmbedWriteResult::Applied(output) => {
                                final_fingerprint = output;
                                capture_embed_complete =
                                    selected_queue == MetadataRewriteQueue::CaptureRepair;
                                outcome.embed_output_fingerprint = output;
                            }
                            EmbedWriteResult::InputChanged => {
                                outcome.embed_failed = true;
                                outcome.embed_input_changed = true;
                            }
                            EmbedWriteResult::Failed => outcome.embed_failed = true,
                            EmbedWriteResult::NoWrite => outcome.embed_no_write = true,
                        }
                    }
                }
                EmbedPrepareResult::NoWrite => {
                    outcome.embed_no_write = true;
                    capture_embed_complete = selected_queue == MetadataRewriteQueue::CaptureRepair;
                }
                EmbedPrepareResult::Failed => outcome.embed_failed = true,
                EmbedPrepareResult::InputChanged => {
                    outcome.embed_failed = true;
                    outcome.embed_input_changed = true;
                }
            }
        }

        #[cfg(feature = "xmp")]
        if metadata_flags.contains(MetadataFlags::XMP_SIDECAR) {
            outcome.sidecar_failed = !write_sidecar_metadata(
                &path,
                Arc::clone(&payload),
                created_local,
                pending_rewrite.source_checksum.as_deref(),
                &temp_suffix,
            )
            .await;
        }

        if drifted {
            // The media still owes its metadata, so the marker cannot retire
            // no matter how the sidecar fared.
            continue;
        }

        if outcome.embed_input_changed {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                asset_id = %record.id,
                path = %path.display(),
                "File changed while its metadata rewrite was being prepared; leaving its checksum and rewrite marker unchanged"
            );
        } else if outcome.embed_no_write {
            match (
                pre_rewrite_fingerprint,
                crate::download::file::fingerprint_regular_file(&path).await,
            ) {
                (Some(before), Ok(after)) if after == before => {}
                (_, Ok(_)) => {
                    outcome.embed_failed = true;
                    outcome.embed_input_changed = true;
                    tracing::warn!(
                        target: "kei::download::metadata_rewrite",
                        asset_id = %record.id,
                        path = %path.display(),
                        "File changed after metadata inspection; leaving its checksum and rewrite marker unchanged"
                    );
                }
                (_, Err(e)) => {
                    outcome.embed_failed = true;
                    outcome.embed_input_changed = true;
                    tracing::warn!(
                        target: "kei::download::metadata_rewrite",
                        asset_id = %record.id,
                        path = %path.display(),
                        error = %e,
                        "Could not verify the file after metadata inspection; leaving its checksum and rewrite marker unchanged"
                    );
                }
            }
        }

        let ordinary_complete = !outcome.any_failed() && !recovered_capture_only;
        let capture_complete =
            selected_queue == MetadataRewriteQueue::CaptureRepair && capture_embed_complete;
        let completion = match selected_queue {
            MetadataRewriteQueue::Ordinary if ordinary_complete => {
                MetadataRewriteCompletion::Ordinary
            }
            MetadataRewriteQueue::Ordinary => MetadataRewriteCompletion::None,
            MetadataRewriteQueue::CaptureRepair if capture_complete && ordinary_complete => {
                MetadataRewriteCompletion::Both
            }
            MetadataRewriteQueue::CaptureRepair if capture_complete => {
                MetadataRewriteCompletion::CaptureRepair
            }
            MetadataRewriteQueue::CaptureRepair => MetadataRewriteCompletion::None,
        };

        let final_checksum = final_fingerprint
            .map(fingerprint_checksum)
            .or_else(|| record.local_checksum.clone());
        let media_checksum_changed = final_checksum != record.local_checksum;
        let input_checksum_for_download = final_checksum
            .as_deref()
            .zip(record.local_checksum.as_deref())
            .and_then(|(after, before)| (after != before).then_some(before));
        if completion != MetadataRewriteCompletion::None || media_checksum_changed {
            match db
                .finish_metadata_rewrite(
                    &pending_for_finish,
                    selected_queue,
                    final_checksum.as_deref(),
                    input_checksum_for_download,
                    completion,
                )
                .await
            {
                Ok(retired) => {
                    applied += usize::from(ordinary_complete);
                    retired_from_selected_queue += usize::from(retired);
                }
                Err(error) => {
                    tracing::warn!(
                        target: "kei::download::metadata_rewrite",
                        asset_id = %record.id,
                        %error,
                        "Failed to finalise metadata rewrite state"
                    );
                    if selected_queue == MetadataRewriteQueue::Ordinary
                        && pending_rewrite.capture_repair_receipt.is_none()
                        && let Err(fallback_error) = db
                            .finish_metadata_rewrite(
                                &pending_rewrite,
                                MetadataRewriteQueue::Ordinary,
                                None,
                                None,
                                MetadataRewriteCompletion::None,
                            )
                            .await
                    {
                        tracing::warn!(
                            target: "kei::download::metadata_rewrite",
                            asset_id = %record.id,
                            error = %fallback_error,
                            "Could not clear the stale media checksum; `kei reconcile` can still repair the file"
                        );
                    }
                    errored += 1;
                    continue;
                }
            }
        }

        if outcome.any_failed() {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                asset_id = %record.id,
                path = %path.display(),
                embed_failed = outcome.embed_failed,
                embed_input_changed = outcome.embed_input_changed,
                embed_no_write = outcome.embed_no_write,
                sidecar_failed = outcome.sidecar_failed,
                "Metadata rewrite failed; leaving remaining marker for future retry"
            );
            errored += 1;
        } else if completion == MetadataRewriteCompletion::None {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                asset_id = %record.id,
                path = %path.display(),
                "Metadata rewrite made no durable progress"
            );
            errored += 1;
        } else {
            tracing::debug!(
                target: "kei::download::metadata_rewrite",
                path = %path.display(), "Metadata rewrite completed");
        }
    }
    tracing::info!(
        target: "kei::download::metadata_rewrite",
        applied,
        errored,
        skipped_missing,
        skipped_drifted,
        skipped_unverified,
        deferred,
        "Metadata rewrite pass complete"
    );
    RewritePass {
        fetched: pending_count,
        applied,
        failed: errored + deferred + skipped_drifted + skipped_unverified,
        retired_from_selected_queue,
    }
}

#[cfg(test)]
mod tests;
