use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use rustc_hash::FxHashMap;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::Stream;

use super::asset::{ChangeEvent, DeltaRecordBuffer, PhotoAsset};
use super::cloudkit::ChangesZoneResponse;
use super::queries::{build_changes_zone_request, encode_params, DESIRED_KEYS_VALUES};
use super::session::{check_changes_zone_error, PhotosSession};
use crate::retry::RetryConfig;

/// How many consecutive empty /records/query pages trigger true EOF.
///
/// CloudKit's /records/query does not expose a `moreComing` flag; an empty
/// page can be either real end-of-list or a transient gap at this rank
/// range (e.g., a block of fully-deleted records aligning with a page
/// boundary). We probe forward by one `page_size` on each empty page and
/// only terminate after this many consecutive empty probes — two gives
/// us defense against a single-page gap at the cost of one extra request
/// on true EOF.
const MAX_EMPTY_PAGE_PROBES: u32 = 2;

/// A boxed, pinned stream of photo asset results.
type PhotoStream = Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>>;

/// Await all fetcher handles, logging and returning `true` if any panicked.
async fn await_fetcher_handles(handles: Vec<JoinHandle<()>>) -> bool {
    let mut panicked = false;
    for handle in handles {
        if let Err(e) = handle.await {
            if e.is_panic() {
                tracing::error!(error = ?e, "Photo fetcher task panicked");
                panicked = true;
            }
        }
    }
    panicked
}

/// A boxed, pinned stream of change event results.
type ChangeStream = Pin<Box<dyn Stream<Item = anyhow::Result<ChangeEvent>> + Send + 'static>>;

/// Determine how many parallel fetcher tasks to spawn.
///
/// We never spawn more fetchers than total pages (no empty fetchers)
/// and never more than the requested concurrency level.
fn determine_fetcher_count(total_items: u64, page_size: usize, concurrency: usize) -> usize {
    let total_pages = total_items.div_ceil(page_size as u64);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded to concurrency (usize) immediately via .min()"
    )]
    let pages_as_usize = total_pages as usize;
    pages_as_usize.min(concurrency).max(1)
}

/// Configuration for creating a `PhotoAlbum`, bundling all non-session fields.
#[derive(Debug)]
pub struct PhotoAlbumConfig {
    pub params: Arc<HashMap<String, Value>>,
    pub service_endpoint: Arc<str>,
    pub name: Arc<str>,
    pub list_type: Arc<str>,
    pub obj_type: Arc<str>,
    pub query_filter: Option<Arc<Value>>,
    pub page_size: usize,
    pub zone_id: Arc<Value>,
    pub retry_config: RetryConfig,
}

pub struct PhotoAlbum {
    pub(crate) name: Arc<str>,
    params: Arc<HashMap<String, Value>>,
    session: Box<dyn PhotosSession>,
    service_endpoint: Arc<str>,
    list_type: Arc<str>,
    obj_type: Arc<str>,
    query_filter: Option<Arc<Value>>,
    page_size: usize,
    zone_id: Arc<Value>,
    retry_config: RetryConfig,
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
            retry_config: config.retry_config,
        }
    }

    /// Return the CloudKit zone name this album belongs to
    /// (e.g. `PrimarySync`, `SharedSync-<uuid>`). Falls back to an empty
    /// string if the zone_id JSON lacks a `zoneName` field, which should
    /// only happen in hand-constructed test fixtures.
    pub fn zone_name(&self) -> &str {
        self.zone_id
            .get("zoneName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
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
                            "value": [&*self.obj_type]
                        },
                        "comparator": "IN",
                    },
                    "recordType": "HyperionIndexCountLookup",
                },
                "zoneWide": true,
                "zoneID": *self.zone_id,
            }]
        });

        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;

        let batch: super::cloudkit::BatchQueryResponse =
            serde_json::from_value(response).context("failed to parse album count response")?;
        let count = batch
            .batch
            .first()
            .and_then(|q| q.records.first())
            .and_then(|r| {
                r.fields
                    .get("itemCount")
                    .and_then(|f| f.get("value"))
                    .and_then(Value::as_u64)
            })
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
        let (stream, handles) = self.photo_stream_inner(limit, total_count, concurrency, None);
        tokio::spawn(await_fetcher_handles(handles));
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
            let fetcher_panicked = await_fetcher_handles(handles).await;
            // Suppress sync token if any fetcher panicked — the enumeration
            // is incomplete and the next sync must do a full re-enumeration.
            let final_token = if fetcher_panicked {
                None
            } else {
                shared_sync_token.lock().await.clone()
            };
            let _ = token_tx.send(final_token);
        });

        (stream, token_rx)
    }

    /// Stream record changes since the given syncToken via `changes/zone`.
    ///
    /// Returns a stream of `ChangeEvent`s and a oneshot receiver for the final syncToken.
    /// The syncToken is sent through the oneshot after all pages have been consumed
    /// (moreComing: false), or on error with the last successfully consumed token.
    ///
    /// This method is inherently sequential -- each page's syncToken feeds the next request.
    /// No parallel fetchers.
    pub fn changes_stream(
        &self,
        sync_token: &str,
    ) -> (ChangeStream, tokio::sync::oneshot::Receiver<String>) {
        let (tx, rx) = mpsc::channel::<anyhow::Result<ChangeEvent>>(200);
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();

        let session = self.session.clone_box();
        let service_endpoint = Arc::clone(&self.service_endpoint);
        let params = Arc::clone(&self.params);
        let zone_id = Arc::clone(&self.zone_id);
        let initial_token = sync_token.to_string();
        let album_name = Arc::clone(&self.name);
        let retry_config = self.retry_config;

        tokio::spawn(async move {
            let mut buffer = DeltaRecordBuffer::new();
            let mut current_token = initial_token;

            let url = format!(
                "{}/changes/zone?{}",
                service_endpoint,
                encode_params(&params)
            );

            let stream_error: Option<anyhow::Error> = loop {
                let body = build_changes_zone_request(&zone_id, Some(&current_token), 200);
                tracing::debug!(
                    album = %album_name,
                    token = %current_token,
                    "changes/zone request"
                );

                let response = match super::session::retry_post(
                    session.as_ref(),
                    &url,
                    &body.to_string(),
                    &[("Content-type", "text/plain")],
                    &retry_config,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => break Some(e),
                };

                let changes_resp: ChangesZoneResponse = match serde_json::from_value(response) {
                    Ok(r) => r,
                    Err(e) => break Some(e.into()),
                };

                let Some(zone_result) = changes_resp.zones.into_iter().next() else {
                    break Some(anyhow::anyhow!("changes/zone returned empty zones array"));
                };

                // Check for zone-level errors BEFORE advancing current_token.
                // On any zone error (including transient RETRY_LATER), the loop
                // breaks with current_token still set to the last-known-good
                // value so the caller can retry from a valid checkpoint.
                let zone_name = zone_result.zone_id.zone_name.clone();
                if let Err(sync_err) = check_changes_zone_error(
                    zone_result.server_error_code.as_deref(),
                    zone_result.reason.as_deref(),
                    &zone_name,
                ) {
                    break Some(sync_err.into());
                }

                current_token = zone_result.sync_token;
                let more_coming = zone_result.more_coming;

                tracing::debug!(
                    album = %album_name,
                    records = zone_result.records.len(),
                    more_coming,
                    new_token = %current_token,
                    "changes/zone page received"
                );

                let events = buffer.process_records(zone_result.records);
                for event in events {
                    if tx.send(Ok(event)).await.is_err() {
                        // Receiver dropped -- no one to flush to
                        let _ = token_tx.send(current_token);
                        return;
                    }
                }

                if !more_coming {
                    break None;
                }
            };

            // Always flush unpaired records, even on error
            let flush_events = buffer.flush();
            if stream_error.is_some() && !flush_events.is_empty() {
                tracing::warn!(
                    album = %album_name,
                    orphaned = flush_events.len(),
                    "flushing unpaired records after stream error"
                );
            }
            for event in flush_events {
                if tx.send(Ok(event)).await.is_err() {
                    let _ = token_tx.send(current_token);
                    return;
                }
            }

            if let Some(e) = stream_error {
                let _ = tx.send(Err(e)).await;
            }

            let _ = token_tx.send(current_token);
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
            .map(|tc| limit.map_or(tc, |lim| tc.min(u64::from(lim))))
            .or_else(|| limit.map(u64::from));

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
            #[allow(
                clippy::expect_used,
                reason = "effective_total is unconditionally set when num_fetchers > 1 (see compute above)"
            )]
            let total = effective_total.expect("effective_total set when num_fetchers > 1");
            // Partition offset range into non-overlapping chunks aligned to
            // page_size boundaries so each fetcher starts on a clean page.
            let chunk_size_items = {
                let raw = total.div_ceil(num_fetchers as u64);
                // Round up to next page_size boundary
                let ps = page_size as u64;
                raw.div_ceil(ps) * ps
            };

            tracing::debug!(
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
                        let remaining = u64::from(lim).saturating_sub(start);
                        #[allow(
                            clippy::cast_possible_truncation,
                            reason = "bounded by min(end-start, limit) where both operands originated from u32 fetcher limits"
                        )]
                        let capped = remaining.min(end - start) as u32;
                        Some(capped)
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
    /// - the API returns zero records (end of album)
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
        let service_endpoint = Arc::clone(&self.service_endpoint);
        let params = Arc::clone(&self.params);
        let name = Arc::clone(&self.name);
        let list_type = Arc::clone(&self.list_type);
        let query_filter = self.query_filter.as_ref().map(Arc::clone);
        let retry_config = self.retry_config;
        let page_size = self.page_size;
        let zone_id = Arc::clone(&self.zone_id);

        tokio::spawn(async move {
            let mut offset = start_offset;
            let mut total_sent: u64 = 0;
            let mut pending_masters: FxHashMap<String, super::cloudkit::Record> =
                FxHashMap::default();
            let mut consecutive_empty_pages: u32 = 0;
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
                    query_filter.as_deref(),
                    page_size,
                    &zone_id,
                    offset,
                    "ASCENDING",
                );
                tracing::debug!(
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
                    &retry_config,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        return;
                    }
                };
                tracing::debug!(
                    album = %name,
                    response = %serde_json::to_string_pretty(&response).unwrap_or_default(),
                    "Fetcher response"
                );

                let query: super::cloudkit::QueryResponse = match serde_json::from_value(response) {
                    Ok(q) => q,
                    Err(e) => {
                        tracing::warn!(
                            album = %name,
                            error = %e,
                            "Failed to deserialize fetcher response (body logged above at DEBUG)",
                        );
                        let _ = tx.send(Err(e.into())).await;
                        return;
                    }
                };

                // Capture the zone-level syncToken from each page response.
                if let Some(shared) = &shared_sync_token {
                    if let Some(token) = &query.sync_token {
                        *shared.lock().await = Some(token.clone());
                    }
                }

                let records = query.records;
                let record_count = records.len();

                tracing::debug!(
                    album = %name,
                    count = record_count,
                    offset,
                    "Got records"
                );

                // An empty page can mean either true end-of-list or a transient
                // gap at this rank range (e.g., a run of fully-deleted records
                // aligning with a page boundary). The API has no `moreComing`
                // flag on /records/query, so we probe forward by one
                // page_size before committing to EOF. The guard terminates
                // after MAX_EMPTY_PAGE_PROBES consecutive empty pages to avoid
                // unbounded scanning on genuinely empty tails.
                if record_count == 0 {
                    consecutive_empty_pages += 1;
                    if consecutive_empty_pages >= MAX_EMPTY_PAGE_PROBES {
                        tracing::debug!(
                            album = %name,
                            offset,
                            probes = consecutive_empty_pages,
                            "End of album (consecutive empty pages)"
                        );
                        break;
                    }
                    tracing::debug!(
                        album = %name,
                        offset,
                        probes = consecutive_empty_pages,
                        "Empty page, probing forward one page_size"
                    );
                    offset += page_size as u64;
                    continue;
                }
                consecutive_empty_pages = 0;

                // Collect current page's records, trying to pair with
                // buffered unpaired records from previous pages.
                let mut page_assets: FxHashMap<String, super::cloudkit::Record> =
                    FxHashMap::default();
                let mut page_masters: Vec<super::cloudkit::Record> = Vec::new();

                for rec in records {
                    tracing::debug!(record_type = %rec.record_type, "  record");
                    if rec.record_type == "CPLAsset" {
                        if let Some(master_id) = rec
                            .fields
                            .get("masterRef")
                            .and_then(|f| f.get("value"))
                            .and_then(|v| v.get("recordName"))
                            .and_then(Value::as_str)
                        {
                            let master_id = master_id.to_string();
                            // Try to pair with a buffered master from a previous page
                            if let Some(master) = pending_masters.remove(&master_id) {
                                let asset = PhotoAsset::from_records(master, rec);
                                if tx.send(Ok(asset)).await.is_err() {
                                    return;
                                }
                                total_sent += 1;
                            } else {
                                page_assets.insert(master_id, rec);
                            }
                        }
                    } else if rec.record_type == "CPLMaster" {
                        page_masters.push(rec);
                    }
                }

                if page_masters.is_empty() {
                    // No masters on this page. Advance offset to avoid
                    // re-requesting the same page forever. Use the unmatched
                    // asset count as a proxy for rank positions covered
                    // (each asset references one master/rank), with a minimum
                    // of 1 to guarantee forward progress.
                    let advance = page_assets.len().max(1) as u64;
                    offset += advance;
                    tracing::warn!(
                        album = %name,
                        record_count,
                        advance,
                        offset,
                        "Page returned records but no CPLMaster entries; advancing offset",
                    );
                }

                let mut limit_reached = false;
                for master in page_masters {
                    if let Some(n) = limit {
                        if total_sent >= u64::from(n) {
                            limit_reached = true;
                            break;
                        }
                    }
                    if let Some(asset_rec) = page_assets.remove(&master.record_name) {
                        let asset = PhotoAsset::from_records(master, asset_rec);
                        if tx.send(Ok(asset)).await.is_err() {
                            return;
                        }
                        total_sent += 1;
                    } else {
                        // Buffer unpaired master for pairing on subsequent pages
                        pending_masters.insert(master.record_name.clone(), master);
                    }
                    offset += 1;
                }

                tracing::debug!(
                    count = total_sent,
                    pending = pending_masters.len(),
                    range_start = start_offset,
                    "fetched photos so far"
                );

                if limit_reached {
                    break;
                }
            }

            // Log any remaining unpaired masters that couldn't be paired
            if !pending_masters.is_empty() {
                tracing::warn!(
                    count = pending_masters.len(),
                    "Unpaired CPLMaster records after full enumeration (no matching CPLAsset found)"
                );
                for id in pending_masters.keys() {
                    tracing::debug!(master_id = %id, "Unpaired CPLMaster");
                }
            }
        })
    }

    #[cfg(test)]
    fn list_query(&self, offset: u64, direction: &str) -> Value {
        Self::build_list_query(
            &self.list_type,
            self.query_filter.as_deref(),
            self.page_size,
            &self.zone_id,
            offset,
            direction,
        )
    }

    fn build_list_query(
        list_type: &str,
        query_filter: Option<&Value>,
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
        tracing::debug!(
            count = filter_by.len(),
            query = %serde_json::to_string(&query_part).unwrap_or_default(),
            "list_query filterBy"
        );
        tracing::debug!(
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
    use crate::test_helpers::MockPhotosSession;
    use serde_json::json;

    struct StubSession;

    #[async_trait::async_trait]
    impl PhotosSession for StubSession {
        async fn post(
            &self,
            _url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            unimplemented!("stub")
        }
        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(StubSession)
        }
    }

    fn make_album(
        page_size: usize,
        query_filter: Option<Arc<Value>>,
        zone_id: Value,
    ) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter,
                page_size,
                zone_id: Arc::new(zone_id),
                retry_config: RetryConfig::default(),
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

    fn make_album_with_session(page_size: usize, session: Box<dyn PhotosSession>) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size,
                zone_id: Arc::new(default_zone()),
                retry_config: RetryConfig::default(),
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
                                "downloadURL": "https://p01.icloud-content.com/photo.jpg",
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

        let mock = MockPhotosSession::new()
            .ok(canned_page("master-1", Some("st-zone-abc")))
            // Second call returns empty records to stop the fetcher
            .ok(json!({"records": [], "syncToken": "st-zone-abc"}));
        let album = make_album_with_session(100, Box::new(mock));

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
        let mock = MockPhotosSession::new()
            .ok(canned_page("master-1", None))
            .ok(json!({"records": []}));
        let album = make_album_with_session(100, Box::new(mock));

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
        let mock = MockPhotosSession::new()
            .ok(canned_page("master-1", Some("st-first")))
            .ok(canned_page("master-2", Some("st-second")))
            .ok(json!({"records": []}));
        let album = make_album_with_session(1, Box::new(mock));

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
        let mock = MockPhotosSession::new().ok(json!({"records": []}));
        let album = make_album_with_session(100, Box::new(mock));

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

        // --recent 0 should produce 0 items. The mock has a valid page
        // available, but limit=0 means the fetcher should never send it.
        let mock = MockPhotosSession::new()
            .ok(canned_page("master-1", None))
            .ok(json!({"records": []}));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(Some(0), Some(10), 1, None);
        tokio::pin!(stream);

        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 0, "--recent 0 should yield 0 items");
    }

    #[tokio::test]
    async fn test_photo_stream_limit_one_yields_exactly_one() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new()
            .ok(canned_page("master-1", None))
            .ok(canned_page("master-2", None))
            .ok(json!({"records": []}));
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(Some(1), Some(10), 1, None);
        tokio::pin!(stream);

        let items: Vec<_> = stream.collect().await;
        assert_eq!(items.len(), 1, "--recent 1 should yield exactly 1 item");
        items[0].as_ref().expect("item should be Ok");
    }

    // --- pagination edge case tests ---

    /// CF-1: When a page returns only CPLAsset records (no CPLMaster), the
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

        // Page 2: Valid paired CPLMaster + CPLAsset.
        let page2 = canned_page("master-ok", None);

        // Page 3: Empty → terminates.
        let mock = MockPhotosSession::new()
            .ok(page1)
            .ok(page2)
            .ok(json!({"records": []}));
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(None, None, 1, None);
        tokio::pin!(stream);

        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        assert_eq!(
            count, 1,
            "page 2's paired asset should be yielded despite page 1 having no masters"
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
            .ok(canned_page("master-1", None))
            // Page 2 is empty (simulated gap); must not terminate.
            .ok(json!({"records": []}))
            // Page 3 contains records past the gap.
            .ok(canned_page("master-2", None));
        // MockPhotosSession then returns the default {"records": []} on
        // every subsequent call; the fetcher requires MAX_EMPTY_PAGE_PROBES
        // consecutive empties to commit to EOF.
        let album = make_album_with_session(1, Box::new(mock));

        let (stream, _handles) = album.photo_stream_inner(None, None, 1, None);
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
                        "downloadURL": "https://p01.icloud-content.com/photo.jpg",
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
        let mock = MockPhotosSession::new().ok(canned_changes_page(records, "token-final", false));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "master-1");
        assert!(events[0].asset.is_some());
        assert_eq!(events[0].record_type.as_deref(), Some("CPLMaster"));

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-final");
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
        let mock = MockPhotosSession::new()
            .ok(canned_changes_page(page1_records, "token-page1", true))
            .ok(canned_changes_page(page2_records, "token-page2", false));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 2);
        assert_eq!(&*events[0].record_name, "master-1");
        assert_eq!(&*events[1].record_name, "master-2");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-page2");
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
        let mock = MockPhotosSession::new()
            .ok(canned_changes_page(vec![], "token-empty", true))
            .ok(canned_changes_page(page2_records, "token-final", false));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1, "should yield the event from page 2");
        assert_eq!(&*events[0].record_name, "master-1");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-final");
    }

    #[tokio::test]
    async fn test_changes_stream_zone_error() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new().ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync"},
                "syncToken": "",
                "moreComing": false,
                "serverErrorCode": "BAD_REQUEST",
                "reason": "Unknown sync continuation type"
            }]
        }));
        let album = make_album_with_session(100, Box::new(mock));

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
        assert_eq!(
            token, "bad-token",
            "on error, should preserve the last-good token for checkpoint"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_transient_zone_error_preserves_initial_token() {
        // A transient zone code (RETRY_LATER, SERVER_INTERNAL_ERROR, etc.)
        // on the very first page must not lose the caller's initial sync_token.
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new().ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "",
                "moreComing": false,
                "serverErrorCode": "RETRY_LATER",
                "reason": "temporary backend issue"
            }]
        }));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-T0");
        tokio::pin!(stream);

        let mut errors = 0usize;
        while let Some(result) = stream.next().await {
            if result.is_err() {
                errors += 1;
            }
        }
        assert_eq!(errors, 1, "should surface the zone error");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "token-T0",
            "transient zone error on first page must preserve the caller's initial token"
        );
    }

    #[tokio::test]
    async fn test_changes_stream_mid_stream_error_preserves_last_good_token() {
        use tokio_stream::StreamExt;

        let page1_records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let page2_records = vec![
            changes_master("master-2"),
            changes_asset("asset-2", "master-2"),
        ];
        // Pages 1-2 succeed, page 3 returns a zone error
        let mock = MockPhotosSession::new()
            .ok(canned_changes_page(page1_records, "token-page1", true))
            .ok(canned_changes_page(page2_records, "token-page2", true))
            .ok(json!({
                "zones": [{
                    "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                    "syncToken": "",
                    "moreComing": false,
                    "serverErrorCode": "BAD_REQUEST",
                    "reason": "Unknown sync continuation type"
                }]
            }));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-initial");
        tokio::pin!(stream);

        let mut events = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => events.push(event),
                Err(e) => errors.push(e),
            }
        }

        assert_eq!(events.len(), 2, "should have events from pages 1 and 2");
        assert_eq!(&*events[0].record_name, "master-1");
        assert_eq!(&*events[1].record_name, "master-2");
        assert_eq!(errors.len(), 1, "should have exactly one error from page 3");

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(
            token, "token-page2",
            "should preserve last-good token from page 2, not initial or error page"
        );
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
        let mock =
            MockPhotosSession::new().ok(canned_changes_page(records, "token-after-delete", false));
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);

        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("should be Ok"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "deleted-record-1");
        assert_eq!(events[0].reason, ChangeReason::HardDeleted);
        assert!(events[0].asset.is_none(), "hard-deleted has no asset");
        assert!(
            events[0].record_type.is_none(),
            "hard-deleted has no record type"
        );

        let token = token_rx.await.expect("oneshot should not be dropped");
        assert_eq!(token, "token-after-delete");
    }

    #[tokio::test]
    async fn test_changes_stream_invalid_token_yields_typed_error() {
        use crate::icloud::photos::session::SyncTokenError;
        use tokio_stream::StreamExt;

        let mock = MockPhotosSession::new().ok(json!({
            "zones": [{
                "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
                "syncToken": "",
                "moreComing": false,
                "serverErrorCode": "BAD_REQUEST",
                "reason": "Unknown sync continuation type"
            }]
        }));
        let album = make_album_with_session(100, Box::new(mock));

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
                assert_eq!(&**reason, "Unknown sync continuation type");
            }
            other => panic!("expected InvalidToken variant, got: {other:?}"),
        }
    }
}
