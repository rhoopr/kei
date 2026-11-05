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
use super::sidecar::write_sidecar_metadata_observed;
use crate::download::filter::MetadataPayload;
use crate::state::db::{CaptureRepairReceipt, MetadataRewriteCompletion, MetadataRewriteQueue};
use crate::state::{MembershipStore, MetadataRewriteStore};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[cfg(all(test, feature = "xmp"))]
static TEST_SUPPRESSIONS: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<PathBuf, std::sync::Weak<std::sync::atomic::AtomicUsize>>,
    >,
> = std::sync::OnceLock::new();
#[cfg(all(test, feature = "xmp"))]
pub(crate) fn test_metadata_suppressions(
    path: &std::path::Path,
) -> Arc<std::sync::atomic::AtomicUsize> {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    TEST_SUPPRESSIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(path.to_owned(), Arc::downgrade(&count));
    count
}
#[cfg(all(test, feature = "xmp"))]
fn record_test_suppression(path: &std::path::Path) {
    if let Some(count) = TEST_SUPPRESSIONS.get().and_then(|counts| {
        counts
            .lock()
            .unwrap()
            .get(path)
            .and_then(std::sync::Weak::upgrade)
    }) {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// An exact selected request already attempted in this tail. The retained
/// namespace and file handles fence target replacement and inode reuse.
#[cfg(feature = "xmp")]
struct BlockedAttempt {
    receipt: crate::state::db::SelectionMetadataReceipt,
    asset: serde_json::Value,
    source_checksum: Option<String>,
    media: crate::download::file::ExistingFileFingerprint,
    media_target: TargetSnapshot,
    target: TargetSnapshot,
}

#[derive(Default)]
pub(in crate::download) struct TailAttempts {
    #[cfg(feature = "xmp")]
    blocked: Vec<Arc<BlockedAttempt>>,
}

#[cfg(all(feature = "xmp", unix))]
#[derive(PartialEq, Eq)]
struct MetadataStamp {
    identity: crate::fs_util::FileIdentity,
    mode: u32,
    uid: u32,
    gid: u32,
    changed: (i64, i64),
    modified: (i64, i64),
    size: u64,
}

#[cfg(all(feature = "xmp", unix))]
fn metadata_stamp(file: &std::fs::File) -> std::io::Result<MetadataStamp> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = file.metadata()?;
    Ok(MetadataStamp {
        identity: crate::fs_util::file_identity(file)?,
        mode: meta.mode(),
        uid: meta.uid(),
        gid: meta.gid(),
        changed: (meta.ctime(), meta.ctime_nsec()),
        modified: (meta.mtime(), meta.mtime_nsec()),
        size: meta.len(),
    })
}

#[cfg(all(feature = "xmp", unix))]
#[derive(PartialEq, Eq)]
enum TargetValue {
    Missing,
    File(MetadataStamp, [u8; 32]),
    Directory(MetadataStamp),
}

#[cfg(all(feature = "xmp", unix))]
struct TargetSnapshot {
    target: crate::fs_util::ConfinedPath,
    parent: std::fs::File,
    parent_stamp: MetadataStamp,
    // Keep a present entry alive, preventing identity reuse.
    _entry: Option<std::fs::File>,
    value: TargetValue,
}

#[cfg(all(feature = "xmp", unix))]
fn target_value(
    target: &crate::fs_util::ConfinedPath,
) -> std::io::Result<(TargetValue, Option<std::fs::File>)> {
    use sha2::Digest as _;
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    target.validate_namespace()?;
    if !target.entry_exists()? {
        return Ok((TargetValue::Missing, None));
    }
    let metadata = std::fs::symlink_metadata(target.path())?;
    if metadata.file_type().is_symlink() {
        return Err(std::io::Error::other("sidecar symlink"));
    }
    if metadata.is_dir() {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(target.path())?;
        let stamp = metadata_stamp(&file)?;
        target.validate_namespace()?;
        return Ok((TargetValue::Directory(stamp), Some(file)));
    }
    let mut file = target
        .open_optional_regular()?
        .ok_or_else(|| std::io::Error::other("sidecar disappeared"))?;
    let stamp = metadata_stamp(&file)?;
    let mut hash = sha2::Sha256::new();
    let mut buf = [0u8; 16384];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(
            buf.get(..n)
                .ok_or_else(|| std::io::Error::other("invalid sidecar read length"))?,
        );
    }
    if metadata_stamp(&file)? != stamp {
        return Err(std::io::Error::other("sidecar changed"));
    }
    target.validate_identity(stamp.identity)?;
    Ok((TargetValue::File(stamp, hash.finalize().into()), Some(file)))
}

#[cfg(all(feature = "xmp", unix))]
fn snapshot_path(path: &std::path::Path) -> Option<TargetSnapshot> {
    let target_path = path.to_path_buf();
    let target = crate::fs_util::ConfinedPath::open(
        path.parent()?,
        &target_path,
        crate::fs_util::ConfinedParents::Existing,
    )
    .ok()?;
    let parent = std::fs::File::open(path.parent()?).ok()?;
    let parent_stamp = metadata_stamp(&parent).ok()?;
    let (value, entry) = target_value(&target).ok()?;
    if metadata_stamp(&parent).ok()? != parent_stamp {
        return None;
    }
    target.validate_namespace().ok()?;
    Some(TargetSnapshot {
        target,
        parent,
        parent_stamp,
        _entry: entry,
        value,
    })
}

#[cfg(all(feature = "xmp", unix))]
fn blocked_target(path: &std::path::Path) -> Option<TargetSnapshot> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".xmp");
    snapshot_path(&path.with_file_name(name))
}

#[cfg(all(feature = "xmp", unix))]
impl TargetSnapshot {
    fn unchanged(&self) -> bool {
        self.target.validate_namespace().is_ok()
            && metadata_stamp(&self.parent).ok().as_ref() == Some(&self.parent_stamp)
            && target_value(&self.target)
                .ok()
                .is_some_and(|(value, _)| value == self.value)
            && metadata_stamp(&self.parent).ok().as_ref() == Some(&self.parent_stamp)
            && self.target.validate_namespace().is_ok()
    }
}

// Unsupported platforms keep retries rather than relying on incomplete file identity.
#[cfg(all(feature = "xmp", not(unix)))]
struct TargetSnapshot;
#[cfg(all(feature = "xmp", not(unix)))]
fn blocked_target(_path: &std::path::Path) -> Option<TargetSnapshot> {
    None
}
#[cfg(all(feature = "xmp", not(unix)))]
fn snapshot_path(_path: &std::path::Path) -> Option<TargetSnapshot> {
    None
}

#[cfg(all(feature = "xmp", not(unix)))]
impl TargetSnapshot {
    fn unchanged(&self) -> bool {
        false
    }
}

#[cfg(feature = "xmp")]
impl BlockedAttempt {
    fn matches(
        &self,
        row: &crate::state::db::PendingMetadataRewrite,
        media: crate::download::file::ExistingFileFingerprint,
    ) -> bool {
        row.selection_receipt.as_ref() == Some(&self.receipt)
            && row.source_checksum == self.source_checksum
            && serde_json::to_value(&row.asset).ok().as_ref() == Some(&self.asset)
            && media == self.media
            && self.media_target.unchanged()
            && self.target.unchanged()
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
    D: MembershipStore + MetadataRewriteStore + crate::state::ReconciliationStateStore + ?Sized,
{
    run_pending_page(
        db,
        metadata_flags,
        CaptureTimestampRepair::Preserve,
        temp_suffix,
        shutdown_token,
        None,
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
    root: Option<&std::path::Path>,
    library_scope: Option<&[&str]>,
    offset: usize,
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + crate::state::ReconciliationStateStore + ?Sized,
{
    run_pending_budget(
        db,
        metadata_flags,
        capture_timestamp_repair,
        temp_suffix,
        shutdown_token,
        root,
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
    root: Option<&std::path::Path>,
    library_scope: Option<&[&str]>,
    page: (usize, usize),
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + crate::state::ReconciliationStateStore + ?Sized,
{
    let mut attempts = TailAttempts::default();
    run_pending_budget_tracked(
        db,
        metadata_flags,
        capture_timestamp_repair,
        temp_suffix,
        shutdown_token,
        root,
        library_scope,
        (page.0, page.1, &mut attempts),
    )
    .await
}

pub(in crate::download) async fn run_pending_budget_tracked<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    root: Option<&std::path::Path>,
    library_scope: Option<&[&str]>,
    page: (usize, usize, &mut TailAttempts),
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + crate::state::ReconciliationStateStore + ?Sized,
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
    Box::pin(run_pending_rows_tracked(
        db,
        metadata_flags,
        capture_timestamp_repair,
        temp_suffix,
        shutdown_token,
        pending,
        (root, page.2),
    ))
    .await
}

/// Both selected destinations and ordinary retry pages use the same physical
/// writer and exact durable finalization owner.
pub(in crate::download) async fn run_pending_rows_tracked<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    pending: Vec<crate::state::db::PendingMetadataRewrite>,
    context: (Option<&std::path::Path>, &mut TailAttempts),
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + crate::state::ReconciliationStateStore + ?Sized,
{
    let (archive_root, attempts) = context;
    #[cfg(not(feature = "xmp"))]
    let _ = attempts;
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
    // Direct maintenance and selected tails share this checked batch boundary.
    // No failed/unknown ownership read may authorize a writer. Rows stay durable.
    match crate::download::planner::OwnershipCheck::inspect(db).await {
        Ok(check) if check.allows_existing_mutation() => {}
        _ => {
            tracing::warn!(
                count = pending.len(),
                "Requested metadata remains pending because historical ownership cannot be indexed"
            );
            return RewritePass {
                fetched: pending.len(),
                failed: pending.len(),
                ..RewritePass::default()
            };
        }
    }

    let needs_groupings = metadata_flags.uses_xmp_groupings()
        || pending.iter().any(|row| {
            row.selection_receipt.as_ref().is_some_and(|receipt| {
                receipt.metadata_flags
                    & (MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR).bits()
                    != 0
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
    let mut retired_from_selected_queue = 0usize;
    for (idx, pending_rewrite) in pending.into_iter().enumerate() {
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
            if !cfg!(feature = "xmp") && flags.uses_xmp_groupings() {
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
            && pending_rewrite
                .selection_receipt
                .as_ref()
                .is_none_or(|receipt| receipt.frozen_groupings.is_none())
            && grouping_read_failures.contains(record.library.as_ref())
        {
            errored += 1;
            continue;
        }
        let Some(local_path) = record.local_path.as_deref() else {
            continue;
        };
        let path = PathBuf::from(local_path);
        let namespace = match archive_root {
            Some(root) => match crate::fs_util::ConfinedPath::open(
                root,
                &path,
                crate::fs_util::ConfinedParents::Existing,
            ) {
                Ok(namespace) => Some(namespace),
                Err(error) => {
                    tracing::warn!(target: "kei::download::metadata_rewrite", %error,
                        "Metadata destination namespace changed; retaining retry");
                    errored += 1;
                    continue;
                }
            },
            None => None,
        };
        #[cfg(not(feature = "xmp"))]
        let _ = &namespace; // Native-only retries still validate the destination namespace.
        let _layout_guard =
            match crate::download::file::lock_download_destination(&path, shutdown_token).await {
                Ok(guard) => guard,
                Err(_) => {
                    errored += 1;
                    continue;
                }
            };
        #[cfg(all(test, feature = "xmp"))]
        if super::take_test_writer_denial(&path) {
            deferred += 1;
            continue;
        }
        if db.validate_metadata_writer(&pending_rewrite).await.is_err() {
            deferred += 1;
            continue;
        }
        if db.guard_primary_writer(&path).await.is_err() {
            deferred += 1;
            continue;
        }

        if let Err(error) = async {
            let retained = namespace
                .as_ref()
                .map(|namespace| namespace.sibling(namespace.path()))
                .transpose()?;
            crate::download::file::recover_metadata_replacements_in_namespace(&path, retained).await
        }
        .await
        {
            tracing::warn!(path = %path.display(), error = %format!("{error:#}"),
                "Could not recover metadata replacement; leaving marker for retry");
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
            if let Some((albums, people)) = pending_rewrite
                .selection_receipt
                .as_ref()
                .and_then(|receipt| receipt.frozen_groupings.as_ref())
            {
                MetadataPayload::from_metadata(&record.metadata)
                    .with_asset_groupings(albums, people)
            } else {
                groupings_by_library
                    .get(record.library.as_ref())
                    .map_or_else(
                        || MetadataPayload::from_metadata(&record.metadata),
                        |groupings| groupings.metadata_payload(&record.id, &record.metadata),
                    )
            },
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
        let pre_rewrite_fingerprint = if metadata_flags.any_embed()
            || pending_rewrite.selection_receipt.is_some()
        {
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
            && !recovered_selection_output;
        if drifted {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                asset_id = %record.id,
                path = %path.display(),
                "On-disk file does not match its recorded checksum; leaving the media and the \
                 marker alone. Run `kei verify --checksums` or `kei reconcile`"
            );
            skipped_drifted += 1;
            if pending_rewrite.selection_receipt.is_some() {
                continue;
            }
        }

        let mut outcome = MetadataWriteOutcome::default();
        let mut pending_for_finish = pending_rewrite.clone();
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
                                Ok(None) | Err(_) => {
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
            let eligible = !metadata_flags.any_embed()
                && selected_queue == MetadataRewriteQueue::Ordinary
                && pending_rewrite.capture_repair_receipt.is_none()
                && pending_rewrite
                    .selection_receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.prepared.is_none());
            let mut duplicate = false;
            if eligible && let Some(media) = pre_rewrite_fingerprint {
                for attempt in &attempts.blocked {
                    if pending_rewrite.selection_receipt.as_ref() != Some(&attempt.receipt) {
                        continue;
                    }
                    let attempt = Arc::clone(attempt);
                    let row = pending_rewrite.clone();
                    if tokio::task::spawn_blocking(move || attempt.matches(&row, media))
                        .await
                        .unwrap_or(false)
                    {
                        duplicate = true;
                        break;
                    }
                }
            }
            if duplicate {
                #[cfg(test)]
                record_test_suppression(&path);
                continue;
            }
            let blocked = if eligible {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    Some((snapshot_path(&path)?, blocked_target(&path)?))
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };
            let result = Box::pin(write_sidecar_metadata_observed(
                &path,
                namespace,
                false,
                Arc::clone(&payload),
                created_local,
                pending_rewrite.source_checksum.as_deref(),
                &temp_suffix,
            ))
            .await;
            outcome.sidecar_failed = !result.success;
            if result.failed_sidecar_attempt.is_some()
                && let (Some((media_target, target)), Some(media), Some(receipt), Ok(asset)) = (
                    blocked,
                    pre_rewrite_fingerprint,
                    pending_rewrite.selection_receipt.clone(),
                    serde_json::to_value(&pending_rewrite.asset),
                )
            {
                let attempt = BlockedAttempt {
                    receipt,
                    asset,
                    source_checksum: pending_rewrite.source_checksum.clone(),
                    media,
                    media_target,
                    target,
                };
                let attempt = Arc::new(attempt);
                let check = Arc::clone(&attempt);
                let row = pending_rewrite.clone();
                if tokio::task::spawn_blocking(move || check.matches(&row, media))
                    .await
                    .unwrap_or(false)
                {
                    attempts.blocked.push(attempt);
                }
            }
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
        // A capture repair that changes bytes must hold the verified output
        // across its state transaction, so no receipt advances for bytes that
        // were replaced after the writer measured them. Without that proof the
        // rewrite stays durable debt.
        let validate_output: Option<
            Box<dyn FnOnce() -> Result<(), crate::state::error::StateError> + Send>,
        > = if selected_queue == MetadataRewriteQueue::CaptureRepair
            && completion.clears(MetadataRewriteQueue::CaptureRepair)
            && media_checksum_changed
        {
            let Some((root, expected)) = archive_root.zip(final_fingerprint) else {
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
                    asset_id = %record.id,
                    path = %path.display(),
                    "Capture repair has no confined root for its verified output; retaining the rewrite"
                );
                errored += 1;
                continue;
            };
            match crate::download::file::retain_pending_file(root, &path).await {
                Ok(Some(retained)) => {
                    if retained.fingerprint != expected {
                        tracing::warn!(
                            target: "kei::download::metadata_rewrite",
                            asset_id = %record.id,
                            path = %path.display(),
                            "Capture-repair output changed before finalisation; retaining the rewrite"
                        );
                        errored += 1;
                        continue;
                    }
                    Some(Box::new(move || {
                        retained.validate_blocking().map_err(|_changed| {
                            crate::state::error::StateError::Invariant {
                                operation: "finish_metadata_rewrite",
                                detail:
                                    "the verified capture-repair output changed before finalisation"
                                        .into(),
                            }
                        })
                    }))
                }
                Ok(None) => {
                    tracing::warn!(
                        target: "kei::download::metadata_rewrite",
                        asset_id = %record.id,
                        path = %path.display(),
                        "Capture-repair output disappeared before finalisation; retaining the rewrite"
                    );
                    errored += 1;
                    continue;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "kei::download::metadata_rewrite",
                        asset_id = %record.id,
                        path = %path.display(),
                        %error,
                        "Could not retain the capture-repair output; retaining the rewrite"
                    );
                    errored += 1;
                    continue;
                }
            }
        } else {
            None
        };
        if completion != MetadataRewriteCompletion::None || media_checksum_changed {
            match db
                .finish_metadata_rewrite_checked(
                    &pending_for_finish,
                    selected_queue,
                    final_checksum.as_deref(),
                    input_checksum_for_download,
                    completion,
                    validate_output,
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
        fetched: pending_count,
        applied,
        failed: errored + deferred + skipped_drifted + skipped_unverified,
        retired_from_selected_queue,
    }
}

#[cfg(test)]
mod tests;
