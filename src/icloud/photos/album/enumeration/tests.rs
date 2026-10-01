use crate::icloud::photos::album::planning::PhotoStreamProfile;
use crate::icloud::photos::album::test_support::{
    default_zone, drain_photo_stream, drain_photo_stream_count, drain_photo_stream_ids, make_album,
    make_album_with_session, test_asset_record, test_records,
};
use crate::icloud::photos::session::PhotosSession;
use crate::test_helpers::{DynamicRecentPhotosSession, MockPhotosSession, mock_photo_query_page};
use serde_json::{Value, json};
use std::sync::Arc;

#[tokio::test]
async fn test_photo_stream_no_total_count_uses_single_fetcher() {
    // When total_count is None, should produce a stream (1 sequential fetcher).
    // We can't easily test the internal spawning, but we verify it doesn't panic.
    let album = make_album(100, None, default_zone());
    let (_stream, _panic_rx) = album.photo_stream(None, None, 10);
    // Stream is valid — the fetcher will fail since StubSession panics on call,
    // but that's fine; we're testing the setup path, not the fetch.
}

#[tokio::test]
async fn test_photo_stream_small_recent_uses_single_fetcher() {
    // --recent 50 with page_size 100 → 1 page → 1 fetcher even with concurrency 10
    let album = make_album(100, None, default_zone());
    let (_stream, _panic_rx) = album.photo_stream(Some(50), Some(1000), 10);
}

// StubSession::post does `unimplemented!("stub")` which panics when the
// fetcher hits the first page. Consuming the stream therefore causes the
// fetcher JoinHandle to finish with a panic; the monitor task should
// forward that through the oneshot as `true`.
#[tokio::test]
async fn photo_stream_surfaces_fetcher_panic_via_oneshot() {
    use tokio_stream::StreamExt;
    let album = make_album(100, None, default_zone());
    let (stream, panic_rx) = album.photo_stream(None, None, 1);
    tokio::pin!(stream);
    // Drain whatever the stream yields before the fetcher dies.
    while stream.next().await.is_some() {}
    assert!(
        panic_rx.await.unwrap_or(false),
        "panic_rx must signal `true` when a fetcher panicked"
    );
}

// The convenience `photos()` wrapper must not hand back a
// silently-truncated Vec when a fetcher panics.
#[tokio::test]
async fn photos_returns_err_when_fetcher_panics() {
    let album = make_album(100, None, default_zone());
    let result = album.photos(None).await;
    assert!(
        result.is_err(),
        "photos() must surface fetcher panic as Err, got Ok({:?})",
        result.ok().map(|v| v.len())
    );
}

#[tokio::test]
async fn underreported_count_tail_proof_emits_every_asset() {
    let session = DynamicRecentPhotosSession::new(2);
    let album = make_album_with_session(100, Box::new(session));

    let (stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(None, Some(1), 1, true);

    assert_eq!(
        drain_photo_stream_ids(stream).await,
        vec!["master-0000", "master-0001"]
    );
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("zone-token")
    );
}

#[tokio::test]
async fn recent_limit_probe_suppresses_token_only_when_extra_asset_exists() {
    let session = DynamicRecentPhotosSession::new(2);
    let album = make_album_with_session(100, Box::new(session));

    let (stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(Some(1), Some(1), 1, true);

    assert_eq!(drain_photo_stream_ids(stream).await, vec!["master-0000"]);
    assert_eq!(
        token_rx.await.expect("sync token sender"),
        None,
        "the N+1 asset proves that the user bound truncated enumeration"
    );
}

#[tokio::test]
async fn recent_limit_probe_keeps_token_when_natural_eof_is_proved() {
    let session = DynamicRecentPhotosSession::new(1);
    let album = make_album_with_session(100, Box::new(session));

    let (stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(Some(1), Some(1), 1, true);

    assert_eq!(drain_photo_stream_ids(stream).await, vec!["master-0000"]);
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("zone-token"),
        "equal count and limit are eligible only after the N+1 probe proves EOF"
    );
}

#[tokio::test]
async fn overreported_count_still_finishes_at_proved_eof() {
    let session = DynamicRecentPhotosSession::new(1);
    let album = make_album_with_session(100, Box::new(session));

    let (stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(None, Some(10_000), 1, true);

    assert_eq!(drain_photo_stream_ids(stream).await, vec!["master-0000"]);
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("zone-token")
    );
}

#[tokio::test]
async fn tail_request_failure_blocks_token_after_prefix_was_observed() {
    let session = DynamicRecentPhotosSession::new(2).with_error_at_offset(2);
    let album = make_album_with_session(100, Box::new(session));

    let (stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(None, Some(1), 1, true);
    let (ids, errors) = drain_photo_stream(stream).await;

    assert_eq!(ids, vec!["master-0000", "master-0001"]);
    assert_eq!(errors.len(), 1);
    assert_eq!(token_rx.await.expect("sync token sender"), None);
}

#[tokio::test]
async fn dropping_stream_during_tail_proof_blocks_token() {
    use tokio_stream::StreamExt;

    let session = DynamicRecentPhotosSession::new(100);
    let album = make_album_with_session(100, Box::new(session));
    let (mut stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(None, Some(1), 1, true);

    assert!(
        stream
            .next()
            .await
            .transpose()
            .expect("first asset")
            .is_some()
    );
    drop(stream);

    assert_eq!(token_rx.await.expect("sync token sender"), None);
}

#[derive(Clone, Debug)]
struct CountingSinglePageSession {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl PhotosSession for CountingSinglePageSession {
    async fn post(
        &self,
        _url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if call == 0 {
            Ok(mock_photo_query_page(
                "master-known-total",
                Some("st-known"),
            ))
        } else {
            Ok(json!({"records": [], "syncToken": "st-known"}))
        }
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn photo_stream_with_known_single_page_total_proves_empty_tail() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let album = make_album_with_session(
        100,
        Box::new(CountingSinglePageSession {
            calls: Arc::clone(&calls),
        }),
    );

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(1), 10);

    assert_eq!(drain_photo_stream_count(stream).await, 1);
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("st-known")
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        6,
        "the count is only a hint, so the final owner must prove the empty tail"
    );
}

#[derive(Clone, Debug)]
struct InitialBoundarySession {
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    max_in_flight: Arc<std::sync::atomic::AtomicUsize>,
    offsets: Arc<std::sync::Mutex<Vec<u64>>>,
}

impl InitialBoundarySession {
    fn note_start(&self, offset: u64) {
        self.offsets.lock().expect("offsets lock").push(offset);
        let current = self
            .in_flight
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let mut observed = self.max_in_flight.load(std::sync::atomic::Ordering::SeqCst);
        while current > observed {
            match self.max_in_flight.compare_exchange(
                observed,
                current,
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(next) => observed = next,
            }
        }
    }
}

#[async_trait::async_trait]
impl PhotosSession for InitialBoundarySession {
    async fn post(
        &self,
        _url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        let request: Value = serde_json::from_str(&body)?;
        let offset = request["query"]["filterBy"]
            .as_array()
            .and_then(|filters| {
                filters.iter().find_map(|filter| {
                    (filter["fieldName"] == "startRank")
                        .then(|| filter["fieldValue"]["value"].as_u64())
                        .flatten()
                })
            })
            .unwrap_or(0);
        self.note_start(offset);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.in_flight
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

        let records = if offset == 0 {
            let mut records = Vec::new();
            for i in 0..99 {
                records.extend(test_records(&format!("master-{i}")));
            }
            records.push(test_asset_record("master-99"));
            records
        } else if offset == 99 {
            test_records("master-99")
        } else {
            Vec::new()
        };
        Ok(json!({"records": records, "syncToken": "st-boundary"}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn photo_stream_limit_probe_is_ordered_and_proves_eof() {
    let session = InitialBoundarySession {
        in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        max_in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        offsets: Arc::new(std::sync::Mutex::new(Vec::new())),
    };
    let album = make_album_with_session(100, Box::new(session.clone()));

    let (stream, token_rx) = album.photo_stream_with_token(Some(100), Some(100), 10);

    assert_eq!(drain_photo_stream_count(stream).await, 100);
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("st-boundary")
    );
    assert_eq!(
        session
            .max_in_flight
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the N+1 proof must remain ordered"
    );
    let mut offsets = session.offsets.lock().expect("offsets lock").clone();
    offsets.sort_unstable();
    assert_eq!(offsets, vec![0, 99, 100, 200, 300, 400, 500]);
}

#[tokio::test]
async fn download_stream_recent_limit_drains_beyond_first_small_page() {
    let session = DynamicRecentPhotosSession::new(100).with_token("st-dynamic");
    let album = make_album_with_session(100, Box::new(session.clone()));

    let (stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(Some(100), Some(100), 10, true);

    assert_eq!(
        drain_photo_stream_count(stream).await,
        100,
        "download-mode --recent must not stop after the first reduced page"
    );
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("st-dynamic")
    );
    assert_eq!(
        session.offsets().as_slice(),
        &[0, 20, 40, 60, 80, 100, 120, 140, 160, 180],
        "download-mode pagination should advance through the data and finite EOF proof"
    );
    assert!(
        session.results_limits().iter().all(|limit| *limit == 40),
        "download-mode fetchers must use the reduced 20-asset page size"
    );
}

#[tokio::test]
async fn download_stream_recent_limit_without_total_count_drains_until_limit() {
    let session = DynamicRecentPhotosSession::new(100);
    let album = make_album_with_session(100, Box::new(session.clone()));

    let (stream, token_rx) =
        album.photo_stream_with_token_for_download_policy(Some(100), None, 10, true);

    let ids = drain_photo_stream_ids(stream).await;
    assert_eq!(ids.len(), 100);
    assert_eq!(ids.first().map(String::as_str), Some("master-0000"));
    assert_eq!(ids.last().map(String::as_str), Some("master-0099"));
    assert_eq!(
        token_rx.await.expect("sync token sender").as_deref(),
        Some("zone-token"),
        "the N+1 probe can prove EOF even when the count side-channel is unavailable"
    );
    assert_eq!(
        session.offsets().as_slice(),
        &[0, 20, 40, 60, 80, 100, 120, 140, 160, 180],
        "unknown-total download-mode --recent should keep paging through the finite EOF proof"
    );
    assert!(
        session.results_limits().iter().all(|limit| *limit == 40),
        "download-mode unknown-total requests must use the reduced 20-asset page size"
    );
}

#[tokio::test]
async fn download_stream_recent_limit_boundary_table() {
    for recent in [19_u32, 20, 21, 99, 100, 101] {
        let session = DynamicRecentPhotosSession::new(u64::from(recent));
        let album = make_album_with_session(100, Box::new(session.clone()));

        let (stream, token_rx) = album.photo_stream_with_token_for_download_policy(
            Some(recent),
            Some(u64::from(recent)),
            10,
            true,
        );
        let ids = drain_photo_stream_ids(stream).await;

        assert_eq!(ids.len(), recent as usize, "recent={recent}");
        for (expected, id) in ids.iter().enumerate() {
            assert_eq!(id, &format!("master-{expected:04}"), "recent={recent}");
        }
        assert_eq!(
            token_rx.await.expect("sync token sender").as_deref(),
            Some("zone-token"),
            "recent={recent}"
        );
        assert_eq!(
            session.offsets().first().copied(),
            Some(0),
            "recent={recent}"
        );
        assert!(
            session.offsets().windows(2).all(|pair| pair[0] < pair[1]),
            "recent={recent}: offsets must advance monotonically, got {:?}",
            session.offsets()
        );
    }
}

#[tokio::test]
async fn test_photo_stream_limit_zero_yields_nothing() {
    use tokio_stream::StreamExt;

    // --recent 0 should produce 0 items. The mock has a valid page
    // available, but limit=0 means the fetcher should never send it.
    let mock = MockPhotosSession::new()
        .ok(mock_photo_query_page("master-1", None))
        .ok(json!({"records": []}));
    let album = make_album_with_session(100, Box::new(mock));

    let (stream, _handles) = album.photo_stream_inner(
        Some(0),
        Some(10),
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        None,
        false,
        false,
    );
    tokio::pin!(stream);

    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 0, "--recent 0 should yield 0 items");
}

#[tokio::test]
async fn test_photo_stream_limit_one_yields_exactly_one() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosSession::new()
        .ok(mock_photo_query_page("master-1", None))
        .ok(mock_photo_query_page("master-2", None))
        .ok(json!({"records": []}));
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, _handles) = album.photo_stream_inner(
        Some(1),
        Some(10),
        PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        None,
        false,
        false,
    );
    tokio::pin!(stream);

    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 1, "--recent 1 should yield exactly 1 item");
    items[0].as_ref().expect("item should be Ok");
}
