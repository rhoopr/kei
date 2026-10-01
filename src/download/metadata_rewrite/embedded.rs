//! Prepare and publish fingerprint-guarded embedded metadata writes.

use super::planning::{
    CaptureTimestampRepair, MetadataFlags, offset_time_original, plan_metadata_write_with_repair,
};
use crate::download::filter::MetadataPayload;
use chrono::{DateTime, FixedOffset};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EmbedWriteResult {
    Applied(Option<crate::download::file::ExistingFileFingerprint>),
    NoWrite,
    Failed,
    InputChanged,
}

pub(super) enum EmbedPrepareResult {
    Prepared(crate::download::metadata::PreparedMetadataFile),
    NoWrite,
    Failed,
    InputChanged,
}

pub(super) async fn prepare_embed_metadata(
    path: &Path,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
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
                target: "kei::download::metadata_rewrite",
                path = %embed_path.display(),
                "Capture timestamp repair has no usable offset; leaving the rewrite marker pending"
            );
            return EmbedPrepareResult::Failed;
        }
        let probe = match crate::download::metadata::probe_exif(&embed_path) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
                    path = %embed_path.display(), error = %e, "Failed to read EXIF");
                crate::download::metadata::ExifProbe::default()
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
                    target: "kei::download::metadata_rewrite",
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
                        target: "kei::download::metadata_rewrite",
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
        match crate::download::metadata::prepare_metadata_with_expected_fingerprint(
            &embed_path,
            &write,
            &metadata_temp_suffix,
            expected_fingerprint,
        ) {
            Err(e) => {
                let disposition = crate::download::file::classify_conditional_publish_error(&e);
                let result = if disposition.target_changed {
                    EmbedPrepareResult::InputChanged
                } else {
                    EmbedPrepareResult::Failed
                };
                tracing::warn!(
                    target: "kei::download::metadata_rewrite",
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
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                error = %e, "EXIF task panicked");
            EmbedPrepareResult::Failed
        }
    }
}

pub(super) async fn publish_embed_metadata(
    path: &Path,
    prepared: crate::download::metadata::PreparedMetadataFile,
) -> EmbedWriteResult {
    let embed_path = path.to_path_buf();
    match tokio::task::spawn_blocking(move || prepared.publish(&embed_path)).await {
        Ok(Ok(output_fingerprint)) => EmbedWriteResult::Applied(Some(output_fingerprint)),
        Ok(Err(error)) => {
            let disposition = crate::download::file::classify_conditional_publish_error(&error);
            let result = if disposition.target_changed {
                EmbedWriteResult::InputChanged
            } else {
                EmbedWriteResult::Failed
            };
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                path = %path.display(),
                error = %error,
                retained_paths = ?disposition.retained_paths,
                "Failed to publish metadata"
            );
            result
        }
        Err(error) => {
            tracing::warn!(
                target: "kei::download::metadata_rewrite",
                error = %error, "EXIF publication task panicked");
            EmbedWriteResult::Failed
        }
    }
}

pub(super) async fn write_embed_metadata(
    path: &Path,
    expected_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
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

#[cfg(test)]
mod tests;
