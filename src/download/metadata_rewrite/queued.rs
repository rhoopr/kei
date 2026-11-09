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

// Test-only causality witness for the authenticated queued-writer proof. It
// records the exact resolved request at the sidecar writer call and the typed
// failure boundary afterwards. Nothing here is compiled into production.
#[cfg(all(test, feature = "xmp", unix))]
static TEST_WRITER_ATTEMPT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, feature = "xmp", unix))]
fn test_file_identity(meta: &std::fs::Metadata) -> serde_json::Value {
    use std::os::unix::fs::MetadataExt as _;
    serde_json::json!({
        "device": meta.dev(),
        "inode": meta.ino(),
        "mode": meta.mode(),
        "size": meta.len(),
        "mtime": [meta.mtime(), meta.mtime_nsec()],
        "ctime": [meta.ctime(), meta.ctime_nsec()],
    })
}

#[cfg(all(test, feature = "xmp", unix))]
fn test_media_evidence(path: &std::path::Path) -> serde_json::Value {
    use sha2::Digest as _;
    let bytes = std::fs::read(path);
    let meta = std::fs::metadata(path);
    let parent = path.parent().map(std::fs::metadata).transpose();
    match (bytes, meta, parent) {
        (Ok(bytes), Ok(meta), Ok(parent)) => {
            let mut value = test_file_identity(&meta);
            if let Some(map) = value.as_object_mut() {
                map.insert("kind".into(), serde_json::json!("file"));
                map.insert(
                    "sha256".into(),
                    serde_json::json!(
                        data_encoding::HEXLOWER.encode(&sha2::Sha256::digest(&bytes))
                    ),
                );
                map.insert(
                    "parent".into(),
                    parent
                        .as_ref()
                        .map_or(serde_json::Value::Null, test_file_identity),
                );
            }
            value
        }
        _ => serde_json::json!({"kind": "unreadable"}),
    }
}

#[cfg(all(test, feature = "xmp", unix))]
fn test_entry_evidence(path: &std::path::Path) -> serde_json::Value {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            let mut value = test_file_identity(&meta);
            if let Some(map) = value.as_object_mut() {
                map.insert("exists".into(), serde_json::json!(true));
                map.insert(
                    "kind".into(),
                    serde_json::json!(if meta.is_dir() {
                        "directory"
                    } else if meta.is_file() {
                        "file"
                    } else {
                        "other"
                    }),
                );
            }
            value
        }
        Err(_) => serde_json::json!({"exists": false, "kind": "missing"}),
    }
}

#[cfg(all(test, feature = "xmp", unix))]
#[allow(
    clippy::too_many_arguments,
    reason = "one call site records the whole resolved writer request"
)]
fn observe_test_writer_request(
    record: &crate::state::AssetRecord,
    pending: &crate::state::db::PendingMetadataRewrite,
    path: &std::path::Path,
    archive_root: Option<&std::path::Path>,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    selected_queue: MetadataRewriteQueue,
    payload: &crate::download::filter::MetadataPayload,
    created_local: chrono::DateTime<chrono::FixedOffset>,
    temp_suffix: &str,
) -> usize {
    let attempt = TEST_WRITER_ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let sidecar_path = path.file_name().map(|name| {
        let mut name = name.to_os_string();
        name.push(".xmp");
        path.with_file_name(name)
    });
    let request = serde_json::json!({
        "attempt": attempt,
        "library": record.library.as_ref(),
        "asset_id": record.id.as_ref(),
        "rendition": record.version_size.as_str(),
        "status": record.status.as_str(),
        "provider_checksum": record.checksum.as_ref(),
        "local_checksum": record.local_checksum,
        "download_checksum": record.download_checksum,
        "source_checksum": pending.source_checksum,
        "recorded_path": record.local_path.as_ref().map(|path| path.display().to_string()),
        "resolved_path": path.display().to_string(),
        "created_local": created_local.to_rfc3339(),
        "effective_flags": metadata_flags.bits(),
        "capture_policy": format!("{capture_timestamp_repair:?}"),
        "queue": format!("{selected_queue:?}"),
        "archive_root": archive_root.map(|path| path.display().to_string()),
        "temp_suffix": temp_suffix,
        "payload": serde_json::to_value(payload).unwrap_or(serde_json::Value::Null),
        "album_groupings": payload.keywords,
        "people_groupings": payload.people,
        "selection_receipt": pending.selection_receipt.is_some(),
        "capture_repair_receipt": pending.capture_repair_receipt.is_some(),
        "asset": serde_json::to_value(&pending.asset).unwrap_or(serde_json::Value::Null),
        "media": test_media_evidence(path),
        "sidecar_path": sidecar_path.as_ref().map(|path| path.display().to_string()),
        "sidecar_entry": sidecar_path.as_deref().map(test_entry_evidence),
    });
    tracing::info!(
        diagnostic = "test_queued_writer_request_v1",
        test_writer_fields = %request,
        "queued writer request witness"
    );
    attempt
}

#[cfg(all(test, feature = "xmp", unix))]
fn observe_test_writer_outcome(
    attempt: usize,
    success: bool,
    boundary: Option<crate::download::metadata::SidecarFailureBoundary>,
) {
    tracing::info!(
        diagnostic = "test_queued_writer_outcome_v1",
        test_writer_fields = %serde_json::json!({
            "attempt": attempt,
            "success": success,
            "failed_sidecar_attempt": boundary.map(|boundary| format!("{boundary:?}")),
        }),
        "queued writer outcome witness"
    );
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
    #[cfg_attr(
        not(all(feature = "xmp", unix)),
        allow(dead_code, reason = "the sidecar witness requires xmp and Unix")
    )]
    witness: Option<Arc<MetadataWriterWitness>>,
    #[cfg(all(test, feature = "xmp", unix))]
    test_capture_offset: Option<chrono::FixedOffset>,
    #[cfg(all(test, feature = "xmp", unix))]
    test_writer_times: Vec<chrono::DateTime<chrono::FixedOffset>>,
}

impl TailAttempts {
    pub(in crate::download) fn with_witness(witness: Option<Arc<MetadataWriterWitness>>) -> Self {
        Self {
            #[cfg(feature = "xmp")]
            blocked: Vec::new(),
            witness,
            #[cfg(all(test, feature = "xmp", unix))]
            test_capture_offset: None,
            #[cfg(all(test, feature = "xmp", unix))]
            test_writer_times: Vec::new(),
        }
    }
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Debug, PartialEq, Eq)]
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
#[derive(Debug, PartialEq, Eq)]
enum TargetValue {
    Missing,
    File(MetadataStamp, [u8; 32]),
    Directory(MetadataStamp),
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Debug)]
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
fn sidecar_path(path: &std::path::Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".xmp");
    Some(path.with_file_name(name))
}

#[cfg(all(feature = "xmp", unix))]
async fn sidecar_is_directory(path: &std::path::Path) -> bool {
    let Some(sidecar) = sidecar_path(path) else {
        return false;
    };
    tokio::fs::symlink_metadata(sidecar)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_dir())
}

#[cfg(all(feature = "xmp", unix))]
fn blocked_target(path: &std::path::Path) -> Option<TargetSnapshot> {
    snapshot_path(&sidecar_path(path)?)
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

    fn retained_heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.target.retained_heap_bytes())
            .saturating_add(WITNESS_ALLOC_OVERHEAD)
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

/// One receiptless Ordinary sidecar request, retained only for the lifetime
/// of one composed operation. The payload is the exact effective payload used
/// by the attempt; no durable identity or timestamp is stored.
#[cfg(all(feature = "xmp", unix))]
#[derive(Clone, Debug, PartialEq)]
struct LegacyRequest {
    library: Arc<str>,
    asset_id: Box<str>,
    version_size: crate::state::VersionSizeKey,
    provider_checksum: Box<str>,
    recorded_path: PathBuf,
    created_local: chrono::DateTime<chrono::FixedOffset>,
    created_offset: chrono::FixedOffset,
    local_checksum: Option<String>,
    download_checksum: Option<String>,
    source_checksum: Option<String>,
    metadata_flags: crate::download::pipeline::MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    archive_root: Option<PathBuf>,
    temp_suffix: Arc<str>,
    payload: Arc<crate::download::filter::MetadataPayload>,
}

#[cfg(all(feature = "xmp", unix))]
impl LegacyRequest {
    fn retained_heap_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.library.len())
            .saturating_add(self.asset_id.len())
            .saturating_add(self.provider_checksum.len())
            .saturating_add(self.recorded_path.capacity())
            .saturating_add(optional_string_capacity(&self.local_checksum))
            .saturating_add(optional_string_capacity(&self.download_checksum))
            .saturating_add(optional_string_capacity(&self.source_checksum))
            .saturating_add(
                self.archive_root
                    .as_ref()
                    .map_or(0, std::path::PathBuf::capacity),
            )
            .saturating_add(self.temp_suffix.len())
            .saturating_add(payload_retained_heap_bytes(&self.payload))
    }
}

#[cfg(all(feature = "xmp", unix))]
fn estimated_legacy_heap(
    record: &crate::state::AssetRecord,
    payload: &crate::download::filter::MetadataPayload,
    archive_root: Option<&std::path::Path>,
    temp_suffix: &str,
    path: &std::path::Path,
) -> usize {
    std::mem::size_of::<LegacyRequest>()
        .saturating_add(record.library.len())
        .saturating_add(record.id.len())
        .saturating_add(record.checksum.len())
        .saturating_add(path.as_os_str().len())
        .saturating_add(optional_string_capacity(&record.local_checksum))
        .saturating_add(optional_string_capacity(&record.download_checksum))
        .saturating_add(archive_root.map_or(0, |path| path.as_os_str().len()))
        .saturating_add(temp_suffix.len())
        .saturating_add(payload_retained_heap_bytes(payload))
}

#[cfg(all(feature = "xmp", unix))]
fn optional_string_capacity(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::capacity)
}

#[cfg(all(feature = "xmp", unix))]
fn payload_retained_heap_bytes(payload: &crate::download::filter::MetadataPayload) -> usize {
    std::mem::size_of::<crate::download::filter::MetadataPayload>()
        .saturating_add(optional_string_capacity(&payload.title))
        .saturating_add(optional_string_capacity(&payload.description))
        .saturating_add(optional_string_capacity(&payload.media_subtype))
        .saturating_add(optional_string_capacity(&payload.burst_id))
        .saturating_add(
            payload
                .keywords
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
        )
        .saturating_add(
            payload
                .people
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
        )
        .saturating_add(payload.keywords.iter().map(String::capacity).sum::<usize>())
        .saturating_add(payload.people.iter().map(String::capacity).sum::<usize>())
}

#[cfg(all(feature = "xmp", unix))]
struct BlockedLegacySidecar {
    request: LegacyRequest,
    media: TargetSnapshot,
    sidecar: TargetSnapshot,
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Default)]
struct WitnessState {
    entries: Vec<Arc<BlockedLegacySidecar>>,
    heap_used: usize,
}

#[cfg(all(feature = "xmp", unix))]
#[derive(Clone, Copy)]
struct WitnessBudget {
    fd_limit: usize,
    heap_limit: usize,
}

#[cfg(all(feature = "xmp", unix))]
const WITNESS_FD_RESERVE: usize = 64;
#[cfg(all(feature = "xmp", unix))]
const WITNESS_FD_CEILING: usize = 4096;
#[cfg(all(feature = "xmp", unix))]
const WITNESS_FDS_PER_SNAPSHOT: usize = 3;
#[cfg(all(feature = "xmp", unix))]
const WITNESS_FDS_PER_ENTRY: usize = 2 * WITNESS_FDS_PER_SNAPSHOT;
#[cfg(all(feature = "xmp", unix))]
const WITNESS_HEAP_BUDGET: usize = 8 * 1024 * 1024;
#[cfg(all(feature = "xmp", unix))]
const WITNESS_HEAP_RESERVE: usize = 256 * 1024;
#[cfg(all(feature = "xmp", unix))]
const WITNESS_ALLOC_OVERHEAD: usize = 256;

/// A transient, writer-owned witness for one composed operation. It is
/// deliberately non-durable: the whole value is dropped at the operation
/// boundary, releasing every retained descriptor and allocation.
#[derive(Default)]
pub(crate) struct MetadataWriterWitness {
    #[cfg(all(feature = "xmp", unix))]
    budget: std::sync::OnceLock<Option<WitnessBudget>>,
    #[cfg(all(feature = "xmp", unix))]
    state: std::sync::Mutex<WitnessState>,
    #[cfg(all(test, feature = "xmp", unix))]
    test_suppress_calls: std::sync::atomic::AtomicUsize,
    #[cfg(all(test, feature = "xmp", unix))]
    test_suppress_matches: std::sync::atomic::AtomicUsize,
    #[cfg(all(test, feature = "xmp", unix))]
    test_suppress_unchanged: std::sync::atomic::AtomicUsize,
}

#[cfg(all(feature = "xmp", unix))]
impl MetadataWriterWitness {
    fn budget(&self) -> Option<&WitnessBudget> {
        self.budget.get_or_init(WitnessBudget::current).as_ref()
    }

    /// Cheap preflight before cloning the owned request or hashing media.
    /// The exact heap check follows after snapshots are captured.
    fn can_attempt_estimate(&self, heap_lower_bound: usize) -> bool {
        let Some(budget) = self.budget() else {
            return false;
        };
        let Some(open) = count_open_fds() else {
            return false;
        };
        if open
            .saturating_add(WITNESS_FD_RESERVE)
            .saturating_add(WITNESS_FDS_PER_ENTRY)
            > budget.fd_limit
        {
            return false;
        }
        let Ok(state) = self.state.lock() else {
            return false;
        };
        state
            .heap_used
            .saturating_add(heap_lower_bound)
            .saturating_add(2 * std::mem::size_of::<TargetSnapshot>())
            .saturating_add(WITNESS_HEAP_RESERVE)
            <= budget.heap_limit
    }

    fn admit(
        &self,
        request: LegacyRequest,
        media: TargetSnapshot,
        sidecar: TargetSnapshot,
    ) -> bool {
        let Some(budget) = self.budget() else {
            return false;
        };
        let Some(open) = count_open_fds() else {
            return false;
        };
        if open
            .saturating_add(WITNESS_FD_RESERVE)
            .saturating_add(WITNESS_FDS_PER_ENTRY)
            > budget.fd_limit
        {
            return false;
        }
        let heap = request
            .retained_heap_bytes()
            .saturating_add(media.retained_heap_bytes())
            .saturating_add(sidecar.retained_heap_bytes())
            .saturating_add(WITNESS_ALLOC_OVERHEAD);
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        if state
            .heap_used
            .saturating_add(heap)
            .saturating_add(WITNESS_HEAP_RESERVE)
            > budget.heap_limit
        {
            return false;
        }
        state.heap_used = state.heap_used.saturating_add(heap);
        state.entries.push(Arc::new(BlockedLegacySidecar {
            request,
            media,
            sidecar,
        }));
        true
    }

    async fn suppresses(&self, request: &LegacyRequest) -> bool {
        #[cfg(test)]
        self.test_suppress_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let entry = {
            let Ok(state) = self.state.lock() else {
                return false;
            };
            state
                .entries
                .iter()
                .find(|entry| entry.request == *request)
                .cloned()
        };
        let Some(entry) = entry else {
            return false;
        };
        #[cfg(test)]
        self.test_suppress_matches
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let unchanged = tokio::task::spawn_blocking(move || {
            entry.media.unchanged() && entry.sidecar.unchanged()
        })
        .await
        .unwrap_or(false);
        #[cfg(test)]
        if unchanged {
            self.test_suppress_unchanged
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        unchanged
    }

    #[cfg(test)]
    pub(crate) fn test_suppression_counts(&self) -> (usize, usize, usize) {
        (
            self.test_suppress_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            self.test_suppress_matches
                .load(std::sync::atomic::Ordering::Relaxed),
            self.test_suppress_unchanged
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

#[cfg(all(feature = "xmp", unix))]
impl WitnessBudget {
    fn current() -> Option<Self> {
        let limit = process_fd_limit()?;
        let limit = usize::try_from(limit).ok()?.min(WITNESS_FD_CEILING);
        if count_open_fds()?.saturating_add(WITNESS_FD_RESERVE) >= limit {
            return None;
        }
        Some(Self {
            fd_limit: limit,
            heap_limit: WITNESS_HEAP_BUDGET,
        })
    }
}

#[cfg(all(feature = "xmp", unix))]
fn process_fd_limit() -> Option<u64> {
    let mut limits = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limits` is a valid, properly aligned, initialized libc::rlimit.
    // getrlimit writes only through that pointer, the return code is checked
    // before reading, and no Rust reference aliases it during the call. This
    // code never calls setrlimit and never raises a process limit.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) };
    if rc != 0 {
        return None;
    }
    if limits.rlim_cur == libc::RLIM_INFINITY {
        return u64::try_from(WITNESS_FD_CEILING).ok();
    }
    Some(limits.rlim_cur)
}

#[cfg(all(feature = "xmp", unix))]
fn count_open_fds() -> Option<usize> {
    #[cfg(target_os = "linux")]
    const FD_DIR: &str = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    const FD_DIR: &str = "/dev/fd";
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return None;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let entries = std::fs::read_dir(FD_DIR).ok()?;
        let mut count = 1usize; // the directory handle held during iteration
        for entry in entries {
            entry.ok()?;
            count = count.saturating_add(1);
        }
        Some(count)
    }
}

#[cfg(not(all(feature = "xmp", unix)))]
impl MetadataWriterWitness {
    #[cfg_attr(
        not(all(feature = "xmp", unix)),
        allow(dead_code, reason = "the sidecar witness requires xmp and Unix")
    )]
    pub(crate) fn supports_legacy_witness(&self) -> bool {
        false
    }
}

#[cfg(all(feature = "xmp", unix))]
impl MetadataWriterWitness {
    pub(crate) fn supports_legacy_witness(&self) -> bool {
        // Uninitialized budgets are probed only in the blocking capture job.
        self.budget.get().is_none_or(Option::is_some)
    }

    #[cfg(test)]
    pub(crate) fn with_test_budget(fd_limit: usize, heap_limit: usize) -> Self {
        let witness = Self::default();
        let _ = witness.budget.set(Some(WitnessBudget {
            fd_limit,
            heap_limit,
        }));
        witness
    }

    #[cfg(test)]
    pub(crate) fn test_retained(&self) -> (usize, usize) {
        let state = self.state.lock().expect("witness state");
        (state.entries.len(), state.heap_used)
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
    pub(in crate::download) witnessed_blocked: usize,
}

/// Process one bounded batch of persisted metadata-rewrite markers: for each
/// asset whose `metadata_write_failed_at` is set and whose local file is still
/// on disk, re-apply EXIF/XMP using the stored metadata. On success clears the
/// marker; on failure leaves it so the next pass retries. Returns the
/// per-batch counts.
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "tests retain the direct bounded drain entry")
)]
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
pub(in crate::download) async fn run_pending_witnessed<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    witness: Option<Arc<MetadataWriterWitness>>,
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + crate::state::ReconciliationStateStore + ?Sized,
{
    let mut attempts = TailAttempts::with_witness(witness);
    run_pending_budget_tracked(
        db,
        metadata_flags,
        CaptureTimestampRepair::Preserve,
        temp_suffix,
        shutdown_token,
        None,
        None,
        (0, METADATA_REWRITE_BATCH, &mut attempts),
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the existing page drain shape plus the transient witness"
)]
pub(in crate::download) async fn run_pending_page_witnessed<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    root: Option<&std::path::Path>,
    library_scope: Option<&[&str]>,
    offset: usize,
    witness: Option<Arc<MetadataWriterWitness>>,
) -> super::RewritePass
where
    D: MembershipStore + MetadataRewriteStore + crate::state::ReconciliationStateStore + ?Sized,
{
    let mut attempts = TailAttempts::with_witness(witness);
    run_pending_budget_tracked(
        db,
        metadata_flags,
        capture_timestamp_repair,
        temp_suffix,
        shutdown_token,
        root,
        library_scope,
        (offset, METADATA_REWRITE_BATCH, &mut attempts),
    )
    .await
}

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "tests retain the page drain entry")
)]
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

#[cfg_attr(
    not(test),
    allow(dead_code, reason = "tests retain the bounded budget entry")
)]
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
    #[allow(unused_mut, reason = "incremented only by the xmp sidecar witness")]
    let mut witnessed_blocked = 0usize;
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
        #[cfg(all(test, feature = "xmp", unix))]
        let created_local = attempts
            .test_capture_offset
            .map_or(created_local, |offset| created_local.with_timezone(&offset));

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
            #[cfg(all(feature = "xmp", unix))]
            let legacy_eligible = selected_queue == MetadataRewriteQueue::Ordinary
                && pending_rewrite.selection_receipt.is_none()
                && pending_rewrite.capture_repair_receipt.is_none()
                && metadata_flags == MetadataFlags::XMP_SIDECAR
                && matches!(capture_timestamp_repair, CaptureTimestampRepair::Preserve)
                && archive_root.is_none();
            #[cfg(all(feature = "xmp", unix))]
            let legacy_witness = attempts.witness.clone();
            #[cfg(all(feature = "xmp", unix))]
            let legacy_request = if legacy_eligible
                && legacy_witness
                    .as_ref()
                    .is_some_and(|witness| witness.supports_legacy_witness())
                && sidecar_is_directory(&path).await
            {
                Some(LegacyRequest {
                    library: Arc::clone(&record.library),
                    asset_id: record.id.clone(),
                    version_size: record.version_size,
                    provider_checksum: record.checksum.clone(),
                    recorded_path: path.clone(),
                    created_local,
                    created_offset: *created_local.offset(),
                    local_checksum: record.local_checksum.clone(),
                    download_checksum: record.download_checksum.clone(),
                    source_checksum: pending_rewrite.source_checksum.clone(),
                    metadata_flags,
                    capture_timestamp_repair,
                    archive_root: archive_root.map(std::path::Path::to_path_buf),
                    temp_suffix: Arc::clone(&temp_suffix),
                    payload: Arc::clone(&payload),
                })
            } else {
                None
            };
            #[cfg(all(feature = "xmp", unix))]
            if let (Some(witness), Some(request)) =
                (legacy_witness.as_ref(), legacy_request.as_ref())
                && witness.suppresses(request).await
            {
                #[cfg(test)]
                record_test_suppression(&path);
                witnessed_blocked = witnessed_blocked.saturating_add(1);
                continue;
            }
            #[cfg(all(feature = "xmp", unix))]
            let heap = legacy_request.as_ref().map(|_| {
                estimated_legacy_heap(record, payload.as_ref(), archive_root, &temp_suffix, &path)
            });
            #[cfg(all(feature = "xmp", unix))]
            let legacy_pre = if let (Some(heap), Some(witness)) = (heap, legacy_witness.clone()) {
                let path = path.clone();
                tokio::task::spawn_blocking(move || {
                    if !witness.can_attempt_estimate(heap) {
                        return None;
                    }
                    Some((snapshot_path(&path)?, blocked_target(&path)?))
                })
                .await
                .ok()
                .flatten()
            } else {
                None
            };
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
            #[cfg(all(test, feature = "xmp", unix))]
            if attempts.test_capture_offset.is_some() {
                attempts.test_writer_times.push(created_local);
            }
            #[cfg(all(test, feature = "xmp", unix))]
            let test_writer_attempt = observe_test_writer_request(
                record,
                &pending_rewrite,
                &path,
                archive_root,
                metadata_flags,
                capture_timestamp_repair,
                selected_queue,
                payload.as_ref(),
                created_local,
                temp_suffix.as_ref(),
            );
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
            #[cfg(all(test, feature = "xmp", unix))]
            observe_test_writer_outcome(
                test_writer_attempt,
                result.success,
                result.failed_sidecar_attempt,
            );
            #[cfg(all(feature = "xmp", unix))]
            if result.failed_sidecar_attempt
                == Some(crate::download::metadata::SidecarFailureBoundary::Destination)
                && let (Some(witness), Some(request), Some((media, sidecar))) =
                    (legacy_witness, legacy_request, legacy_pre)
            {
                let _ = tokio::task::spawn_blocking(move || {
                    if media.unchanged() && sidecar.unchanged() {
                        witness.admit(request, media, sidecar);
                    }
                })
                .await;
            }
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
        witnessed_blocked,
    }
}

#[cfg(test)]
mod tests;
