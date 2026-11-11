//! Source-aware sidecar planning and execution.

use super::capture::fingerprint_checksum;
use super::planning::{gps_from_payload, offset_time_original};
use crate::download::filter::MetadataPayload;
use chrono::{DateTime, FixedOffset};
use std::path::Path;
use std::sync::Arc;

#[cfg(test)]
static TEST_SIDECAR_SUCCESSES: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<
            std::path::PathBuf,
            std::sync::Weak<std::sync::atomic::AtomicUsize>,
        >,
    >,
> = std::sync::OnceLock::new();
#[cfg(all(test, unix))]
pub(crate) fn test_sidecar_successes(path: &Path) -> Arc<std::sync::atomic::AtomicUsize> {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    TEST_SIDECAR_SUCCESSES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(path.to_owned(), Arc::downgrade(&count));
    count
}
#[cfg(test)]
fn record_test_sidecar_success(path: &Path) {
    if let Some(count) = TEST_SIDECAR_SUCCESSES.get().and_then(|counts| {
        counts
            .lock()
            .unwrap()
            .get(path)
            .and_then(std::sync::Weak::upgrade)
    }) {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(all(test, feature = "xmp"))]
static TEST_SIDECAR_ENTRIES: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<
            std::path::PathBuf,
            std::sync::Weak<std::sync::atomic::AtomicUsize>,
        >,
    >,
> = std::sync::OnceLock::new();

#[cfg(all(test, feature = "xmp"))]
pub(crate) fn test_sidecar_entries(path: &Path) -> Arc<std::sync::atomic::AtomicUsize> {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    TEST_SIDECAR_ENTRIES
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(path.to_owned(), Arc::downgrade(&count));
    count
}

#[cfg(all(test, feature = "xmp"))]
fn record_test_sidecar_entry(path: &Path) {
    if let Some(count) = TEST_SIDECAR_ENTRIES.get().and_then(|entries| {
        entries
            .lock()
            .unwrap()
            .get(path)
            .and_then(std::sync::Weak::upgrade)
    }) {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(feature = "xmp")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SidecarOutcome {
    pub(super) success: bool,
    pub(super) failed_sidecar_attempt: Option<crate::download::metadata::SidecarFailureBoundary>,
}

#[cfg(feature = "xmp")]
pub(super) async fn write_sidecar_metadata(
    path: &Path,
    namespace: Option<crate::fs_util::ConfinedPath>,
    require_created_entry: bool,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    source_checksum: Option<&str>,
    temp_suffix: &str,
) -> bool {
    Box::pin(write_sidecar_metadata_observed(
        path,
        namespace,
        require_created_entry,
        payload,
        created_local,
        source_checksum,
        temp_suffix,
    ))
    .await
    .success
}

#[cfg(feature = "xmp")]
pub(super) async fn write_sidecar_metadata_observed(
    path: &Path,
    namespace: Option<crate::fs_util::ConfinedPath>,
    require_created_entry: bool,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    source_checksum: Option<&str>,
    temp_suffix: &str,
) -> SidecarOutcome {
    #[cfg(test)]
    record_test_sidecar_entry(path);
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
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                error = %error, "XMP sidecar planning task panicked");
            return SidecarOutcome {
                success: false,
                failed_sidecar_attempt: None,
            };
        }
    };
    if let Some(error) = source_error.as_ref() {
        crate::support::metadata_error("source_gps", error, Some(false));
    }
    crate::support::observe_metadata(
        "support_metadata_v1",
        serde_json::json!({
            "operation":"sidecar", "source_fractional":created_local.timestamp_subsec_nanos()!=0,
            "planned_fractional":write.datetime.as_ref().is_some_and(|v|v.contains('.')),
            "timestamp_planned":write.datetime.is_some(),
        }),
    );
    if write.is_empty() {
        return SidecarOutcome {
            success: source_error.is_none(),
            failed_sidecar_attempt: None,
        };
    }
    let report = Box::pin(crate::download::metadata::write_sidecar_reported(
        path,
        namespace,
        require_created_entry,
        &write,
        temp_suffix,
    ))
    .await;
    match report.result {
        Ok(()) => {
            #[cfg(test)]
            record_test_sidecar_success(path);
            crate::support::observe_metadata(
                "support_metadata_v1",
                serde_json::json!({"operation":"sidecar","applied":true}),
            );
            let Some(error) = source_error else {
                return SidecarOutcome {
                    success: true,
                    failed_sidecar_attempt: None,
                };
            };
            crate::support::metadata_error("source_gps", &error, Some(true));
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                path = %log_path.display(),
                error = %error,
                "Source GPS read failed after sidecar publication; leaving marker for retry"
            );
            SidecarOutcome {
                success: false,
                failed_sidecar_attempt: None,
            }
        }
        Err(e) => {
            crate::support::metadata_error("sidecar_write", &e, None);
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                path = %log_path.display(), error = %format!("{e:#}"), "Failed to write XMP sidecar");
            SidecarOutcome {
                success: false,
                failed_sidecar_attempt: report.failure_boundary,
            }
        }
    }
}

pub(in crate::download) async fn create_layout_sidecar(
    root: &Path,
    plan_media: &Path,
    target: crate::fs_util::ConfinedPath,
    seed: Option<(&Path, &crate::state::db::primary_layout::LayoutFingerprint)>,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    source_checksum: Option<&str>,
    temp_suffix: &str,
) -> anyhow::Result<Option<crate::state::db::primary_layout::LayoutFingerprint>> {
    let stage_owned = plan_media.to_owned();
    let checksum = source_checksum.map(str::to_owned);
    let (write, source_error) = tokio::task::spawn_blocking(move || {
        plan_sidecar_write(&stage_owned, &payload, &created_local, checksum.as_deref())
    })
    .await?;
    if let Some(error) = source_error {
        return Err(error);
    }
    crate::download::metadata::create_layout_sidecar(root, target, seed, &write, temp_suffix).await
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
) -> (
    crate::download::metadata::MetadataWrite,
    Option<anyhow::Error>,
) {
    let (source_gps, mut source_error) = match crate::download::metadata::read_source_gps(path) {
        Ok(metadata) => (metadata, None),
        Err(error) => (
            crate::download::metadata::SourceGpsMetadata::default(),
            Some(error),
        ),
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
        match crate::download::file::fingerprint_regular_file_snapshot_blocking(path) {
            Ok(actual) => fingerprint_checksum(actual.fingerprint) == expected,
            Err(error) => {
                source_error = Some(error);
                false
            }
        }
    } else {
        false
    };
    let accuracy = if original_source {
        NativeAccuracy::Verified
    } else {
        NativeAccuracy::Unknown
    };
    (
        plan_sidecar_from_gps(
            payload,
            created_local,
            source_gps,
            accuracy,
            source_error.as_ref(),
        ),
        source_error,
    )
}

#[cfg(feature = "xmp")]
#[derive(PartialEq, Eq)]
enum NativeAccuracy {
    Verified,
    Unknown,
}

#[cfg(feature = "xmp")]
fn plan_sidecar_from_gps(
    payload: &MetadataPayload,
    created_local: &DateTime<FixedOffset>,
    source_gps: crate::download::metadata::SourceGpsMetadata,
    accuracy: NativeAccuracy,
    source_error: Option<&anyhow::Error>,
) -> crate::download::metadata::MetadataWrite {
    let mut write = crate::download::metadata::MetadataWrite {
        datetime: Some(created_local.format("%Y-%m-%dT%H:%M:%S%.f").to_string()),
        offset_time_original: offset_time_original(payload),
        gps_datetime: source_gps.datetime,
        gps_speed: source_gps.speed,
        gps_speed_ref: source_gps.speed_ref,
        gps_h_positioning_error: source_gps
            .horizontal_positioning_error
            .filter(|_| accuracy == NativeAccuracy::Verified),
        preserve_source_gps: source_error.is_some(),
        rating: payload.rating,
        gps: gps_from_payload(payload),
        is_hidden: payload.is_hidden,
        is_archived: payload.is_archived,
        ..crate::download::metadata::MetadataWrite::default()
    };
    write.title.clone_from(&payload.title);
    write.description.clone_from(&payload.description);
    write.keywords.clone_from(&payload.keywords);
    write.people.clone_from(&payload.people);
    write.media_subtype.clone_from(&payload.media_subtype);
    write.burst_id.clone_from(&payload.burst_id);
    write
}

#[cfg(test)]
mod tests;
