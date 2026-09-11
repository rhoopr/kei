//! Metadata write orchestration for downloaded files and retry markers.
//!
//! The download pipeline owns byte transfer and `.part` promotion. This module
//! owns the opt-in local-file metadata mutation work that can happen around
//! that transfer: embed writes before publish, sidecar writes after publish,
//! metadata-only retry marker tagging, and pending retry marker draining.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use tokio_util::sync::CancellationToken;

use crate::download::filter::MetadataPayload;
use crate::icloud::photos::PhotoAsset;
use crate::state::db::{
    CaptureRepairReceipt, MetadataRewriteCompletion, MetadataRewriteQueue, PendingMetadataRewrite,
};
use crate::state::{MembershipStore, MetadataRewriteStore, VersionSizeKey};

use super::{AssetGroupings, DownloadConfig, DownloadContext};

/// Whether an explicitly requested metadata repair may replace an existing
/// embedded capture timestamp.
#[must_use]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CaptureTimestampRepair {
    #[default]
    Preserve,
    ReplaceWithCaptureLocal,
}

bitflags::bitflags! {
    /// Per-tag write toggles. `any_embed()` drives the `.part`-and-modify-before-rename
    /// flow; individual flags gate which fields get embedded into the media file.
    ///
    /// `EMBED_XMP` enables the XMP-only fields that have no native EXIF equivalent
    /// (title, keywords, people, hidden/archived, media subtype, burst id).
    /// `XMP_SIDECAR` is orthogonal - it writes a `.xmp` file next to the photo
    /// without touching the photo bytes.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub(super) struct MetadataFlags: u8 {
        const DATETIME    = 1 << 0;
        const RATING      = 1 << 1;
        const GPS         = 1 << 2;
        const DESCRIPTION = 1 << 3;
        const EMBED_XMP   = 1 << 4;
        const XMP_SIDECAR = 1 << 5;
    }
}

impl MetadataFlags {
    /// Set of flags that drive the `.part`-and-modify-before-rename flow.
    /// Sidecar writes happen after the rename so `XMP_SIDECAR` is excluded.
    /// Derived as `all() \ XMP_SIDECAR` so any future embed-style flag
    /// added to this type is automatically picked up.
    const EMBED_MASK: Self = Self::all().difference(Self::XMP_SIDECAR);

    /// Whether any flag needs the downloaded bytes to stay as a `.part` file
    /// for in-place metadata editing before the atomic rename.
    pub(super) fn any_embed(self) -> bool {
        self.intersects(Self::EMBED_MASK)
    }

    pub(super) fn has_any_write(self) -> bool {
        !self.is_empty()
    }

    fn uses_xmp_groupings(self) -> bool {
        self.intersects(Self::EMBED_XMP | Self::XMP_SIDECAR)
    }
}

impl From<&DownloadConfig> for MetadataFlags {
    fn from(config: &DownloadConfig) -> Self {
        Self::from(&config.metadata)
    }
}

impl From<&crate::config::MetadataConfig> for MetadataFlags {
    fn from(metadata: &crate::config::MetadataConfig) -> Self {
        let mut flags = Self::empty();
        flags.set(Self::DATETIME, metadata.set_exif_datetime);
        flags.set(Self::RATING, metadata.set_exif_rating);
        flags.set(Self::GPS, metadata.set_exif_gps);
        flags.set(Self::DESCRIPTION, metadata.set_exif_description);
        #[cfg(feature = "xmp")]
        {
            flags.set(Self::EMBED_XMP, metadata.embed_xmp);
            flags.set(Self::XMP_SIDECAR, metadata.xmp_sidecar);
        }
        flags
    }
}

#[must_use]
pub(crate) fn writers_enabled(metadata: &crate::config::MetadataConfig) -> bool {
    MetadataFlags::from(metadata).has_any_write()
}

/// Result of metadata writes attempted for one downloaded file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MetadataWriteOutcome {
    embed_failed: bool,
    embed_input_changed: bool,
    embed_no_write: bool,
    embed_output_fingerprint: Option<super::file::ExistingFileFingerprint>,
    sidecar_failed: bool,
}

impl MetadataWriteOutcome {
    pub(super) fn any_failed(self) -> bool {
        self.embed_failed || self.sidecar_failed
    }
}

/// Request describing metadata work for one file. `embed_path` is the path to
/// mutate in place, usually the `.part` file before promotion. `final_path` is
/// the intended media path and is used for initial format routing. The actual
/// embed path is checked again so its content takes precedence over a
/// misleading final extension. `sidecar_path` is the media path next to which
/// the `.xmp` sidecar should be written. `capture_timestamp_repair` applies
/// only to the embedded path.
pub(super) struct MetadataWriteRequest<'a> {
    pub(super) final_path: &'a Path,
    pub(super) embed_path: Option<&'a Path>,
    pub(super) expected_embed_fingerprint: Option<super::file::ExistingFileFingerprint>,
    /// SHA-256 of the bytes before any kei metadata rewrite. Missing evidence
    /// permits provider metadata, but not a native horizontal accuracy claim.
    #[cfg_attr(
        not(feature = "xmp"),
        allow(dead_code, reason = "native-only builds do not write sidecars")
    )]
    pub(super) source_checksum: Option<&'a str>,
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(super) sidecar_path: Option<&'a Path>,
    pub(super) payload: Arc<MetadataPayload>,
    pub(super) created_local: DateTime<FixedOffset>,
    pub(super) flags: MetadataFlags,
    pub(super) capture_timestamp_repair: CaptureTimestampRepair,
    pub(super) temp_suffix: &'a str,
}

/// Apply opt-in metadata writes for a single file.
///
/// The caller remains responsible for transfer, mtime, `.part` promotion,
/// counters, and final state writes.
pub(super) async fn write_download_metadata(
    request: MetadataWriteRequest<'_>,
) -> MetadataWriteOutcome {
    // CONTRACT: METADATA_WRITES_REQUIRE_OPT_IN
    let mut outcome = MetadataWriteOutcome::default();

    if request.flags.any_embed()
        && let Some(embed_path) = request.embed_path
        && (request.expected_embed_fingerprint.is_some()
            || (super::metadata::is_embed_writable_path(request.final_path)
                && (embed_path == request.final_path
                    || super::metadata::is_embed_writable_path(embed_path))))
    {
        match write_embed_metadata(
            embed_path,
            request.expected_embed_fingerprint,
            Arc::clone(&request.payload),
            request.created_local,
            request.flags,
            request.capture_timestamp_repair,
            request.temp_suffix,
        )
        .await
        {
            EmbedWriteResult::Applied(output_fingerprint) => {
                outcome.embed_output_fingerprint = output_fingerprint;
            }
            EmbedWriteResult::NoWrite => outcome.embed_no_write = true,
            EmbedWriteResult::Failed => outcome.embed_failed = true,
            EmbedWriteResult::InputChanged => {
                outcome.embed_failed = true;
                outcome.embed_input_changed = true;
            }
        }
    }

    #[cfg(feature = "xmp")]
    if request.flags.contains(MetadataFlags::XMP_SIDECAR)
        && let Some(sidecar_path) = request.sidecar_path
    {
        outcome.sidecar_failed = !write_sidecar_metadata(
            sidecar_path,
            Arc::clone(&request.payload),
            request.created_local,
            request.source_checksum,
            request.temp_suffix,
        )
        .await;
    }

    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbedWriteResult {
    Applied(Option<super::file::ExistingFileFingerprint>),
    NoWrite,
    Failed,
    InputChanged,
}

enum EmbedPrepareResult {
    Prepared(super::metadata::PreparedMetadataFile),
    NoWrite,
    Failed,
    InputChanged,
}

async fn prepare_embed_metadata(
    path: &Path,
    expected_fingerprint: Option<super::file::ExistingFileFingerprint>,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: &str,
) -> EmbedPrepareResult {
    let embed_path = path.to_path_buf();
    let metadata_temp_suffix = temp_suffix.to_string();
    match tokio::task::spawn_blocking(move || {
        let repair_requested = matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        );
        let repair_offset = if repair_requested {
            offset_time_original(&payload)
        } else {
            None
        };
        if repair_requested && repair_offset.is_none() {
            tracing::warn!(
                path = %embed_path.display(),
                "Capture timestamp repair has no usable offset; leaving the rewrite marker pending"
            );
            return EmbedPrepareResult::Failed;
        }
        let probe = match super::metadata::probe_exif(&embed_path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(path = %embed_path.display(), error = %e, "Failed to read EXIF");
                super::metadata::ExifProbe::default()
            }
        };
        let mut write = plan_metadata_write_with_repair(
            flags,
            &payload,
            &created_local,
            capture_timestamp_repair,
            &probe,
        );
        if write.require_native_heif_capture_time {
            let Some(expected_offset) = repair_offset else {
                tracing::warn!(
                    path = %embed_path.display(),
                    "Capture timestamp repair lost its validated offset; leaving the rewrite marker pending"
                );
                return EmbedPrepareResult::Failed;
            };
            match probe
                .native_heif_capture_time_repair_required(&created_local, &expected_offset)
            {
                Ok(Some(_)) => {
                    write.datetime =
                        Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string());
                    write.offset_time_original = Some(expected_offset);
                    write.clear_datetime_offsets = probe.has_any_datetime_offset();
                }
                Ok(None) => {}
                Err(reason) => {
                    tracing::warn!(
                        path = %embed_path.display(),
                        reason,
                        "Cannot safely repair the native HEIF capture timestamp; leaving the rewrite marker pending"
                    );
                    return EmbedPrepareResult::Failed;
                }
            }
        }
        if write.is_empty() {
            return EmbedPrepareResult::NoWrite;
        }
        match super::metadata::prepare_metadata_with_expected_fingerprint(
            &embed_path,
            &write,
            &metadata_temp_suffix,
            expected_fingerprint,
        ) {
            Err(e) => {
                let disposition = super::file::classify_conditional_publish_error(&e);
                let result = if disposition.target_changed {
                    EmbedPrepareResult::InputChanged
                } else {
                    EmbedPrepareResult::Failed
                };
                tracing::warn!(
                    path = %embed_path.display(),
                    error = %e,
                    retained_paths = ?disposition.retained_paths,
                    "Failed to prepare metadata"
                );
                result
            }
            Ok(prepared) => EmbedPrepareResult::Prepared(prepared),
        }
    })
    .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!(error = %e, "EXIF task panicked");
            EmbedPrepareResult::Failed
        }
    }
}

async fn publish_embed_metadata(
    path: &Path,
    prepared: super::metadata::PreparedMetadataFile,
) -> EmbedWriteResult {
    let embed_path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || prepared.publish(&embed_path)).await {
        Ok(Ok(output_fingerprint)) => EmbedWriteResult::Applied(Some(output_fingerprint)),
        Ok(Err(error)) => {
            let disposition = super::file::classify_conditional_publish_error(&error);
            let result = if disposition.target_changed {
                EmbedWriteResult::InputChanged
            } else {
                EmbedWriteResult::Failed
            };
            tracing::warn!(
                path = %path.display(),
                error = %error,
                retained_paths = ?disposition.retained_paths,
                "Failed to publish metadata"
            );
            result
        }
        Err(error) => {
            tracing::warn!(error = %error, "EXIF publication task panicked");
            EmbedWriteResult::Failed
        }
    }
}

async fn write_embed_metadata(
    path: &Path,
    expected_fingerprint: Option<super::file::ExistingFileFingerprint>,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: &str,
) -> EmbedWriteResult {
    match prepare_embed_metadata(
        path,
        expected_fingerprint,
        payload,
        created_local,
        flags,
        capture_timestamp_repair,
        temp_suffix,
    )
    .await
    {
        EmbedPrepareResult::Prepared(prepared) => publish_embed_metadata(path, prepared).await,
        EmbedPrepareResult::NoWrite => EmbedWriteResult::NoWrite,
        EmbedPrepareResult::Failed => EmbedWriteResult::Failed,
        EmbedPrepareResult::InputChanged => EmbedWriteResult::InputChanged,
    }
}

#[cfg(feature = "xmp")]
async fn write_sidecar_metadata(
    path: &Path,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    source_checksum: Option<&str>,
    temp_suffix: &str,
) -> bool {
    let source_checksum = source_checksum.map(str::to_owned);
    let sidecar_path = path.to_path_buf();
    let log_path = sidecar_path.clone();
    let planned = tokio::task::spawn_blocking(move || {
        plan_sidecar_write(
            &sidecar_path,
            &payload,
            &created_local,
            source_checksum.as_deref(),
        )
    })
    .await;
    let (write, source_error) = match planned {
        Ok(planned) => planned,
        Err(error) => {
            tracing::warn!(error = %error, "XMP sidecar planning task panicked");
            return false;
        }
    };
    if write.is_empty() {
        return source_error.is_none();
    }
    match Box::pin(super::metadata::write_sidecar(path, &write, temp_suffix)).await {
        Ok(()) => {
            let Some(error) = source_error else {
                return true;
            };
            tracing::warn!(
                path = %log_path.display(),
                error = %error,
                "Source GPS read failed after sidecar publication; leaving marker for retry"
            );
            false
        }
        Err(e) => {
            tracing::warn!(path = %log_path.display(), error = %e, "Failed to write XMP sidecar");
            false
        }
    }
}

fn gps_from_payload(payload: &MetadataPayload) -> Option<super::metadata::GpsCoords> {
    match (payload.latitude, payload.longitude) {
        (Some(lat), Some(lng)) => Some(super::metadata::GpsCoords {
            latitude: lat,
            longitude: lng,
            altitude: payload.altitude,
        }),
        _ => None,
    }
}

fn offset_time_original(payload: &MetadataPayload) -> Option<String> {
    let offset = payload.timezone_offset.and_then(FixedOffset::east_opt)?;
    let seconds = offset.local_minus_utc();
    if seconds % 60 != 0 {
        return None;
    }
    let minutes = i64::from(seconds).abs() / 60;
    let sign = if seconds < 0 { '-' } else { '+' };
    Some(format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60))
}

/// Comprehensive snapshot of every field a payload can contribute. Used as
/// the sidecar plan (sidecars are fresh files; no probe gating applies).
/// Source-media GPS facts are read here on every attempt so metadata-only
/// retries do not depend on a reduced durable payload.
#[cfg(feature = "xmp")]
fn plan_sidecar_write(
    path: &Path,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    source_checksum: Option<&str>,
) -> (super::metadata::MetadataWrite, Option<anyhow::Error>) {
    let (source_gps, mut source_error) = match super::metadata::read_source_gps(path) {
        Ok(metadata) => (metadata, None),
        Err(error) => (super::metadata::SourceGpsMetadata::default(), Some(error)),
    };
    // CONTRACT: XMP_GPS_ACCURACY_REQUIRES_MATCHING_LOCATION
    // Exact decoded-coordinate equality is conservative: rounding differences
    // omit accuracy rather than using proximity as evidence of the same fix.
    // I/O failures remain unknown under the shared sidecar ownership policy.
    let accuracy_matches_location =
        source_gps
            .latitude
            .zip(source_gps.longitude)
            .is_some_and(|(latitude, longitude)| {
                payload.latitude == Some(latitude) && payload.longitude == Some(longitude)
            });
    // Current native coordinates may have been inserted by an earlier embed.
    // Reuse the pre-embed checksum, including on retries after a state reopen.
    // A changed or unrecorded source cannot establish native provenance. This
    // conservatively omits accuracy after unrelated embedded edits as well.
    let original_source = if accuracy_matches_location
        && source_gps.horizontal_positioning_error.is_some()
        && let Some(expected) = source_checksum
    {
        match super::file::fingerprint_regular_file_snapshot_blocking(path) {
            Ok(actual) => fingerprint_checksum(actual.fingerprint) == expected,
            Err(error) => {
                source_error = Some(error);
                false
            }
        }
    } else {
        false
    };
    let mut write = super::metadata::MetadataWrite {
        datetime: Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string()),
        offset_time_original: offset_time_original(payload),
        gps_datetime: source_gps.datetime,
        gps_speed: source_gps.speed,
        gps_speed_ref: source_gps.speed_ref,
        gps_h_positioning_error: source_gps
            .horizontal_positioning_error
            .filter(|_| original_source),
        preserve_source_gps: source_error.is_some(),
        rating: payload.rating,
        gps: gps_from_payload(payload),
        is_hidden: payload.is_hidden,
        is_archived: payload.is_archived,
        ..super::metadata::MetadataWrite::default()
    };
    write.title.clone_from(&payload.title);
    write.description.clone_from(&payload.description);
    write.keywords.clone_from(&payload.keywords);
    write.people.clone_from(&payload.people);
    write.media_subtype.clone_from(&payload.media_subtype);
    write.burst_id.clone_from(&payload.burst_id);
    (write, source_error)
}

/// Plan the embed-path write. Per-tag gates:
///
/// - datetime / GPS: only when the flag is on AND the file has no existing
///   value (probe gate preserves camera-supplied data). Explicit capture-time
///   repair may replace an existing datetime only with a usable matching offset.
/// - offset: only alongside a timestamp this pass writes, or one the probe
///   proves already renders the capture-local instant.
/// - rating / description: flag gate only - iCloud is the source of truth.
/// - XMP-only fields (title, keywords, people, hidden/archived,
///   media_subtype, burst_id): gated on the `EMBED_XMP` flag.
#[cfg(test)]
fn plan_metadata_write(
    flags: MetadataFlags,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    probe: &super::metadata::ExifProbe,
) -> super::metadata::MetadataWrite {
    plan_metadata_write_with_repair(
        flags,
        payload,
        created_local,
        CaptureTimestampRepair::Preserve,
        probe,
    )
}

fn plan_metadata_write_with_repair(
    flags: MetadataFlags,
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    capture_timestamp_repair: CaptureTimestampRepair,
    probe: &super::metadata::ExifProbe,
) -> super::metadata::MetadataWrite {
    let mut write = super::metadata::MetadataWrite::default();

    if flags.contains(MetadataFlags::DATETIME) {
        let offset_time_original = offset_time_original(payload);
        write.require_native_heif_capture_time = matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ) && offset_time_original.is_some();
        let replace_existing = matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ) && offset_time_original.as_deref().is_some_and(
            |expected_offset| {
                probe.datetime_original.is_some()
                    && (!probe.denotes_capture_time(created_local)
                        || probe.offset_time_original.as_deref() != Some(expected_offset))
            },
        );
        if probe.datetime_original.is_none() || replace_existing {
            write.datetime = Some(created_local.format("%Y:%m:%d %H:%M:%S").to_string());
            write.clear_datetime_offsets = probe.has_any_datetime_offset();
        }
        // An offset describes one specific timestamp. Attach it only to a
        // timestamp this pass writes, or to one already proven to render the
        // capture-local instant.
        if write.datetime.is_some()
            || (probe.offset_time_original.is_none() && probe.denotes_capture_time(created_local))
        {
            write.offset_time_original = offset_time_original;
        }
    }
    if flags.contains(MetadataFlags::RATING) {
        write.rating = payload.rating;
    }
    if flags.contains(MetadataFlags::GPS) && !probe.has_gps {
        write.gps = gps_from_payload(payload);
    }
    if flags.contains(MetadataFlags::DESCRIPTION) {
        write.description.clone_from(&payload.description);
    }
    #[cfg(feature = "xmp")]
    if flags.contains(MetadataFlags::EMBED_XMP) {
        write.title.clone_from(&payload.title);
        write.keywords.clone_from(&payload.keywords);
        write.people.clone_from(&payload.people);
        write.is_hidden = payload.is_hidden;
        write.is_archived = payload.is_archived;
        write.media_subtype.clone_from(&payload.media_subtype);
        write.burst_id.clone_from(&payload.burst_id);
    }

    write
}

/// Persist a metadata-rewrite marker for each candidate version whose
/// metadata drifted from the stored hash, or that already carries a marker
/// from a prior sync. No-op when metadata writing is off or the state DB
/// is absent.
pub(super) async fn tag_if_needed<D>(
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
    let new_hash = asset.metadata().metadata_hash.as_deref();
    let library = asset.source_zone().unwrap_or(config.library.as_ref());
    for &(vs, _) in candidates {
        if !ctx.needs_metadata_rewrite(library, asset.state_id(), vs, new_hash) {
            continue;
        }
        tracing::info!(
            asset_id = %asset.id(),
            version_size = vs.as_str(),
            "Metadata-only change detected; tagging for rewrite"
        );
        if let Err(e) = db
            .record_metadata_write_failure(library, asset.state_id(), vs.as_str())
            .await
        {
            tracing::warn!(
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
pub(super) struct RewritePass {
    pub(super) fetched: usize,
    pub(super) applied: usize,
    pub(super) failed: usize,
    pub(super) retired_from_selected_queue: usize,
}

fn fingerprint_checksum(fingerprint: super::file::ExistingFileFingerprint) -> String {
    data_encoding::HEXLOWER.encode(&fingerprint.sha256)
}

fn receipt_matches_fingerprint(
    receipt: &CaptureRepairReceipt,
    fingerprint: super::file::ExistingFileFingerprint,
) -> bool {
    matches!(
        receipt,
        CaptureRepairReceipt::Prepared {
            output_checksum,
            output_size,
            ..
        } if *output_size == fingerprint.size
            && output_checksum == &fingerprint_checksum(fingerprint)
    )
}

async fn capture_repair_is_verified(
    path: &Path,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
) -> bool {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Some(expected_offset) = offset_time_original(&payload) else {
            return false;
        };
        let Ok(probe) = super::metadata::probe_exif(&path) else {
            return false;
        };
        if !probe.denotes_capture_time(&created_local)
            || probe.offset_time_original.as_deref() != Some(expected_offset.as_str())
        {
            return false;
        }
        matches!(
            probe.native_heif_capture_time_repair_required(&created_local, &expected_offset),
            Ok(None | Some(false))
        )
    })
    .await
    .unwrap_or(false)
}

/// Process one bounded batch of persisted metadata-rewrite markers: for each
/// asset whose `metadata_write_failed_at` is set and whose local file is still
/// on disk, re-apply EXIF/XMP using the stored metadata. On success clears the
/// marker; on failure leaves it so the next pass retries. Returns the
/// per-batch counts.
pub(super) async fn run_pending<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
) -> RewritePass
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

async fn load_pending_groupings<D>(
    db: &D,
    pending: &[PendingMetadataRewrite],
) -> (HashMap<String, AssetGroupings>, HashSet<String>)
where
    D: MembershipStore + ?Sized,
{
    let mut ids_by_library: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pending in pending {
        let record = &pending.asset;
        ids_by_library
            .entry(record.library.as_ref())
            .or_default()
            .push(record.id.as_ref());
    }

    let mut groupings_by_library = HashMap::with_capacity(ids_by_library.len());
    let mut failed_libraries = HashSet::new();
    for (library, asset_ids) in ids_by_library {
        match db.get_asset_groupings(library, &asset_ids).await {
            Ok(rows) => {
                let mut groupings = AssetGroupings::default();
                for (asset_id, album) in rows.albums {
                    groupings.albums.entry(asset_id).or_default().push(album);
                }
                for (asset_id, person) in rows.people {
                    groupings.people.entry(asset_id).or_default().push(person);
                }
                groupings_by_library.insert(library.to_owned(), groupings);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    library,
                    "Failed to load asset groupings for metadata rewrites; leaving markers for retry"
                );
                failed_libraries.insert(library.to_owned());
            }
        }
    }
    (groupings_by_library, failed_libraries)
}

pub(super) async fn run_pending_page<D>(
    db: &D,
    metadata_flags: MetadataFlags,
    capture_timestamp_repair: CaptureTimestampRepair,
    temp_suffix: Arc<str>,
    shutdown_token: &CancellationToken,
    library_scope: Option<&[&str]>,
    offset: usize,
) -> RewritePass
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
            tracing::warn!(error = %e, "Failed to load pending metadata rewrites");
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
            tracing::info!("Shutdown requested, deferring remaining metadata rewrites");
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
            metadata_flags.any_embed() && super::metadata::is_embed_writable_path(&path);
        if matches!(
            capture_timestamp_repair,
            CaptureTimestampRepair::ReplaceWithCaptureLocal
        ) && !embed_writable
        {
            tracing::warn!(
                path = %path.display(),
                "Capture timestamp repair does not support this embedded format; leaving the file and rewrite marker unchanged"
            );
            errored += 1;
            continue;
        }

        if selected_queue == MetadataRewriteQueue::CaptureRepair {
            let Some(receipt) = pending_rewrite.capture_repair_receipt.as_ref() else {
                tracing::warn!(
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
            match super::file::fingerprint_regular_file(&path).await {
                Ok(fingerprint) => Some(fingerprint),
                Err(e) => {
                    tracing::warn!(
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
                                        path = %path.display(),
                                        "Capture-repair receipt changed before preparation completed"
                                    );
                                    errored += 1;
                                    continue;
                                }
                                Err(error) => {
                                    tracing::warn!(
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
                asset_id = %record.id,
                path = %path.display(),
                "File changed while its metadata rewrite was being prepared; leaving its checksum and rewrite marker unchanged"
            );
        } else if outcome.embed_no_write {
            match (
                pre_rewrite_fingerprint,
                super::file::fingerprint_regular_file(&path).await,
            ) {
                (Some(before), Ok(after)) if after == before => {}
                (_, Ok(_)) => {
                    outcome.embed_failed = true;
                    outcome.embed_input_changed = true;
                    tracing::warn!(
                        asset_id = %record.id,
                        path = %path.display(),
                        "File changed after metadata inspection; leaving its checksum and rewrite marker unchanged"
                    );
                }
                (_, Err(e)) => {
                    outcome.embed_failed = true;
                    outcome.embed_input_changed = true;
                    tracing::warn!(
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
                asset_id = %record.id,
                path = %path.display(),
                "Metadata rewrite made no durable progress"
            );
            errored += 1;
        } else {
            tracing::debug!(path = %path.display(), "Metadata rewrite completed");
        }
    }
    tracing::info!(
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
mod tests {
    #[cfg(feature = "xmp")]
    use std::sync::Arc;

    #[cfg(feature = "xmp")]
    use xmp_toolkit::{XmpMeta, xmp_ns};

    use super::*;
    use chrono::TimeZone;

    /// Capture-local time for an asset whose stored offset is +11:00, which is
    /// what every payload below carries. Production derives this offset and
    /// the written `OffsetTimeOriginal` from that same stored value, so the
    /// two always agree.
    fn now_local() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(39_600)
            .unwrap()
            .with_ymd_and_hms(2024, 6, 15, 10, 0, 0)
            .unwrap()
    }

    #[cfg(feature = "xmp")]
    fn with_capture_xmp(input: &[u8]) -> Vec<u8> {
        XmpMeta::register_namespace("http://cipa.jp/exif/1.0/", "exifEX").unwrap();
        let mut xmp = XmpMeta::new().unwrap();
        xmp.set_property(
            xmp_ns::EXIF,
            "DateTimeOriginal",
            &xmp_toolkit::XmpValue::new("2024-06-15T10:00:00+11:00".to_string()),
        )
        .unwrap();
        xmp.set_property(
            "http://cipa.jp/exif/1.0/",
            "OffsetTimeOriginal",
            &xmp_toolkit::XmpValue::new("+11:00".to_string()),
        )
        .unwrap();
        let mut output = Vec::new();
        crate::download::heif::rewrite_xmp(input, xmp.to_string().as_bytes(), &mut output).unwrap();
        output
    }

    async fn queue_capture_repair(
        db: &crate::state::SqliteStateDb,
        record: &crate::state::types::AssetRecord,
    ) {
        db.refresh_downloaded_asset_metadata(
            &record.library,
            &record.id,
            &record.metadata,
            true,
            true,
            crate::state::METADATA_CAPTURE_REVISION,
        )
        .await
        .unwrap();
    }

    #[cfg(feature = "xmp")]
    fn authority_cloudkit_time() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(39_600)
            .unwrap()
            .with_ymd_and_hms(2019, 3, 4, 5, 6, 7)
            .unwrap()
    }

    #[cfg(feature = "xmp")]
    const CLOUDKIT_ALTITUDE: f64 = 9.250_123_456_789;
    #[cfg(feature = "xmp")]
    const CLOUDKIT_ALTITUDE_XMP: &str = "2032686204/219746927";

    #[cfg(feature = "xmp")]
    fn assert_cloudkit_authority_with_source_gps(meta: &XmpMeta) {
        const CAPTURE: &str = "2019-03-04T05:06:07+11:00";
        for (namespace, property) in [
            (xmp_ns::XMP, "CreateDate"),
            (xmp_ns::XMP, "ModifyDate"),
            (xmp_ns::EXIF, "DateTimeOriginal"),
            (xmp_ns::PHOTOSHOP, "DateCreated"),
        ] {
            assert_eq!(
                meta.property(namespace, property).expect(property).value,
                CAPTURE
            );
        }
        let coordinate = |name: &str| meta.property(xmp_ns::EXIF, name).expect(name).value;
        assert_eq!(coordinate("GPSLatitude"), "12,20.7360N");
        assert_eq!(coordinate("GPSLongitude"), "78,54.0720W");
        assert_eq!(coordinate("GPSAltitude"), CLOUDKIT_ALTITUDE_XMP);
        crate::test_helpers::assert_source_gps_in_xmp(meta);
        assert_ne!(
            meta.property(xmp_ns::EXIF, "GPSTimeStamp")
                .expect("GPSTimeStamp")
                .value,
            CAPTURE
        );
    }

    #[cfg(feature = "xmp")]
    fn rich_payload() -> MetadataPayload {
        MetadataPayload {
            timezone_offset: Some(39_600),
            rating: Some(4),
            latitude: Some(37.7),
            longitude: Some(-122.4),
            altitude: Some(10.0),
            title: Some("T".into()),
            description: Some("D".into()),
            keywords: vec!["vacation".into(), "beach".into()],
            people: vec!["Alice".into()],
            is_hidden: true,
            is_archived: true,
            media_subtype: Some("portrait".into()),
            burst_id: Some("b1".into()),
        }
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_metadata_write_gates_xmp_fields_on_embed_xmp() {
        let payload = rich_payload();
        let flags_no_embed = MetadataFlags::default();
        let w = plan_metadata_write(
            flags_no_embed,
            &payload,
            &now_local(),
            &crate::download::metadata::ExifProbe::default(),
        );
        assert!(
            w.title.is_none(),
            "title must not write when embed_xmp is off"
        );
        assert!(w.keywords.is_empty());
        assert!(w.people.is_empty());
        assert!(!w.is_hidden);
        assert!(w.offset_time_original.is_none());

        let flags_embed = MetadataFlags::DATETIME | MetadataFlags::EMBED_XMP;
        let w = plan_metadata_write(
            flags_embed,
            &payload,
            &now_local(),
            &crate::download::metadata::ExifProbe::default(),
        );
        assert_eq!(w.title.as_deref(), Some("T"));
        assert_eq!(w.keywords, vec!["vacation", "beach"]);
        assert_eq!(w.people, vec!["Alice"]);
        assert!(w.is_hidden);
        assert!(w.is_archived);
        assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(w.burst_id.as_deref(), Some("b1"));
        assert_eq!(w.offset_time_original.as_deref(), Some("+11:00"));
    }

    #[test]
    fn plan_metadata_write_respects_probe_skip_for_datetime_and_gps() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            latitude: Some(37.7),
            longitude: Some(-122.4),
            ..MetadataPayload::default()
        };
        let flags = MetadataFlags::DATETIME | MetadataFlags::GPS;
        let created_local = now_local();
        let capture_local = created_local.format("%Y:%m:%d %H:%M:%S").to_string();

        let matching_clock = crate::download::metadata::ExifProbe {
            datetime_original: Some(capture_local),
            offset_time_original: None,
            has_other_datetime_offset: false,
            has_gps: true,
            ..crate::download::metadata::ExifProbe::default()
        };
        let write = plan_metadata_write(flags, &payload, &created_local, &matching_clock);
        assert!(
            write.datetime.is_none(),
            "must skip datetime when file already has one"
        );
        assert_eq!(
            write.offset_time_original.as_deref(),
            Some("+11:00"),
            "an offset may join a timestamp already rendering capture-local time"
        );
        assert!(
            write.gps.is_none(),
            "must skip gps when file already has one"
        );

        let existing_offset = crate::download::metadata::ExifProbe {
            offset_time_original: Some("+10:00".into()),
            ..matching_clock
        };
        let write = plan_metadata_write(flags, &payload, &created_local, &existing_offset);
        assert!(write.datetime.is_none());
        assert!(write.offset_time_original.is_none());
        assert!(write.gps.is_none());
    }

    #[test]
    fn plan_metadata_write_replaces_offsets_orphaned_from_datetime_original() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        };
        let flags = MetadataFlags::DATETIME;
        let created_local = now_local();

        for probe in [
            crate::download::metadata::ExifProbe {
                offset_time_original: Some("+10:00".into()),
                ..crate::download::metadata::ExifProbe::default()
            },
            crate::download::metadata::ExifProbe {
                has_other_datetime_offset: true,
                ..crate::download::metadata::ExifProbe::default()
            },
        ] {
            let write = plan_metadata_write(flags, &payload, &created_local, &probe);
            assert!(write.datetime.is_some());
            assert!(write.clear_datetime_offsets);
            assert_eq!(write.offset_time_original.as_deref(), Some("+11:00"));
        }

        let write = plan_metadata_write(
            flags,
            &MetadataPayload::default(),
            &created_local,
            &crate::download::metadata::ExifProbe {
                offset_time_original: Some("+10:00".into()),
                ..crate::download::metadata::ExifProbe::default()
            },
        );
        assert!(write.datetime.is_some());
        assert!(write.clear_datetime_offsets);
        assert!(write.offset_time_original.is_none());
    }

    /// A file written before capture-local resolution holds a wall clock in the
    /// backup host's timezone. Apple's offset does not describe that clock, so
    /// pairing the two would publish an instant the asset never had.
    #[test]
    fn plan_metadata_write_withholds_offset_from_an_unverified_timestamp() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        };
        let flags = MetadataFlags::DATETIME;
        let created_local = now_local();
        let host_local = (created_local - chrono::Duration::hours(11))
            .format("%Y:%m:%d %H:%M:%S")
            .to_string();

        for existing in [
            host_local,
            "not a timestamp".to_string(),
            String::new(),
            "2024-06-15".to_string(),
            // Capture-local wall clock, but claiming a different zone.
            created_local.format("%Y-%m-%dT%H:%M:%S+05:00").to_string(),
        ] {
            let probe = crate::download::metadata::ExifProbe {
                datetime_original: Some(existing.clone()),
                offset_time_original: None,
                has_other_datetime_offset: false,
                has_gps: false,
                ..crate::download::metadata::ExifProbe::default()
            };
            let write = plan_metadata_write(flags, &payload, &created_local, &probe);
            assert!(write.datetime.is_none());
            assert!(
                write.offset_time_original.is_none(),
                "offset must not join the unverified timestamp {existing:?}"
            );
        }
    }

    #[test]
    fn capture_timestamp_repair_requires_an_offset_and_replaces_the_pair() {
        let created_local = now_local();
        let probe = crate::download::metadata::ExifProbe {
            datetime_original: Some("2024:06:14 23:00:00".into()),
            offset_time_original: Some("+05:00".into()),
            has_other_datetime_offset: true,
            has_gps: false,
            ..crate::download::metadata::ExifProbe::default()
        };
        let write = plan_metadata_write_with_repair(
            MetadataFlags::DATETIME,
            &MetadataPayload {
                timezone_offset: Some(39_600),
                ..MetadataPayload::default()
            },
            &created_local,
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &probe,
        );
        assert_eq!(write.datetime.as_deref(), Some("2024:06:15 10:00:00"));
        assert_eq!(write.offset_time_original.as_deref(), Some("+11:00"));
        assert!(write.clear_datetime_offsets);

        let correct_probe = crate::download::metadata::ExifProbe {
            datetime_original: write.datetime.clone(),
            offset_time_original: write.offset_time_original.clone(),
            has_other_datetime_offset: false,
            has_gps: false,
            ..crate::download::metadata::ExifProbe::default()
        };
        let already_repaired = plan_metadata_write_with_repair(
            MetadataFlags::DATETIME,
            &MetadataPayload {
                timezone_offset: Some(39_600),
                ..MetadataPayload::default()
            },
            &created_local,
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &correct_probe,
        );
        assert!(already_repaired.is_empty());

        let write_without_offset = plan_metadata_write_with_repair(
            MetadataFlags::DATETIME,
            &MetadataPayload::default(),
            &created_local,
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &probe,
        );
        assert!(write_without_offset.datetime.is_none());
        assert!(write_without_offset.offset_time_original.is_none());
        assert!(!write_without_offset.clear_datetime_offsets);
    }

    /// XMP stores capture times as ISO 8601, and kei's own writer appends the
    /// offset. Such a timestamp is already capture-local, so it still accepts
    /// the offset tag.
    #[test]
    fn plan_metadata_write_accepts_iso_timestamps_from_the_xmp_probe() {
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            ..MetadataPayload::default()
        };
        let created_local = now_local();

        for existing in [
            created_local.format("%Y-%m-%dT%H:%M:%S").to_string(),
            created_local.format("%Y-%m-%dT%H:%M:%S+11:00").to_string(),
            format!("  {}\0", created_local.format("%Y:%m:%d %H:%M:%S")),
        ] {
            let probe = crate::download::metadata::ExifProbe {
                datetime_original: Some(existing.clone()),
                offset_time_original: None,
                has_other_datetime_offset: false,
                has_gps: false,
                ..crate::download::metadata::ExifProbe::default()
            };
            let write =
                plan_metadata_write(MetadataFlags::DATETIME, &payload, &created_local, &probe);
            assert_eq!(
                write.offset_time_original.as_deref(),
                Some("+11:00"),
                "capture-local timestamp {existing:?} must accept its offset"
            );
        }
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_metadata_write_skips_datetime_for_heif_native_exif() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.heic");
        let probe = crate::download::metadata::probe_exif(&path).expect("sample HEIC probe");
        let write = plan_metadata_write(
            MetadataFlags::DATETIME,
            &MetadataPayload::default(),
            &now_local(),
            &probe,
        );

        assert!(
            write.datetime.is_none(),
            "native HEIF DateTimeOriginal must suppress the derived datetime write"
        );
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_sidecar_write_is_comprehensive_regardless_of_flags() {
        let payload = rich_payload();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let (w, source_error) = plan_sidecar_write(&path, &payload, &now_local(), None);
        assert!(source_error.is_none());
        // Every payload field should land in the sidecar write, no flag gating.
        assert!(w.datetime.is_some());
        assert_eq!(w.offset_time_original.as_deref(), Some("+11:00"));
        assert_eq!(w.rating, Some(4));
        assert!(w.gps.is_some());
        assert_eq!(w.title.as_deref(), Some("T"));
        assert_eq!(w.description.as_deref(), Some("D"));
        assert_eq!(w.keywords.len(), 2);
        assert_eq!(w.people, vec!["Alice"]);
        assert!(w.is_hidden);
        assert!(w.is_archived);
        assert_eq!(w.media_subtype.as_deref(), Some("portrait"));
        assert_eq!(w.burst_id.as_deref(), Some("b1"));
    }

    #[cfg(feature = "xmp")]
    #[test]
    fn plan_sidecar_write_empty_payload_yields_datetime_only() {
        // datetime comes from the local clock; the rest stays empty.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let (w, source_error) =
            plan_sidecar_write(&path, &MetadataPayload::default(), &now_local(), None);
        assert!(source_error.is_none());
        assert!(w.datetime.is_some());
        assert!(w.gps_datetime.is_none());
        assert!(w.gps_speed.is_none());
        assert!(w.gps_speed_ref.is_none());
        assert!(w.gps_h_positioning_error.is_none());
        assert!(w.rating.is_none());
        assert!(w.gps.is_none());
        assert!(w.title.is_none());
        assert!(w.keywords.is_empty());
        assert!(!w.is_hidden);
    }

    #[test]
    fn metadata_flags_any_embed_captures_embed_only() {
        let mut flags = MetadataFlags::default();
        assert!(!flags.any_embed());
        flags.insert(MetadataFlags::XMP_SIDECAR);
        assert!(
            !flags.any_embed(),
            "sidecar-only must not trigger the .part-edit flow"
        );
        flags.remove(MetadataFlags::XMP_SIDECAR);
        flags.insert(MetadataFlags::EMBED_XMP);
        assert!(flags.any_embed());
    }

    #[tokio::test]
    async fn contract_metadata_writes_require_opt_in_leaves_files_untouched() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let photo_path = dir.path().join("photo.jpg");
        std::fs::write(&photo_path, b"original-media").expect("seed media");

        write_download_metadata(MetadataWriteRequest {
            final_path: &photo_path,
            embed_path: Some(&photo_path),
            expected_embed_fingerprint: None,
            source_checksum: None,
            sidecar_path: Some(&photo_path),
            payload: Arc::new(MetadataPayload::default()),
            created_local: now_local(),
            flags: MetadataFlags::default(),
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".metadata-test",
        })
        .await;

        assert_eq!(
            std::fs::read(&photo_path).expect("read media"),
            b"original-media",
            "disabled metadata options must not change media bytes"
        );
        let files = std::fs::read_dir(dir.path())
            .expect("read metadata temp dir")
            .count();
        assert_eq!(files, 1, "disabled metadata options created another file");
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn enabled_heif_embed_failure_preserves_invalid_media() {
        const FIXTURE_LEN: u64 = 1024 * 1024;
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let photo_path = dir.path().join("large.heic");
        let mut file = std::fs::File::create(&photo_path).expect("create sparse HEIF fixture");
        std::io::Write::write_all(&mut file, b"\0\0\0\x18ftypheic\0\0\0\0heicmif1")
            .expect("write HEIF routing header");
        file.set_len(FIXTURE_LEN)
            .expect("extend sparse HEIF fixture");
        drop(file);

        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &photo_path,
            embed_path: Some(&photo_path),
            expected_embed_fingerprint: None,
            source_checksum: None,
            sidecar_path: None,
            payload: Arc::new(MetadataPayload {
                rating: Some(5),
                ..MetadataPayload::default()
            }),
            created_local: now_local(),
            flags: MetadataFlags::RATING | MetadataFlags::EMBED_XMP,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".metadata-test",
        })
        .await;

        assert!(outcome.any_failed());
        assert_eq!(
            std::fs::metadata(&photo_path)
                .expect("stat sparse HEIF fixture")
                .len(),
            FIXTURE_LEN,
            "a failed HEIF embed must leave the media length unchanged"
        );
        assert_eq!(
            std::fs::read(&photo_path)
                .expect("read sparse HEIF fixture")
                .get(..24),
            Some(b"\0\0\0\x18ftypheic\0\0\0\0heicmif1".as_slice()),
            "a failed HEIF embed must leave the routing header unchanged"
        );
        assert!(!dir.path().join("large.heic.metadata-test").exists());
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn heif_embed_reports_change_after_approved_retry_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("changed-after-approval.heic");
        let original = include_bytes!("../../tests/data/sample.heic");
        std::fs::write(&photo_path, original).unwrap();
        let approved = crate::download::file::fingerprint_regular_file(&photo_path)
            .await
            .unwrap();
        let mut changed = original.to_vec();
        changed.push(0);
        std::fs::write(&photo_path, &changed).unwrap();

        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &photo_path,
            embed_path: Some(&photo_path),
            expected_embed_fingerprint: Some(approved),
            source_checksum: None,
            sidecar_path: None,
            payload: Arc::new(MetadataPayload {
                rating: Some(5),
                ..MetadataPayload::default()
            }),
            created_local: now_local(),
            flags: MetadataFlags::RATING | MetadataFlags::EMBED_XMP,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".metadata-test",
        })
        .await;

        assert!(outcome.any_failed());
        assert!(outcome.embed_input_changed);
        assert_eq!(std::fs::read(&photo_path).unwrap(), changed);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn approved_heif_fingerprint_bypasses_changed_format_routing_gate() {
        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("extensionless-asset");
        let original = include_bytes!("../../tests/data/sample.heic");
        std::fs::write(&photo_path, original).unwrap();
        let approved = crate::download::file::fingerprint_regular_file(&photo_path)
            .await
            .unwrap();
        let changed = b"unsupported concurrent replacement";
        std::fs::write(&photo_path, changed).unwrap();

        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &photo_path,
            embed_path: Some(&photo_path),
            expected_embed_fingerprint: Some(approved),
            source_checksum: None,
            sidecar_path: None,
            payload: Arc::new(MetadataPayload {
                rating: Some(5),
                ..MetadataPayload::default()
            }),
            created_local: now_local(),
            flags: MetadataFlags::RATING | MetadataFlags::EMBED_XMP,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".metadata-test",
        })
        .await;

        assert!(outcome.any_failed());
        assert!(outcome.embed_input_changed);
        assert_eq!(std::fs::read(&photo_path).unwrap(), changed);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn pending_heif_rewrite_detects_drift_before_format_routing() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("extensionless-asset");
        let original = include_bytes!("../../tests/data/sample.heic");
        std::fs::write(&photo_path, original).unwrap();
        let recorded = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        seed_downloaded_marker(
            &db,
            "ROUTING_DRIFT",
            "extensionless-asset",
            &photo_path,
            &recorded,
            AssetMetadata {
                rating: Some(5),
                metadata_hash: Some("routing-drift".into()),
                ..AssetMetadata::default()
            },
            None,
        )
        .await;
        let changed = b"unsupported concurrent replacement";
        std::fs::write(&photo_path, changed).unwrap();

        let pass = run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(pass.failed, 1);
        assert_eq!(std::fs::read(&photo_path).unwrap(), changed);
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(local.as_deref(), Some(recorded.as_str()));
        assert_eq!(download, None);
    }

    /// Minimal valid JPEG (SOI + APP0 JFIF + EOI). XMP Toolkit can write
    /// into this container; small enough to keep the test hermetic.
    fn minimal_jpeg_bytes() -> Vec<u8> {
        vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ]
    }

    #[tokio::test]
    async fn embed_path_preserves_unverified_host_local_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("host-local.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let before = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(before.datetime_original.is_some());
        assert!(before.offset_time_original.is_none());

        assert!(matches!(
            write_embed_metadata(
                &photo_path,
                None,
                Arc::new(MetadataPayload {
                    timezone_offset: Some(39_600),
                    ..MetadataPayload::default()
                }),
                now_local(),
                MetadataFlags::DATETIME,
                CaptureTimestampRepair::Preserve,
                ".metadata-test",
            )
            .await,
            EmbedWriteResult::NoWrite
        ));

        let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert_eq!(after.datetime_original, before.datetime_original);
        assert!(after.offset_time_original.is_none());
    }

    #[tokio::test]
    async fn embed_path_repairs_host_local_timestamp_and_offset_together() {
        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("host-local-repair.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                offset_time_original: Some("+05:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();

        let created_local = now_local();
        assert!(matches!(
            write_embed_metadata(
                &photo_path,
                None,
                Arc::new(MetadataPayload {
                    timezone_offset: Some(39_600),
                    ..MetadataPayload::default()
                }),
                created_local,
                MetadataFlags::DATETIME,
                CaptureTimestampRepair::ReplaceWithCaptureLocal,
                ".metadata-test",
            )
            .await,
            EmbedWriteResult::Applied(Some(_))
        ));

        let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(after.denotes_capture_time(&created_local));
        assert_eq!(after.offset_time_original.as_deref(), Some("+11:00"));

        assert!(
            matches!(
                write_embed_metadata(
                    &photo_path,
                    None,
                    Arc::new(MetadataPayload {
                        timezone_offset: Some(39_600),
                        ..MetadataPayload::default()
                    }),
                    created_local,
                    MetadataFlags::DATETIME,
                    CaptureTimestampRepair::ReplaceWithCaptureLocal,
                    ".metadata-test",
                )
                .await,
                EmbedWriteResult::NoWrite
            ),
            "a repaired timestamp and offset must be idempotent"
        );
    }

    #[tokio::test]
    async fn refresh_drain_repairs_a_state_recorded_host_local_timestamp() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("host-local-drain.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let created_local = now_local();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR")
            .filename("host-local-drain.jpg")
            .checksum("provider-checksum")
            .created_at(created_local.with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CAPTURE_REPAIR",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 0);
        assert!(
            db.get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .is_empty()
        );
        let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(probe.denotes_capture_time(&created_local));
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn capture_repair_recovers_publication_after_state_finalisation_failure() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("capture-repair-recovery.jpg");
        let db_path = dir.path().join("state.sqlite");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                offset_time_original: Some("+05:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR_RECOVERY")
            .filename("capture-repair-recovery.jpg")
            .checksum("provider-checksum")
            .created_at(now_local().with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CAPTURE_REPAIR_RECOVERY",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;
        db.fail_metadata_checksum_write_for_test();

        let config = MetadataConfig {
            set_exif_datetime: true,
            ..MetadataConfig::default()
        };
        let first = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &config,
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(first, 1);
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap();
        assert!(matches!(
            pending[0].capture_repair_receipt,
            Some(CaptureRepairReceipt::Prepared { .. })
        ));
        let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(probe.denotes_capture_time(&now_local()));
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
        drop(db);

        let reopened = SqliteStateDb::open(&db_path).await.unwrap();
        let second = crate::download::drain_pending_metadata_rewrites(
            &reopened as &dyn DownloadStore,
            &config,
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(second, 0);
        assert!(
            reopened
                .get_pending_metadata_rewrites_page_for_queue(
                    MetadataRewriteQueue::CaptureRepair,
                    None,
                    0,
                    1,
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reopened
                .get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .len(),
            1,
            "receipt recovery proves only capture completion, not current ordinary metadata"
        );
        assert_eq!(
            crate::download::drain_pending_metadata_rewrites(
                &reopened as &dyn DownloadStore,
                &config,
                CaptureTimestampRepair::Preserve,
                &["PrimarySync"],
                Arc::from(".metadata-test"),
                &CancellationToken::new(),
            )
            .await,
            0
        );
        assert!(
            reopened
                .get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn capture_repair_reprepares_an_unpublished_receipt() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("capture-repair-reprepare.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                offset_time_original: Some("+05:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR_REPREPARE")
            .filename("capture-repair-reprepare.jpg")
            .checksum("provider-checksum")
            .created_at(now_local().with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CAPTURE_REPAIR_REPREPARE",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(
            db.record_capture_repair_prepared(&pending, "unpublished-output", 1)
                .await
                .unwrap()
                .is_some()
        );

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 0);
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .is_empty()
        );
        let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(probe.denotes_capture_time(&now_local()));
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn capture_timestamp_repair_requires_a_recorded_local_checksum() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("unverified-capture-repair.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                offset_time_original: Some("+05:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let before = std::fs::read(&photo_path).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("UNVERIFIED_CAPTURE_REPAIR")
            .filename("unverified-capture-repair.jpg")
            .checksum("provider-checksum")
            .created_at(now_local().with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "UNVERIFIED_CAPTURE_REPAIR",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.clear_local_checksum_for_test("PrimarySync", "UNVERIFIED_CAPTURE_REPAIR", "original");
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
        assert_eq!(
            std::fs::read(&photo_path).unwrap(),
            before,
            "a checksum-less row must not change the file"
        );
    }

    #[tokio::test]
    async fn capture_timestamp_repair_without_offset_keeps_marker_pending() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("capture-repair-without-offset.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let before = std::fs::read(&photo_path).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_REPAIR_WITHOUT_OFFSET")
            .filename("capture-repair-without-offset.jpg")
            .checksum("provider-checksum")
            .created_at(now_local().with_timezone(&chrono::Utc))
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CAPTURE_REPAIR_WITHOUT_OFFSET",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
        assert_eq!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .len(),
            1
        );

        let ordinary_residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(ordinary_residual, 0);
        assert!(
            db.get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .len(),
            1,
            "ordinary metadata completion must not consume capture-repair debt"
        );
        assert_eq!(std::fs::read(&photo_path).unwrap(), before);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn ordinary_embed_waits_for_pending_capture_repair() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("ordinary-before-capture.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let before = std::fs::read(&photo_path).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("ORDINARY_BEFORE_CAPTURE")
            .filename("ordinary-before-capture.jpg")
            .checksum("provider-checksum")
            .created_at(now_local().with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                rating: Some(5),
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "ORDINARY_BEFORE_CAPTURE",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_rating: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert_eq!(std::fs::read(&photo_path).unwrap(), before);
        assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
        assert_eq!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                1,
            )
            .await
            .unwrap()
            .len(),
            1
        );
    }

    #[cfg(not(feature = "xmp"))]
    #[tokio::test]
    async fn capture_timestamp_repair_defers_unsupported_heif_without_xmp() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("unsupported-capture-repair.heic");
        let source = include_bytes!("../../tests/data/sample.heic");
        std::fs::write(&photo_path, source).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("UNSUPPORTED_CAPTURE_REPAIR")
            .filename("unsupported-capture-repair.heic")
            .checksum("provider-checksum")
            .created_at(now_local().with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "UNSUPPORTED_CAPTURE_REPAIR",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
        assert_eq!(std::fs::read(&photo_path).unwrap(), source);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn refresh_drain_repairs_native_heif_capture_time_before_retiring_marker() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;
        use little_exif::exif_tag::ExifTag;
        use little_exif::filetype::FileExtension;
        use little_exif::metadata::Metadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("native-capture-repair.heic");
        std::fs::write(
            &photo_path,
            with_capture_xmp(include_bytes!("../../tests/data/sample.heic")),
        )
        .unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let created_local = now_local();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("HEIF_CAPTURE_REPAIR")
            .filename("native-capture-repair.heic")
            .checksum("provider-checksum")
            .created_at(created_local.with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "HEIF_CAPTURE_REPAIR",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 0);
        assert!(
            db.get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .is_empty(),
            "the marker may retire only after native Exif is repaired"
        );
        let bytes = std::fs::read(&photo_path).unwrap();
        let tiff = crate::download::heif::extract_exif_tiff_bytes(&bytes)
            .unwrap()
            .expect("native Exif item");
        let metadata = Metadata::new_from_vec(&tiff, FileExtension::TIFF).unwrap();
        assert!(metadata
            .get_tag(&ExifTag::DateTimeOriginal(String::new()))
            .any(|tag| matches!(tag, ExifTag::DateTimeOriginal(value) if value == "2024:06:15 10:00:00")));
        assert!(
            metadata
                .get_tag(&ExifTag::OffsetTimeOriginal(String::new()))
                .any(|tag| matches!(tag, ExifTag::OffsetTimeOriginal(value) if value == "+11:00"))
        );
        let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(probe.denotes_capture_time(&created_local));
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn heif_capture_repair_writes_missing_xmp_before_retiring_marker() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("missing-xmp-capture-repair.heic");
        let source = include_bytes!("../../tests/data/sample.heic");
        assert!(
            crate::download::heif::extract_xmp_strict(source)
                .unwrap()
                .is_none(),
            "the fixture must start without XMP"
        );
        std::fs::write(&photo_path, source).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let created_local = FixedOffset::east_opt(10_800)
            .unwrap()
            .with_ymd_and_hms(2023, 9, 3, 9, 28, 14)
            .unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("HEIF_MISSING_XMP_REPAIR")
            .filename("missing-xmp-capture-repair.heic")
            .checksum("provider-checksum")
            .created_at(created_local.with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(10_800),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "HEIF_MISSING_XMP_REPAIR",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 0);
        assert!(
            db.get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .is_empty()
        );
        let bytes = std::fs::read(&photo_path).unwrap();
        assert!(
            crate::download::heif::extract_xmp_strict(&bytes)
                .unwrap()
                .is_some(),
            "the marker may retire only after XMP is present"
        );
        let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(probe.denotes_capture_time(&created_local));
        assert_eq!(probe.offset_time_original.as_deref(), Some("+03:00"));
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn heif_capture_repair_without_native_exif_keeps_marker_pending() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("xmp-only-capture-repair.avif");
        let source = with_capture_xmp(include_bytes!("../../tests/data/white_1x1.avif"));
        assert!(
            crate::download::heif::extract_exif_tiff_bytes(&source)
                .unwrap()
                .is_none(),
            "the fixture must have no native Exif item"
        );
        std::fs::write(&photo_path, &source).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("XMP_ONLY_CAPTURE_REPAIR")
            .filename("xmp-only-capture-repair.avif")
            .checksum("provider-checksum")
            .created_at(now_local().with_timezone(&chrono::Utc))
            .metadata(AssetMetadata {
                timezone_offset: Some(39_600),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "XMP_ONLY_CAPTURE_REPAIR",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        queue_capture_repair(&db, &record).await;

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert_eq!(
            db.get_pending_metadata_rewrites(1).await.unwrap().len(),
            1,
            "a HEIF without native Exif must retain its repair marker"
        );
        assert_eq!(
            std::fs::read(&photo_path).unwrap(),
            source,
            "a refused repair must not change the file"
        );
    }

    #[tokio::test]
    async fn embed_path_replaces_orphaned_offset_before_writing_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("orphaned-offset.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &photo_path,
            &crate::download::metadata::MetadataWrite {
                offset_time_original: Some("+10:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let before = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(before.datetime_original.is_none());
        assert_eq!(before.offset_time_original.as_deref(), Some("+10:00"));

        let created_local = now_local();
        assert!(matches!(
            write_embed_metadata(
                &photo_path,
                None,
                Arc::new(MetadataPayload {
                    timezone_offset: Some(39_600),
                    ..MetadataPayload::default()
                }),
                created_local,
                MetadataFlags::DATETIME,
                CaptureTimestampRepair::Preserve,
                ".metadata-test",
            )
            .await,
            EmbedWriteResult::Applied(Some(_))
        ));

        let after = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert!(after.denotes_capture_time(&created_local));
        assert_eq!(after.offset_time_original.as_deref(), Some("+11:00"));
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn contract_xmp_gps_accuracy_requires_matching_location_new_writes() {
        use crate::test_helpers::ur64;
        use little_exif::exif_tag::ExifTag;
        use little_exif::filetype::FileExtension;
        use little_exif::metadata::Metadata;

        let source = crate::test_helpers::minimal_jpeg_with_source_gps_and_location();
        let dir = tempfile::tempdir().unwrap();
        // Even a one-ULP edit does not establish provenance. Invalid provider
        // evidence must not authorize accuracy, whether or not it is rendered.
        let provider_cases = [
            ("matching", Some(1.5), Some(2.5), true),
            ("edited-latitude", Some(12.3456), Some(2.5), false),
            ("edited-longitude", Some(1.5), Some(-78.9012), false),
            (
                "adjacent-float",
                Some(f64::from_bits(1.5_f64.to_bits() + 1)),
                Some(2.5),
                false,
            ),
            ("missing-latitude", None, Some(2.5), false),
            ("missing-longitude", Some(1.5), None, false),
            ("invalid-latitude", Some(91.0), Some(2.5), false),
            ("invalid-longitude", Some(1.5), Some(181.0), false),
            ("nan", Some(f64::NAN), Some(2.5), false),
            ("infinite", Some(1.5), Some(f64::INFINITY), false),
        ];
        for (name, latitude, longitude, expected) in provider_cases {
            let path = dir.path().join(format!("{name}.jpg"));
            std::fs::write(&path, &source).unwrap();
            let payload = Arc::new(MetadataPayload {
                latitude,
                longitude,
                ..MetadataPayload::default()
            });
            let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
            assert!(
                write_sidecar_metadata(&path, payload, now_local(), Some(&checksum), ".gps-test")
                    .await
            );
            let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
                expected,
                "{name}"
            );
            if expected {
                assert_eq!(
                    xmp.property(xmp_ns::EXIF, "GPSHPositioningError")
                        .unwrap()
                        .value,
                    "3/2"
                );
            }
            assert_eq!(std::fs::read(&path).unwrap(), source, "{name}");
        }

        // Neither an absent legacy baseline nor a different recorded source
        // can certify matching coordinates in the current file.
        let path = dir.path().join("unproven.jpg");
        std::fs::write(&path, &source).unwrap();
        for checksum in [None, Some("different-source-checksum")] {
            assert!(
                write_sidecar_metadata(
                    &path,
                    Arc::new(MetadataPayload {
                        latitude: Some(1.5),
                        longitude: Some(2.5),
                        ..MetadataPayload::default()
                    }),
                    now_local(),
                    checksum,
                    ".gps-test"
                )
                .await
            );
            let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
            assert_eq!(std::fs::read(&path).unwrap(), source);
        }

        // Remove a tag, or replace it with invalid readable EXIF. Both are
        // known absence, not retryable I/O failures.
        let native_cases = [
            (
                "missing-native-latitude",
                ExifTag::GPSLatitude(vec![]),
                None,
            ),
            (
                "missing-native-longitude",
                ExifTag::GPSLongitude(vec![]),
                None,
            ),
            (
                "missing-native-reference",
                ExifTag::GPSLatitudeRef(String::new()),
                None,
            ),
            (
                "missing-accuracy",
                ExifTag::GPSHPositioningError(vec![]),
                None,
            ),
            (
                "invalid-native-latitude",
                ExifTag::GPSLatitude(vec![]),
                Some(ExifTag::GPSLatitude(vec![
                    ur64(91, 1),
                    ur64(0, 1),
                    ur64(0, 1),
                ])),
            ),
            (
                "invalid-native-longitude",
                ExifTag::GPSLongitude(vec![]),
                Some(ExifTag::GPSLongitude(vec![
                    ur64(181, 1),
                    ur64(0, 1),
                    ur64(0, 1),
                ])),
            ),
            (
                "invalid-minutes",
                ExifTag::GPSLatitude(vec![]),
                Some(ExifTag::GPSLatitude(vec![
                    ur64(0, 1),
                    ur64(90, 1),
                    ur64(0, 1),
                ])),
            ),
            (
                "invalid-seconds",
                ExifTag::GPSLatitude(vec![]),
                Some(ExifTag::GPSLatitude(vec![
                    ur64(1, 1),
                    ur64(29, 1),
                    ur64(60, 1),
                ])),
            ),
            (
                "invalid-reference",
                ExifTag::GPSLatitudeRef(String::new()),
                Some(ExifTag::GPSLatitudeRef("E".into())),
            ),
            (
                "zero-coordinate-denominator",
                ExifTag::GPSLatitude(vec![]),
                Some(ExifTag::GPSLatitude(vec![
                    ur64(1, 0),
                    ur64(30, 1),
                    ur64(0, 1),
                ])),
            ),
            (
                "zero-accuracy-denominator",
                ExifTag::GPSHPositioningError(vec![]),
                Some(ExifTag::GPSHPositioningError(vec![ur64(3, 0)])),
            ),
        ];
        for (name, removed, replacement) in native_cases {
            let mut metadata = Metadata::new_from_vec(&source, FileExtension::JPEG).unwrap();
            assert!(metadata.remove_tag(removed) > 0);
            if let Some(tag) = replacement {
                metadata.set_tag(tag);
            }
            let mut bytes = minimal_jpeg_bytes();
            metadata
                .write_to_vec(&mut bytes, FileExtension::JPEG)
                .unwrap();
            let path = dir.path().join(format!("{name}.jpg"));
            std::fs::write(&path, &bytes).unwrap();
            let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
            // Out-of-range native values deliberately match the provider so
            // coordinate validation, not mere inequality, must reject them.
            let (latitude, longitude, rendered_latitude) = match name {
                "invalid-native-latitude" => (91.0, 2.5, "91,0.0000N"),
                "invalid-native-longitude" => (1.5, 181.0, "1,30.0000N"),
                _ => (1.5, 2.5, "1,30.0000N"),
            };
            assert!(
                write_sidecar_metadata(
                    &path,
                    Arc::new(MetadataPayload {
                        latitude: Some(latitude),
                        longitude: Some(longitude),
                        ..MetadataPayload::default()
                    }),
                    now_local(),
                    Some(&checksum),
                    ".gps-test"
                )
                .await,
                "{name}"
            );
            let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                !xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
                "{name}"
            );
            assert_eq!(
                xmp.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value,
                rendered_latitude,
                "{name}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name}");
        }
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn contract_xmp_gps_accuracy_requires_matching_location_after_embed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jpg");
        std::fs::write(&path, crate::test_helpers::minimal_jpeg_with_source_gps()).unwrap();
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let native = super::super::metadata::read_source_gps(&path).unwrap();
        assert!(native.latitude.is_none());
        assert!(native.longitude.is_none());
        assert!(native.horizontal_positioning_error.is_some());
        // Match the download path: embed before publication, then write the
        // sidecar from the published media in a separate call.
        for (embed_path, sidecar_path) in
            [(Some(path.as_path()), None), (None, Some(path.as_path()))]
        {
            let outcome = write_download_metadata(MetadataWriteRequest {
                final_path: &path,
                embed_path,
                expected_embed_fingerprint: None,
                source_checksum: Some(&checksum),
                sidecar_path,
                payload: Arc::new(MetadataPayload {
                    latitude: Some(1.5),
                    longitude: Some(2.5),
                    ..MetadataPayload::default()
                }),
                created_local: now_local(),
                flags: MetadataFlags::GPS | MetadataFlags::XMP_SIDECAR,
                capture_timestamp_repair: CaptureTimestampRepair::Preserve,
                temp_suffix: ".gps-test",
            })
            .await;
            assert!(!outcome.any_failed());
        }
        let rewritten = super::super::metadata::read_source_gps(&path).unwrap();
        assert_eq!(rewritten.latitude, Some(1.5));
        assert_eq!(rewritten.longitude, Some(2.5));
        let xmp: XmpMeta = std::fs::read_to_string(path.with_extension("jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn contract_xmp_gps_accuracy_requires_matching_location_embed_retry() {
        use crate::state::{AssetMetadata, SqliteStateDb};

        for fail_state_write in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.jpg");
            let sidecar = path.with_extension("jpg.xmp");
            let source = crate::test_helpers::minimal_jpeg_with_source_gps();
            std::fs::write(&path, &source).unwrap();
            let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
            let db_path = dir.path().join("state.db");
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            seed_downloaded_marker(
                &db,
                "GPS_EMBED_RETRY",
                "source.jpg",
                &path,
                &checksum,
                AssetMetadata {
                    latitude: Some(1.5),
                    longitude: Some(2.5),
                    ..AssetMetadata::default()
                },
                None,
            )
            .await;
            // The native coordinate mutation succeeds, then publication and,
            // in the second case, checksum finalization fail.
            std::fs::create_dir(&sidecar).unwrap();
            if fail_state_write {
                db.fail_metadata_checksum_write_for_test();
            }
            let pass = run_pending(
                &db,
                MetadataFlags::GPS | MetadataFlags::XMP_SIDECAR,
                Arc::from(".gps-test"),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!(pass.failed, 1);
            assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
            let rewritten = std::fs::read(&path).unwrap();
            assert_ne!(rewritten, source);
            let native = super::super::metadata::read_source_gps(&path).unwrap();
            assert_eq!((native.latitude, native.longitude), (Some(1.5), Some(2.5)));
            assert!(
                native.horizontal_positioning_error.is_some(),
                "native accuracy must not be deleted"
            );
            let checksums = stored_checksums(&db).await;
            if fail_state_write {
                assert_eq!(checksums, (None, None));
            } else {
                assert_eq!(checksums.1.as_deref(), Some(checksum.as_str()));
            }
            drop(db);
            std::fs::remove_dir(&sidecar).unwrap();
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            let pass = run_pending(
                &db,
                MetadataFlags::XMP_SIDECAR,
                Arc::from(".gps-test"),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!((pass.applied, pass.failed), (1, 0));
            let xmp: XmpMeta = std::fs::read_to_string(&sidecar).unwrap().parse().unwrap();
            assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
            assert_eq!(
                xmp.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value,
                "1,30.0000N"
            );
            assert_eq!(std::fs::read(&path).unwrap(), rewritten);
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );

            // A later no-op embed may establish a local checksum after failed
            // finalization. It must not promote those rewritten bytes into
            // original-source evidence for the next sidecar-only retry.
            for flags in [
                MetadataFlags::GPS | MetadataFlags::XMP_SIDECAR,
                MetadataFlags::XMP_SIDECAR,
            ] {
                db.record_metadata_write_failure("PrimarySync", "GPS_EMBED_RETRY", "original")
                    .await
                    .unwrap();
                let pass = run_pending(
                    &db,
                    flags,
                    Arc::from(".gps-test"),
                    &CancellationToken::new(),
                )
                .await;
                assert_eq!((pass.applied, pass.failed), (1, 0));
                let xmp: XmpMeta = std::fs::read_to_string(&sidecar).unwrap().parse().unwrap();
                assert!(!xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"));
                assert_eq!(std::fs::read(&path).unwrap(), rewritten);
            }
            let before = std::fs::read(&sidecar).unwrap();
            let steady = run_pending(
                &db,
                MetadataFlags::XMP_SIDECAR,
                Arc::from(".gps-test"),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!((steady.applied, steady.failed), (0, 0));
            assert_eq!(std::fs::read(&sidecar).unwrap(), before);
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn contract_xmp_gps_accuracy_requires_matching_location_checksum_recovery() {
        use crate::state::{AssetMetadata, SqliteStateDb};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.jpg");
        let sidecar = path.with_extension("jpg.xmp");
        std::fs::write(&path, crate::test_helpers::minimal_jpeg_with_source_gps()).unwrap();
        let original_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let original_gps = super::super::metadata::read_source_gps(&path).unwrap();
        assert!(original_gps.latitude.is_none() && original_gps.longitude.is_none());
        assert!(original_gps.horizontal_positioning_error.is_some());
        let db_path = dir.path().join("state.db");
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let mut metadata = AssetMetadata {
            rating: Some(1),
            latitude: Some(1.5),
            longitude: Some(2.5),
            ..AssetMetadata::default()
        };
        seed_downloaded_marker(
            &db,
            "PROVENANCE",
            "source.jpg",
            &path,
            &original_checksum,
            metadata.clone(),
            None,
        )
        .await;
        db.fail_metadata_checksum_write_for_test();
        let first = run_pending(
            &db,
            MetadataFlags::GPS | MetadataFlags::RATING | MetadataFlags::XMP_SIDECAR,
            Arc::from(".review"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(first.failed, 1);
        assert_eq!(stored_checksums(&db).await, (None, None));
        let gps = super::super::metadata::read_source_gps(&path).unwrap();
        assert_eq!((gps.latitude, gps.longitude), (Some(1.5), Some(2.5)));
        drop(db);

        let db = SqliteStateDb::open(&db_path).await.unwrap();
        let retry = run_pending(
            &db,
            MetadataFlags::RATING | MetadataFlags::XMP_SIDECAR,
            Arc::from(".review"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((retry.applied, retry.failed), (1, 0));
        let rewritten_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        assert_ne!(rewritten_checksum, original_checksum);
        assert_eq!(
            stored_checksums(&db).await,
            (Some(rewritten_checksum.clone()), None)
        );
        for rating in [2, 1] {
            metadata.rating = Some(rating);
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "PROVENANCE",
                &metadata,
                true,
                false,
                crate::state::METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap();
            let pass = run_pending(
                &db,
                MetadataFlags::RATING | MetadataFlags::XMP_SIDECAR,
                Arc::from(".review"),
                &CancellationToken::new(),
            )
            .await;
            assert_eq!((pass.applied, pass.failed), (1, 0));
        }
        assert_eq!(
            stored_checksums(&db).await,
            (Some(rewritten_checksum.clone()), Some(rewritten_checksum))
        );
        let xmp: XmpMeta = std::fs::read_to_string(&sidecar).unwrap().parse().unwrap();
        assert!(
            !xmp.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
            "kei-inserted coordinates acquired fabricated original-byte provenance"
        );
        let media = std::fs::read(&path).unwrap();
        let sidecar_bytes = std::fs::read(&sidecar).unwrap();
        drop(db);
        let db = SqliteStateDb::open(&db_path).await.unwrap();
        db.record_metadata_write_failure("PrimarySync", "PROVENANCE", "original")
            .await
            .unwrap();
        let pending = db
            .get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::Ordinary,
                None,
                0,
                10,
            )
            .await
            .unwrap();
        assert!(pending[0].source_checksum.is_none());
        let retry = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".review"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((retry.applied, retry.failed), (1, 0));
        let steady = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".review"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!((steady.applied, steady.failed), (0, 0));
        assert_eq!(std::fs::read(&path).unwrap(), media);
        assert_eq!(std::fs::read(&sidecar).unwrap(), sidecar_bytes);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn contract_xmp_gps_accuracy_requires_matching_location_native_boundaries() {
        use crate::test_helpers::ur64;
        use little_exif::exif_tag::ExifTag;

        let dir = tempfile::tempdir().unwrap();
        for (name, latitude, longitude, lat_ref, lon_ref) in [
            ("south-west", 1, 2, "S", "W"),
            ("north-east-limits", 90, 180, "N", "E"),
            ("south-west-limits", 90, 180, "S", "W"),
            ("zero", 0, 0, "N", "E"),
        ] {
            let mut metadata = crate::test_helpers::exif_with_source_gps();
            metadata.set_tag(ExifTag::GPSLatitude(vec![
                ur64(latitude, 1),
                ur64(0, 1),
                ur64(0, 1),
            ]));
            metadata.set_tag(ExifTag::GPSLongitude(vec![
                ur64(longitude, 1),
                ur64(0, 1),
                ur64(0, 1),
            ]));
            metadata.set_tag(ExifTag::GPSLatitudeRef(lat_ref.into()));
            metadata.set_tag(ExifTag::GPSLongitudeRef(lon_ref.into()));
            metadata.set_tag(ExifTag::GPSHPositioningError(vec![ur64(0, 1)]));
            let tiff = metadata.encode().unwrap();
            let heic = crate::download::heif::apple_tmap_insertion_heic(&tiff, b"");
            for (extension, bytes) in [("dng", &tiff), ("heic", &heic)] {
                let path = dir.path().join(format!("{name}.{extension}"));
                std::fs::write(&path, bytes).unwrap();
                let payload = Arc::new(MetadataPayload {
                    latitude: Some(f64::from(latitude) * if lat_ref == "S" { -1.0 } else { 1.0 }),
                    longitude: Some(f64::from(longitude) * if lon_ref == "W" { -1.0 } else { 1.0 }),
                    ..MetadataPayload::default()
                });
                let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
                assert!(
                    write_sidecar_metadata(
                        &path,
                        payload,
                        now_local(),
                        Some(&checksum),
                        ".gps-test"
                    )
                    .await
                );
                let xmp: XmpMeta =
                    std::fs::read_to_string(path.with_extension(format!("{extension}.xmp")))
                        .unwrap()
                        .parse()
                        .unwrap();
                assert_eq!(
                    xmp.property(xmp_ns::EXIF, "GPSHPositioningError")
                        .unwrap()
                        .value,
                    "0/1",
                    "{name}.{extension}"
                );
                assert_eq!(std::fs::read(&path).unwrap(), *bytes);
            }
        }
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn contract_xmp_gps_accuracy_requires_matching_location_rewrite_and_retry() {
        use crate::state::{AssetMetadata, DownloadStateStore, SqliteStateDb};
        use xmp_toolkit::XmpValue;

        for owned in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.jpg");
            let sidecar_path = path.with_extension("jpg.xmp");
            let source = crate::test_helpers::minimal_jpeg_with_source_gps_and_location();
            std::fs::write(&path, &source).unwrap();
            let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
            let db = SqliteStateDb::open(&dir.path().join("state.db"))
                .await
                .unwrap();
            let mut metadata = AssetMetadata {
                latitude: Some(1.5),
                longitude: Some(2.5),
                ..AssetMetadata::default()
            };
            seed_downloaded_marker(
                &db,
                "GPS_PROVENANCE",
                "source.jpg",
                &path,
                &checksum,
                metadata.clone(),
                None,
            )
            .await;
            db.mark_verified_download(
                "PrimarySync",
                "GPS_PROVENANCE",
                "original",
                &path,
                &checksum,
                Some(&checksum),
                false,
            )
            .await
            .unwrap();
            db.record_metadata_write_failure("PrimarySync", "GPS_PROVENANCE", "original")
                .await
                .unwrap();
            let token = CancellationToken::new();
            let drain = || {
                run_pending(
                    &db,
                    MetadataFlags::XMP_SIDECAR,
                    Arc::from(".gps-test"),
                    &token,
                )
            };
            assert_eq!(drain().await.applied, 1);
            let mut initial: XmpMeta = std::fs::read_to_string(&sidecar_path)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                initial
                    .property(xmp_ns::EXIF, "GPSHPositioningError")
                    .unwrap()
                    .value,
                "3/2"
            );
            if !owned {
                initial
                    .delete_property("https://github.com/rhoopr/kei/ns/1.0/", "managedFields")
                    .unwrap();
            }
            initial
                .set_property(xmp_ns::XMP, "Label", &XmpValue::new("keep me".to_owned()))
                .unwrap();
            std::fs::write(&sidecar_path, initial.to_string()).unwrap();
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );

            metadata.latitude = Some(12.3456);
            metadata.longitude = Some(-78.9012);
            db.refresh_downloaded_asset_metadata(
                "PrimarySync",
                "GPS_PROVENANCE",
                &metadata,
                true,
                false,
                crate::state::METADATA_CAPTURE_REVISION,
            )
            .await
            .unwrap();

            // A source read failure is unknown: preserve the old accuracy and
            // its ownership while publishing current provider coordinates.
            let saved_media = dir.path().join("saved.jpg");
            std::fs::rename(&path, &saved_media).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert_eq!(drain().await.failed, 1);
            let unknown: XmpMeta = std::fs::read_to_string(&sidecar_path)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                unknown
                    .property(xmp_ns::EXIF, "GPSHPositioningError")
                    .unwrap()
                    .value,
                "3/2"
            );
            assert_eq!(
                unknown.property(xmp_ns::EXIF, "GPSLatitude").unwrap().value,
                "12,20.7360N"
            );
            assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
            std::fs::remove_dir(&path).unwrap();
            std::fs::rename(&saved_media, &path).unwrap();

            // Publication failure must not consume the pending correction.
            let saved_sidecar = dir.path().join("saved.xmp");
            let before_failure = std::fs::read(&sidecar_path).unwrap();
            std::fs::rename(&sidecar_path, &saved_sidecar).unwrap();
            std::fs::create_dir(&sidecar_path).unwrap();
            assert_eq!(drain().await.failed, 1);
            assert!(sidecar_path.is_dir());
            assert_eq!(std::fs::read(&saved_sidecar).unwrap(), before_failure);
            assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
            std::fs::remove_dir(&sidecar_path).unwrap();
            std::fs::rename(&saved_sidecar, &sidecar_path).unwrap();

            assert_eq!(drain().await.applied, 1);
            let corrected_bytes = std::fs::read(&sidecar_path).unwrap();
            let corrected: XmpMeta = std::str::from_utf8(&corrected_bytes)
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(
                corrected.contains_property(xmp_ns::EXIF, "GPSHPositioningError"),
                !owned
            );
            if !owned {
                assert_eq!(
                    corrected
                        .property(xmp_ns::EXIF, "GPSHPositioningError")
                        .unwrap()
                        .value,
                    "3/2"
                );
            }
            assert_eq!(
                corrected
                    .property(xmp_ns::EXIF, "GPSLatitude")
                    .unwrap()
                    .value,
                "12,20.7360N"
            );
            assert_eq!(
                corrected
                    .property(xmp_ns::EXIF, "GPSLongitude")
                    .unwrap()
                    .value,
                "78,54.0720W"
            );
            assert_eq!(
                corrected.property(xmp_ns::XMP, "Label").unwrap().value,
                "keep me"
            );
            assert!(
                db.get_pending_metadata_rewrites(10)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let steady = drain().await;
            assert_eq!((steady.applied, steady.failed), (0, 0));
            assert_eq!(std::fs::read(&sidecar_path).unwrap(), corrected_bytes);
            assert_eq!(std::fs::read(&path).unwrap(), source);
            assert_eq!(
                stored_checksums(&db).await.0.as_deref(),
                Some(checksum.as_str())
            );
        }
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn sidecar_write_carries_source_gps_and_corrected_coordinates() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let media_path = dir.path().join("source.jpg");
        let source_bytes = crate::test_helpers::minimal_jpeg_with_source_gps_and_location();
        std::fs::write(&media_path, &source_bytes).expect("write source media");
        let payload = MetadataPayload {
            timezone_offset: Some(39_600),
            latitude: Some(12.3456),
            longitude: Some(-78.9012),
            altitude: Some(CLOUDKIT_ALTITUDE),
            ..MetadataPayload::default()
        };
        let cloudkit_created = authority_cloudkit_time();
        let request = || MetadataWriteRequest {
            final_path: &media_path,
            embed_path: None,
            expected_embed_fingerprint: None,
            source_checksum: None,
            sidecar_path: Some(&media_path),
            payload: Arc::new(payload.clone()),
            created_local: cloudkit_created,
            flags: MetadataFlags::XMP_SIDECAR,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".gps-sidecar-test",
        };

        let outcome = write_download_metadata(request()).await;
        assert!(!outcome.any_failed());
        let sidecar_path = media_path.with_file_name("source.jpg.xmp");
        let first = std::fs::read(&sidecar_path).expect("read generated sidecar");
        let xmp = String::from_utf8(first.clone())
            .expect("sidecar UTF-8")
            .parse::<XmpMeta>()
            .expect("parse generated sidecar");
        assert_cloudkit_authority_with_source_gps(&xmp);
        assert_eq!(
            std::fs::read(&media_path).expect("read media after sidecar"),
            source_bytes
        );

        let second_outcome = write_download_metadata(request()).await;
        assert!(!second_outcome.any_failed());
        assert_eq!(
            std::fs::read(&sidecar_path).expect("read repeated sidecar"),
            first,
            "repeating the same sidecar write must be idempotent"
        );

        use crate::state::{AssetMetadata, SqliteStateDb};
        let db = SqliteStateDb::open_in_memory().expect("metadata state DB");
        let checksum = crate::download::file::compute_sha256(&media_path)
            .await
            .expect("media checksum");
        seed_downloaded_marker(
            &db,
            "GPS_RETRY",
            "source.jpg",
            &media_path,
            &checksum,
            AssetMetadata {
                timezone_offset: Some(39_600),
                latitude: Some(12.3456),
                longitude: Some(-78.9012),
                altitude: Some(CLOUDKIT_ALTITUDE),
                metadata_hash: Some("gps-retry-hash".into()),
                ..AssetMetadata::default()
            },
            Some(cloudkit_created.with_timezone(&chrono::Utc)),
        )
        .await;
        run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".gps-retry-test"),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .expect("read retry markers")
                .is_empty(),
            "successful sidecar retry must retire its marker"
        );
        let retry_xmp = std::fs::read_to_string(&sidecar_path)
            .expect("read sidecar after retry")
            .parse::<XmpMeta>()
            .expect("parse sidecar after retry");
        assert_cloudkit_authority_with_source_gps(&retry_xmp);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn sidecar_write_reads_source_gps_from_dng_content() {
        let dir = tempfile::tempdir().expect("metadata temp dir");
        let media_path = dir.path().join("source.DNG");
        let source_bytes = crate::test_helpers::minimal_tiff_with_source_gps();
        std::fs::write(&media_path, &source_bytes).expect("write source DNG");
        let payload = MetadataPayload {
            rating: Some(4),
            latitude: Some(12.3456),
            longitude: Some(-78.9012),
            altitude: Some(9.25),
            ..MetadataPayload::default()
        };

        let outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &media_path,
            embed_path: None,
            expected_embed_fingerprint: None,
            source_checksum: None,
            sidecar_path: Some(&media_path),
            payload: Arc::new(payload),
            created_local: now_local(),
            flags: MetadataFlags::XMP_SIDECAR,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".gps-dng-test",
        })
        .await;

        assert!(!outcome.any_failed());
        let sidecar_path = media_path.with_file_name("source.DNG.xmp");
        let xmp = std::fs::read_to_string(&sidecar_path)
            .expect("read DNG sidecar")
            .parse::<XmpMeta>()
            .expect("parse DNG sidecar");
        crate::test_helpers::assert_source_gps_in_xmp(&xmp);
        let value = |namespace, name| xmp.property(namespace, name).expect(name).value;
        assert_eq!(value(xmp_ns::XMP, "Rating"), "4");
        assert_eq!(value(xmp_ns::EXIF, "GPSLatitude"), "12,20.7360N");
        assert_eq!(value(xmp_ns::EXIF, "GPSLongitude"), "78,54.0720W");
        assert_eq!(value(xmp_ns::EXIF, "GPSAltitude"), "37/4");
        assert_eq!(
            std::fs::read(&media_path).expect("read source DNG after sidecar"),
            source_bytes,
            "sidecar-only metadata must not alter DNG bytes"
        );
        assert!(
            !media_path
                .with_file_name("source.DNG.xmp.gps-dng-test")
                .exists(),
            "successful sidecar publication must remove its temporary file"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn sidecar_retry_recovers_exif_less_media_and_keeps_marker_for_unreadable_source() {
        use crate::state::{AssetMetadata, SqliteStateDb};

        let dir = tempfile::tempdir().expect("metadata temp dir");

        // A structurally valid HEIC with no EXIF block. There is no source GPS
        // to read, so the sidecar carries only the CloudKit payload and the
        // marker must retire.
        let exif_less_path = dir.path().join("exif-less.heic");
        std::fs::write(
            &exif_less_path,
            crate::test_helpers::heif_ftyp_without_meta_bytes(),
        )
        .expect("write EXIF-less HEIC");

        // A source the reader cannot open at all. Planning the sidecar fails,
        // so the durable marker survives for a future retry.
        let unreadable_path = dir.path().join("unreadable.heic");
        std::fs::create_dir(&unreadable_path).expect("create unreadable source");

        let db = SqliteStateDb::open_in_memory().expect("metadata state DB");
        let exif_less_checksum = crate::download::file::compute_sha256(&exif_less_path)
            .await
            .expect("media checksum");
        seed_downloaded_marker(
            &db,
            "GPS_EXIF_LESS",
            "exif-less.heic",
            &exif_less_path,
            &exif_less_checksum,
            AssetMetadata {
                metadata_hash: Some("exif-less-hash".into()),
                ..AssetMetadata::default()
            },
            None,
        )
        .await;
        seed_downloaded_marker(
            &db,
            "GPS_UNREADABLE",
            "unreadable.heic",
            &unreadable_path,
            "0000000000000000000000000000000000000000000000000000000000000000",
            AssetMetadata {
                metadata_hash: Some("unreadable-hash".into()),
                ..AssetMetadata::default()
            },
            None,
        )
        .await;

        let pass = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".gps-parse-retry-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            pass.applied, 1,
            "EXIF-less media must still complete its sidecar"
        );
        assert_eq!(
            pass.failed, 1,
            "an unreadable source must fail so the retry marker survives"
        );
        assert!(
            exif_less_path.with_file_name("exif-less.heic.xmp").exists(),
            "EXIF-less media must still publish its sidecar"
        );
        assert!(
            unreadable_path
                .with_file_name("unreadable.heic.xmp")
                .exists(),
            "an unreadable source still publishes the CloudKit-only sidecar"
        );

        let pending = db
            .get_pending_metadata_rewrites(10)
            .await
            .expect("read retry markers");
        assert_eq!(
            pending.len(),
            1,
            "only the unreadable source keeps its durable marker"
        );
        assert_eq!(
            pending[0].id.as_ref(),
            "GPS_UNREADABLE",
            "the retained marker must be the unreadable source"
        );
    }

    /// End-to-end test of the metadata-rewrite pass. Seeds a downloaded row
    /// with a `metadata_write_failed_at` marker, then proves the configured
    /// metadata is applied while durable download state remains coherent.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_applies_embed_and_clears_marker() {
        use crate::state::types::AssetMetadata;
        use crate::state::{AssetStatus, SqliteStateDb};

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("rewrite_target.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let seeded_checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();

        let seeded_hash = "seed_hash_before_rewrite".to_string();
        let metadata = AssetMetadata {
            rating: Some(4),
            timezone_offset: Some(39_600),
            metadata_hash: Some(seeded_hash.clone()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("REWRITE_1")
            .filename("rewrite_target.jpg")
            .checksum("rewrite_ck")
            .size(22)
            .created_at(
                chrono::Utc
                    .with_ymd_and_hms(2026, 1, 31, 22, 31, 59)
                    .unwrap(),
            )
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_1",
            "original",
            &photo_path,
            &seeded_checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "REWRITE_1", "original")
            .await
            .unwrap();

        // Sanity: the rewrite pass sees our row.
        let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(&*pending[0].id, "REWRITE_1");

        let flags = MetadataFlags::DATETIME | MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
        let token = CancellationToken::new();
        run_pending(&db, flags, Arc::from(".meta-tmp"), &token).await;

        // Marker must be gone; row must still be `downloaded`.
        let remaining = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert!(
            remaining.is_empty(),
            "marker must be cleared after successful rewrite"
        );
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 1);

        // metadata_hash must have been refreshed. We don't care what the
        // new hash value is - only that it reflects the rewrite pass ran
        // to completion (not the seeded placeholder).
        let hashes = db.get_downloaded_metadata_hashes().await.unwrap();
        let new_hash = hashes
            .get(&(
                "PrimarySync".to_string(),
                "REWRITE_1".to_string(),
                "original".to_string(),
            ))
            .expect("row must remain in the downloaded set");
        assert_eq!(
            new_hash, &seeded_hash,
            "a successful rewrite leaves the recorded metadata_hash in place"
        );

        let current_checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        assert_ne!(current_checksum, seeded_checksum);
        let downloaded = db.get_downloaded_page(0, 1).await.unwrap();
        assert_eq!(downloaded[0].checksum.as_ref(), "rewrite_ck");
        assert_eq!(
            downloaded[0].local_checksum.as_deref(),
            Some(current_checksum.as_str())
        );

        let probe = crate::download::metadata::probe_exif(&photo_path).unwrap();
        assert_eq!(
            probe.datetime_original.as_deref(),
            Some("2026-02-01T09:31:59")
        );
        assert_eq!(probe.offset_time_original.as_deref(), Some("+11:00"));

        let bytes = std::fs::read(&photo_path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("Rating") || text.contains("rating"),
            "embed should have written a Rating property into the JPEG"
        );

        // summary.downloaded == 1 above already proves the row stayed in
        // the downloaded state; AssetStatus is referenced here for
        // documentation and as an import check.
        let _ = AssetStatus::Downloaded;
    }

    #[cfg(feature = "xmp")]
    fn grouped_xmp(meta: &xmp_toolkit::XmpMeta) -> (Vec<String>, Vec<String>) {
        let keywords: Vec<String> = meta
            .property_array(xmp_toolkit::xmp_ns::DC, "subject")
            .map(|value| value.value)
            .collect();
        let people: Vec<String> = meta
            .property_array(xmp_toolkit::xmp_ns::IPTC_EXT, "PersonInImage")
            .map(|value| value.value)
            .collect();
        (keywords, people)
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_matches_normal_grouped_xmp_payload() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;
        use xmp_toolkit::{OpenFileOptions, XmpFile};

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("grouped.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let metadata = AssetMetadata {
            keywords: Some(r#"["beach"]"#.into()),
            metadata_hash: Some("grouped-hash".into()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("GROUPED")
            .filename("grouped.jpg")
            .metadata(metadata)
            .build();
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "GROUPED",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "GROUPED", "original")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "GROUPED", "Vacation", "icloud")
            .await
            .unwrap();
        db.add_asset_album("PrimarySync", "GROUPED", "beach", "icloud")
            .await
            .unwrap();
        {
            let conn = db.acquire_lock("seed person").unwrap();
            conn.execute(
                "INSERT INTO asset_people (library, asset_id, person_name) \
                 VALUES ('PrimarySync', 'GROUPED', 'Alice')",
                [],
            )
            .unwrap();
        }

        let grouping_rows = db
            .get_asset_groupings("PrimarySync", &["GROUPED"])
            .await
            .unwrap();
        let mut normal_groupings = AssetGroupings::default();
        for (asset_id, album) in grouping_rows.albums {
            normal_groupings
                .albums
                .entry(asset_id)
                .or_default()
                .push(album);
        }
        for (asset_id, person) in grouping_rows.people {
            normal_groupings
                .people
                .entry(asset_id)
                .or_default()
                .push(person);
        }
        let normal_payload = normal_groupings.metadata_payload("GROUPED", &record.metadata);
        assert_eq!(normal_payload.keywords, vec!["beach", "Vacation"]);
        assert_eq!(normal_payload.people, vec!["Alice"]);

        let normal_path = dir.path().join("normal.jpg");
        std::fs::write(&normal_path, minimal_jpeg_bytes()).unwrap();
        let normal_outcome = write_download_metadata(MetadataWriteRequest {
            final_path: &normal_path,
            embed_path: Some(&normal_path),
            expected_embed_fingerprint: None,
            source_checksum: None,
            sidecar_path: Some(&normal_path),
            payload: Arc::new(normal_payload),
            created_local: record.metadata.capture_local(record.created_at),
            flags: MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
            capture_timestamp_repair: CaptureTimestampRepair::Preserve,
            temp_suffix: ".meta-tmp",
        })
        .await;
        assert!(!normal_outcome.any_failed());

        let mut normal_file = XmpFile::new().unwrap();
        normal_file
            .open_file(&normal_path, OpenFileOptions::default().for_read())
            .unwrap();
        let normal_embedded = normal_file.xmp().expect("normal embedded XMP");
        let normal_sidecar = std::fs::read_to_string(dir.path().join("normal.jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(pass.applied, 1);

        let mut embedded_file = XmpFile::new().unwrap();
        embedded_file
            .open_file(&photo_path, OpenFileOptions::default().for_read())
            .unwrap();
        assert_eq!(
            grouped_xmp(&embedded_file.xmp().expect("rewritten embedded XMP")),
            grouped_xmp(&normal_embedded)
        );

        let sidecar = std::fs::read_to_string(dir.path().join("grouped.jpg.xmp"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(grouped_xmp(&sidecar), grouped_xmp(&normal_sidecar));
        assert_eq!(
            grouped_xmp(&sidecar),
            (
                vec!["beach".into(), "Vacation".into()],
                vec!["Alice".into()]
            )
        );
        assert!(
            db.get_pending_metadata_rewrites(1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn grouping_read_failure_keeps_marker_without_writing() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("grouping_read_failure.jpg");
        let before = minimal_jpeg_bytes();
        std::fs::write(&photo_path, &before).unwrap();
        let checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("GROUPING_READ_FAILURE")
            .filename("grouping_read_failure.jpg")
            .metadata(AssetMetadata {
                keywords: Some(r#"["beach"]"#.into()),
                ..AssetMetadata::default()
            })
            .build();
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "GROUPING_READ_FAILURE",
            "original",
            &photo_path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "GROUPING_READ_FAILURE", "original")
            .await
            .unwrap();
        db.acquire_lock("break grouping read")
            .unwrap()
            .execute("DROP TABLE asset_people", [])
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(pass.failed, 1);
        assert_eq!(std::fs::read(&photo_path).unwrap(), before);
        assert!(!dir.path().join("grouping_read_failure.jpg.xmp").exists());
        assert_eq!(db.get_pending_metadata_rewrites(1).await.unwrap().len(), 1);
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_applies_heic_rating_and_records_rewrite_checksums() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("rewrite_target.heic");
        let source = crate::download::heif::apple_tmap_insertion_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
            b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>",
        );
        std::fs::write(&photo_path, &source).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let seeded_checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("REWRITE_HEIC")
            .filename("rewrite_target.heic")
            .checksum("rewrite_heic_ck")
            .size(source.len() as u64)
            .metadata(AssetMetadata {
                rating: Some(5),
                metadata_hash: Some("heic_hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_HEIC",
            "original",
            &photo_path,
            &seeded_checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "REWRITE_HEIC", "original")
            .await
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::RATING | MetadataFlags::EMBED_XMP | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(pass.applied, 1);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty()
        );

        let bytes = std::fs::read(&photo_path).unwrap();
        let xmp = crate::download::heif::extract_xmp_bytes(&bytes).expect("HEIC XMP");
        let meta: xmp_toolkit::XmpMeta = std::str::from_utf8(&xmp).unwrap().parse().unwrap();
        assert_eq!(
            meta.property_i32(xmp_toolkit::xmp_ns::XMP, "Rating")
                .unwrap()
                .value,
            5
        );
        let sidecar = std::fs::read_to_string(photo_path.with_file_name("rewrite_target.heic.xmp"))
            .expect("read HEIC sidecar");
        let sidecar: xmp_toolkit::XmpMeta = sidecar.parse().expect("parse HEIC sidecar");
        crate::test_helpers::assert_source_gps_in_xmp(&sidecar);

        let (local_checksum, download_checksum) = stored_checksums(&db).await;
        assert_ne!(local_checksum.as_deref(), Some(seeded_checksum.as_str()));
        assert_eq!(download_checksum.as_deref(), Some(seeded_checksum.as_str()));
        assert!(
            !photo_path
                .with_file_name("rewrite_target.heic.meta-tmp")
                .exists()
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_keeps_marker_for_conflicting_tone_map_xmp_ownership() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("conflicting_tone_map_xmp.heic");
        let source = crate::download::heif::apple_tmap_conflicting_xmp_heic(
            &crate::test_helpers::minimal_tiff_with_source_gps(),
            b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description xmlns:HDRGainMap='http://ns.apple.com/HDRGainMap/1.0/' HDRGainMap:HDRGainMapHeadroom='2.67'/></rdf:RDF></x:xmpmeta>",
        );
        std::fs::write(&photo_path, &source).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let seeded_checksum = crate::download::file::compute_sha256(&photo_path)
            .await
            .unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("CONFLICTING_TMAP_XMP")
            .filename("conflicting_tone_map_xmp.heic")
            .checksum("conflicting_tmap_xmp_ck")
            .size(source.len() as u64)
            .metadata(AssetMetadata {
                rating: Some(5),
                metadata_hash: Some("conflicting_tmap_xmp_hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CONFLICTING_TMAP_XMP",
            "original",
            &photo_path,
            &seeded_checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "CONFLICTING_TMAP_XMP", "original")
            .await
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::RATING | MetadataFlags::EMBED_XMP,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(pass.applied, 0);
        assert_eq!(pass.failed, 1);
        assert_eq!(std::fs::read(&photo_path).unwrap(), source);
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
        let (local_checksum, download_checksum) = stored_checksums(&db).await;
        assert_eq!(local_checksum.as_deref(), Some(seeded_checksum.as_str()));
        assert_eq!(download_checksum, None);
        assert!(
            !photo_path
                .with_file_name("conflicting_tone_map_xmp.heic.meta-tmp")
                .exists()
        );
    }

    /// If the on-disk file has vanished between tagging and the rewrite
    /// pass, the pass must not error out. The marker stays, so a future
    /// sync that re-downloads the asset re-drives the writer.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn run_pending_skips_missing_file_and_leaves_marker() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let vanished_path = dir.path().join("never_written.jpg");

        let db = SqliteStateDb::open_in_memory().unwrap();

        let metadata = AssetMetadata {
            rating: Some(3),
            metadata_hash: Some("untouched_hash".to_string()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("MISSING_FILE")
            .filename("never_written.jpg")
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "MISSING_FILE",
            "original",
            &vanished_path,
            "checksum123",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "MISSING_FILE", "original")
            .await
            .unwrap();

        let flags = MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
        let token = CancellationToken::new();
        run_pending(&db, flags, Arc::from(".meta-tmp"), &token).await;

        let still_pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(
            still_pending.len(),
            1,
            "marker must survive when the file is absent so a future sync retries"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn cancel_returns_partial_and_keeps_retry_marker() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let photo_path = dir.path().join("rewrite_cancel.jpg");
        std::fs::write(&photo_path, minimal_jpeg_bytes()).unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        let metadata = AssetMetadata {
            rating: Some(5),
            metadata_hash: Some("retry_hash".to_string()),
            ..AssetMetadata::default()
        };
        let record = crate::test_helpers::TestAssetRecord::new("REWRITE_CANCEL")
            .filename("rewrite_cancel.jpg")
            .checksum("rewrite_cancel_ck")
            .metadata(metadata)
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "REWRITE_CANCEL",
            "original",
            &photo_path,
            "rewrite_cancel_ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "REWRITE_CANCEL", "original")
            .await
            .unwrap();

        let flags = MetadataFlags::RATING | MetadataFlags::EMBED_XMP;
        let token = CancellationToken::new();
        token.cancel();
        let deferred = run_pending(&db, flags, Arc::from(".meta-tmp"), &token)
            .await
            .failed;

        assert_eq!(
            deferred, 1,
            "cancelled metadata rewrite must count as a partial retryable item"
        );
        let still_pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(
            still_pending.len(),
            1,
            "cancelled metadata rewrite must keep marker for retry"
        );
    }

    #[tokio::test]
    async fn run_pending_batch_is_bounded() {
        use crate::state::SqliteStateDb;

        let db = SqliteStateDb::open_in_memory().unwrap();
        for i in 0..(METADATA_REWRITE_BATCH + 100) {
            let id = format!("A{i}");
            let record = crate::test_helpers::TestAssetRecord::new(&id).build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                std::path::Path::new("/nonexistent/missing.jpg"),
                "ck",
                None,
            )
            .await
            .unwrap();
            db.record_metadata_write_failure("PrimarySync", &id, "original")
                .await
                .unwrap();
        }

        let token = CancellationToken::new();
        let pass = run_pending(
            &db,
            MetadataFlags::RATING,
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert_eq!(
            pass.fetched, METADATA_REWRITE_BATCH,
            "one pass fetches at most a bounded batch, never the whole queue"
        );
        assert_eq!(pass.applied, 0, "missing files apply nothing");
    }

    #[tokio::test]
    async fn drain_scope_skips_unselected_and_soft_deleted_failures() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;

        let db = SqliteStateDb::open_in_memory().unwrap();
        for i in 0..3 {
            let id = format!("M{i}");
            let record = crate::test_helpers::TestAssetRecord::new(&id).build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                std::path::Path::new("/nonexistent/missing.jpg"),
                "ck",
                None,
            )
            .await
            .unwrap();
            db.record_metadata_write_failure("PrimarySync", &id, "original")
                .await
                .unwrap();
        }

        let invalid_dir = tempfile::tempdir().unwrap();
        let invalid_path = invalid_dir.path().join("invalid.jpg");
        std::fs::write(&invalid_path, b"not an image").unwrap();
        for (library, id, soft_deleted) in [
            ("SharedSync-OTHER", "UNSELECTED", false),
            ("PrimarySync", "SOFT_DELETED", true),
        ] {
            let record = crate::test_helpers::TestAssetRecord::new(id)
                .library(library)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(library, id, "original", &invalid_path, "ck", None)
                .await
                .unwrap();
            db.record_metadata_write_failure(library, id, "original")
                .await
                .unwrap();
            if soft_deleted {
                db.mark_soft_deleted(library, id, None).await.unwrap();
            }
        }

        let cfg = MetadataConfig {
            set_exif_rating: true,
            ..MetadataConfig::default()
        };
        let token = CancellationToken::new();
        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &cfg,
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert_eq!(
            residual, 0,
            "unselected and soft-deleted failures must not fail the selected repair"
        );
        let pending = db.get_pending_metadata_rewrites(32).await.unwrap();
        assert_eq!(pending.len(), 4);
        assert!(
            pending
                .iter()
                .any(|record| record.id.as_ref() == "UNSELECTED"),
            "unselected library marker must remain untouched"
        );
        assert!(
            !pending
                .iter()
                .any(|record| record.id.as_ref() == "SOFT_DELETED"),
            "a soft-deleted row must not be offered for rewrite, since its \
             metadata is frozen at the values held before the deletion"
        );
    }

    #[tokio::test]
    async fn drain_reports_residual_on_cancellation() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;

        let db = SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("C1").build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "C1",
            "original",
            std::path::Path::new("/x/c1.jpg"),
            "ck",
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "C1", "original")
            .await
            .unwrap();

        let cfg = MetadataConfig {
            set_exif_rating: true,
            ..MetadataConfig::default()
        };
        let token = CancellationToken::new();
        token.cancel();
        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &cfg,
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert!(
            residual >= 1,
            "a cancelled drain must report a non-zero residual so the sync exits non-zero"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_stops_when_rewrite_marker_cannot_be_cleared() {
        use crate::config::MetadataConfig;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("clear-failure.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let seeded_checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let db = crate::state::SqliteStateDb::open_in_memory().unwrap();
        let record = crate::test_helpers::TestAssetRecord::new("CLEAR_FAIL")
            .filename("clear-failure.jpg")
            .metadata(AssetMetadata {
                rating: Some(3),
                metadata_hash: Some("fresh-hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "CLEAR_FAIL",
            "original",
            &path,
            &seeded_checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "CLEAR_FAIL", "original")
            .await
            .unwrap();
        db.fail_metadata_marker_clear_for_test();

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db,
            &MetadataConfig {
                set_exif_rating: true,
                embed_xmp: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert_eq!(db.get_pending_metadata_rewrites(10).await.unwrap().len(), 1);
    }

    /// Seeds one downloaded asset carrying a rewrite marker into `db`, running
    /// the same sequence a real download failure leaves behind: `upsert_seen`,
    /// `mark_downloaded`, then `record_metadata_write_failure`. The caller
    /// supplies the on-disk path and its checksum so both readable media and
    /// deliberately unreadable sources can be staged.
    #[cfg(feature = "xmp")]
    async fn seed_downloaded_marker(
        db: &crate::state::SqliteStateDb,
        asset_id: &str,
        filename: &str,
        path: &std::path::Path,
        checksum: &str,
        metadata: crate::state::types::AssetMetadata,
        created_at: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        let mut record = crate::test_helpers::TestAssetRecord::new(asset_id).filename(filename);
        if let Some(created_at) = created_at {
            record = record.created_at(created_at);
        }
        let record = record.metadata(metadata).build();
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded("PrimarySync", asset_id, "original", path, checksum, None)
            .await
            .unwrap();
        db.record_metadata_write_failure("PrimarySync", asset_id, "original")
            .await
            .unwrap();
    }

    /// Seeds a downloaded JPEG carrying a rewrite marker. Returns the
    /// database, the media path, and the checksum recorded for the file.
    /// `rating` drives whether the embedded writer has anything to write.
    #[cfg(feature = "xmp")]
    async fn seed_marked_jpeg(
        dir: &std::path::Path,
        asset_id: &'static str,
        rating: Option<u8>,
    ) -> (crate::state::SqliteStateDb, PathBuf, String) {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let path = dir.join(format!("{asset_id}.jpg"));
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let recorded = crate::download::file::compute_sha256(&path).await.unwrap();

        // File backed so a test can reopen it and drop connection-scoped
        // failure triggers between passes.
        let db = SqliteStateDb::open(&dir.join("state.db")).await.unwrap();
        seed_downloaded_marker(
            &db,
            asset_id,
            &format!("{asset_id}.jpg"),
            &path,
            &recorded,
            AssetMetadata {
                rating,
                metadata_hash: Some("fresh-hash".to_string()),
                ..AssetMetadata::default()
            },
            None,
        )
        .await;

        (db, path, recorded)
    }

    #[cfg(feature = "xmp")]
    fn embedded_rating_flags() -> MetadataFlags {
        MetadataFlags::RATING | MetadataFlags::EMBED_XMP
    }

    #[cfg(feature = "xmp")]
    async fn stored_checksums(
        db: &crate::state::SqliteStateDb,
    ) -> (Option<String>, Option<String>) {
        let row = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        (row.local_checksum, row.download_checksum)
    }

    /// #707 review: the drain must not re-hash a file it did not write. A file
    /// that already drifted from its recorded checksum is damage `verify` and
    /// `reconcile` must keep reporting, so the rewrite is refused outright.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_refuses_to_rewrite_a_file_that_drifted_from_its_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "DRIFTED", Some(3)).await;
        // Trailing bytes after EOI keep the container readable, so the writer
        // would still accept the file if the drain offered it.
        let mut drifted_bytes = minimal_jpeg_bytes();
        drifted_bytes.push(0x00);
        std::fs::write(&path, &drifted_bytes).unwrap();

        let pass = run_pending_page(
            &db,
            embedded_rating_flags() | MetadataFlags::XMP_SIDECAR,
            CaptureTimestampRepair::Preserve,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
            None,
            0,
        )
        .await;

        assert_eq!(
            std::fs::read(&path).unwrap(),
            drifted_bytes,
            "a file that does not match its checksum must not be rewritten"
        );
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(".xmp");
        assert!(
            PathBuf::from(sidecar).exists(),
            "the sidecar is a separate file, so the export still runs"
        );
        assert_eq!(
            pass.failed, 1,
            "a refused rewrite is unfinished work, not a clean pass"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(recorded.as_str()),
            "the recorded checksum is the evidence of damage and must survive"
        );
        assert_eq!(download, None, "a refused rewrite records no download hash");
        assert_eq!(
            db.get_pending_metadata_rewrites(10).await.unwrap().len(),
            1,
            "the marker stays because the metadata never reached the file"
        );
    }

    /// #707 review: an embedded rewrite changes the media, so the row must
    /// carry the new hash and keep the pre-rewrite hash as the provider
    /// download checksum.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_records_the_rewritten_media_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "REWRITTEN", Some(3)).await;

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        assert_ne!(
            on_disk, recorded,
            "precondition: the rewrite must change the media bytes"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(local.as_deref(), Some(on_disk.as_str()));
        assert_eq!(download.as_deref(), Some(recorded.as_str()));
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "a complete rewrite retires its marker"
        );
    }

    /// #707 review: the checksum is stored before the marker retires, so a
    /// failure in between leaves a rewritten file the next pass can still
    /// recognise as its own.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_keeps_the_marker_when_the_rewritten_checksum_cannot_be_stored() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "CKFAIL", Some(3)).await;
        db.fail_metadata_checksum_write_for_test();

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        assert_ne!(on_disk, recorded, "precondition: the media was rewritten");
        assert_eq!(
            db.get_pending_metadata_rewrites(10).await.unwrap().len(),
            1,
            "the marker must survive so the rewrite is retried"
        );
        let (local, _) = stored_checksums(&db).await;
        assert_eq!(
            local, None,
            "a hash kei could not confirm must read as unknown, not as the              pre-rewrite value the next pass would treat as damage"
        );

        // A later pass, once the state write works again, must be able to
        // finish the job rather than refuse its own rewrite forever.
        let db = crate::state::SqliteStateDb::open(db.path()).await.unwrap();
        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let healed = crate::download::file::compute_sha256(&path).await.unwrap();
        let (local, _) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(healed.as_str()),
            "the retry must record the bytes on disk"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "the retry must retire the marker"
        );
    }

    /// #707 review: a sidecar-only rewrite leaves the media untouched, so it
    /// must not restate either checksum.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_leaves_checksums_untouched_for_a_sidecar_only_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "SIDECAR", Some(3)).await;
        let before = std::fs::read(&path).unwrap();

        run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a sidecar write must not touch the media"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(local.as_deref(), Some(recorded.as_str()));
        assert_eq!(
            download, None,
            "no media rewrite happened, so no pre-rewrite hash is established"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "the sidecar rewrite completed, so its marker retires"
        );
    }

    /// #682: a metadata-only drain must remove source fields that a prior kei
    /// sidecar write established, then retire the marker only after that
    /// updated sidecar lands.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_clears_previously_owned_sidecar_fields() {
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SIDECAR_CLEAR.jpg");
        std::fs::write(&path, minimal_jpeg_bytes()).unwrap();
        let checksum = crate::download::file::compute_sha256(&path).await.unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();

        let initial = crate::test_helpers::TestAssetRecord::new("SIDECAR_CLEAR")
            .filename("SIDECAR_CLEAR.jpg")
            .metadata(AssetMetadata {
                rating: Some(5),
                description: Some("Old description".into()),
                metadata_hash: Some("initial-hash".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&initial).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "SIDECAR_CLEAR",
            "original",
            &path,
            &checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "SIDECAR_CLEAR", "original")
            .await
            .unwrap();

        run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;
        let sidecar_path = dir.path().join("SIDECAR_CLEAR.jpg.xmp");
        let initial_sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(initial_sidecar.contains("Rating"));
        assert!(initial_sidecar.contains("Old description"));

        let cleared = crate::test_helpers::TestAssetRecord::new("SIDECAR_CLEAR")
            .filename("SIDECAR_CLEAR.jpg")
            .metadata(AssetMetadata {
                metadata_hash: Some("cleared-hash".into()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&cleared).await.unwrap();
        db.record_metadata_write_failure("PrimarySync", "SIDECAR_CLEAR", "original")
            .await
            .unwrap();

        let pass = run_pending(
            &db,
            MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(pass.applied, 1);
        assert_eq!(pass.failed, 0);
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "the marker retires after the cleared sidecar lands"
        );
        let cleared_sidecar = std::fs::read_to_string(&sidecar_path).unwrap();
        assert!(!cleared_sidecar.contains("Old description"));
        assert!(
            !cleared_sidecar.contains("xmp:Rating"),
            "the old rating and its ownership marker must both be gone"
        );
    }

    /// #707 review: a legacy row carries no checksum, so there is nothing to
    /// verify against and nothing to protect. The rewrite proceeds and
    /// establishes the baseline.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_establishes_a_checksum_baseline_when_none_was_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, _recorded) = seed_marked_jpeg(dir.path(), "LEGACY", Some(3)).await;
        db.clear_local_checksum_for_test("PrimarySync", "LEGACY", "original");

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(on_disk.as_str()),
            "the rewrite establishes the checksum a legacy row never had"
        );
        assert_eq!(
            download, None,
            "the pre-rewrite bytes were never verified, so kei cannot claim              them as the provider download"
        );
        assert!(
            db.get_pending_metadata_rewrites(10)
                .await
                .unwrap()
                .is_empty(),
            "a legacy row must not be stranded behind a missing checksum"
        );
        let (counts, _) = crate::commands::reconcile::scan_local_drift(
            &db,
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .unwrap();
        assert_eq!(
            counts.damaged, 1,
            "a legacy file short of its provider size must still read as damaged"
        );
    }

    /// #707 review: the rewritten hash is stored before the marker retires, so
    /// a failure later in the same attempt still leaves the row describing the
    /// bytes on disk. Without that, the next pass would read its own rewrite as
    /// damage and refuse to touch it again.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_records_the_rewritten_checksum_even_when_the_sidecar_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "SIDEFAIL", Some(3)).await;

        // A directory where the sidecar belongs fails the sidecar write while
        // leaving the embedded write free to change the media.
        let mut sidecar = path.clone().into_os_string();
        sidecar.push(".xmp");
        std::fs::create_dir(PathBuf::from(sidecar)).unwrap();

        run_pending(
            &db,
            embedded_rating_flags() | MetadataFlags::XMP_SIDECAR,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let on_disk = crate::download::file::compute_sha256(&path).await.unwrap();
        assert_ne!(
            on_disk, recorded,
            "precondition: the embedded write must land before the sidecar fails"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(
            local.as_deref(),
            Some(on_disk.as_str()),
            "the row must describe the rewritten bytes even though the attempt failed"
        );
        assert_eq!(download.as_deref(), Some(recorded.as_str()));
        assert_eq!(
            db.get_pending_metadata_rewrites(10).await.unwrap().len(),
            1,
            "the sidecar still owes a write, so the marker stays"
        );
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn capture_queue_pagination_counts_retired_capture_debt() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let seed_path = dir.path().join("seed.jpg");
        std::fs::write(&seed_path, minimal_jpeg_bytes()).unwrap();
        crate::download::metadata::apply_metadata(
            &seed_path,
            &crate::download::metadata::MetadataWrite {
                datetime: Some("2024:06:14 23:00:00".into()),
                offset_time_original: Some("+05:00".into()),
                ..crate::download::metadata::MetadataWrite::default()
            },
            ".seed-tmp",
        )
        .unwrap();
        let seed = std::fs::read(&seed_path).unwrap();
        let checksum = crate::download::file::compute_sha256(&seed_path)
            .await
            .unwrap();

        let db = SqliteStateDb::open_in_memory().unwrap();
        for index in 0..=METADATA_REWRITE_BATCH {
            let id = format!("A{index:04}");
            let path = dir.path().join(format!("{id}.jpg"));
            std::fs::write(&path, &seed).unwrap();
            let record = crate::test_helpers::TestAssetRecord::new(&id)
                .filename(&format!("{id}.jpg"))
                .checksum(&format!("provider-{id}"))
                .created_at(now_local().with_timezone(&chrono::Utc))
                .metadata(AssetMetadata {
                    timezone_offset: Some(39_600),
                    ..AssetMetadata::default()
                })
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded("PrimarySync", &id, "original", &path, &checksum, None)
                .await
                .unwrap();
            queue_capture_repair(&db, &record).await;
        }
        std::fs::create_dir(dir.path().join("A0000.jpg.xmp")).unwrap();

        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &MetadataConfig {
                set_exif_datetime: true,
                xmp_sidecar: true,
                ..MetadataConfig::default()
            },
            CaptureTimestampRepair::ReplaceWithCaptureLocal,
            &["PrimarySync"],
            Arc::from(".metadata-test"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(residual, 1);
        assert!(
            db.get_pending_metadata_rewrites_page_for_queue(
                MetadataRewriteQueue::CaptureRepair,
                None,
                0,
                METADATA_REWRITE_BATCH + 1,
            )
            .await
            .unwrap()
            .is_empty(),
            "every capture row, including the second page, must retire"
        );
        let ordinary = db
            .get_pending_metadata_rewrites(METADATA_REWRITE_BATCH + 1)
            .await
            .unwrap();
        assert_eq!(ordinary.len(), 1);
        assert_eq!(ordinary[0].id.as_ref(), "A0000");
        let last = crate::download::metadata::probe_exif(
            &dir.path().join(format!("A{METADATA_REWRITE_BATCH:04}.jpg")),
        )
        .unwrap();
        assert!(last.denotes_capture_time(&now_local()));
    }

    /// #707 review: a rewrite with nothing to write leaves the media alone, so
    /// the row must not gain a checksum for bytes kei never wrote. Vouching for
    /// an untouched file would hide damage that arrived some other way.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_writing_nothing_does_not_vouch_for_the_file() {
        let dir = tempfile::tempdir().unwrap();
        // No rating to write, and only the rating writer is enabled, so the
        // embedded plan comes out empty.
        let (db, path, recorded) = seed_marked_jpeg(dir.path(), "UNTOUCHED", None).await;
        let before = std::fs::read(&path).unwrap();

        run_pending(
            &db,
            MetadataFlags::RATING,
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "precondition: an empty plan must leave the media alone"
        );
        let (local, download) = stored_checksums(&db).await;
        assert_eq!(local.as_deref(), Some(recorded.as_str()));
        assert_eq!(
            download, None,
            "kei must not claim a pre-rewrite hash for a file it did not rewrite"
        );
    }

    /// #718 taught reconcile that a metadata-rewritten file is legitimately
    /// smaller than the provider size, proving it by hashing against
    /// `local_checksum`. That proof needs both checksums, so a drained file
    /// must not be reported as truncated damage.
    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drained_file_is_not_reported_as_truncated_damage() {
        let dir = tempfile::tempdir().unwrap();
        let (db, _path, _recorded) = seed_marked_jpeg(dir.path(), "SHRUNK", Some(3)).await;

        let (before, _) = stored_checksums(&db).await;
        let (counts, drift) = crate::commands::reconcile::scan_local_drift(
            &db,
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .unwrap();
        assert_eq!(
            counts.damaged, 1,
            "precondition: the provider size is larger than the file, so an \
             unexplained difference reads as damage"
        );
        assert_eq!(drift.len(), 1);

        run_pending(
            &db,
            embedded_rating_flags(),
            Arc::from(".meta-tmp"),
            &CancellationToken::new(),
        )
        .await;

        let (after, download) = stored_checksums(&db).await;
        assert_ne!(after, before, "precondition: the drain rewrote the media");
        assert!(
            download.is_some(),
            "reconcile needs the pre-rewrite hash to run its proof"
        );

        let (counts, drift) = crate::commands::reconcile::scan_local_drift(
            &db,
            |_: &crate::commands::reconcile::LocalDriftAsset| {},
            |_: &str| {},
        )
        .await
        .unwrap();
        assert_eq!(counts.present, 1);
        assert_eq!(counts.damaged, 0, "a drained file is intact, not truncated");
        assert!(drift.is_empty());
    }

    #[cfg(feature = "xmp")]
    #[tokio::test]
    async fn drain_reaches_newer_marker_after_retained_batch() {
        use crate::config::MetadataConfig;
        use crate::download::DownloadStore;
        use crate::state::SqliteStateDb;
        use crate::state::types::AssetMetadata;

        let dir = tempfile::tempdir().unwrap();
        let db = SqliteStateDb::open_in_memory().unwrap();
        let invalid_path = dir.path().join("invalid.jpg");
        std::fs::write(&invalid_path, b"not a jpeg").unwrap();
        let invalid_checksum = crate::download::file::compute_sha256(&invalid_path)
            .await
            .unwrap();
        for i in 0..METADATA_REWRITE_BATCH {
            let id = format!("A{i:04}");
            let metadata = AssetMetadata {
                rating: Some(3),
                metadata_hash: Some(format!("h{i}")),
                ..AssetMetadata::default()
            };
            let record = crate::test_helpers::TestAssetRecord::new(&id)
                .filename(&format!("{id}.jpg"))
                .metadata(metadata)
                .build();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                &id,
                "original",
                &invalid_path,
                &invalid_checksum,
                None,
            )
            .await
            .unwrap();
            db.record_metadata_write_failure("PrimarySync", &id, "original")
                .await
                .unwrap();
        }
        let valid_path = dir.path().join("valid.jpg");
        std::fs::write(&valid_path, minimal_jpeg_bytes()).unwrap();
        let valid_checksum = crate::download::file::compute_sha256(&valid_path)
            .await
            .unwrap();
        let valid = crate::test_helpers::TestAssetRecord::new("Z_VALID")
            .filename("valid.jpg")
            .metadata(AssetMetadata {
                rating: Some(3),
                metadata_hash: Some("valid-hash".to_string()),
                ..AssetMetadata::default()
            })
            .build();
        db.upsert_seen(&valid).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            "Z_VALID",
            "original",
            &valid_path,
            &valid_checksum,
            None,
        )
        .await
        .unwrap();
        db.record_metadata_write_failure("PrimarySync", "Z_VALID", "original")
            .await
            .unwrap();

        let cfg = MetadataConfig {
            set_exif_rating: true,
            embed_xmp: true,
            ..MetadataConfig::default()
        };
        let token = CancellationToken::new();
        let residual = crate::download::drain_pending_metadata_rewrites(
            &db as &dyn DownloadStore,
            &cfg,
            CaptureTimestampRepair::Preserve,
            &["PrimarySync"],
            std::sync::Arc::from(".meta-tmp"),
            &token,
        )
        .await;
        assert_eq!(residual, METADATA_REWRITE_BATCH);
        let pending = db
            .get_pending_metadata_rewrites(METADATA_REWRITE_BATCH + 1)
            .await
            .unwrap();
        assert_eq!(pending.len(), METADATA_REWRITE_BATCH);
        assert!(
            pending.iter().all(|record| record.id.as_ref() != "Z_VALID"),
            "retained older markers must not prevent newer work from completing"
        );
    }
}
