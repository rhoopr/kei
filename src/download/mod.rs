//! Local file work: path rules, transfer and publication, and metadata
//! writers. The sync core drives these through `core_io`.

pub(crate) mod core_io;
pub mod error;
#[allow(
    dead_code,
    unused_imports,
    reason = "metadata writers wait for the opt-in metadata pass; it removes this allow"
)]
pub mod file;
pub(crate) mod filter;
#[cfg(feature = "xmp")]
#[allow(
    dead_code,
    unused_imports,
    reason = "metadata writers wait for the opt-in metadata pass; it removes this allow"
)]
pub(crate) mod heif;
pub(crate) mod limiter;
#[allow(
    dead_code,
    unused_imports,
    reason = "metadata writers wait for the opt-in metadata pass; it removes this allow"
)]
pub mod metadata;
pub mod paths;

pub(crate) use limiter::BandwidthLimiter;
pub(crate) use metadata::CaptureTimestampRepair;

pub use crate::sync_stats::SyncStats;
