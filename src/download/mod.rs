//! Local file work: path rules, transfer and publication, and metadata
//! writers. The sync core drives these through `core_io`.

pub(crate) mod core_io;
pub mod error;
pub mod file;
pub(crate) mod filter;
#[cfg(feature = "xmp")]
pub(crate) mod heif;
pub(crate) mod limiter;
pub mod metadata;
pub(crate) mod metadata_pass;
pub mod paths;

pub(crate) use limiter::BandwidthLimiter;
pub(crate) use metadata::CaptureTimestampRepair;

pub use crate::sync_stats::SyncStats;
