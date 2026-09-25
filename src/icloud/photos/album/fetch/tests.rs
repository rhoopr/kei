use crate::icloud::photos::album::planning::PhotoStreamProfile;
use crate::icloud::photos::album::test_support::{
    default_zone, drain_photo_stream, drain_photo_stream_count, make_album,
    make_album_with_session, test_asset_record, test_asset_record_for, test_master_record,
};
use crate::test_helpers::{MockPhotosFlow, MockPhotosSession, mock_photo_query_page};
use serde_json::json;
use std::sync::Arc;

use super::log_fetcher_response;

#[test]
fn test_list_query_ascending_offset_zero() {
    let album = make_album(200, None, default_zone());
    let q = album.list_query(0, "ASCENDING");
    let filters = q["query"]["filterBy"].as_array().unwrap();
    assert_eq!(filters.len(), 2);
    assert_eq!(filters[0]["fieldValue"]["value"], json!(0));
    assert_eq!(filters[1]["fieldValue"]["value"], "ASCENDING");
}

#[test]
fn test_list_query_with_offset() {
    let album = make_album(200, None, default_zone());
    let q = album.list_query(42, "ASCENDING");
    assert_eq!(q["query"]["filterBy"][0]["fieldValue"]["value"], json!(42));
}

#[test]
fn test_list_query_results_limit_double_page_size() {
    let album = make_album(100, None, default_zone());
    let q = album.list_query(0, "ASCENDING");
    assert_eq!(q["resultsLimit"], json!(200));
}

#[test]
fn test_list_query_with_extra_filter() {
    let extra = json!([{"fieldName": "albumName", "comparator": "EQUALS", "fieldValue": {"type": "STRING", "value": "Favorites"}}]);
    let album = make_album(200, Some(Arc::new(extra)), default_zone());
    let q = album.list_query(0, "ASCENDING");
    let filters = q["query"]["filterBy"].as_array().unwrap();
    assert_eq!(filters.len(), 3);
    assert_eq!(filters[2]["fieldName"], "albumName");
}

#[test]
fn test_list_query_zone_id_passed_through() {
    let zone = json!({"zoneName": "CustomZone"});
    let album = make_album(200, None, zone.clone());
    let q = album.list_query(0, "ASCENDING");
    assert_eq!(q["zoneID"], zone);
}

#[tokio::test]
async fn offline_replay_full_pass_fixture() {
    let mock = MockPhotosFlow::new()
        .query_photo_page("master-replay-full", Some("token-full"))
        .empty_query_page(Some("token-full"))
        .build();
    let album = make_album_with_session(100, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(1), 1);

    assert_eq!(drain_photo_stream_count(stream).await, 1);
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("token-full")
    );
}

#[tokio::test]
async fn offline_replay_paginated_full_pass_fixture() {
    let mock = MockPhotosFlow::new()
        .query_photo_page("master-replay-page-1", Some("token-page-1"))
        .query_photo_page("master-replay-page-2", Some("token-page-2"))
        .empty_query_page(Some("token-page-2"))
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 1);

    assert_eq!(drain_photo_stream_count(stream).await, 2);
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("token-page-2")
    );
}

#[tokio::test]
async fn offline_replay_empty_page_probe_fixture() {
    let mock = MockPhotosFlow::new()
        .query_photo_page("master-before-gap", None)
        .empty_query_page(None)
        .query_photo_page("master-after-gap", None)
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, _handles) = album.photo_stream_inner(
        None,
        None,
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        None,
        false,
        false,
    );

    assert_eq!(
        drain_photo_stream_count(stream).await,
        2,
        "single empty records/query page must be treated as a gap, not EOF"
    );
}

#[tokio::test]
async fn full_query_asset_before_master_cross_page_emits_asset_once() {
    let mock = MockPhotosFlow::new()
        .query_page(
            vec![test_asset_record("master-cross-page")],
            Some("token-full"),
        )
        .query_page(
            vec![test_master_record("master-cross-page")],
            Some("token-full"),
        )
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 1);
    let (ids, errors) = drain_photo_stream(stream).await;

    assert_eq!(ids, vec!["master-cross-page"]);
    assert!(errors.is_empty(), "unexpected stream errors: {errors:?}");
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("token-full")
    );
}

#[tokio::test]
async fn full_query_sibling_assets_share_master_without_clobbering() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosFlow::new()
        .query_page(
            vec![
                test_asset_record_for("asset-sibling-b", "master-sibling"),
                test_master_record("master-sibling"),
                test_asset_record_for("asset-sibling-a", "master-sibling"),
            ],
            Some("token-full"),
        )
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 1);
    tokio::pin!(stream);
    let mut seen = Vec::new();
    while let Some(result) = stream.next().await {
        let asset = result.expect("sibling asset should parse");
        seen.push((
            asset.id().to_string(),
            asset.asset_record_name().to_string(),
            asset.state_id().to_string(),
        ));
    }

    assert_eq!(
        seen,
        vec![
            (
                "master-sibling".to_string(),
                "asset-sibling-b".to_string(),
                "master-sibling".to_string(),
            ),
            (
                "master-sibling".to_string(),
                "asset-sibling-a".to_string(),
                "asset-sibling-a".to_string(),
            ),
        ]
    );
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("token-full")
    );
}

#[tokio::test]
async fn full_query_late_earlier_sibling_keeps_first_seen_master_state_id() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosFlow::new()
        .query_page(
            vec![
                test_asset_record_for("asset-late-b", "master-late-earlier"),
                test_master_record("master-late-earlier"),
            ],
            Some("token-full"),
        )
        .query_page(
            vec![test_asset_record_for("asset-late-a", "master-late-earlier")],
            Some("token-full"),
        )
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 1);
    tokio::pin!(stream);
    let mut seen = Vec::new();
    while let Some(result) = stream.next().await {
        let asset = result.expect("late sibling asset should parse");
        seen.push((
            asset.id().to_string(),
            asset.asset_record_name().to_string(),
            asset.state_id().to_string(),
        ));
    }

    assert_eq!(
        seen,
        vec![
            (
                "master-late-earlier".to_string(),
                "asset-late-b".to_string(),
                "master-late-earlier".to_string(),
            ),
            (
                "master-late-earlier".to_string(),
                "asset-late-a".to_string(),
                "asset-late-a".to_string(),
            ),
        ]
    );
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("token-full")
    );
}

#[tokio::test]
async fn full_query_late_sibling_asset_pairs_with_recent_master() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosFlow::new()
        .query_page(
            vec![
                test_master_record("master-late-sibling"),
                test_asset_record_for("asset-late-a", "master-late-sibling"),
            ],
            Some("token-full"),
        )
        .query_page(
            vec![test_asset_record_for("asset-late-b", "master-late-sibling")],
            Some("token-full"),
        )
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 1);
    tokio::pin!(stream);
    let mut seen = Vec::new();
    while let Some(result) = stream.next().await {
        let asset = result.expect("late sibling asset should parse");
        seen.push((
            asset.id().to_string(),
            asset.asset_record_name().to_string(),
            asset.state_id().to_string(),
        ));
    }

    assert_eq!(
        seen,
        vec![
            (
                "master-late-sibling".to_string(),
                "asset-late-a".to_string(),
                "master-late-sibling".to_string(),
            ),
            (
                "master-late-sibling".to_string(),
                "asset-late-b".to_string(),
                "asset-late-b".to_string(),
            ),
        ]
    );
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("token-full")
    );
}

#[tokio::test]
async fn full_query_unpaired_records_block_token() {
    let mock = MockPhotosFlow::new()
        .query_page(
            vec![test_asset_record("master-unpaired")],
            Some("token-full"),
        )
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(1), 1);
    let (ids, errors) = drain_photo_stream(stream).await;

    assert!(ids.is_empty());
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("unpaired CPLMaster records and 1 unpaired CPLAsset records"),
        "unexpected error: {}",
        errors[0]
    );
    assert_eq!(
        token_rx.await.expect("sync token sender"),
        None,
        "unpaired records must suppress the sync token"
    );
}

#[tokio::test]
async fn full_query_consecutive_empty_pages_prove_eof() {
    let mock = MockPhotosFlow::new()
        .query_photo_page("master-before-empty-tail", Some("token-full"))
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
    let (ids, errors) = drain_photo_stream(stream).await;

    assert_eq!(ids, vec!["master-before-empty-tail"]);
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("token-full"),
        "the finite consecutive-empty-page policy is positive EOF proof"
    );
}

#[tokio::test]
async fn offline_replay_retryable_error_page_fixture() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosFlow::new()
        .changes_zone_error("RETRY_LATER", "temporary backend issue", "")
        .build();
    let album = make_album_with_session(100, Box::new(mock));

    let (stream, token_rx) = album.changes_stream("token-before");
    tokio::pin!(stream);
    let mut errors = Vec::new();
    while let Some(result) = stream.next().await {
        if let Err(error) = result {
            errors.push(error);
        }
    }

    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].to_string().contains("RETRY_LATER"),
        "retryable fixture should surface the CloudKit retry code: {}",
        errors[0]
    );
    assert_eq!(
        token_rx.await.expect("sync token sender"),
        "token-before",
        "retryable error must preserve the last-good token"
    );
}

/// When a page returns only CPLAsset records (no CPLMaster), the
/// fetcher must advance the offset and continue to subsequent pages
/// instead of terminating prematurely.
#[tokio::test]
async fn test_photo_stream_continues_past_master_less_page() {
    use tokio_stream::StreamExt;

    // Page 1: Only CPLAsset records — no matching CPLMaster on this page.
    let page1 = json!({
        "records": [
            {
                "recordName": "asset-orphan-1",
                "recordType": "CPLAsset",
                "fields": {
                    "masterRef": {
                        "value": {"recordName": "orphan-master", "zoneID": {"zoneName": "PrimarySync"}},
                        "type": "REFERENCE"
                    },
                    "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                    "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                },
                "recordChangeTag": "ct1"
            }
        ]
    });

    // Page 2: Matching CPLMaster for page 1's asset-only record.
    let page2 = json!({"records": [test_master_record("orphan-master")]});
    // Page 3: Valid paired CPLMaster + CPLAsset.
    let page3 = mock_photo_query_page("master-ok", None);

    // Page 4: Empty -> terminates.
    let mock = MockPhotosSession::new()
        .ok(page1)
        .ok(page2)
        .ok(page3)
        .ok(json!({"records": []}));
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, _handles) = album.photo_stream_inner(
        None,
        None,
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        None,
        false,
        false,
    );
    tokio::pin!(stream);

    let mut count = 0u32;
    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
        count += 1;
    }
    assert_eq!(
        count, 2,
        "later pages should be yielded despite page 1 having no masters"
    );
}

/// A single empty /records/query page is not sufficient to conclude
/// EOF. The fetcher must probe forward by one `page_size` before
/// terminating so a transient gap doesn't silently cut enumeration
/// short.
#[tokio::test]
async fn test_photo_stream_probes_past_single_empty_page() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosSession::new()
        .ok(mock_photo_query_page("master-1", None))
        // Page 2 is empty (simulated gap); must not terminate.
        .ok(json!({"records": []}))
        // Page 3 contains records past the gap.
        .ok(mock_photo_query_page("master-2", None));
    // MockPhotosSession then returns the default {"records": []} on
    // every subsequent call; the fetcher requires MAX_EMPTY_PAGE_PROBES
    // consecutive empties to commit to EOF.
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, _handles) = album.photo_stream_inner(
        None,
        None,
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        None,
        false,
        false,
    );
    tokio::pin!(stream);

    let mut count = 0u32;
    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
        count += 1;
    }
    assert_eq!(
        count, 2,
        "both master-1 and master-2 should be yielded; the single empty page in between must not terminate enumeration"
    );
}

/// Robustness regression for empty-page-run truncation: a contiguous
/// run of fully-deleted records aligned to the page boundary used to
/// truncate enumeration after 2 empty probes, leaving real assets
/// past the run silently absent. With `MAX_EMPTY_PAGE_PROBES = 5`,
/// four consecutive empty pages must not terminate; records on page 6
/// must still be enumerated.
#[tokio::test]
async fn test_photo_stream_tolerates_four_consecutive_empty_pages() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosSession::new()
        .ok(mock_photo_query_page("master-1", None))
        // 4 consecutive empty pages (within tolerance).
        .ok(json!({"records": []}))
        .ok(json!({"records": []}))
        .ok(json!({"records": []}))
        .ok(json!({"records": []}))
        // Records reappear past the empty run.
        .ok(mock_photo_query_page("master-2", None));
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, _handles) = album.photo_stream_inner(
        None,
        None,
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        None,
        false,
        false,
    );
    tokio::pin!(stream);

    let mut count = 0u32;
    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
        count += 1;
    }
    assert_eq!(
        count, 2,
        "master-2 must be yielded even after 4 consecutive empty probes; \
         the previous threshold of 2 would have silently dropped it"
    );
}

/// Pins the upper bound on the probe walk: `MAX_EMPTY_PAGE_PROBES`
/// consecutive empty pages must terminate before any subsequent
/// records are observed. This guards against an unbounded probe
/// regression in the other direction (the fetcher walking forever on
/// a genuinely empty tail).
#[tokio::test]
async fn test_photo_stream_terminates_after_max_empty_probes() {
    use tokio_stream::StreamExt;

    // 1 record, then 5 empty pages (= MAX_EMPTY_PAGE_PROBES). A 6th
    // page with a record would be unreachable; the test asserts it is
    // never observed.
    let mock = MockPhotosSession::new()
        .ok(mock_photo_query_page("master-1", None))
        .ok(json!({"records": []}))
        .ok(json!({"records": []}))
        .ok(json!({"records": []}))
        .ok(json!({"records": []}))
        .ok(json!({"records": []}))
        // Should never be requested — terminator should fire first.
        .ok(mock_photo_query_page("master-unreachable", None));
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, _handles) = album.photo_stream_inner(
        None,
        None,
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        None,
        false,
        false,
    );
    tokio::pin!(stream);

    let mut count = 0u32;
    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
        count += 1;
    }
    assert_eq!(
        count, 1,
        "only master-1 should be yielded; enumeration must terminate \
         after MAX_EMPTY_PAGE_PROBES consecutive empty pages"
    );
}

// Initialize the shared subscriber before installing local filters, so parallel
// album streams and these filter assertions start with the same subscriber.
#[tracing_test::traced_test]
#[test]
fn fetcher_response_body_only_logs_at_trace() {
    use std::io::Write;
    use std::sync::Arc;

    struct VecMakeWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    struct VecWriter(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VecMakeWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            VecWriter(Arc::clone(&self.0))
        }
    }

    // A unique marker buried in the response. If anything in the
    // event includes the response value via Display/Debug, the
    // marker leaks into the formatted output and the assertion
    // fires.
    const MARKER: &str = "FETCHER_RESPONSE_BODY_TEST_MARKER_xyz123";
    let response = serde_json::json!({
        "records": [{"recordName": "abc", "fields": {"value": MARKER}}],
        "continuationMarker": MARKER,
    });

    let buf_debug = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sub_debug = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("debug"))
        .with_writer(VecMakeWriter(Arc::clone(&buf_debug)))
        .with_ansi(false)
        .finish();
    {
        let _g = tracing::subscriber::set_default(sub_debug);
        log_fetcher_response("TestAlbum", &response);
    }
    let out_debug = String::from_utf8_lossy(
        &buf_debug
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned();
    assert!(
        out_debug.contains("Fetcher response"),
        "DEBUG-level log should produce a Fetcher response event; got: {out_debug}",
    );
    assert!(
        !out_debug.contains(MARKER),
        "DEBUG-level log MUST NOT include the response body. The marker \
         leaked into captured output, indicating the per-page log carries \
         the full response value at DEBUG (issue #347 regression). \
         Captured: {out_debug}",
    );

    let buf_trace = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sub_trace = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("trace"))
        .with_writer(VecMakeWriter(Arc::clone(&buf_trace)))
        .with_ansi(false)
        .finish();
    {
        let _g = tracing::subscriber::set_default(sub_trace);
        log_fetcher_response("TestAlbum", &response);
    }
    let out_trace = String::from_utf8_lossy(
        &buf_trace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .into_owned();
    assert!(
        out_trace.contains("Fetcher response body"),
        "TRACE level should produce a 'Fetcher response body' event; got: {out_trace}",
    );
    assert!(
        out_trace.contains(MARKER),
        "TRACE level should include the response body in the event; got: {out_trace}",
    );
}
