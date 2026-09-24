//! Source-aware sidecar planning and execution, including reconciliation.

use super::capture::fingerprint_checksum;
use super::planning::{gps_from_payload, offset_time_original};
use crate::download::filter::MetadataPayload;
use chrono::{DateTime, FixedOffset};
use std::path::Path;
use std::sync::Arc;

#[cfg(feature = "xmp")]
pub(super) async fn write_sidecar_metadata(
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
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                error = %error, "XMP sidecar planning task panicked");
            return false;
        }
    };
    if write.is_empty() {
        return source_error.is_none();
    }
    match Box::pin(crate::download::metadata::write_sidecar(
        path,
        &write,
        temp_suffix,
    ))
    .await
    {
        Ok(()) => {
            let Some(error) = source_error else {
                return true;
            };
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                path = %log_path.display(),
                error = %error,
                "Source GPS read failed after sidecar publication; leaving marker for retry"
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                path = %log_path.display(), error = %e, "Failed to write XMP sidecar");
            false
        }
    }
}

/// Read source GPS before publishing any generated sidecar. A transient read
/// failure must not leave an incomplete packet that prevents a corrected retry.
#[cfg(feature = "xmp")]
pub(in crate::download) async fn write_reconciled_sidecar(
    copy: Arc<crate::download::file::ReconciledFile>,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    temp_suffix: String,
) -> anyhow::Result<Arc<crate::download::metadata::ReconciledSidecar>> {
    write_reconciled_sidecar_with_reader(
        copy,
        payload,
        created_local,
        temp_suffix,
        crate::download::metadata::read_source_gps_from_file,
    )
    .await
}

#[cfg(feature = "xmp")]
async fn write_reconciled_sidecar_with_reader(
    copy: Arc<crate::download::file::ReconciledFile>,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
    temp_suffix: String,
    read_gps: fn(
        &mut std::fs::File,
        &Path,
    ) -> anyhow::Result<crate::download::metadata::SourceGpsMetadata>,
) -> anyhow::Result<Arc<crate::download::metadata::ReconciledSidecar>> {
    tokio::task::spawn_blocking(move || {
        copy.validate_blocking()?;
        crate::download::metadata::write_reconciled_sidecar(
            &copy,
            || {
                let mut source = copy.open_source_for_metadata()?;
                let gps = read_gps(&mut source, copy.source.path())?;
                copy.validate_blocking()?;
                // Reconciliation does not invent original-byte provenance from a
                // recovered local checksum. Existing source packets are preserved.
                Ok(plan_sidecar_from_gps(
                    &payload,
                    &created_local,
                    gps,
                    NativeAccuracy::Unknown,
                    None,
                ))
            },
            &temp_suffix,
        )
    })
    .await?
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
