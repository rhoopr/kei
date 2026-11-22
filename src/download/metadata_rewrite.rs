//! Metadata write orchestration for downloaded files and retry markers.
//!
//! The pipeline owns byte transfer and `.part` promotion. Planning owns opt-in
//! flags and embedded field decisions. Immediate writes coordinate download-time
//! execution; embedded and sidecar owners execute local writes. Queued execution
//! owns retry markers and retirement, with grouping and capture verification
//! supplied by focused owners. Child dependencies are one-way.
//!
//! Tests retain their names under `<owner>::tests` instead of `tests`.
//! Planning tests follow planning; writer tests follow embedded, sidecar, or
//! immediate execution; grouping-failure tests follow grouping; durable retry,
//! recovery, and drain tests follow queued execution. Shared fixtures live in
//! `test_support`.

mod capture;
mod embedded;
mod grouping;
mod immediate;
mod planning;
mod queued;
#[cfg(feature = "xmp")]
mod sidecar;

#[cfg(test)]
mod test_support;

pub(crate) use immediate::MetadataWriteOutcome;
pub(super) use immediate::{MetadataWriteRequest, write_download_metadata};
pub(super) use planning::MetadataFlags;
pub(crate) use planning::{CaptureTimestampRepair, writers_enabled};
pub(super) use queued::{
    RewritePass, run_backfill_rows, run_pending, run_pending_budget, run_pending_page,
    run_pending_rows, tag_if_needed,
};
#[cfg(feature = "xmp")]
pub(super) use sidecar::write_reconciled_sidecar;

#[cfg(all(test, feature = "xmp"))]
pub(in crate::download) use embedded::publication_pause;

/// Operational storage failures that make the local destination unusable for
/// metadata work. Capacity, read-only mounts, and access denial are distinct
/// from provider data, unsupported formats, and ordinary retryable metadata
/// errors: only the former become cycle health evidence.
pub(in crate::download) fn is_operational_storage_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(is_operational_storage_io)
}

pub(in crate::download) fn is_operational_storage_io(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::StorageFull
            | std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::PermissionDenied
    )
}

#[cfg(test)]
mod tests;
