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
use crate::state::db::provider_generations::WriterOutput;
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
    /// At least one local destination rejected metadata work for operational
    /// storage reasons (capacity, read-only mount, or access denial). Ordinary
    /// retryable metadata failures do not set this flag.
    pub(in crate::download) storage_failed: bool,
    pub(in crate::download) retired_from_selected_queue: usize,
    pub(in crate::download) state_failed: bool,
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
    run_pending_budget(
        db,
        metadata_flags,
        capture_timestamp_repair,
        temp_suffix,
        shutdown_token,
        library_scope,
        (offset, METADATA_REWRITE_BATCH),
    )
    .await
}

pub(in crate::download) async fn run_pending_budget<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    library_scope: Option<&[&str]>,
    page: (usize, usize),
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
            page.0,
            page.1.min(METADATA_REWRITE_BATCH),
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(target: "kei::download::metadata_rewrite", %error, "Failed to load pending metadata rewrites");
            return RewritePass {
                failed: 1,
                ..RewritePass::default()
            };
        }
    };
    Box::pin(run_pending_rows(
        db,
        metadata_flags,
        capture_timestamp_repair,
        temp_suffix,
        shutdown_token,
        pending,
    ))
    .await
}

/// Both selected destinations and ordinary retry pages use the same physical
/// writer and exact durable finalization owner.
pub(in crate::download) async fn run_pending_rows<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    pending: Vec<crate::state::db::PendingMetadataRewrite>,
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + ?Sized,
{
    write_rows(
        db,
        metadata_flags,
        capture_timestamp_repair,
        temp_suffix,
        shutdown_token,
        pending,
        WriterPhase::Inline,
    )
    .await
}

pub(in crate::download) async fn run_backfill_rows(
    db: &crate::state::SqliteStateDb,
    flags: MetadataFlags,
    suffix: Arc<str>,
    cancel: &CancellationToken,
    rows: Vec<crate::state::db::PendingMetadataRewrite>,
    generation: Option<&str>,
) -> RewritePass {
    write_rows(
        db,
        flags,
        CaptureTimestampRepair::Preserve,
        suffix,
        cancel,
        rows,
        WriterPhase::AfterSync { db, generation },
    )
    .await
}

#[derive(Clone, Copy)]
enum WriterPhase<'a> {
    Inline,
    AfterSync {
        db: &'a crate::state::SqliteStateDb,
        generation: Option<&'a str>,
    },
}

async fn write_rows<D: MembershipStore + MetadataRewriteStore + ?Sized>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    pending: Vec<crate::state::db::PendingMetadataRewrite>,
    phase: WriterPhase<'_>,
) -> RewritePass {
    let post_sync = matches!(phase, WriterPhase::AfterSync { .. });
    let selected_queue = if matches!(
        capture_timestamp_repair,
        CaptureTimestampRepair::ReplaceWithCaptureLocal
    ) {
        MetadataRewriteQueue::CaptureRepair
    } else {
        MetadataRewriteQueue::Ordinary
    };
    if pending.is_empty() {
        return RewritePass::default();
    }
    let needs_groupings = metadata_flags.uses_xmp_groupings()
        || pending.iter().any(|row| {
            row.selection_receipt.as_ref().is_some_and(|receipt| {
                MetadataFlags::from_bits(receipt.metadata_flags)
                    .is_some_and(MetadataFlags::uses_xmp_groupings)
            })
        });
    let (groupings_by_library, grouping_read_failures) = if needs_groupings {
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
    let mut storage_failed = false;
    let mut state_failed = !grouping_read_failures.is_empty();
    let mut retired_from_selected_queue = 0usize;
    for (idx, pending_rewrite) in pending.into_iter().enumerate() {
        if state_failed && post_sync {
            break;
        }
        if storage_failed {
            deferred += pending_count - idx;
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                remaining = pending_count - idx,
                "Operational storage failure; deferring remaining metadata rewrites"
            );
            break;
        }
        // Selected destinations already carry an exact prepared-output receipt.
        // Use that publication owner for an explicitly requested timestamp
        // rewrite rather than requiring a second legacy repair receipt.
        let selected_queue = if pending_rewrite.selection_receipt.is_some()
            && pending_rewrite.capture_repair_receipt.is_none()
        {
            MetadataRewriteQueue::Ordinary
        } else {
            selected_queue
        };
        let record = &pending_rewrite.asset;
        let metadata_flags = if let Some(receipt) = &pending_rewrite.selection_receipt {
            let Some(flags) = MetadataFlags::from_bits(receipt.metadata_flags) else {
                errored += 1;
                continue;
            };
            // A frozen obligation retains its requirements, but cannot turn a
            // writer back on after the current explicit options disable it.
            if !metadata_flags.contains(flags) {
                deferred += 1;
                continue;
            }
            if !cfg!(feature = "xmp")
                && flags.intersects(MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR)
            {
                deferred += 1;
                continue;
            }
            flags
        } else {
            metadata_flags
        };
        if !metadata_flags.has_any_write() {
            deferred += 1;
            continue;
        }
        if shutdown_token.is_cancelled() {
            deferred += pending_count - idx;
            tracing::info!(
                target: "kei::download::metadata_rewrite",
                "Shutdown requested, deferring remaining metadata rewrites");
            break;
        }
        if metadata_flags.uses_xmp_groupings()
            && grouping_read_failures.contains(record.library.as_ref())
        {
            errored += 1;
            continue;
        }
        let Some(local_path) = record.local_path.as_deref() else {
            continue;
        };
        let path = PathBuf::from(local_path);
        let _layout_guard =
            match crate::download::file::lock_download_destination(&path, shutdown_token).await {
                Ok(guard) => guard,
                Err(_) => {
                    errored += 1;
                    continue;
                }
            };
        if db.guard_primary_writer(&path).await.is_err() {
            deferred += 1;
            continue;
        }
        if let Some(receipt) = &pending_rewrite.selection_receipt
            && metadata_flags.uses_xmp_groupings()
        {
            let groups = groupings_by_library.get(record.library.as_ref());
            let albums = groups
                .and_then(|g| g.albums.get(record.id.as_ref()))
                .map_or(&[][..], Vec::as_slice);
            let people = groups
                .and_then(|g| g.people.get(record.id.as_ref()))
                .map_or(&[][..], Vec::as_slice);
            if !crate::state::db::provider_generations::writer_grouping_hash(albums, people)
                .is_ok_and(|hash| hash == receipt.grouping_hash)
            {
                deferred += 1;
                continue;
            }
        }
        let claim = match phase {
            WriterPhase::Inline => db.claim_selection_writer(&pending_rewrite).await,
            WriterPhase::AfterSync { db, generation } => {
                db.claim_backfill_writer(&pending_rewrite, generation).await
            }
        };
        match claim {
            Ok(true) => {}
            Ok(false) => {
                deferred += 1;
                continue;
            }
            Err(error) => {
                tracing::warn!(%error, "Could not validate queued metadata writer");
                state_failed = true;
                errored += 1;
                continue;
            }
        }

        if let Err(error) = crate::download::file::recover_metadata_replacements(&path).await {
            tracing::warn!(path = %path.display(), error = %format!("{error:#}"),
                "Could not recover metadata replacement; leaving marker for retry");
            storage_failed |= super::is_operational_storage_error(&error);
            errored += 1;
            continue;
        }
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
                storage_failed |= super::is_operational_storage_io(&e);
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

        let mut payload = groupings_by_library
            .get(record.library.as_ref())
            .map_or_else(
                || MetadataPayload::from_metadata(&record.metadata),
                |groupings| {
                    groupings.metadata_payload(&record.id, &record.metadata, metadata_flags)
                },
            );
        if metadata_flags.contains(MetadataFlags::ORIENTATION) {
            payload.original_orientation = crate::icloud::photos::metadata::original_orientation(
                &record.metadata,
                record.version_size,
                &record.checksum,
            );
        }
        let ownership = match db
            .embedded_metadata_receipt(
                &record.library,
                &record.id,
                record.version_size.as_str(),
                &path,
                Some(&pending_rewrite),
            )
            .await
        {
            Ok(receipt) => receipt,
            Err(error) => {
                state_failed = true;
                tracing::warn!(%error, "Could not read embedded field ownership");
                errored += 1;
                continue;
            }
        };
        let mut field_recovery_checksum = None;
        let mut field_recovery_size = None;
        if let Some(receipt) = ownership {
            let measured = if !receipt.published {
                crate::download::file::fingerprint_regular_file(&path)
                    .await
                    .ok()
            } else {
                None
            };
            let measured_checksum = measured.map(fingerprint_checksum);
            let current = measured_checksum
                .as_deref()
                .or(record.local_checksum.as_deref());
            if !receipt.published
                && current == Some(receipt.output_checksum.as_str())
                && pending_rewrite.capture_repair_receipt.is_none()
            {
                if let Some(preserved) = pending_rewrite
                    .selection_receipt
                    .as_ref()
                    .and_then(|r| r.preserved_local.as_ref())
                {
                    let modified = tokio::fs::metadata(&path)
                        .await
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map(chrono::DateTime::<chrono::Utc>::from);
                    if modified != preserved.modified {
                        skipped_drifted += 1;
                        continue;
                    }
                }
                let Some(measured) = measured else {
                    errored += 1;
                    continue;
                };
                if let Err(error) = db
                    .recover_embedded_metadata_receipt(&pending_rewrite, &receipt, measured.size)
                    .await
                {
                    state_failed = true;
                    tracing::warn!(%error, "Could not finalize previous embedded field output");
                    errored += 1;
                    continue;
                }
                field_recovery_checksum = Some(receipt.output_checksum.clone());
                field_recovery_size = Some(measured.size);
            }
            if current == Some(&receipt.output_checksum) {
                payload.prior_embedded = receipt.written;
            } else if !receipt.published && current == Some(&receipt.input_checksum) {
                payload.prior_embedded = receipt.previous;
            }
        }
        let payload = Arc::new(payload);
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
        let preserved_local = pending_rewrite
            .selection_receipt
            .as_ref()
            .and_then(|receipt| receipt.preserved_local.as_ref());
        if let Some(preserved) = preserved_local {
            let current_time = tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(chrono::DateTime::<chrono::Utc>::from);
            if current_time != preserved.modified
                || (embed_writable && preserved.modified.is_none())
            {
                skipped_drifted += 1;
                continue;
            }
        }
        let pre_rewrite_fingerprint = if metadata_flags.any_embed() || preserved_local.is_some() {
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
                    storage_failed |= super::is_operational_storage_error(&e);
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

        let recovered_selection_output = pending_rewrite
            .selection_receipt
            .as_ref()
            .and_then(|receipt| receipt.prepared.as_ref())
            .zip(pre_rewrite_fingerprint)
            .is_some_and(|((checksum, size), fingerprint)| {
                *size == fingerprint.size && *checksum == fingerprint_checksum(fingerprint)
            });

        // A file that no longer matches the recorded hash is not kei's to
        // rewrite: embedding would overwrite the evidence that `verify` and
        // `reconcile` rely on, and re-hashing would bless the damage. The
        // sidecar is a separate file, so it still runs.
        let drifted = matches!(
            (&pre_rewrite_checksum, &record.local_checksum),
            (Some(actual), Some(recorded)) if actual != recorded
        ) && !recovered_prepared_output
            && !recovered_selection_output
            && field_recovery_checksum
                .as_ref()
                .is_none_or(|checksum| Some(checksum) != pre_rewrite_checksum.as_ref());
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
        if let Some(checksum) = &field_recovery_checksum {
            // Recovery advanced only the previous output fingerprint. Complete
            // today's intent against that recovered input, not the stale snapshot.
            pending_for_finish.asset.local_checksum = Some(checksum.clone());
            if let Some(selection) = pending_for_finish.selection_receipt.as_mut() {
                selection.prepared = None;
            }
            if let Some(preserved) = pending_for_finish
                .selection_receipt
                .as_mut()
                .and_then(|r| r.preserved_local.as_mut())
            {
                preserved.checksum = checksum.clone();
                if let Some(size) = field_recovery_size {
                    preserved.size = size;
                }
            }
        }
        let mut final_fingerprint = pre_rewrite_fingerprint;
        let mut capture_embed_complete = false;
        let mut recovered_capture_only = false;

        if recovered_selection_output {
            outcome.embed_no_write = true;
        } else if recovered_prepared_output {
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
                pending_rewrite.source_checksum.as_deref(),
                Arc::clone(&payload),
                created_local,
                metadata_flags,
                capture_timestamp_repair,
                &temp_suffix,
            )
            .await
            {
                EmbedPrepareResult::Prepared(prepared) => {
                    let prepared =
                        if let Some(modified) = preserved_local.and_then(|local| local.modified) {
                            match prepared.require_mtime(modified.into()).await {
                                Ok(prepared) => prepared,
                                Err(error) => {
                                    storage_failed |= super::is_operational_storage_error(&error);
                                    errored += 1;
                                    continue;
                                }
                            }
                        } else {
                            prepared
                        };
                    let fields = crate::state::types::EmbeddedMetadataReceipt {
                        published: false,
                        input_checksum: pre_rewrite_fingerprint
                            .map(fingerprint_checksum)
                            .unwrap_or_default(),
                        output_checksum: fingerprint_checksum(prepared.output_fingerprint()),
                        previous: payload.prior_embedded.clone(),
                        written: (*prepared.embedded_fields).clone(),
                    };
                    if let Err(error) = db
                        .record_embedded_metadata_receipt(&pending_for_finish, &fields)
                        .await
                    {
                        tracing::warn!(%error, "Could not retain prepared embedded field ownership");
                        state_failed = true;
                        errored += 1;
                        continue;
                    }
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
                        if pending_for_finish.selection_receipt.is_some() {
                            let output = prepared.output_fingerprint();
                            match db
                                .record_selection_metadata_prepared(
                                    &pending_for_finish,
                                    &fingerprint_checksum(output),
                                    output.size,
                                )
                                .await
                            {
                                Ok(Some(receipt)) => {
                                    pending_for_finish.selection_receipt = Some(receipt)
                                }
                                result => {
                                    state_failed |= result.is_err();
                                    errored += 1;
                                    continue;
                                }
                            }
                        }
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
                                    state_failed = true;
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
                            EmbedWriteResult::Applied(output, fields) => {
                                outcome.embedded_fields = fields;
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
                            EmbedWriteResult::Storage => {
                                outcome.embed_failed = true;
                                storage_failed = true;
                            }
                            EmbedWriteResult::NoWrite => outcome.embed_no_write = true,
                        }
                    }
                }
                EmbedPrepareResult::NoWrite => {
                    outcome.embed_no_write = true;
                    capture_embed_complete = selected_queue == MetadataRewriteQueue::CaptureRepair;
                }
                EmbedPrepareResult::Failed => outcome.embed_failed = true,
                EmbedPrepareResult::Storage => {
                    outcome.embed_failed = true;
                    storage_failed = true;
                }
                EmbedPrepareResult::InputChanged => {
                    outcome.embed_failed = true;
                    outcome.embed_input_changed = true;
                }
            }
        }

        #[cfg(feature = "xmp")]
        if metadata_flags.contains(MetadataFlags::XMP_SIDECAR) {
            // Only certify a packet that was planned from the recorded media bytes.
            // In particular, a sidecar-only retry need not own the media itself.
            let certified_input = if pending_for_finish.selection_receipt.is_some() || post_sync {
                match final_fingerprint {
                    Some(output) => Some(output),
                    None => crate::download::file::fingerprint_regular_file(&path)
                        .await
                        .ok()
                        .filter(|measured| {
                            Some(fingerprint_checksum(*measured)) == record.local_checksum
                        }),
                }
            } else {
                None
            };
            if post_sync && certified_input.is_none() {
                skipped_drifted += 1;
                continue;
            }
            let sidecar = write_sidecar_metadata(
                &path,
                Arc::clone(&payload),
                created_local,
                pending_rewrite.source_checksum.as_deref(),
                &temp_suffix,
            )
            .await;
            outcome.sidecar_failed = sidecar.failed;
            if let Some(receipt) = pending_for_finish.selection_receipt.as_mut()
                && let Some(before) = certified_input
                && let Some(packet) = sidecar.output
            {
                if crate::download::file::fingerprint_regular_file(&path)
                    .await
                    .ok()
                    == Some(before)
                {
                    receipt.writer_output =
                        Some((WriterOutput::Sidecar(packet), *created_local.offset()));
                } else if post_sync {
                    skipped_drifted += 1;
                    continue;
                }
            }
            storage_failed |= sidecar.storage_failed;
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
                    storage_failed |= super::is_operational_storage_error(&e);
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
        if !metadata_flags.contains(MetadataFlags::XMP_SIDECAR)
            && ordinary_complete
            && final_fingerprint.is_some()
            && let Some(receipt) = pending_for_finish.selection_receipt.as_mut()
        {
            receipt.writer_output = Some((WriterOutput::EmbeddedOnly, *created_local.offset()));
        }
        if let Some(preserved) = preserved_local {
            let current_time = tokio::fs::metadata(&path)
                .await
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(chrono::DateTime::<chrono::Utc>::from);
            if current_time != preserved.modified {
                skipped_drifted += 1;
                continue;
            }
        }
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
            .or_else(|| pending_for_finish.asset.local_checksum.clone());
        let media_checksum_changed = final_checksum != pending_for_finish.asset.local_checksum;
        let input_checksum_for_download = final_checksum
            .as_deref()
            // Recovered metadata output is the current writer input, but is
            // not evidence of the original downloaded bytes.
            .zip(record.local_checksum.as_deref().filter(|before| {
                Some(*before) == pending_for_finish.asset.local_checksum.as_deref()
            }))
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
                    state_failed = true;
                    tracing::warn!(
                        target: "kei::download::metadata_rewrite",
                        asset_id = %record.id,
                        %error,
                        "Failed to finalise metadata rewrite state"
                    );
                    if !post_sync
                        && selected_queue == MetadataRewriteQueue::Ordinary
                        && pending_rewrite.selection_receipt.is_none()
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
        state_failed,
        fetched: pending_count,
        applied,
        failed: errored + deferred + skipped_drifted + skipped_unverified,
        storage_failed,
        retired_from_selected_queue,
    }
}

#[cfg(test)]
mod tests;
