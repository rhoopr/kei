//! Streaming download facade. Preserves the download-scoped entry points.
//!
//! `streaming` composes producer, consumer, and outcome owners. `adoption`
//! owns local-file evidence, `task` owns one transfer, `pass` runs retry tasks,
//! and `progress` owns summary formatting.

use std::sync::Arc;

use indicatif::ProgressBar;
use tokio_util::sync::CancellationToken;

use crate::download::{DownloadConfig, DownloadStore};

mod adoption;
mod consumer;
mod outcome;
mod pass;
mod producer;
mod progress;
mod streaming;
mod task;

#[cfg(test)]
mod test_support;

pub(super) use crate::download::metadata_rewrite::MetadataFlags;
pub(super) use adoption::{
    PendingRetryAdoption, PendingRetryFileEvidence, PendingRetryLocalPath,
    adopt_pending_on_disk_for_retry, recorded_current_path_exists,
    state_confirmed_current_path_exists,
};
pub(super) use outcome::{StreamingResult, build_download_outcome};
pub(super) use pass::{PassConfig, PassResult, run_download_pass};
#[expect(
    unused_imports,
    reason = "preserve the existing download-scoped facade paths"
)]
pub(super) use producer::{FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES, ProducerSkipSummary};
pub(super) use progress::{format_duration, log_sync_summary};
#[expect(
    unused_imports,
    reason = "preserve the existing download-scoped facade path"
)]
pub(super) use streaming::stream_and_download_from_stream_with_context;
pub(super) use streaming::{StreamRuntime, stream_and_download_from_stream};
pub(super) use task::AUTH_ERROR_THRESHOLD;

#[derive(Clone)]
struct StreamPipelineShared {
    config: Arc<DownloadConfig>,
    state_db: Option<Arc<dyn DownloadStore>>,
    pb: ProgressBar,
    pipeline_shutdown: CancellationToken,
}
