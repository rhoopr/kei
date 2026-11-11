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

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub(in crate::download) use embedded::{EmbedPrepareResult, prepare_embed_metadata};
pub(crate) use immediate::MetadataWriteOutcome;
pub(super) use immediate::{MetadataWriteRequest, write_download_metadata};
pub(crate) use planning::CaptureTimestampRepair;
pub(super) use planning::MetadataFlags;
pub(crate) use queued::MetadataWriterWitness;
#[allow(
    unused_imports,
    reason = "tests retain the direct bounded drain entry points"
)]
pub(super) use queued::{
    RewritePass, TailAttempts, run_pending, run_pending_budget_tracked, run_pending_page,
    run_pending_page_witnessed, run_pending_rows_tracked, run_pending_witnessed,
};
#[cfg(feature = "xmp")]
pub(in crate::download) use sidecar::create_layout_sidecar;

#[cfg(all(test, feature = "xmp"))]
pub(in crate::download) use embedded::publication_pause;

#[cfg(all(test, feature = "xmp"))]
pub(crate) use crate::download::metadata::test_sidecar_attempts;
#[cfg(all(test, feature = "xmp", unix))]
pub(crate) use crate::download::metadata::{
    TestPreparationFault, test_corrupt_publication_once, test_fail_before_sidecar_probe_once,
    test_fail_sidecar_preparation_once, test_fail_source_gps_once,
};

#[cfg(all(test, feature = "xmp"))]
pub(super) use queued::run_pending_budget;

#[cfg(all(test, feature = "xmp"))]
type TailHook = std::sync::Arc<dyn Fn(bool) + Send + Sync>;
#[cfg(all(test, feature = "xmp"))]
static TEST_TAIL_HOOKS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, TailHook>>,
> = std::sync::OnceLock::new();
#[cfg(all(test, feature = "xmp"))]
pub(crate) fn test_metadata_tail_hook(root: &std::path::Path, hook: TailHook) {
    TEST_TAIL_HOOKS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(root.to_owned(), hook);
}
#[cfg(all(test, feature = "xmp"))]
pub(super) fn fire_test_metadata_tail_hook(root: &std::path::Path, after_priority: bool) {
    let hook = TEST_TAIL_HOOKS
        .get()
        .and_then(|hooks| hooks.lock().unwrap().get(root).cloned());
    if let Some(hook) = hook {
        hook(after_priority);
    }
}
#[cfg(all(test, feature = "xmp"))]
static TEST_DENY_ONCE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
> = std::sync::OnceLock::new();
#[cfg(all(test, feature = "xmp", unix))]
pub(crate) fn test_deny_metadata_writer_once(path: &std::path::Path) {
    TEST_DENY_ONCE
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(path.to_owned());
}
#[cfg(all(test, feature = "xmp"))]
pub(super) fn take_test_writer_denial(path: &std::path::Path) -> bool {
    TEST_DENY_ONCE
        .get()
        .is_some_and(|paths| paths.lock().unwrap().remove(path))
}

#[cfg(all(test, feature = "xmp"))]
pub(crate) use queued::test_metadata_suppressions;

#[cfg(all(test, feature = "xmp", unix))]
pub(crate) async fn test_metadata_budget<F: std::future::Future>(
    budget: usize,
    future: F,
) -> F::Output {
    crate::download::orchestration::generation::TEST_METADATA_WRITER_BUDGET
        .scope(budget, future)
        .await
}

#[cfg(all(test, feature = "xmp"))]
pub(crate) use sidecar::test_sidecar_entries;
#[cfg(all(test, feature = "xmp", unix))]
pub(crate) use sidecar::test_sidecar_successes;

#[cfg(all(test, feature = "xmp", unix))]
pub(crate) use crate::download::file::test_sidecar_publications;
