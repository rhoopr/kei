//! Metadata writes around download-time media publication.

use super::embedded::{EmbedWriteResult, write_embed_metadata};
use super::planning::{CaptureTimestampRepair, MetadataFlags};
#[cfg(feature = "xmp")]
use super::sidecar::write_sidecar_metadata;
use crate::download::filter::MetadataPayload;
use chrono::{DateTime, FixedOffset};
use std::path::Path;
use std::sync::Arc;

/// Result of metadata writes attempted for one downloaded file.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MetadataWriteOutcome {
    pub(super) embed_failed: bool,
    pub(super) embed_input_changed: bool,
    pub(super) embed_no_write: bool,
    pub(super) embed_output_fingerprint: Option<crate::download::file::ExistingFileFingerprint>,
    pub(super) sidecar_failed: bool,
}

impl MetadataWriteOutcome {
    pub(in crate::download) fn any_failed(self) -> bool {
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
pub(in crate::download) struct MetadataWriteRequest<'a> {
    pub(in crate::download) final_path: &'a Path,
    pub(in crate::download) embed_path: Option<&'a Path>,
    pub(in crate::download) expected_embed_fingerprint:
        Option<crate::download::file::ExistingFileFingerprint>,
    /// SHA-256 of the bytes before any kei metadata rewrite. Missing evidence
    /// permits provider metadata, but not a native horizontal accuracy claim.
    #[cfg_attr(
        not(feature = "xmp"),
        allow(dead_code, reason = "native-only builds do not write sidecars")
    )]
    pub(in crate::download) source_checksum: Option<&'a str>,
    #[cfg_attr(not(feature = "xmp"), allow(dead_code))]
    pub(in crate::download) sidecar_path: Option<&'a Path>,
    pub(in crate::download) payload: Arc<MetadataPayload>,
    pub(in crate::download) created_local: DateTime<FixedOffset>,
    pub(in crate::download) flags: MetadataFlags,
    pub(in crate::download) capture_timestamp_repair: CaptureTimestampRepair,
    pub(in crate::download) temp_suffix: &'a str,
}

/// Apply opt-in metadata writes for a single file.
///
/// The caller remains responsible for transfer, mtime, `.part` promotion,
/// counters, and final state writes.
pub(in crate::download) async fn write_download_metadata(
    request: MetadataWriteRequest<'_>,
) -> MetadataWriteOutcome {
    // CONTRACT: METADATA_WRITES_REQUIRE_OPT_IN
    let mut outcome = MetadataWriteOutcome::default();

    if request.flags.any_embed()
        && let Some(embed_path) = request.embed_path
        && (request.expected_embed_fingerprint.is_some()
            || (crate::download::metadata::is_embed_writable_path(request.final_path)
                && (embed_path == request.final_path
                    || crate::download::metadata::is_embed_writable_path(embed_path))))
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

#[cfg(test)]
mod tests;
