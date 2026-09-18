use std::sync::Arc;
use std::time::Instant;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::download::DownloadControls;
use crate::download::pipeline::outcome::{StreamingResult, build_download_outcome};
use crate::download::pipeline::streaming::{StreamRuntime, stream_and_download_from_stream};
use crate::download::pipeline::test_support::{FailingDownloadStore, build_zero_download_outcome};

/// When zero assets were downloaded but the producer saw enumeration
/// errors (e.g. malformed API page), `build_download_outcome` must
/// return `PartialFailure` — not `Success`. Before the fix, the
/// zero-download branch ignored `enumeration_errors`, letting the
/// sync-token advance and silently skipping the errored assets.
#[tokio::test]
async fn zero_downloads_with_enumeration_errors_returns_partial_failure() {
    use crate::download::DownloadOutcome;

    let streaming_result = StreamingResult {
        enumeration_errors: 3,
        ..StreamingResult::default()
    };
    let (outcome, stats) =
        build_zero_download_outcome(streaming_result, DownloadControls::download_hidden()).await;
    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 3 }),
        "expected PartialFailure with failed_count=3, got {outcome:?}"
    );
    assert_eq!(stats.enumeration_errors, 3);
}

#[tokio::test]
async fn producer_incomplete_enumeration_returns_partial_failure_and_blocks_token() {
    use crate::download::DownloadOutcome;

    let streaming_result = StreamingResult {
        assets_seen: 1,
        enumeration_complete: false,
        ..StreamingResult::default()
    };
    let (outcome, stats) =
        build_zero_download_outcome(streaming_result, DownloadControls::download_hidden()).await;

    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
        "incomplete producer enumeration must not report Success, got {outcome:?}"
    );
    assert!(stats.enumeration_incomplete);
    assert!(stats.sync_token_blocked);
    assert_eq!(
        stats.sync_token_blocked_reason,
        Some(crate::download::PRODUCER_ENUMERATION_INCOMPLETE_REASON)
    );
    assert!(
        !crate::sync_cycle::should_store_sync_token(&outcome, false),
        "partial incomplete-enumeration outcomes must not advance sync tokens"
    );
}

#[tokio::test]
async fn zero_downloads_with_state_write_failures_returns_partial_failure() {
    use crate::download::DownloadOutcome;

    let streaming_result = StreamingResult {
        state_write_failures: 1,
        ..StreamingResult::default()
    };
    let (outcome, stats) =
        build_zero_download_outcome(streaming_result, DownloadControls::download_hidden()).await;
    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
        "expected PartialFailure with failed_count=1, got {outcome:?}"
    );
    assert_eq!(stats.state_write_failures, 1);
}

#[tokio::test]
async fn expired_url_abort_returns_durable_partial_failure() {
    use crate::download::DownloadOutcome;

    let streaming_result = StreamingResult {
        downloaded: 1,
        url_expired_abort: true,
        ..StreamingResult::default()
    };
    let (outcome, stats) =
        build_zero_download_outcome(streaming_result, DownloadControls::download_hidden()).await;
    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
        "expired CDN URL must stop the batch as a PartialFailure, got {outcome:?}"
    );
    assert_eq!(stats.downloaded, 1);
    assert!(
        !stats.interrupted,
        "a durable transfer abort is not a process interruption"
    );
}

#[tokio::test]
async fn expired_url_abort_with_zero_downloads_is_not_success() {
    use crate::download::DownloadOutcome;

    let streaming_result = StreamingResult {
        url_expired_abort: true,
        ..StreamingResult::default()
    };
    let (outcome, stats) =
        build_zero_download_outcome(streaming_result, DownloadControls::download_hidden()).await;

    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
        "expired CDN URL before any success must not be reported as a clean no-op, got {outcome:?}"
    );
    assert_eq!(stats.downloaded, 0);
    assert!(!stats.interrupted);
}

#[tokio::test]
async fn complete_sync_run_failure_without_downloads_returns_partial_failure() {
    use crate::download::{DownloadConfig, DownloadOutcome};
    use crate::icloud::photos::PhotoAsset;
    use futures_util::stream;

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.state_db = Some(Arc::new(
        FailingDownloadStore::with_failing_complete_sync_run(),
    ));
    let config = Arc::new(config);
    let client = reqwest::Client::new();
    let controls = DownloadControls::download_hidden();

    let streaming_result = stream_and_download_from_stream(
        &client,
        stream::empty::<anyhow::Result<PhotoAsset>>(),
        &config,
        controls,
        0,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("empty sync should finish the stream pipeline");

    assert_eq!(streaming_result.downloaded, 0);
    assert_eq!(streaming_result.state_write_failures, 1);

    let (outcome, stats) = build_download_outcome(
        &client,
        &[],
        &config,
        controls,
        streaming_result,
        Instant::now(),
        CancellationToken::new(),
    )
    .await
    .expect("outcome should build");

    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 1 }),
        "complete_sync_run failure must not report Success, got {outcome:?}"
    );
    assert_eq!(stats.state_write_failures, 1);
}

#[tokio::test]
async fn dry_run_zero_downloads_with_enumeration_errors_returns_partial_failure() {
    use crate::download::DownloadOutcome;

    let streaming_result = StreamingResult {
        enumeration_errors: 2,
        ..StreamingResult::default()
    };
    let (outcome, stats) =
        build_zero_download_outcome(streaming_result, DownloadControls::dry_run_hidden()).await;
    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { failed_count: 2 }),
        "expected dry-run PartialFailure with failed_count=2, got {outcome:?}"
    );
    assert_eq!(stats.enumeration_errors, 2);
}
