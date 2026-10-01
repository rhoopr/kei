use crate::icloud::photos::album::test_support::default_zone;
use crate::icloud::photos::album::{PhotoAlbum, PhotoAlbumConfig};
use crate::icloud::photos::session::PhotosSession;
use crate::retry::RetryConfig;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
struct BatchCountSession {
    calls: Arc<AtomicUsize>,
    batch_sizes: Arc<Mutex<Vec<usize>>>,
}

#[async_trait::async_trait]
impl PhotosSession for BatchCountSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        assert!(
            url.contains("/internal/records/query/batch"),
            "unexpected URL: {url}"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body: Value = serde_json::from_str(&body)?;
        let batch_len = body["batch"].as_array().map_or(0, Vec::len);
        self.batch_sizes.lock().unwrap().push(batch_len);
        let batch: Vec<Value> = (0..batch_len)
            .map(|index| {
                json!({
                    "records": [{
                        "fields": {
                            "itemCount": {"value": ((index + 1) as u64) * 10}
                        }
                    }]
                })
            })
            .collect();
        Ok(json!({ "batch": batch }))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn len_many_batches_same_library_count_queries() {
    let params = Arc::new(HashMap::new());
    let service_endpoint: Arc<str> = Arc::from("https://example.com");
    let calls = Arc::new(AtomicUsize::new(0));
    let batch_sizes = Arc::new(Mutex::new(Vec::new()));
    let session = BatchCountSession {
        calls: Arc::clone(&calls),
        batch_sizes: Arc::clone(&batch_sizes),
    };
    let make_count_album = |name: &str| {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::clone(&params),
                service_endpoint: Arc::clone(&service_endpoint),
                name: Arc::from(name),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(default_zone()),
                retry_config: RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(session.clone()),
        )
    };
    let albums = [
        make_count_album("Album A"),
        make_count_album("Album B"),
        make_count_album("Album C"),
    ];
    let album_refs: Vec<&PhotoAlbum> = albums.iter().collect();

    let counts = PhotoAlbum::len_many(&album_refs)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(counts, vec![10, 20, 30]);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(*batch_sizes.lock().unwrap(), vec![3]);
}

fn count_query_from_fields(fields: Value) -> crate::icloud::photos::cloudkit::QueryResponse {
    let batch: crate::icloud::photos::cloudkit::BatchQueryResponse =
        serde_json::from_value(json!({
            "batch": [{"records": [{"fields": fields}]}]
        }))
        .expect("test count batch parses");
    batch.batch.into_iter().next().expect("count query exists")
}

#[test]
fn count_from_query_accepts_well_formed_zero() {
    let query = count_query_from_fields(json!({"itemCount": {"value": 0}}));

    let count = PhotoAlbum::count_from_query(Some(&query)).expect("zero is a valid count");

    assert_eq!(count, 0);
}

#[test]
fn count_from_query_rejects_missing_item_count() {
    let query = count_query_from_fields(json!({}));

    let err = PhotoAlbum::count_from_query(Some(&query)).expect_err("missing count fails");

    assert!(
        err.to_string().contains("did not include itemCount"),
        "unexpected error: {err:#}"
    );
}

#[test]
fn count_from_query_rejects_malformed_item_count() {
    let query = count_query_from_fields(json!({"itemCount": {"value": "0"}}));

    let err = PhotoAlbum::count_from_query(Some(&query)).expect_err("malformed count fails");

    assert!(
        err.to_string().contains("was not a non-negative integer"),
        "unexpected error: {err:#}"
    );
}
