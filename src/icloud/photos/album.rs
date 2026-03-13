use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::Stream;
use tracing::debug;

use super::asset::{ChangeEvent, DeltaRecordBuffer, PhotoAsset};
use super::cloudkit::ChangesZoneResponse;
use super::queries::{build_changes_zone_request, encode_params, DESIRED_KEYS_VALUES};
use super::session::{check_changes_zone_error, PhotosSession};

/// A boxed, pinned stream of photo asset results.
type PhotoStream = Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>>;

/// A boxed, pinned stream of change event results.
type ChangeStream = Pin<Box<dyn Stream<Item = anyhow::Result<ChangeEvent>> + Send + 'static>>;

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
    ) -> PhotoStream {
        let (stream, _handles) = self.photo_stream_inner(limit, total_count, concurrency, None);
        stream
    }

    /// Like [`photo_stream()`](Self::photo_stream), but also returns a
    /// `oneshot::Receiver` that will yield the zone-level `syncToken` from
    /// the last API response page once the stream is fully consumed.
    ///
    /// The caller should `.await` the receiver **after** the stream is
    /// exhausted:
    ///
    /// ```ignore
    /// let (stream, token_rx) = album.photo_stream_with_token(limit, count, concurrency);
    /// tokio::pin!(stream);
    /// while let Some(item) = stream.next().await { /* ... */ }
    /// let sync_token = token_rx.await.ok().flatten();
    /// ```
    pub fn photo_stream_with_token(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        concurrency: usize,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let shared_sync_token: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        let (stream, handles) = self.photo_stream_inner(
            limit,
            total_count,
            concurrency,
            Some(shared_sync_token.clone()),
        );

        // Spawn a monitor task that waits for all fetcher tasks to complete,
        // then delivers the captured syncToken through the oneshot channel.
        // The fetchers' mpsc senders are dropped when they finish, which
        // closes the ReceiverStream. The caller awaits the oneshot after the
        // stream is exhausted.
        tokio::spawn(async move {
            for handle in handles {
                let _ = handle.await;
            }
            let final_token = shared_sync_token.lock().await.clone();
            let _ = token_tx.send(final_token);
        });

        (stream, token_rx)
    }

    /// Stream record changes since the given syncToken via `changes/zone`.
    ///
    /// Returns a stream of `ChangeEvent`s and a oneshot receiver for the final syncToken.
    /// The syncToken is sent through the oneshot after all pages have been consumed
    /// (moreComing: false).
    ///
    /// This method is inherently sequential -- each page's syncToken feeds the next request.
    /// No parallel fetchers.
    pub fn changes_stream(
        &self,
        sync_token: &str,
    ) -> (ChangeStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        let (tx, rx) = mpsc::channel::<anyhow::Result<ChangeEvent>>(200);
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();

        let session = self.session.clone_box();
        let service_endpoint = self.service_endpoint.clone();
        let params = Arc::clone(&self.params);
        let zone_id = self.zone_id.clone();
        let initial_token = sync_token.to_string();
        let album_name = self.name.clone();

        tokio::spawn(async move {
            let mut buffer = DeltaRecordBuffer::new();
            let mut current_token = initial_token;

            let url = format!(
                "{}/changes/zone?{}",
                service_endpoint,
                encode_params(&params)
            );

            loop {
                let body = build_changes_zone_request(&zone_id, Some(&current_token), 200);
                debug!(
                    album = %album_name,
                    token = %current_token,
                    "changes/zone request"
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
                        let _ = token_tx.send(None);
                        return;
                    }
                };

                let changes_resp: ChangesZoneResponse = match serde_json::from_value(response) {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        let _ = token_tx.send(None);
                        return;
                    }
                };

                let zone_result = match changes_resp.zones.into_iter().next() {
                    Some(zr) => zr,
                    None => {
                        let _ = tx
                            .send(Err(anyhow::anyhow!(
                                "changes/zone returned empty zones array"
                            )))
                            .await;
                        let _ = token_tx.send(None);
                        return;
                    }
                };

                // Check for zone-level errors
                let zone_name = zone_result.zone_id.zone_name.clone();
                if let Err(sync_err) = check_changes_zone_error(
                    zone_result.server_error_code.as_deref(),
                    zone_result.reason.as_deref(),
                    &zone_name,
                ) {
                    let _ = tx.send(Err(sync_err.into())).await;
                    let _ = token_tx.send(None);
                    return;
                }

                current_token = zone_result.sync_token;
                let more_coming = zone_result.more_coming;

                debug!(
                    album = %album_name,
                    records = zone_result.records.len(),
                    more_coming,
                    new_token = %current_token,
                    "changes/zone page received"
                );

                let events = buffer.process_records(zone_result.records);
                for event in events {
                    if tx.send(Ok(event)).await.is_err() {
                        // Receiver dropped
                        let _ = token_tx.send(Some(current_token));
                        return;
                    }
                }

                if !more_coming {
                    break;
                }
            }

            // Flush remaining unpaired records
            let flush_events = buffer.flush();
            for event in flush_events {
                if tx.send(Ok(event)).await.is_err() {
                    let _ = token_tx.send(Some(current_token));
                    return;
                }
            }

            let _ = token_tx.send(Some(current_token));
        });

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            token_rx,
        )
    }

    /// Shared implementation for `photo_stream` and `photo_stream_with_token`.
    ///
    /// When `shared_sync_token` is `Some`, each fetcher writes its last
    /// observed `syncToken` into the shared mutex.
    ///
    /// Returns the stream and all spawned fetcher `JoinHandle`s.
    fn photo_stream_inner(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        concurrency: usize,
        shared_sync_token: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    ) -> (PhotoStream, Vec<JoinHandle<()>>) {
        let page_size = self.page_size;
        let mut handles = Vec::new();

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
                handles.push(self.spawn_fetcher(
                    tx.clone(),
                    start,
                    end,
                    fetcher_limit,
                    shared_sync_token.clone(),
                ));
            }
            // Drop our sender so channel closes when all fetchers finish.
            drop(tx);
        } else {
            tracing::info!("Fetching photos from iCloud...");
            // Move tx directly — no clone needed for a single fetcher.
            handles.push(self.spawn_fetcher(tx, 0, u64::MAX, limit, shared_sync_token));
        }

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            handles,
        )
    }

    /// Spawn a background tokio task that pages through records from
    /// `start_offset` up to (but not including) `end_offset`, sending each
    /// `PhotoAsset` into `tx`. The task stops when:
    /// - `offset >= end_offset`
    /// - the API returns no master records (end of album)
    /// - the per-fetcher `limit` is reached
    /// - the receiver is dropped
    ///
    /// If `shared_sync_token` is provided, the fetcher writes the last non-None
    /// `syncToken` from each `QueryResponse` page into it. Because the token is
    /// a zone-level invariant, any fetcher's final value is correct.
    fn spawn_fetcher(
        &self,
        tx: mpsc::Sender<anyhow::Result<PhotoAsset>>,
        start_offset: u64,
        end_offset: u64,
        limit: Option<u32>,
        shared_sync_token: Option<Arc<tokio::sync::Mutex<Option<String>>>>,
    ) -> JoinHandle<()> {
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
                    album = %name,
                    range_start = start_offset,
                    range_end = end_offset,
                    offset,
                    "Fetcher POST"
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
                    album = %name,
                    response = %serde_json::to_string_pretty(&response).unwrap_or_default(),
                    "Fetcher response"
                );

                let query: super::cloudkit::QueryResponse = match serde_json::from_value(response) {
                    Ok(q) => q,
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };

                // Capture the zone-level syncToken from each page response.
                if let Some(ref shared) = shared_sync_token {
                    if let Some(ref token) = query.sync_token {
                        *shared.lock().await = Some(token.clone());
                    }
                }

                let records = query.records;

                debug!(
                    album = %name,
                    count = records.len(),
                    offset,
                    "Got records"
                );

                let mut asset_records: HashMap<String, super::cloudkit::Record> = HashMap::new();
                let mut master_records: Vec<super::cloudkit::Record> = Vec::new();

                for rec in records {
                    debug!(record_type = %rec.record_type, "  record");
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
                    if let Some(n) = limit {
                        if total_sent >= n as u64 {
                            limit_reached = true;
                            break;
                        }
                    }
                    if let Some(asset_rec) = asset_records.remove(&master.record_name) {
                        let asset = PhotoAsset::from_records(master, asset_rec);
                        if tx.send(Ok(asset)).await.is_err() {
                            return;
                        }
                        total_sent += 1;
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
        })
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
            count = filter_by.len(),
            query = %serde_json::to_string(&query_part).unwrap_or_default(),
            "list_query filterBy"
        );
        debug!(
            zone_id = %serde_json::to_string(zone_id).unwrap_or_default(),
            "list_query zoneID"
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

    // --- photo_stream_with_token tests ---

    /// A session stub that returns canned QueryResponse JSON. Each call to
    /// `post()` pops the next response from the front of the queue. If the
    /// queue is empty, returns an empty records array.
    struct CannedSession {
        responses: std::sync::Mutex<std::collections::VecDeque<Value>>,
    }

    impl CannedSession {
        fn new(responses: Vec<Value>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for CannedSession {
        async fn post(
            &self,
            _url: &str,
            _body: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            let next = self
                .responses
                .lock()
                .expect("poisoned")
                .pop_front()
                .unwrap_or_else(|| json!({"records": []}));
            Ok(next)
        }
        async fn get(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<reqwest::Response> {
            unimplemented!("stub")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            // Clone-box snapshots remaining responses so the spawned fetcher
            // task gets its own copy of the queue.
            let remaining: Vec<Value> = self
                .responses
                .lock()
                .expect("poisoned")
                .iter()
                .cloned()
                .collect();
            Box::new(CannedSession::new(remaining))
        }
    }

    fn make_album_with_session(page_size: usize, session: Box<dyn PhotosSession>) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: "https://example.com".into(),
                name: "TestAlbum".into(),
                list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                query_filter: None,
                page_size,
                zone_id: default_zone(),
            },
            session,
        )
    }

    /// Build a canned QueryResponse with one paired CPLMaster+CPLAsset
    /// record and an optional syncToken.
    fn canned_page(record_name: &str, sync_token: Option<&str>) -> Value {
        let mut resp = json!({
            "records": [
                {
                    "recordName": record_name,
                    "recordType": "CPLMaster",
                    "fields": {
                        "filenameEnc": {"value": "dGVzdC5qcGc=", "type": "STRING"},
                        "resOriginalRes": {
                            "value": {
                                "downloadURL": "https://example.com/photo.jpg",
                                "size": 1024,
                                "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                            }
                        },
                        "resOriginalWidth": {"value": 100, "type": "INT64"},
                        "resOriginalHeight": {"value": 100, "type": "INT64"},
                        "resOriginalFileType": {"value": "public.jpeg"},
                        "itemType": {"value": "public.jpeg"},
                        "adjustmentRenderType": {"value": 0, "type": "INT64"}
                    },
                    "recordChangeTag": "ct1"
                },
                {
                    "recordName": format!("asset-{record_name}"),
                    "recordType": "CPLAsset",
                    "fields": {
                        "masterRef": {
                            "value": {"recordName": record_name, "zoneID": {"zoneName": "PrimarySync"}},
                            "type": "REFERENCE"
                        },
                        "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                        "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
                    },
                    "recordChangeTag": "ct2"
                }
            ]
        });
        if let Some(token) = sync_token {
            resp["syncToken"] = json!(token);
        }
        resp
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_returns_sync_token() {
        use tokio_stream::StreamExt;

        let session = CannedSession::new(vec![
            canned_page("master-1", Some("st-zone-abc")),
            // Second call returns empty records to stop the fetcher
            json!({"records": [], "syncToken": "st-zone-abc"}),
        ]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
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
        let session =
            CannedSession::new(vec![canned_page("master-1", None), json!({"records": []})]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
        tokio::pin!(stream);

        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
        }

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, None, "no syncToken in responses means None");
    }

    #[tokio::test]
    async fn test_photo_stream_with_token_last_token_wins() {
        use tokio_stream::StreamExt;

        // Two pages with different syncTokens — last one should be captured.
        // page_size=1 so each page yields 1 master record and the fetcher
        // advances offset by 1.
        let session = CannedSession::new(vec![
            canned_page("master-1", Some("st-first")),
            canned_page("master-2", Some("st-second")),
            json!({"records": []}),
        ]);
        let album = make_album_with_session(1, Box::new(session));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
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

    #[tokio::test]
    async fn test_photo_stream_with_token_empty_album() {
        use tokio_stream::StreamExt;

        // Album with no records at all
        let session = CannedSession::new(vec![json!({"records": []})]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.photo_stream_with_token(None, None, 1);
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

    // --- limit / --recent edge case tests ---

    #[tokio::test]
    async fn test_photo_stream_limit_zero_yields_nothing() {
        use tokio_stream::StreamExt;

        // --recent 0 should produce 0 items. The CannedSession has a valid
        // page available, but limit=0 means the fetcher should never send it.
        let session =
            CannedSession::new(vec![canned_page("master-1", None), json!({"records": []})]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, _handles) = album.photo_stream_inner(Some(0), Some(10), 1, None);
        tokio::pin!(stream);

        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 0, "--recent 0 should yield 0 items");
    }

    #[tokio::test]
    async fn test_photo_stream_limit_one_yields_exactly_one() {
        use tokio_stream::StreamExt;

        let session = CannedSession::new(vec![
            canned_page("master-1", None),
            canned_page("master-2", None),
            json!({"records": []}),
        ]);
        let album = make_album_with_session(1, Box::new(session));

        let (stream, _handles) = album.photo_stream_inner(Some(1), Some(10), 1, None);
        tokio::pin!(stream);

        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1, "--recent 1 should yield exactly 1 item");
        items[0].as_ref().expect("item should be Ok");
    }

    // --- changes_stream tests ---

    /// Build a canned `ChangesZoneResponse` JSON with the given records,
    /// syncToken, and moreComing flag.
    fn canned_changes_page(records: Vec<Value>, sync_token: &str, more_coming: bool) -> Value {
        json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": more_coming,
                "records": records
            }]
        })
    }

    /// Build a CPLMaster record for changes/zone tests.
    fn changes_master(record_name: &str) -> Value {
        json!({
            "recordName": record_name,
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": "dGVzdC5qcGc=", "type": "STRING"},
                "resOriginalRes": {
                    "value": {
                        "downloadURL": "https://example.com/photo.jpg",
                        "size": 1024,
                        "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                    }
                },
                "resOriginalWidth": {"value": 100, "type": "INT64"},
                "resOriginalHeight": {"value": 100, "type": "INT64"},
                "resOriginalFileType": {"value": "public.jpeg"},
                "itemType": {"value": "public.jpeg"},
                "adjustmentRenderType": {"value": 0, "type": "INT64"}
            },
            "recordChangeTag": "ct1"
        })
    }

    /// Build a CPLAsset record that references the given master.
    fn changes_asset(record_name: &str, master_ref: &str) -> Value {
        json!({
            "recordName": record_name,
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                    "value": {"recordName": master_ref, "zoneID": {"zoneName": "PrimarySync"}},
                    "type": "REFERENCE"
                },
                "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"},
                "addedDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
            },
            "recordChangeTag": "ct2"
        })
    }

    #[tokio::test]
    async fn test_changes_stream_single_page() {
        use tokio_stream::StreamExt;

        let records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let session = CannedSession::new(vec![canned_changes_page(records, "token-final", false)]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].record_name, "master-1");
        assert!(events[0].asset.is_some());
        assert_eq!(events[0].record_type.as_deref(), Some("CPLMaster"));

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token.as_deref(), Some("token-final"));
    }

    #[tokio::test]
    async fn test_changes_stream_multiple_pages() {
        use tokio_stream::StreamExt;

        let page1_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let page2_records = vec![
            changes_master("master-2"),
            changes_asset("asset-2", "master-2"),
        ];
        let session = CannedSession::new(vec![
            canned_changes_page(page1_records, "token-page1", true),
            canned_changes_page(page2_records, "token-page2", false),
        ]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].record_name, "master-1");
        assert_eq!(events[1].record_name, "master-2");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token.as_deref(), Some("token-page2"));
    }

    #[tokio::test]
    async fn test_changes_stream_empty_page_continues() {
        use tokio_stream::StreamExt;

        // First page: empty records but moreComing: true (normal API behavior)
        // Second page: actual records, moreComing: false
        let page2_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let session = CannedSession::new(vec![
            canned_changes_page(vec![], "token-empty", true),
            canned_changes_page(page2_records, "token-final", false),
        ]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1, "should yield the event from page 2");
        assert_eq!(events[0].record_name, "master-1");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token.as_deref(), Some("token-final"));
    }

    #[tokio::test]
    async fn test_changes_stream_zone_error() {
        use tokio_stream::StreamExt;

        let session = CannedSession::new(vec![json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync"},
                "syncToken": "",
                "moreComing": false,
                "serverErrorCode": "BAD_REQUEST",
                "reason": "Unknown sync continuation type"
            }]
        })]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.changes_stream("bad-token");
        tokio::pin!(stream);

        let mut items: Vec<anyhow::Result<ChangeEvent>> = Vec::new();
        while let Some(result) = stream.next().await {
            items.push(result);
        }

        assert_eq!(items.len(), 1, "should have exactly one error item");
        let err = items.into_iter().next().expect("should have item");
        assert!(err.is_err());
        let err_msg = format!("{}", err.unwrap_err());
        assert!(
            err_msg.contains("Invalid sync token"),
            "error should mention invalid sync token, got: {err_msg}"
        );

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, None, "token should be None on error");
    }

    #[tokio::test]
    async fn test_changes_stream_hard_deleted_record() {
        use super::super::types::ChangeReason;
        use tokio_stream::StreamExt;

        let records = vec![json!({
            "recordName": "deleted-record-1",
            "recordType": null,
            "deleted": true,
            "recordChangeTag": "ct-del"
        })];
        let session = CannedSession::new(vec![canned_changes_page(
            records,
            "token-after-delete",
            false,
        )]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].record_name, "deleted-record-1");
        assert_eq!(events[0].reason, ChangeReason::HardDeleted);
        assert!(events[0].asset.is_none(), "hard-deleted has no asset");
        assert!(
            events[0].record_type.is_none(),
            "hard-deleted has no record type"
        );

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token.as_deref(), Some("token-after-delete"));
    }

    #[tokio::test]
    async fn test_changes_stream_invalid_token_yields_typed_error() {
        use crate::icloud::photos::session::SyncTokenError;
        use tokio_stream::StreamExt;

        let session = CannedSession::new(vec![json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "",
                "moreComing": false,
                "serverErrorCode": "BAD_REQUEST",
                "reason": "Unknown sync continuation type"
            }]
        })]);
        let album = make_album_with_session(100, Box::new(session));

        let (stream, _token_rx) = album.changes_stream("old-token");
        tokio::pin!(stream);

        let mut items: Vec<anyhow::Result<ChangeEvent>> = Vec::new();
        while let Some(result) = stream.next().await {
            items.push(result);
        }

        assert_eq!(items.len(), 1, "should have exactly one error item");
        let err = items
            .into_iter()
            .next()
            .expect("should have item")
            .expect_err("should be an error");

        let sync_err = err
            .downcast_ref::<SyncTokenError>()
            .expect("error should downcast to SyncTokenError");

        match sync_err {
            SyncTokenError::InvalidToken { reason } => {
                assert_eq!(reason, "Unknown sync continuation type");
            }
            other => panic!("expected InvalidToken variant, got: {other:?}"),
        }
    }
}
