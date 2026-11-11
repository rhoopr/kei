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
pub(in crate::download) use task::{DownloadSingleContext, download_single_task};

#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::{recover_bound_publication_for_test, run_bound_publication_for_test};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MediaFailureCause {
    Transfer,
    ExpiredUrl,
}

/// A typed failure observed in this execution and its committed state successor.
#[derive(Clone)]
pub(crate) struct AccountedMediaFailure {
    pub(crate) cause: MediaFailureCause,
    pub(crate) successor: crate::state::db::provider_generations::DeferredMediaSuccessor,
    pub(in crate::download) task_key: crate::download::orchestration::url_refresh::RetryTaskKey,
}

impl AccountedMediaFailure {
    pub(crate) fn task_identity(&self) -> &(impl Eq + std::hash::Hash) {
        &self.task_key
    }

    pub(crate) fn matches_source(&self, source: &str) -> bool {
        let captured = &self.successor.captured;
        self.task_key.library.as_ref() == source
            && self.task_key.asset_id.as_ref() == captured.state_id
            && self.task_key.version_size.as_str() == captured.destination.version_size
            && self.task_key.download_path == captured.destination.path.to_path()
    }
}

impl std::fmt::Debug for AccountedMediaFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountedMediaFailure")
            .field("cause", &self.cause)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub(in crate::download) struct CapturedMediaFailure {
    pub(in crate::download) task_key: crate::download::orchestration::url_refresh::RetryTaskKey,
    pub(in crate::download) generation: String,
    pub(in crate::download) specification_hash: String,
    pub(in crate::download) pass_key: String,
    pub(in crate::download) child: String,
    pub(in crate::download) state_id: String,
    pub(in crate::download) manifest_hash: String,
    pub(in crate::download) admission_hash: String,
    pub(in crate::download) destination: crate::state::db::provider_selection::SelectionDestination,
}

#[derive(Clone, Debug)]
pub(in crate::download) struct MediaFailureTask {
    pub(in crate::download) task: crate::download::filter::DownloadTask,
    pub(in crate::download) captured: Option<std::sync::Arc<CapturedMediaFailure>>,
    pub(in crate::download) cause: MediaFailureCause,
}

#[derive(Clone, Debug)]
pub(in crate::download) struct FailedFailureWrite {
    pub(in crate::download) task_key: crate::download::orchestration::url_refresh::RetryTaskKey,
}

#[must_use]
pub(in crate::download) fn media_failure_cause(error: &anyhow::Error) -> Option<MediaFailureCause> {
    use crate::download::error::DownloadError;
    let download = error.downcast_ref::<DownloadError>()?;
    match download {
        DownloadError::HttpStatus {
            status: 401 | 403, ..
        } => None,
        DownloadError::HttpStatus { status: 410, .. } => Some(MediaFailureCause::ExpiredUrl),
        DownloadError::HttpStatus { .. }
        | DownloadError::Http { .. }
        | DownloadError::ContentLengthMismatch { .. }
        | DownloadError::InvalidContent { .. } => Some(MediaFailureCause::Transfer),
        DownloadError::Interrupted { .. } | DownloadError::Disk(_) | DownloadError::Other(_) => {
            None
        }
    }
}

pub(super) use crate::download::metadata_rewrite::MetadataFlags;
pub(super) use adoption::{
    PendingRetryAdoption, mark_reserved_pending_task, recover_current_pending_publication,
    validate_enumerated_reserved_pending,
};
pub(super) use outcome::{StreamingResult, build_download_outcome, build_download_result};
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
