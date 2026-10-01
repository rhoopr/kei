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
pub(super) use queued::{RewritePass, run_pending, run_pending_page, tag_if_needed};
#[cfg(feature = "xmp")]
pub(super) use sidecar::write_reconciled_sidecar;
