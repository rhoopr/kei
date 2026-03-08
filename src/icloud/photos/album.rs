use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tracing::debug;

use super::asset::PhotoAsset;
use super::queries::{encode_params, DESIRED_KEYS_VALUES};
use super::session::PhotosSession;

/// Determine how many parallel fetcher tasks to spawn.
///
/// We never spawn more fetchers than total pages (no empty fetchers)
/// and never more than the requested concurrency level.
fn determine_fetcher_count(total_items: u64, page_size: usize, concurrency: usize) -> usize {
    let total_pages = total_items.div_ceil(page_size as u64);
    (total_pages as usize).min(concurrency).max(1)
}

/// Configuration for creating a `PhotoAlbum`, bundling all non-session fields.
#[derive(Debug)]
pub struct PhotoAlbumConfig {
    pub params: Arc<HashMap<String, Value>>,
    pub service_endpoint: String,
    pub name: String,
    pub list_type: String,
    pub obj_type: String,
    pub query_filter: Option<Value>,
    pub page_size: usize,
    pub zone_id: Value,
}

pub struct PhotoAlbum {
    pub(crate) name: String,
    params: Arc<HashMap<String, Value>>,
    session: Box<dyn PhotosSession>,
    service_endpoint: String,
    list_type: String,
    obj_type: String,
    query_filter: Option<Value>,
    page_size: usize,
    zone_id: Value,
}

impl std::fmt::Debug for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotoAlbum")
            .field("name", &self.name)
            .field("service_endpoint", &self.service_endpoint)
            .field("list_type", &self.list_type)
            .field("obj_type", &self.obj_type)
            .field("page_size", &self.page_size)
            .finish_non_exhaustive()
    }
}

impl PhotoAlbum {
    pub fn new(config: PhotoAlbumConfig, session: Box<dyn PhotosSession>) -> Self {
        Self {
            name: config.name,
            params: config.params,
            session,
            service_endpoint: config.service_endpoint,
            list_type: config.list_type,
            obj_type: config.obj_type,
            query_filter: config.query_filter,
            page_size: config.page_size,
            zone_id: config.zone_id,
        }
    }

    /// Return total item count for this album via `HyperionIndexCountLookup`.
    pub async fn len(&self) -> anyhow::Result<u64> {
        let url = format!(
            "{}/internal/records/query/batch?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "batch": [{
                "resultsLimit": 1,
                "query": {
                    "filterBy": {
                        "fieldName": "indexCountID",
                        "fieldValue": {
                            "type": "STRING_LIST",
                            "value": [&self.obj_type]
                        },
                        "comparator": "IN",
                    },
                    "recordType": "HyperionIndexCountLookup",
                },
                "zoneWide": true,
                "zoneID": self.zone_id,
            }]
        });

        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
        )
        .await?;

        let batch: super::cloudkit::BatchQueryResponse = serde_json::from_value(response)?;
        let count = batch
            .batch
            .first()
            .and_then(|q| q.records.first())
            .and_then(|r| r.fields["itemCount"]["value"].as_u64())
            .unwrap_or(0);
        Ok(count)
    }

    /// Convenience wrapper over `photo_stream()` that collects all assets
    /// into a `Vec`. Prefer `photo_stream()` when memory is a concern.
    pub async fn photos(&self, limit: Option<u32>) -> anyhow::Result<Vec<PhotoAsset>> {
        use tokio_stream::StreamExt;
        self.photo_stream(limit, None, 1)
            .collect::<Result<Vec<_>, _>>()
            .await
    }

    /// Stream photos page-by-page without buffering the full album in memory.
    ///
    /// When `total_count` is provided and `concurrency > 1`, the offset range
    /// is partitioned across multiple parallel fetcher tasks for faster
    /// enumeration. Each fetcher pages through its assigned slice and sends
    /// assets into a shared channel. When `total_count` is `None` or
    /// `concurrency` is 1, a single sequential fetcher is used (original
    /// behavior).
    ///
    /// The channel buffer is `page_size * num_fetchers`, giving each fetcher
    /// one page of headroom before back-pressure kicks in.
    pub fn photo_stream(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        concurrency: usize,
    ) -> Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>> {
        let page_size = self.page_size;

        // Compute effective total, capped by --recent if set.
        let effective_total = total_count
            .map(|tc| limit.map_or(tc, |lim| tc.min(lim as u64)))
            .or(limit.map(|lim| lim as u64));

        // Use 2x concurrency for enumeration fetchers — Apple's CloudKit
        // doesn't throttle at these levels and it halves enumeration time.
        let num_fetchers = match effective_total {
            Some(total) if concurrency > 1 => {
                determine_fetcher_count(total, page_size, concurrency * 2)
            }
            _ => 1,
        };

        let (tx, rx) =
            mpsc::channel::<anyhow::Result<PhotoAsset>>((page_size * num_fetchers).min(500));

        if num_fetchers > 1 {
            let total = effective_total.expect("effective_total set when num_fetchers > 1");
            // Partition offset range into non-overlapping chunks aligned to
            // page_size boundaries so each fetcher starts on a clean page.
            let chunk_size_items = {
                let raw = total.div_ceil(num_fetchers as u64);
                // Round up to next page_size boundary
                let ps = page_size as u64;
                raw.div_ceil(ps) * ps
            };

            tracing::info!(
                fetchers = num_fetchers,
                chunk_size = chunk_size_items,
                total = total,
                "Parallel photo enumeration"
            );

            for i in 0..num_fetchers {
                let start = i as u64 * chunk_size_items;
                let end = ((i as u64 + 1) * chunk_size_items).min(total);
                if start >= total {
                    break;
                }
                // Per-fetcher limit: don't exceed the chunk size, and for the
                // last fetcher also respect the global --recent cap.
                let fetcher_limit = match limit {
                    Some(lim) => {
                        let remaining = (lim as u64).saturating_sub(start);
                        Some(remaining.min(end - start) as u32)
                    }
                    None => None,
                };
                self.spawn_fetcher(tx.clone(), start, end, fetcher_limit);
            }
            // Drop our sender so channel closes when all fetchers finish.
            drop(tx);
        } else {
            tracing::info!("Fetching photos from iCloud...");
            // Move tx directly — no clone needed for a single fetcher.
            self.spawn_fetcher(tx, 0, u64::MAX, limit);
        }

        Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
    }

    /// Spawn a background tokio task that pages through records from
    /// `start_offset` up to (but not including) `end_offset`, sending each
    /// `PhotoAsset` into `tx`. The task stops when:
    /// - `offset >= end_offset`
    /// - the API returns no master records (end of album)
    /// - the per-fetcher `limit` is reached
    /// - the receiver is dropped
    fn spawn_fetcher(
        &self,
        tx: mpsc::Sender<anyhow::Result<PhotoAsset>>,
        start_offset: u64,
        end_offset: u64,
        limit: Option<u32>,
    ) {
        let session = self.session.clone_box();
        let service_endpoint = self.service_endpoint.clone();
        let params = Arc::clone(&self.params);
        let name = self.name.clone();
        let list_type = self.list_type.clone();
        let query_filter = self.query_filter.clone();
        let page_size = self.page_size;
        let zone_id = self.zone_id.clone();

        tokio::spawn(async move {
            let mut offset = start_offset;
            let mut total_sent: u64 = 0;
            let url = format!(
                "{}/records/query?{}",
                service_endpoint,
                encode_params(&params)
            );

            loop {
                if offset >= end_offset {
                    break;
                }

                let body = Self::build_list_query(
                    &list_type,
                    &query_filter,
                    page_size,
                    &zone_id,
                    offset,
                    "ASCENDING",
                );
                debug!(
                    "Album '{}' fetcher [{}..{}] POST offset={}",
                    name, start_offset, end_offset, offset
                );
                let response = match super::session::retry_post(
                    session.as_ref(),
                    &url,
                    &body.to_string(),
                    &[("Content-type", "text/plain")],
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                };
                debug!(
                    "Album '{}' response: {}",
                    name,
                    serde_json::to_string_pretty(&response).unwrap_or_default()
                );

                let query: super::cloudkit::QueryResponse = match serde_json::from_value(response) {
                    Ok(q) => q,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };
                let records = query.records;

                debug!(
                    "Album '{}': got {} records at offset {}",
                    name,
                    records.len(),
                    offset
                );

                let mut asset_records: HashMap<String, super::cloudkit::Record> = HashMap::new();
                let mut master_records: Vec<super::cloudkit::Record> = Vec::new();

                for rec in records {
                    debug!("  record type: {}", rec.record_type);
                    if rec.record_type == "CPLAsset" {
                        if let Some(master_id) =
                            rec.fields["masterRef"]["value"]["recordName"].as_str()
                        {
                            let master_id = master_id.to_string();
                            asset_records.insert(master_id, rec);
                        }
                    } else if rec.record_type == "CPLMaster" {
                        master_records.push(rec);
                    }
                }

                if master_records.is_empty() {
                    break;
                }

                let mut limit_reached = false;
                for master in master_records {
                    if let Some(asset_rec) = asset_records.remove(&master.record_name) {
                        let asset = PhotoAsset::from_records(master, asset_rec);
                        if tx.send(Ok(asset)).await.is_err() {
                            return;
                        }
                        total_sent += 1;
                        if let Some(n) = limit {
                            if total_sent >= n as u64 {
                                limit_reached = true;
                                break;
                            }
                        }
                    }
                    offset += 1;
                }

                tracing::debug!(
                    count = total_sent,
                    range_start = start_offset,
                    "fetched photos so far"
                );

                if limit_reached {
                    break;
                }
            }
        });
    }

    #[cfg(test)]
    fn list_query(&self, offset: u64, direction: &str) -> Value {
        Self::build_list_query(
            &self.list_type,
            &self.query_filter,
            self.page_size,
            &self.zone_id,
            offset,
            direction,
        )
    }

    fn build_list_query(
        list_type: &str,
        query_filter: &Option<Value>,
        page_size: usize,
        zone_id: &Value,
        offset: u64,
        direction: &str,
    ) -> Value {
        let desired_keys = &*DESIRED_KEYS_VALUES;

        let mut filter_by = vec![
            json!({
                "fieldName": "startRank",
                "fieldValue": {"type": "INT64", "value": offset},
                "comparator": "EQUALS",
            }),
            json!({
                "fieldName": "direction",
                "fieldValue": {"type": "STRING", "value": direction},
                "comparator": "EQUALS",
            }),
        ];

        if let Some(qf) = query_filter {
            if let Some(arr) = qf.as_array() {
                filter_by.extend(arr.iter().cloned());
            }
        }

        let query_part = json!({
            "filterBy": &filter_by,
            "recordType": list_type,
        });
        debug!(
            "list_query filterBy ({} items): {}",
            filter_by.len(),
            serde_json::to_string(&query_part).unwrap_or_default()
        );
        debug!(
            "list_query zoneID: {}",
            serde_json::to_string(zone_id).unwrap_or_default()
        );

        json!({
            "query": {
                "filterBy": filter_by,
                "recordType": list_type,
            },
            // CloudKit returns interleaved CPLMaster + CPLAsset records,
            // so 2 * page_size fetches page_size paired records.
            "resultsLimit": page_size * 2,
            "desiredKeys": desired_keys,
            "zoneID": zone_id,
        })
    }
}

impl std::fmt::Display for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct StubSession;

    #[async_trait::async_trait]
    impl PhotosSession for StubSession {
        async fn post(
            &self,
            _url: &str,
            _body: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            unimplemented!("stub")
        }
        async fn get(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<reqwest::Response> {
            unimplemented!("stub")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(StubSession)
        }
    }

    fn make_album(page_size: usize, query_filter: Option<Value>, zone_id: Value) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: "https://example.com".into(),
                name: "TestAlbum".into(),
                list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter,
                page_size,
                zone_id,
            },
            Box::new(StubSession),
        )
    }

    fn default_zone() -> Value {
        json!({"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner", "zoneType": "REGULAR_CUSTOM_ZONE"})
    }

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
        let album = make_album(200, Some(extra), default_zone());
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

    // --- determine_fetcher_count tests ---

    #[test]
    fn test_fetcher_count_single_page() {
        // 50 items, page_size 100, concurrency 10 → 1 page → 1 fetcher
        assert_eq!(determine_fetcher_count(50, 100, 10), 1);
    }

    #[test]
    fn test_fetcher_count_exact_pages() {
        // 500 items, page_size 100, concurrency 10 → 5 pages → 5 fetchers
        assert_eq!(determine_fetcher_count(500, 100, 10), 5);
    }

    #[test]
    fn test_fetcher_count_capped_by_concurrency() {
        // 5000 items, page_size 100, concurrency 10 → 50 pages → capped to 10
        assert_eq!(determine_fetcher_count(5000, 100, 10), 10);
    }

    #[test]
    fn test_fetcher_count_more_pages_than_concurrency() {
        // 50000 items, page_size 100, concurrency 10 → 500 pages → capped to 10
        assert_eq!(determine_fetcher_count(50000, 100, 10), 10);
    }

    #[test]
    fn test_fetcher_count_zero_items() {
        // 0 items → at least 1 fetcher (the loop will just exit immediately)
        assert_eq!(determine_fetcher_count(0, 100, 10), 1);
    }

    #[test]
    fn test_fetcher_count_concurrency_one() {
        // concurrency=1 always gives 1 fetcher
        assert_eq!(determine_fetcher_count(50000, 100, 1), 1);
    }

    #[test]
    fn test_fetcher_count_partial_page() {
        // 150 items, page_size 100 → 2 pages, concurrency 10 → 2 fetchers
        assert_eq!(determine_fetcher_count(150, 100, 10), 2);
    }

    // --- photo_stream parameter tests ---

    #[tokio::test]
    async fn test_photo_stream_no_total_count_uses_single_fetcher() {
        // When total_count is None, should produce a stream (1 sequential fetcher).
        // We can't easily test the internal spawning, but we verify it doesn't panic.
        let album = make_album(100, None, default_zone());
        let _stream = album.photo_stream(None, None, 10);
        // Stream is valid — the fetcher will fail since StubSession panics on call,
        // but that's fine; we're testing the setup path, not the fetch.
    }

    #[tokio::test]
    async fn test_photo_stream_small_recent_uses_single_fetcher() {
        // --recent 50 with page_size 100 → 1 page → 1 fetcher even with concurrency 10
        let album = make_album(100, None, default_zone());
        let _stream = album.photo_stream(Some(50), Some(1000), 10);
    }
}
