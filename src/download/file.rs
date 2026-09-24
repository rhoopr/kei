//! File transfer, validation, and safe publication.
//!
//! Transfer owns HTTP retries and temporary-file writes. Validation owns media
//! and response checks; fingerprinting supplies same-read byte evidence.
//! Publication handles no-overwrite collisions. Replacement owns stable-input
//! checks and restoration. Reconciliation retains confined file capabilities.
//! Platform primitives stay below these policies. Child dependencies are one-way.
//!
//! Existing tests keep their names under `<owner>::tests`; HTTP wiremock tests
//! move to `transfer::tests::wiremock_tests`. Shared media bytes live in
//! `test_support`. The external facade paths and visibility stay unchanged.

mod fingerprint;
mod platform;
mod publication;
mod reconciliation;
mod replacement;
mod transfer;
mod validation;

#[cfg(test)]
mod test_support;

pub(super) use fingerprint::ExistingFileFingerprint;
pub(crate) use fingerprint::compute_sha256;
pub(super) use fingerprint::fingerprint_regular_file;
pub(super) use publication::publish_part_to_final;
#[cfg(test)]
pub(super) use publication::rename_part_to_final;
#[cfg_attr(
    not(feature = "xmp"),
    allow(
        unused_imports,
        reason = "Preserve the file facade type without XMP callers"
    )
)]
pub(crate) use reconciliation::ReconciledFile;
pub(crate) use reconciliation::copy_local_file_no_replace;
pub(super) use reconciliation::validate_reconciliation_paths;
pub(super) use replacement::ConditionalPublishTargetChanged;
pub(super) use replacement::FinalPublication;
pub(super) use replacement::classify_conditional_publish_error;
#[cfg(feature = "xmp")]
pub(super) use replacement::publish_file_if_unchanged;
pub(super) use replacement::publish_file_if_unchanged_blocking;
pub(super) use transfer::DownloadClient;
pub(super) use transfer::DownloadLimits;
pub(super) use transfer::DownloadOpts;
pub(super) use transfer::download_file_with_mode;
pub(super) use transfer::temp_download_path;
pub(crate) use validation::LocalFileSizeExpectation;
pub(crate) use validation::local_file_size_matches_state;

// These paths remain available even when current sibling callers infer their types.
#[allow(
    unused_imports,
    reason = "Preserve the existing file facade API and visibility"
)]
pub(super) use self::{
    fingerprint::ExistingFileSnapshot, fingerprint::fingerprint_file,
    fingerprint::fingerprint_regular_file_snapshot_blocking, platform::PublishResult,
    platform::publish_reconciliation_part_blocking, publication::FinalPathCollision,
    replacement::ConditionalPublishErrorDisposition, transfer::DownloadResponse,
};
