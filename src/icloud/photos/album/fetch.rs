//! CloudKit page fetching, record pairing, and asset emission.

use super::completion::{EnumerationCompletion, EnumerationFailure, FetcherSyncTokenCapture};
use super::planning::{FetcherRange, FetcherRangeRole};
use super::{MAX_EMPTY_PAGE_PROBES, PhotoAlbum};
use crate::icloud::photos::asset::{PhotoAsset, RequiredAssetFields};
use crate::icloud::photos::cloudkit;
use crate::icloud::photos::queries::{DESIRED_KEYS_VALUES, encode_params};
use crate::icloud::photos::session;
use rustc_hash::{FxHashMap, FxHashSet};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

fn prune_paired_master_cache(
    paired_masters: &mut FxHashMap<String, cloudkit::Record>,
    max_records: usize,
) {
    if paired_masters.len() <= max_records {
        return;
    }
    if let Some(key) = paired_masters.keys().next().cloned() {
        paired_masters.remove(&key);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FetcherBehavior {
    pub(super) page_size: usize,
    pub(super) preserve_blank_sync_tokens_for_diagnostics: bool,
    pub(super) allow_unpaired_at_range_boundary: bool,
    pub(super) treat_empty_tail_as_error: bool,
}

/// Metadata at DEBUG; raw body only at TRACE. Including the body in
/// the DEBUG event allocates ~MB per page on busy libraries (every
/// fetched page formats the full response value).
fn log_fetcher_response(album: &str, response: &Value) {
    tracing::debug!(target: "kei::icloud::photos::album", album = %album, "Fetcher response");
    tracing::trace!(target: "kei::icloud::photos::album",
        album = %album,
        response = %response,
        "Fetcher response body",
    );
}

fn should_emit_asset_record(
    record_name: &str,
    range_start: u64,
    range_role: FetcherRangeRole,
    emitted_in_range: &mut FxHashSet<String>,
    range_record_owners: &std::sync::Mutex<FxHashMap<String, u64>>,
) -> bool {
    if !matches!(range_role, FetcherRangeRole::Data)
        && !emitted_in_range.insert(record_name.to_owned())
    {
        return false;
    }

    let Ok(mut owners) = range_record_owners.lock() else {
        return true;
    };
    match owners.get(record_name) {
        Some(owner) => *owner == range_start,
        None => {
            owners.insert(record_name.to_owned(), range_start);
            true
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullAssetStateId {
    Master,
    Asset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FullAssetEmission {
    Sent,
    Malformed,
    ReceiverClosed,
}

async fn emit_full_asset(
    tx: &mpsc::Sender<anyhow::Result<PhotoAsset>>,
    master: cloudkit::Record,
    asset_record: &cloudkit::Record,
    state_id: FullAssetStateId,
) -> FullAssetEmission {
    let mut asset =
        match PhotoAsset::try_from_records(master, asset_record, RequiredAssetFields::Downloadable)
        {
            Ok(asset) => asset,
            Err(error) => {
                return if tx
                    .send(Err(anyhow::anyhow!(
                        "Malformed required CloudKit asset field: {error}"
                    )))
                    .await
                    .is_ok()
                {
                    FullAssetEmission::Malformed
                } else {
                    FullAssetEmission::ReceiverClosed
                };
            }
        };
    if state_id == FullAssetStateId::Asset {
        asset = asset.with_state_record_name(Arc::from(asset_record.record_name.as_str()));
    }
    if tx.send(Ok(asset)).await.is_ok() {
        FullAssetEmission::Sent
    } else {
        FullAssetEmission::ReceiverClosed
    }
}

impl PhotoAlbum {
    /// Spawn a background tokio task that pages through records from
    /// `start_offset` up to (but not including) `end_offset`, sending each
    /// `PhotoAsset` into `tx`. The task stops when:
    /// - `offset >= end_offset`
    /// - the API returns zero records (end of album)
    /// - the per-fetcher `limit` is reached
    /// - the receiver is dropped
    ///
    /// If `fetcher_sync_tokens` is provided, the fetcher appends the last
    /// non-None `syncToken` it observed. The monitor task compares every
    /// fetcher's final token before allowing token advancement.
    pub(super) fn spawn_fetcher(
        &self,
        tx: mpsc::Sender<anyhow::Result<PhotoAsset>>,
        range: FetcherRange,
        range_record_owners: Arc<std::sync::Mutex<FxHashMap<String, u64>>>,
        fetcher_sync_tokens: Option<Arc<FetcherSyncTokenCapture>>,
        behavior: FetcherBehavior,
    ) -> JoinHandle<()> {
        let session = self.session.clone_box();
        let service_endpoint = Arc::clone(&self.service_endpoint);
        let params = Arc::clone(&self.params);
        let name = Arc::clone(&self.name);
        let list_type = Arc::clone(&self.list_type);
        let query_filter = self.query_filter.as_ref().map(Arc::clone);
        let retry_config = self.retry_config;
        let zone_id = Arc::clone(&self.zone_id);

        tokio::spawn(async move {
            let FetcherRange {
                start: start_offset,
                end: end_offset,
                limit,
                role: range_role,
            } = range;
            let mut offset = start_offset;
            let mut total_sent: u64 = 0;
            let mut last_sync_token: Option<String> = None;
            let mut saw_blank_sync_token = false;
            let mut pending_masters: FxHashMap<String, cloudkit::Record> = FxHashMap::default();
            let mut pending_assets: FxHashMap<String, Vec<cloudkit::Record>> = FxHashMap::default();
            let mut paired_masters: FxHashMap<String, cloudkit::Record> = FxHashMap::default();
            let mut emitted_asset_records: FxHashSet<String> = FxHashSet::default();
            let mut consecutive_empty_pages: u32 = 0;
            let mut enumeration_incomplete = false;
            let mut malformed_record = false;
            let mut stopped_for_limit = false;
            let url = format!(
                "{}/records/query?{}",
                service_endpoint,
                encode_params(&params)
            );
            let max_pending_records = behavior.page_size.saturating_mul(4).max(1);

            loop {
                // Dropping the stream is the caller's cancellation signal.
                // Do not continue probing the provider or report checkpoint
                // evidence after the consumer has stopped observing assets.
                if tx.is_closed() {
                    if let Some(shared) = &fetcher_sync_tokens {
                        shared
                            .complete(
                                None,
                                EnumerationCompletion::Incomplete(
                                    EnumerationFailure::ConsumerDropped,
                                ),
                            )
                            .await;
                    }
                    return;
                }
                if offset >= end_offset {
                    break;
                }

                let body = Self::build_list_query(
                    &list_type,
                    query_filter.as_deref(),
                    behavior.page_size,
                    &zone_id,
                    offset,
                    "ASCENDING",
                );
                tracing::debug!(target: "kei::icloud::photos::album",
                    album = %name,
                    range_start = start_offset,
                    range_end = end_offset,
                    offset,
                    "Fetcher POST"
                );
                let response = match session::retry_post(
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
                        if let Some(shared) = &fetcher_sync_tokens {
                            shared
                                .complete(
                                    None,
                                    EnumerationCompletion::Incomplete(
                                        EnumerationFailure::FetcherError,
                                    ),
                                )
                                .await;
                        }
                        return;
                    }
                };
                log_fetcher_response(&name, &response);

                let query: cloudkit::QueryResponse = match serde_json::from_value(response) {
                    Ok(q) => q,
                    Err(e) => {
                        tracing::warn!(target: "kei::icloud::photos::album",
                            album = %name,
                            error = %e,
                            "Failed to deserialize fetcher response (body logged above at DEBUG)",
                        );
                        let _ = tx.send(Err(e.into())).await;
                        if let Some(shared) = &fetcher_sync_tokens {
                            shared
                                .complete(
                                    None,
                                    EnumerationCompletion::Incomplete(
                                        EnumerationFailure::FetcherError,
                                    ),
                                )
                                .await;
                        }
                        return;
                    }
                };

                // Capture the zone-level syncToken from each page response.
                // Treat blank tokens as missing so we never persist an
                // unusable marker that forces the next cycle back to full.
                if let Some(token) = query.sync_token.as_deref() {
                    let trimmed = token.trim();
                    if trimmed.is_empty() {
                        saw_blank_sync_token = true;
                        tracing::warn!(target: "kei::icloud::photos::album",
                            album = %name,
                            offset,
                            "Fetcher response contained blank syncToken; treating as unavailable"
                        );
                    } else {
                        last_sync_token = Some(trimmed.to_string());
                    }
                }

                let records = query.records;
                let record_count = records.len();

                tracing::debug!(target: "kei::icloud::photos::album",
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
                        // Promoted to info! so an enumeration that
                        // terminates after probing past empty pages is
                        // visible in normal logs — operators chasing a
                        // suspected silent truncation should see the
                        // probe count and total_sent here.
                        tracing::info!(target: "kei::icloud::photos::album",
                            album = %name,
                            offset,
                            probes = consecutive_empty_pages,
                            total_sent,
                            "End of album (consecutive empty pages)"
                        );
                        if behavior.treat_empty_tail_as_error
                            && matches!(range_role, FetcherRangeRole::Data)
                            && limit.is_none()
                            && end_offset == u64::MAX
                            && fetcher_sync_tokens.is_some()
                        {
                            enumeration_incomplete = true;
                            let _ = tx
                                .send(Err(anyhow::anyhow!(
                                    "Photo enumeration is incomplete: Apple returned {} consecutive empty pages without confirming the end of the stream.",
                                    MAX_EMPTY_PAGE_PROBES
                                )))
                                .await;
                        }
                        break;
                    }
                    tracing::debug!(target: "kei::icloud::photos::album",
                        album = %name,
                        offset,
                        probes = consecutive_empty_pages,
                        "Empty page, probing forward one page_size"
                    );
                    offset += behavior.page_size as u64;
                    continue;
                }
                // Collect current page's records, trying to pair with
                // buffered unpaired records from previous pages.
                let mut page_assets: FxHashMap<String, Vec<cloudkit::Record>> =
                    FxHashMap::default();
                let mut page_masters: Vec<cloudkit::Record> = Vec::new();
                let mut masters_seen_on_page = false;
                let mut limit_reached = false;
                let mut page_emitted = 0u64;

                for rec in records {
                    tracing::debug!(target: "kei::icloud::photos::album", record_type = %rec.record_type, "  record");
                    if rec.record_type == "CPLAsset" {
                        if let Some(master_id) = rec
                            .fields
                            .get("masterRef")
                            .and_then(|f| f.get("value"))
                            .and_then(|v| v.get("recordName"))
                            .and_then(Value::as_str)
                        {
                            let master_id = master_id.to_string();
                            page_assets.entry(master_id).or_default().push(rec);
                        }
                    } else if rec.record_type == "CPLMaster" {
                        masters_seen_on_page = true;
                        page_masters.push(rec);
                    }
                }

                if limit_reached {
                    stopped_for_limit = matches!(range_role, FetcherRangeRole::LimitProbe);
                    break;
                }

                if !masters_seen_on_page {
                    // No masters on this page. Advance offset to avoid
                    // re-requesting the same page forever. Use the unmatched
                    // asset count as a proxy for rank positions covered
                    // (each asset references one master/rank), with a minimum
                    // of 1 to guarantee forward progress.
                    let advance = page_assets.values().map(Vec::len).sum::<usize>().max(1) as u64;
                    offset += advance;
                    tracing::warn!(target: "kei::icloud::photos::album",
                        album = %name,
                        record_count,
                        advance,
                        offset,
                        "Page returned records but no CPLMaster entries; advancing offset",
                    );
                }

                for master in page_masters {
                    let mut asset_records = pending_assets.remove(&master.record_name);
                    if let Some(page_records) = page_assets.remove(&master.record_name) {
                        asset_records
                            .get_or_insert_with(Vec::new)
                            .extend(page_records);
                    }
                    if let Some(asset_records) = asset_records {
                        let sibling_count = asset_records.len();
                        for (index, asset_rec) in asset_records.into_iter().enumerate() {
                            if !should_emit_asset_record(
                                &asset_rec.record_name,
                                start_offset,
                                range_role,
                                &mut emitted_asset_records,
                                &range_record_owners,
                            ) {
                                continue;
                            }
                            if let Some(n) = limit
                                && total_sent >= u64::from(n)
                            {
                                limit_reached = true;
                                break;
                            }
                            let state_id = if sibling_count > 1 && index > 0 {
                                FullAssetStateId::Asset
                            } else {
                                FullAssetStateId::Master
                            };
                            match emit_full_asset(&tx, master.clone(), &asset_rec, state_id).await {
                                FullAssetEmission::Sent => {
                                    total_sent += 1;
                                    page_emitted += 1;
                                }
                                FullAssetEmission::Malformed => {
                                    enumeration_incomplete = true;
                                    malformed_record = true;
                                }
                                FullAssetEmission::ReceiverClosed => return,
                            }
                        }
                        paired_masters.insert(master.record_name.clone(), master);
                        prune_paired_master_cache(&mut paired_masters, max_pending_records);
                    } else {
                        // Buffer unpaired master for pairing on subsequent pages
                        pending_masters.insert(master.record_name.clone(), master);
                    }
                    offset += 1;
                }

                tracing::debug!(target: "kei::icloud::photos::album",
                    count = total_sent,
                    pending = pending_masters.len(),
                    range_start = start_offset,
                    "fetched photos so far"
                );

                if limit_reached {
                    stopped_for_limit = matches!(range_role, FetcherRangeRole::LimitProbe);
                    break;
                }

                for (master_id, records) in page_assets {
                    if let Some(master) = pending_masters.remove(&master_id) {
                        let sibling_count = records.len();
                        for (index, asset_rec) in records.into_iter().enumerate() {
                            if !should_emit_asset_record(
                                &asset_rec.record_name,
                                start_offset,
                                range_role,
                                &mut emitted_asset_records,
                                &range_record_owners,
                            ) {
                                continue;
                            }
                            if let Some(n) = limit
                                && total_sent >= u64::from(n)
                            {
                                limit_reached = true;
                                break;
                            }
                            let state_id = if sibling_count > 1 && index > 0 {
                                FullAssetStateId::Asset
                            } else {
                                FullAssetStateId::Master
                            };
                            match emit_full_asset(&tx, master.clone(), &asset_rec, state_id).await {
                                FullAssetEmission::Sent => {
                                    total_sent += 1;
                                    page_emitted += 1;
                                }
                                FullAssetEmission::Malformed => {
                                    enumeration_incomplete = true;
                                    malformed_record = true;
                                }
                                FullAssetEmission::ReceiverClosed => return,
                            }
                        }
                        paired_masters.insert(master.record_name.clone(), master);
                        prune_paired_master_cache(&mut paired_masters, max_pending_records);
                    } else if let Some(master) = paired_masters.get(&master_id) {
                        for asset_rec in records {
                            if !should_emit_asset_record(
                                &asset_rec.record_name,
                                start_offset,
                                range_role,
                                &mut emitted_asset_records,
                                &range_record_owners,
                            ) {
                                continue;
                            }
                            if let Some(n) = limit
                                && total_sent >= u64::from(n)
                            {
                                limit_reached = true;
                                break;
                            }
                            match emit_full_asset(
                                &tx,
                                master.clone(),
                                &asset_rec,
                                FullAssetStateId::Asset,
                            )
                            .await
                            {
                                FullAssetEmission::Sent => {
                                    total_sent += 1;
                                    page_emitted += 1;
                                }
                                FullAssetEmission::Malformed => {
                                    enumeration_incomplete = true;
                                    malformed_record = true;
                                }
                                FullAssetEmission::ReceiverClosed => return,
                            }
                        }
                    } else {
                        pending_assets.entry(master_id).or_default().extend(records);
                    }
                }
                if limit_reached {
                    stopped_for_limit = matches!(range_role, FetcherRangeRole::LimitProbe);
                    break;
                }
                let pending_record_count =
                    pending_masters.len() + pending_assets.values().map(Vec::len).sum::<usize>();
                if pending_record_count > max_pending_records {
                    enumeration_incomplete = true;
                    let _ = tx
                        .send(Err(anyhow::anyhow!(
                            "Photo enumeration is incomplete: {} unpaired CPLMaster/CPLAsset records exceeded the pending-pair buffer.",
                            pending_record_count
                        )))
                        .await;
                    break;
                }
                if page_emitted == 0 {
                    consecutive_empty_pages += 1;
                    if consecutive_empty_pages >= MAX_EMPTY_PAGE_PROBES {
                        tracing::info!(target: "kei::icloud::photos::album",
                            album = %name,
                            offset,
                            probes = consecutive_empty_pages,
                            total_sent,
                            "End of album (consecutive pages without new assets)"
                        );
                        break;
                    }
                } else {
                    consecutive_empty_pages = 0;
                }
            }

            // Surface any remaining unpaired records that couldn't be paired.
            // A full query stream cannot safely advance a sync token if it saw
            // only one half of a CPLMaster/CPLAsset pair.
            if !stopped_for_limit
                && !behavior.allow_unpaired_at_range_boundary
                && (!pending_masters.is_empty() || !pending_assets.is_empty())
            {
                enumeration_incomplete = true;
                tracing::warn!(target: "kei::icloud::photos::album",
                    masters = pending_masters.len(),
                    assets = pending_assets.values().map(Vec::len).sum::<usize>(),
                    "Unpaired CPLMaster/CPLAsset records after full enumeration"
                );
                for id in pending_masters.keys() {
                    tracing::debug!(target: "kei::icloud::photos::album", master_id = %id, "Unpaired CPLMaster");
                }
                for (id, records) in &pending_assets {
                    tracing::debug!(target: "kei::icloud::photos::album", master_id = %id, count = records.len(), "Unpaired CPLAsset");
                }
                let pending_asset_count = pending_assets.values().map(Vec::len).sum::<usize>();
                let _ = tx
                    .send(Err(anyhow::anyhow!(
                        "Photo enumeration is incomplete: {} unpaired CPLMaster records and {} unpaired CPLAsset records remained at the end of the stream.",
                        pending_masters.len(),
                        pending_asset_count
                    )))
                    .await;
            }

            if let Some(shared) = &fetcher_sync_tokens {
                if stopped_for_limit {
                    shared.suppress();
                }
                let token = last_sync_token.or_else(|| {
                    (saw_blank_sync_token && behavior.preserve_blank_sync_tokens_for_diagnostics)
                        .then(String::new)
                });
                let completion = if malformed_record {
                    EnumerationCompletion::Incomplete(EnumerationFailure::MalformedRecord)
                } else if enumeration_incomplete {
                    EnumerationCompletion::Incomplete(EnumerationFailure::UnpairedRecords)
                } else if stopped_for_limit {
                    EnumerationCompletion::UserBoundReached
                } else {
                    EnumerationCompletion::ProvenEof
                };
                shared.complete(token, completion).await;
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

        if let Some(qf) = query_filter
            && let Some(arr) = qf.as_array()
        {
            filter_by.extend(arr.iter().cloned());
        }

        let query_part = json!({
            "filterBy": &filter_by,
            "recordType": list_type,
        });
        tracing::debug!(target: "kei::icloud::photos::album",
            count = filter_by.len(),
            query = %serde_json::to_string(&query_part).unwrap_or_default(),
            "list_query filterBy"
        );
        tracing::debug!(target: "kei::icloud::photos::album",
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

#[cfg(test)]
mod tests;
