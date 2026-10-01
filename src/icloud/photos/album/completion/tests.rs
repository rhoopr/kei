use crate::icloud::photos::album::test_support::{
    default_zone, drain_photo_stream_count, make_album, make_album_with_session,
};
use crate::icloud::photos::session::PhotosSession;
use crate::test_helpers::{MockPhotosFlow, MockPhotosSession, mock_photo_query_page};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

#[tokio::test]
async fn test_photo_stream_with_token_returns_sync_token() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosFlow::new()
        .query_photo_page("master-1", Some("st-zone-abc"))
        // Second call returns empty records to stop the fetcher
        .empty_query_page(Some("st-zone-abc"))
        .build();
    let album = make_album_with_session(100, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(1), 1);
    tokio::pin!(stream);

    let mut count = 0u32;
    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
        count += 1;
    }
    assert_eq!(count, 1, "should yield exactly one photo asset");

    let token = token_rx.await.expect("oneshot should not be dropped");
    assert_eq!(token.as_deref(), Some("st-zone-abc"));
}

#[tokio::test]
async fn test_photo_stream_with_token_no_sync_token_in_response() {
    use tokio_stream::StreamExt;

    // Responses without syncToken field
    let mock = MockPhotosFlow::new()
        .query_photo_page("master-1", None)
        .empty_query_page(None)
        .build();
    let album = make_album_with_session(100, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(1), 1);
    tokio::pin!(stream);

    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
    }

    let token = token_rx.await.expect("oneshot should not be dropped");
    assert_eq!(token, None, "no syncToken in responses means None");
}

#[tokio::test]
async fn test_photo_stream_with_token_blank_sync_token_treated_as_none() {
    use tokio_stream::StreamExt;

    let mock = MockPhotosFlow::new()
        .query_photo_page("master-1", Some(""))
        .empty_query_page(Some(""))
        .build();
    let album = make_album_with_session(100, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(1), 1);
    tokio::pin!(stream);

    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
    }

    let token = token_rx.await.expect("oneshot should not be dropped");
    assert_eq!(
        token, None,
        "blank syncToken must be treated as unavailable"
    );
}

#[tokio::test]
async fn test_photo_stream_with_token_last_token_wins() {
    use tokio_stream::StreamExt;

    // Two pages with different syncTokens — last one should be captured.
    // page_size=1 so each page yields 1 master record and the fetcher
    // advances offset by 1.
    let mock = MockPhotosFlow::new()
        .query_photo_page("master-1", Some("st-first"))
        .query_photo_page("master-2", Some("st-second"))
        .empty_query_page(None)
        .build();
    let album = make_album_with_session(1, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 1);
    tokio::pin!(stream);

    let mut count = 0u32;
    while let Some(result) = stream.next().await {
        result.expect("photo asset should be Ok");
        count += 1;
    }
    assert_eq!(count, 2);

    let token = token_rx.await.expect("oneshot should not be dropped");
    assert_eq!(token.as_deref(), Some("st-second"));
}

#[derive(Clone, Debug)]
struct TokenByOffsetSession {
    tokens_by_offset: Arc<HashMap<u64, &'static str>>,
}

#[async_trait::async_trait]
impl PhotosSession for TokenByOffsetSession {
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

        Ok(self.tokens_by_offset.get(&offset).map_or_else(
            || json!({"records": []}),
            |token| mock_photo_query_page(&format!("master-{offset}"), Some(token)),
        ))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn token_by_offset_session(tokens: &[(u64, &'static str)]) -> TokenByOffsetSession {
    TokenByOffsetSession {
        tokens_by_offset: Arc::new(tokens.iter().copied().collect()),
    }
}

#[tokio::test]
async fn test_photo_stream_with_token_parallel_fetchers_agree() {
    let album = make_album_with_session(
        1,
        Box::new(token_by_offset_session(&[(0, "st-same"), (1, "st-same")])),
    );

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 2);
    assert_eq!(drain_photo_stream_count(stream).await, 2);

    let token = token_rx.await.expect("oneshot should not be dropped");
    assert_eq!(token.as_deref(), Some("st-same"));
}

#[tokio::test]
async fn test_photo_stream_with_token_parallel_fetchers_disagree_suppresses_token() {
    let album = make_album_with_session(
        1,
        Box::new(token_by_offset_session(&[
            (0, "st-first"),
            (1, "st-second"),
        ])),
    );

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(2), 2);
    assert_eq!(drain_photo_stream_count(stream).await, 2);

    let token = token_rx.await.expect("oneshot should not be dropped");
    assert_eq!(
        token, None,
        "mismatched parallel fetcher tokens must block advancement"
    );
}

#[tokio::test]
async fn test_photo_stream_with_token_empty_album() {
    use tokio_stream::StreamExt;

    // Album with no records at all
    let mock = MockPhotosSession::new().ok(json!({"records": []}));
    let album = make_album_with_session(100, Box::new(mock));

    let (stream, token_rx) = album.photo_stream_with_token(None, Some(0), 1);
    tokio::pin!(stream);

    let items: Vec<_> = stream.collect().await;
    assert!(items.is_empty());

    let token = token_rx.await.expect("oneshot should not be dropped");
    assert_eq!(token, None);
}

#[tokio::test]
async fn test_photo_stream_with_token_setup_does_not_panic() {
    // Verify photo_stream_with_token setup path works with StubSession
    // (which panics on call). Same as the photo_stream setup tests.
    let album = make_album(100, None, default_zone());
    let (_stream, _token_rx) = album.photo_stream_with_token(None, None, 10);
}
