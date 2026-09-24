//! Capture-repair receipt fingerprints and completed timestamp verification.

use super::planning::offset_time_original;
use crate::download::filter::MetadataPayload;
use crate::state::db::CaptureRepairReceipt;
use chrono::{DateTime, FixedOffset};
use std::path::Path;
use std::sync::Arc;

pub(super) fn fingerprint_checksum(
    fingerprint: crate::download::file::ExistingFileFingerprint,
) -> String {
    data_encoding::HEXLOWER.encode(&fingerprint.sha256)
}

pub(super) fn receipt_matches_fingerprint(
    receipt: &CaptureRepairReceipt,
    fingerprint: crate::download::file::ExistingFileFingerprint,
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

pub(super) async fn capture_repair_is_verified(
    path: &Path,
    payload: Arc<MetadataPayload>,
    created_local: DateTime<FixedOffset>,
) -> bool {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Some(expected_offset) = offset_time_original(&payload) else {
            return false;
        };
        let Ok(probe) = crate::download::metadata::probe_exif(&path) else {
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
