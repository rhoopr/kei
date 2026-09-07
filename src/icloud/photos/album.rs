use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};

use anyhow::Context;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_stream::Stream;
use tokio_util::sync::CancellationToken;

use super::asset::{
    ChangeEvent, DeltaRecordBuffer, PhotoAsset, RequiredAssetFields, extract_master_ref,
};
use super::cloudkit::ChangesZoneResponse;
use super::queries::{DESIRED_KEYS_VALUES, build_changes_zone_request, encode_params};
use super::session::{PhotosSession, check_changes_zone_error};
use crate::retry::RetryConfig;

/// How many consecutive empty /records/query pages trigger true EOF.
///
/// CloudKit's /records/query does not expose a `moreComing` flag; an empty
/// page can be either real end-of-list or a transient gap at this rank
/// range (e.g., a block of fully-deleted records aligning with a page
/// boundary). We probe forward by one `page_size` on each empty page and
/// only terminate after this many consecutive empty probes.
///
/// Set conservatively so a multi-page run of fully-deleted records does
/// not silently truncate enumeration; the cost on true EOF is at most
/// `MAX_EMPTY_PAGE_PROBES - 1` extra empty requests per fetcher.
pub(crate) const MAX_EMPTY_PAGE_PROBES: u32 = 5;
pub(crate) const DEFAULT_PAGE_SIZE: usize = 100;
pub(crate) const QUERY_ALL_LIST: &str = "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted";
pub(crate) const QUERY_ALL_OBJ: &str = "CPLAssetByAssetDateWithoutHiddenOrDeleted";
pub(crate) const QUERY_FOLDER_LIST: &str = "CPLContainerRelationLiveByAssetDate";

/// A boxed, pinned stream of photo asset results.
type PhotoStream = Pin<Box<dyn Stream<Item = anyhow::Result<PhotoAsset>> + Send + 'static>>;

const RECORD_LOOKUP_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProviderRecordId(Arc<str>);

impl ProviderRecordId {
    pub(crate) fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordLookupRequest {
    pub(crate) state_id: ProviderRecordId,
    pub(crate) master_record_name: ProviderRecordId,
    pub(crate) asset_record_name: Option<ProviderRecordId>,
    target: RecordLookupTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordLookupTarget {
    Master,
    Asset,
}

impl RecordLookupRequest {
    pub(crate) fn paired(
        state_id: ProviderRecordId,
        master_record_name: ProviderRecordId,
        asset_record_name: ProviderRecordId,
    ) -> Self {
        Self {
            state_id,
            master_record_name,
            asset_record_name: Some(asset_record_name),
            target: RecordLookupTarget::Master,
        }
    }

    pub(crate) fn master_only(
        state_id: ProviderRecordId,
        master_record_name: ProviderRecordId,
    ) -> Self {
        Self {
            state_id,
            master_record_name,
            asset_record_name: None,
            target: RecordLookupTarget::Master,
        }
    }

    pub(crate) fn asset_only(asset_record_name: ProviderRecordId) -> Self {
        Self {
            state_id: asset_record_name.clone(),
            master_record_name: asset_record_name,
            asset_record_name: None,
            target: RecordLookupTarget::Asset,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ProviderLookupError {
    #[error("provider authentication failed with HTTP {status}: {message}")]
    Authentication { status: u16, message: String },
    #[error("provider record lookup was rate limited with HTTP {status}: {message}")]
    RateLimited { status: u16, message: String },
    #[error("provider record lookup request failed: {0}")]
    Request(String),
    #[error("provider record lookup response was malformed: {0}")]
    Malformed(String),
}

fn classify_provider_lookup_error(error: &anyhow::Error) -> ProviderLookupError {
    let message = error.to_string();
    let Some(http) = error.downcast_ref::<super::session::HttpStatusError>() else {
        return ProviderLookupError::Request(message);
    };
    match http.status {
        401 | 403 | 421 => ProviderLookupError::Authentication {
            status: http.status,
            message,
        },
        429 | 503 => ProviderLookupError::RateLimited {
            status: http.status,
            message,
        },
        _ => ProviderLookupError::Request(message),
    }
}

#[derive(Debug)]
pub(crate) enum RecordResolution {
    Present(PhotoAsset),
    AssetPresent {
        master_record_name: ProviderRecordId,
    },
    MasterPresent,
    Deleted {
        deleted_at: Option<chrono::DateTime<chrono::Utc>>,
        master_family: bool,
    },
    Unknown,
    TransientFailure(ProviderLookupError),
}

#[derive(Debug)]
pub(crate) struct RecordResolutionBatch {
    pub(crate) results: Vec<(ProviderRecordId, RecordResolution)>,
    pub(crate) complete: bool,
    pub(crate) rate_limit_observations: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ResolutionEvidence {
    ChildDeleted,
    Inconclusive,
    MasterPresent,
    AssetPresent,
    MasterDeleted,
    Present,
}

fn resolution_evidence(resolution: &RecordResolution) -> ResolutionEvidence {
    match resolution {
        RecordResolution::Present(_) => ResolutionEvidence::Present,
        RecordResolution::AssetPresent { .. } => ResolutionEvidence::AssetPresent,
        RecordResolution::MasterPresent => ResolutionEvidence::MasterPresent,
        RecordResolution::Deleted {
            master_family: true,
            ..
        } => ResolutionEvidence::MasterDeleted,
        RecordResolution::Unknown | RecordResolution::TransientFailure(_) => {
            ResolutionEvidence::Inconclusive
        }
        RecordResolution::Deleted {
            master_family: false,
            ..
        } => ResolutionEvidence::ChildDeleted,
    }
}

// Multiple provider records can map to one durable state identity. Merge them
// conservatively: a present sibling resolves the work, a missing master proves
// family deletion, and any inconclusive sibling blocks child-only deletion.
fn merge_record_resolution(existing: &mut RecordResolution, incoming: RecordResolution) {
    if resolution_evidence(&incoming) > resolution_evidence(existing) {
        *existing = incoming;
    }
}

fn prune_paired_master_cache(
    paired_masters: &mut FxHashMap<String, super::cloudkit::Record>,
    max_records: usize,
) {
    if paired_masters.len() <= max_records {
        return;
    }
    if let Some(key) = paired_masters.keys().next().cloned() {
        paired_masters.remove(&key);
    }
}

/// Keep signed CDN URLs close to the download workers that will consume them.
///
/// A normal CloudKit page is optimized for fast enumeration, but each
/// `PhotoAsset` also carries short-lived content URLs. During real downloads,
/// fetch only a small number of waves ahead of the worker pool so slow media
/// cannot age thousands of prefetched URLs before transfer starts.
fn download_stream_page_size(default_page_size: usize, download_concurrency: usize) -> usize {
    default_page_size
        .min(download_concurrency.max(1).saturating_mul(2))
        .max(1)
}

/// Await all fetcher handles, logging and returning `true` if any panicked.
async fn await_fetcher_handles(handles: Vec<JoinHandle<()>>) -> bool {
    let mut panicked = false;
    for handle in handles {
        if let Err(e) = handle.await
            && e.is_panic()
        {
            tracing::error!(error = ?e, "Photo fetcher task panicked");
            panicked = true;
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

/// Profile for CloudKit record enumeration.
///
/// This intentionally separates *which ranks are enumerated* from *how much
/// URL-bearing data is allowed to sit ahead of the downloader*. Download mode
/// can use smaller pages and less concurrency to keep signed CDN URLs fresh,
/// but it must not change the covered rank range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhotoStreamProfile {
    FastEnumeration { concurrency: usize },
    BackpressuredDownload { download_concurrency: usize },
}

impl PhotoStreamProfile {
    fn request_page_size(self, default_page_size: usize) -> usize {
        match self {
            Self::FastEnumeration { .. } => default_page_size.max(1),
            Self::BackpressuredDownload {
                download_concurrency,
            } => download_stream_page_size(default_page_size, download_concurrency),
        }
    }

    fn fetcher_concurrency(self) -> usize {
        match self {
            Self::FastEnumeration { concurrency } => concurrency.max(1),
            Self::BackpressuredDownload { .. } => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetcherRangeRole {
    /// Count-partitioned work that covers a known rank interval.
    Data,
    /// The single owner that scans past the count hint until natural EOF.
    TailProof,
    /// A bounded stream that must inspect one more eligible asset to prove
    /// whether the caller's limit truncated the inventory.
    LimitProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnumerationFailure {
    FetcherError,
    ConsumerDropped,
    MalformedRecord,
    UnpairedRecords,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnumerationCompletion {
    ProvenEof,
    UserBoundReached,
    Incomplete(EnumerationFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FetcherRange {
    start: u64,
    end: u64,
    limit: Option<u32>,
    role: FetcherRangeRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FetcherBehavior {
    page_size: usize,
    preserve_blank_sync_tokens_for_diagnostics: bool,
    allow_unpaired_at_range_boundary: bool,
    treat_empty_tail_as_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EnumerationPlan {
    page_size: usize,
    ranges: Vec<FetcherRange>,
}

impl EnumerationPlan {
    fn channel_fetchers(&self) -> usize {
        self.ranges.len().max(1)
    }

    #[cfg(test)]
    fn covers_prefix(&self, end: u64) -> bool {
        if end == 0 {
            return true;
        }

        let mut ranges = self.ranges.clone();
        ranges.sort_unstable_by_key(|range| range.start);
        let mut cursor = 0;
        for range in ranges {
            if range.end <= cursor {
                continue;
            }
            if range.start > cursor {
                return false;
            }
            cursor = range.end;
            if cursor >= end {
                return true;
            }
        }
        false
    }
}

fn effective_total(limit: Option<u32>, total_count: Option<u64>) -> Option<u64> {
    total_count
        .map(|tc| limit.map_or(tc, |lim| tc.min(u64::from(lim))))
        .or_else(|| limit.map(u64::from))
}

fn range_limit(limit: Option<u32>, start: u64, end: u64) -> Option<u32> {
    limit.map(|lim| {
        let remaining = u64::from(lim).saturating_sub(start);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "bounded by min(end-start, limit) where both operands originated from u32 fetcher limits"
        )]
        {
            remaining.min(end.saturating_sub(start)) as u32
        }
    })
}

fn push_fetcher_range(ranges: &mut Vec<FetcherRange>, limit: Option<u32>, start: u64, end: u64) {
    if start >= end {
        return;
    }
    ranges.push(FetcherRange {
        start,
        end,
        limit: range_limit(limit, start, end),
        role: FetcherRangeRole::Data,
    });
}

fn build_enumeration_plan(
    limit: Option<u32>,
    total_count: Option<u64>,
    default_page_size: usize,
    profile: PhotoStreamProfile,
) -> EnumerationPlan {
    let page_size = profile.request_page_size(default_page_size);
    // A caller limit needs an ordered N+1 probe. Running count-partitioned
    // ranges in parallel cannot prove which asset is the first item beyond
    // the bound, and historically `limit == count` was incorrectly treated
    // as EOF. Keep bounded streams sequential and use the count only as a
    // scheduling hint for unbounded inventories.
    if let Some(limit) = limit {
        return EnumerationPlan {
            page_size,
            ranges: vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: Some(limit),
                role: FetcherRangeRole::LimitProbe,
            }],
        };
    }

    let Some(total) = total_count else {
        return EnumerationPlan {
            page_size,
            ranges: vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: None,
                role: FetcherRangeRole::TailProof,
            }],
        };
    };

    // Download streams deliberately keep one ordered fetcher so signed URLs
    // stay near their consumers. Let that fetcher own EOF proof directly
    // rather than stopping at the count hint and starting a concurrent tail.
    if matches!(profile, PhotoStreamProfile::BackpressuredDownload { .. })
        || profile.fetcher_concurrency() == 1
    {
        return EnumerationPlan {
            page_size,
            ranges: vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: None,
                role: FetcherRangeRole::TailProof,
            }],
        };
    }

    let concurrency = profile.fetcher_concurrency();
    let num_fetchers = if concurrency > 1 && total > 0 {
        determine_fetcher_count(total, page_size, concurrency * 2)
    } else {
        1
    };
    let chunk_size_items = {
        let raw = total.div_ceil(num_fetchers as u64);
        let ps = page_size as u64;
        raw.div_ceil(ps) * ps
    };

    let mut ranges = Vec::with_capacity(num_fetchers + 1);
    for i in 0..num_fetchers {
        let start = i as u64 * chunk_size_items;
        let end = ((i as u64 + 1) * chunk_size_items).min(total);
        if start >= total {
            break;
        }
        push_fetcher_range(&mut ranges, None, start, end);
    }

    // Counts are hints, not EOF proof. The final owner starts exactly where
    // the count-partitioned ranges stop and continues until the provider's
    // consecutive-empty-page policy proves natural EOF.
    ranges.push(FetcherRange {
        start: total,
        end: u64::MAX,
        limit: None,
        role: FetcherRangeRole::TailProof,
    });

    EnumerationPlan { page_size, ranges }
}

/// Metadata at DEBUG; raw body only at TRACE. Including the body in
/// the DEBUG event allocates ~MB per page on busy libraries (every
/// fetched page formats the full response value).
fn log_fetcher_response(album: &str, response: &Value) {
    tracing::debug!(album = %album, "Fetcher response");
    tracing::trace!(
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
    master: super::cloudkit::Record,
    asset_record: &super::cloudkit::Record,
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

/// Return a full-enumeration sync token only when every fetcher that reported
/// one agreed. A single overwritten token is unsafe: if two parallel fetchers
/// observed different zone tokens, advancing either token could skip records
/// that were not present in the other fetcher's snapshot.
fn unanimous_fetcher_sync_token(album: &str, tokens: &[String]) -> Option<String> {
    let first = tokens.first()?;
    if tokens.iter().all(|token| token == first) {
        return Some(first.clone());
    }

    let mut unique_tokens = FxHashSet::default();
    for token in tokens {
        unique_tokens.insert(token.as_str());
    }
    tracing::warn!(
        album,
        token_count = tokens.len(),
        unique_token_count = unique_tokens.len(),
        "Full enumeration syncToken mismatch across parallel fetchers; \
         blocking sync token advancement"
    );
    None
}

#[derive(Debug, Default)]
struct FetcherSyncTokenCapture {
    observations: tokio::sync::Mutex<Vec<(Option<String>, EnumerationCompletion)>>,
    expected_fetchers: AtomicUsize,
    completed_fetchers: AtomicUsize,
    suppressed: AtomicBool,
}

impl FetcherSyncTokenCapture {
    fn expect(&self, fetchers: usize) {
        self.expected_fetchers.store(fetchers, Ordering::Relaxed);
    }

    async fn complete(&self, token: Option<String>, completion: EnumerationCompletion) {
        self.observations.lock().await.push((token, completion));
        self.completed_fetchers.fetch_add(1, Ordering::Relaxed);
    }

    fn suppress(&self) {
        self.suppressed.store(true, Ordering::Relaxed);
    }

    async fn resolve(&self, album: &str) -> Option<String> {
        if self.suppressed.load(Ordering::Relaxed) {
            tracing::debug!(
                album,
                "Full enumeration stopped at the caller's limit; syncToken is not a complete-zone checkpoint"
            );
            return None;
        }

        let expected = self.expected_fetchers.load(Ordering::Relaxed);
        let completed = self.completed_fetchers.load(Ordering::Relaxed);
        if completed != expected {
            tracing::warn!(
                album,
                expected_fetchers = expected,
                completed_fetchers = completed,
                "Full enumeration did not receive completion evidence from every fetcher; blocking sync token advancement"
            );
            return None;
        }

        let observations = self.observations.lock().await;
        if let Some(failure) = observations
            .iter()
            .find_map(|(_, completion)| match completion {
                EnumerationCompletion::Incomplete(failure) => Some(*failure),
                _ => None,
            })
        {
            tracing::warn!(
                album,
                ?failure,
                "Full enumeration was incomplete in a fetcher"
            );
            return None;
        }
        if observations
            .iter()
            .any(|(_, completion)| matches!(completion, EnumerationCompletion::UserBoundReached))
        {
            tracing::debug!(album, "Full enumeration stopped at a user bound");
            return None;
        }
        let present = observations
            .iter()
            .filter_map(|(token, _)| token.as_ref())
            .cloned()
            .collect::<Vec<_>>();
        // Completion is required from every fetcher, but CloudKit may omit a
        // syncToken on an empty tail page. In that case the unanimous token
        // observed by the completed data fetchers is still the pass token;
        // an incomplete fetcher is rejected by the count above.
        unanimous_fetcher_sync_token(album, &present)
    }
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
    pub container_id: Option<Arc<str>>,
    pub cross_zone_sources: Vec<PhotoAlbum>,
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
    container_id: Option<Arc<str>>,
    cross_zone_sources: Vec<PhotoAlbum>,
}

impl Clone for PhotoAlbum {
    fn clone(&self) -> Self {
        Self::new(
            PhotoAlbumConfig {
                params: Arc::clone(&self.params),
                service_endpoint: Arc::clone(&self.service_endpoint),
                name: Arc::clone(&self.name),
                list_type: Arc::clone(&self.list_type),
                obj_type: Arc::clone(&self.obj_type),
                query_filter: self.query_filter.as_ref().map(Arc::clone),
                page_size: self.page_size,
                zone_id: Arc::clone(&self.zone_id),
                retry_config: self.retry_config,
                container_id: self.container_id.as_ref().map(Arc::clone),
                cross_zone_sources: self.cross_zone_sources.clone(),
            },
            self.session.clone_box(),
        )
    }
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
            container_id: config.container_id,
            cross_zone_sources: config.cross_zone_sources,
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

    pub(crate) fn container_id(&self) -> Option<&str> {
        self.container_id.as_deref()
    }

    pub(crate) fn with_cross_zone_sources(mut self, sources: Vec<PhotoAlbum>) -> Self {
        self.cross_zone_sources = sources;
        self
    }

    pub(crate) fn clone_for_cross_zone_source(&self) -> PhotoAlbum {
        self.clone_for_task_without_sources()
    }

    fn has_cross_zone_hydration(&self) -> bool {
        self.container_id.is_some() && !self.cross_zone_sources.is_empty()
    }

    pub(crate) fn clone_as_library_wide(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::clone(&self.params),
                service_endpoint: Arc::clone(&self.service_endpoint),
                name: Arc::from(""),
                list_type: Arc::from(QUERY_ALL_LIST),
                obj_type: Arc::from(QUERY_ALL_OBJ),
                query_filter: None,
                page_size: self.page_size,
                zone_id: Arc::clone(&self.zone_id),
                retry_config: self.retry_config,
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            self.session.clone_box(),
        )
    }

    /// Return total item count for this album via `HyperionIndexCountLookup`.
    pub async fn len(&self) -> anyhow::Result<u64> {
        let url = format!(
            "{}/internal/records/query/batch?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let body = json!({
            "batch": [Self::count_query(&self.obj_type, &self.zone_id)]
        });

        let response = super::session::retry_post(
            self.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &self.retry_config,
        )
        .await?;

        let batch: super::cloudkit::BatchQueryResponse = serde_json::from_value(response)
            .context("Could not read Apple's album count response")?;
        Self::count_from_query(batch.batch.first())
            .context("Could not find the album count in Apple's response")
    }

    /// Return item counts for a same-library pass set with one
    /// `/internal/records/query/batch` call. Falls back to per-album count
    /// calls if the albums do not share the same endpoint/params context.
    pub(crate) async fn len_many(albums: &[&Self]) -> Vec<anyhow::Result<u64>> {
        let Some(first) = albums.first() else {
            return Vec::new();
        };
        if albums.len() == 1 {
            return vec![first.len().await];
        }
        let can_batch = albums.iter().all(|album| {
            Arc::ptr_eq(&album.service_endpoint, &first.service_endpoint)
                && Arc::ptr_eq(&album.params, &first.params)
        });
        if !can_batch {
            let mut results = Vec::with_capacity(albums.len());
            for album in albums {
                results.push(album.len().await);
            }
            return results;
        }

        let url = format!(
            "{}/internal/records/query/batch?{}",
            first.service_endpoint,
            encode_params(&first.params)
        );
        let batch: Vec<Value> = albums
            .iter()
            .map(|album| Self::count_query(&album.obj_type, &album.zone_id))
            .collect();
        let body = json!({ "batch": batch });

        let response = match super::session::retry_post(
            first.session.as_ref(),
            &url,
            &body.to_string(),
            &[("Content-type", "text/plain")],
            &first.retry_config,
        )
        .await
        {
            Ok(response) => response,
            Err(e) => {
                tracing::debug!(error = %e, "Batched album count failed; falling back to per-pass counts");
                let mut results = Vec::with_capacity(albums.len());
                for album in albums {
                    results.push(album.len().await);
                }
                return results;
            }
        };

        let batch: super::cloudkit::BatchQueryResponse = match serde_json::from_value(response) {
            Ok(batch) => batch,
            Err(e) => {
                tracing::debug!(error = %e, "Failed to parse batched album count response; falling back to per-pass counts");
                let mut results = Vec::with_capacity(albums.len());
                for album in albums {
                    results.push(album.len().await);
                }
                return results;
            }
        };

        (0..albums.len())
            .map(|index| {
                let query = batch.batch.get(index).ok_or_else(|| {
                    anyhow::anyhow!("Apple did not return an album count for pass {index}.")
                })?;
                Self::count_from_query(Some(query)).with_context(|| {
                    format!("Could not read Apple's album count result for pass {index}")
                })
            })
            .collect()
    }

    fn count_query(obj_type: &str, zone_id: &Value) -> Value {
        json!({
            "resultsLimit": 1,
            "query": {
                "filterBy": {
                    "fieldName": "indexCountID",
                    "fieldValue": {
                        "type": "STRING_LIST",
                        "value": [obj_type]
                    },
                    "comparator": "IN",
                },
                "recordType": "HyperionIndexCountLookup",
            },
            "zoneWide": true,
            "zoneID": zone_id,
        })
    }

    fn count_from_query(query: Option<&super::cloudkit::QueryResponse>) -> anyhow::Result<u64> {
        let query = query.context("Apple did not return an album count query result")?;
        let record = query
            .records
            .first()
            .context("Apple's album count query returned no records")?;
        let item_count = record
            .fields
            .get("itemCount")
            .context("Apple's album count record did not include itemCount")?;
        let value = item_count
            .get("value")
            .context("Apple's album count itemCount did not include a value")?;
        value.as_u64().with_context(|| {
            format!("Apple's album count itemCount was not a non-negative integer: {value}")
        })
    }

    /// Convenience wrapper over `photo_stream()` that collects all assets
    /// into a `Vec`. Prefer `photo_stream()` when memory is a concern.
    ///
    /// Fetcher panics are surfaced as an `Err` so the caller cannot mistake
    /// a truncated enumeration for a complete one. Propagating the real
    /// panic payload back through `anyhow` isn't worth the ceremony — a
    /// sentinel string is enough for the operator to know the enumeration
    /// was incomplete and to correlate with the fetcher's prior
    /// `tracing::error!` log line.
    pub async fn photos(&self, limit: Option<u32>) -> anyhow::Result<Vec<PhotoAsset>> {
        use tokio_stream::StreamExt;
        let (stream, panic_rx) = self.photo_stream(limit, None, 1);
        let items = stream.collect::<Result<Vec<_>, _>>().await?;
        if panic_rx.await.unwrap_or(false) {
            anyhow::bail!(
                "Photo enumeration stopped because a fetcher task crashed. Results are incomplete; see the earlier error log."
            );
        }
        Ok(items)
    }

    /// Resolve durable pending identities without scanning the surrounding
    /// album or library. Missing response members are inconclusive; only an
    /// explicit CloudKit not-found result or tombstone is deletion evidence.
    pub(crate) async fn resolve_records(
        &self,
        requests: &[RecordLookupRequest],
    ) -> RecordResolutionBatch {
        let mut results = Vec::with_capacity(requests.len());
        let mut rate_limit_observations = 0usize;
        let url = format!(
            "{}/records/lookup?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );

        for batch in requests.chunks(RECORD_LOOKUP_BATCH_SIZE) {
            let mut record_names = FxHashSet::default();
            let mut records = Vec::with_capacity(batch.len().saturating_mul(2));
            for request in batch {
                for record_id in std::iter::once(&request.master_record_name)
                    .chain(request.asset_record_name.as_ref())
                {
                    if record_names.insert(record_id.as_str().to_string()) {
                        records.push(json!({
                            "recordName": record_id.as_str(),
                        }));
                    }
                }
            }
            let body = json!({
                "records": records,
                "zoneID": self.zone_id.as_ref(),
                "desiredKeys": &*DESIRED_KEYS_VALUES,
            });
            let response = match super::session::retry_post_allowing_record_errors(
                self.session.as_ref(),
                &url,
                &body.to_string(),
                &[("Content-type", "text/plain")],
                &self.retry_config,
            )
            .await
            {
                Ok(retried) => {
                    rate_limit_observations =
                        rate_limit_observations.saturating_add(retried.rate_limit_observations);
                    retried.response
                }
                Err(error) => {
                    let error = classify_provider_lookup_error(&error);
                    crate::metrics::record_targeted_lookup("transient_failure", batch.len());
                    results.extend(batch.iter().map(|request| {
                        (
                            request.state_id.clone(),
                            RecordResolution::TransientFailure(error.clone()),
                        )
                    }));
                    continue;
                }
            };

            let Some(response_records) = response.get("records").and_then(Value::as_array) else {
                crate::metrics::record_targeted_lookup("transient_failure", batch.len());
                results.extend(batch.iter().map(|request| {
                    (
                        request.state_id.clone(),
                        RecordResolution::TransientFailure(ProviderLookupError::Malformed(
                            "missing records array".to_string(),
                        )),
                    )
                }));
                continue;
            };
            let by_name: FxHashMap<&str, &Value> = response_records
                .iter()
                .filter_map(|record| {
                    record
                        .get("recordName")
                        .and_then(Value::as_str)
                        .map(|name| (name, record))
                })
                .collect();

            for request in batch {
                let master = by_name.get(request.master_record_name.as_str()).copied();
                let asset = request
                    .asset_record_name
                    .as_ref()
                    .and_then(|record_name| by_name.get(record_name.as_str()).copied());
                let explicit_not_found = |record: Option<&Value>| {
                    record
                        .and_then(|record| record.get("serverErrorCode"))
                        .and_then(Value::as_str)
                        .is_some_and(|code| matches!(code, "UNKNOWN_ITEM" | "NOT_FOUND"))
                };
                let tombstoned = |record: Option<&Value>| {
                    record
                        .and_then(|record| record.get("deleted"))
                        .and_then(Value::as_bool)
                        == Some(true)
                };
                let deleted_at = master.into_iter().chain(asset).find_map(|record| {
                    record
                        .get("fields")
                        .and_then(|fields| fields.get("deletedDate"))
                        .and_then(|field| field.get("value"))
                        .and_then(Value::as_i64)
                        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                });

                let primary_deleted = explicit_not_found(master) || tombstoned(master);
                let asset_deleted = explicit_not_found(asset) || tombstoned(asset);
                let resolution = if primary_deleted || asset_deleted {
                    RecordResolution::Deleted {
                        deleted_at,
                        master_family: primary_deleted
                            && request.target == RecordLookupTarget::Master,
                    }
                } else if let Some(master) = master {
                    match (
                        serde_json::from_value::<super::cloudkit::Record>(master.clone()),
                        asset.map(|asset| {
                            serde_json::from_value::<super::cloudkit::Record>(asset.clone())
                        }),
                    ) {
                        (Ok(master), Some(Ok(asset)))
                            if master.record_type == "CPLMaster"
                                && asset.record_type == "CPLAsset" =>
                        {
                            match PhotoAsset::try_from_records(
                                master,
                                &asset,
                                RequiredAssetFields::Downloadable,
                            ) {
                                Ok(mut photo) => {
                                    if request.state_id.as_str()
                                        != request.master_record_name.as_str()
                                    {
                                        photo = photo.with_state_record_name(Arc::from(
                                            request.state_id.as_str(),
                                        ));
                                    }
                                    RecordResolution::Present(photo)
                                }
                                Err(error) => RecordResolution::TransientFailure(
                                    ProviderLookupError::Malformed(error.to_string()),
                                ),
                            }
                        }
                        (Ok(master), None)
                            if request.asset_record_name.is_none()
                                && master.record_type == "CPLMaster" =>
                        {
                            RecordResolution::MasterPresent
                        }
                        (Ok(asset), None)
                            if request.target == RecordLookupTarget::Asset
                                && asset.record_type == "CPLAsset" =>
                        {
                            extract_master_ref(&asset.fields).map_or(
                                RecordResolution::Unknown,
                                |master_record_name| RecordResolution::AssetPresent {
                                    master_record_name: ProviderRecordId::new(master_record_name),
                                },
                            )
                        }
                        _ => RecordResolution::Unknown,
                    }
                } else {
                    RecordResolution::Unknown
                };
                let outcome = match &resolution {
                    RecordResolution::Present(_) => "present",
                    RecordResolution::AssetPresent { .. } => "asset_present",
                    RecordResolution::MasterPresent => "master_present_unpaired",
                    RecordResolution::Deleted { .. } => "deleted",
                    RecordResolution::Unknown => "unknown",
                    RecordResolution::TransientFailure(_) => "transient_failure",
                };
                crate::metrics::record_targeted_lookup(outcome, 1);
                results.push((request.state_id.clone(), resolution));
            }
        }

        let mut grouped: Vec<(ProviderRecordId, RecordResolution)> =
            Vec::with_capacity(results.len());
        let mut positions: FxHashMap<ProviderRecordId, usize> = FxHashMap::default();
        for (state_id, resolution) in results {
            if let Some(existing) = positions
                .get(&state_id)
                .and_then(|index| grouped.get_mut(*index))
                .map(|(_, resolution)| resolution)
            {
                merge_record_resolution(existing, resolution);
            } else {
                positions.insert(state_id.clone(), grouped.len());
                grouped.push((state_id, resolution));
            }
        }
        let complete = grouped.iter().all(|(_, resolution)| {
            matches!(
                resolution,
                RecordResolution::Present(_) | RecordResolution::Deleted { .. }
            )
        });

        RecordResolutionBatch {
            results: grouped,
            complete,
            rate_limit_observations,
        }
    }

    /// Stream photos page-by-page without buffering the full album in memory.
    ///
    /// Returns the stream paired with a `oneshot::Receiver<bool>` that
    /// yields `true` once every fetcher task has completed **iff any
    /// fetcher panicked**. The caller should await the receiver
    /// **after** the stream is exhausted and fail the enumeration if
    /// the flag is set — otherwise a panicked fetcher presents as a
    /// silently truncated stream (a "No silent failures" violation).
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
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<bool>) {
        let (panic_tx, panic_rx) = tokio::sync::oneshot::channel();
        let (stream, handles) = self.photo_stream_inner(
            limit,
            total_count,
            PhotoStreamProfile::FastEnumeration { concurrency },
            None,
            false,
            false,
        );
        tokio::spawn(async move {
            let panicked = await_fetcher_handles(handles).await;
            let _ = panic_tx.send(panicked);
        });
        (stream, panic_rx)
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
        self.photo_stream_with_token_inner(
            limit,
            total_count,
            PhotoStreamProfile::FastEnumeration { concurrency },
            false,
            true,
        )
    }

    pub(crate) fn photo_stream_with_token_policy(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        concurrency: usize,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        self.photo_stream_with_token_inner(
            limit,
            total_count,
            PhotoStreamProfile::FastEnumeration { concurrency },
            false,
            treat_empty_tail_as_error,
        )
    }

    pub(crate) fn photo_stream_with_token_for_download_policy(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        download_concurrency: usize,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        self.photo_stream_with_token_inner(
            limit,
            total_count,
            PhotoStreamProfile::BackpressuredDownload {
                download_concurrency,
            },
            true,
            treat_empty_tail_as_error,
        )
    }

    fn photo_stream_with_token_inner(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        profile: PhotoStreamProfile,
        preserve_blank_sync_tokens_for_diagnostics: bool,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        if self.has_cross_zone_hydration() {
            return self.photo_stream_with_cross_zone_hydration(
                limit,
                total_count,
                profile,
                preserve_blank_sync_tokens_for_diagnostics,
                treat_empty_tail_as_error,
            );
        }

        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let fetcher_sync_tokens = Arc::new(FetcherSyncTokenCapture::default());

        let (stream, handles) = self.photo_stream_inner(
            limit,
            total_count,
            profile,
            Some(fetcher_sync_tokens.clone()),
            preserve_blank_sync_tokens_for_diagnostics,
            treat_empty_tail_as_error,
        );
        let album_name = Arc::clone(&self.name);

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
                fetcher_sync_tokens.resolve(&album_name).await
            };
            let _ = token_tx.send(final_token);
        });

        (stream, token_rx)
    }

    fn photo_stream_with_cross_zone_hydration(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        profile: PhotoStreamProfile,
        preserve_blank_sync_tokens_for_diagnostics: bool,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        let (tx, rx) = mpsc::channel::<anyhow::Result<PhotoAsset>>(500);
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let (base_stream, base_token_rx) = self.photo_stream_with_token_inner_no_cross_zone(
            limit,
            total_count,
            profile,
            preserve_blank_sync_tokens_for_diagnostics,
            treat_empty_tail_as_error,
        );
        let Some(container_id) = self.container_id.clone() else {
            let _ = token_tx.send(None);
            return (
                Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
                token_rx,
            );
        };
        let album_name = Arc::clone(&self.name);
        let sources = self.cross_zone_sources_for_task();
        let owner = self.clone_for_task_without_sources();

        tokio::spawn(async move {
            use futures_util::StreamExt;

            let mut base_stream = Box::pin(base_stream);
            let mut seen_asset_records = FxHashSet::<String>::default();
            let mut base_seen = 0u64;
            let mut stream_error = false;

            while let Some(item) = base_stream.next().await {
                match item {
                    Ok(asset) => {
                        seen_asset_records.insert(asset.asset_record_name().to_string());
                        base_seen += 1;
                        if tx.send(Ok(asset)).await.is_err() {
                            let _ = token_tx.send(None);
                            return;
                        }
                    }
                    Err(e) => {
                        stream_error = true;
                        let _ = tx.send(Err(e)).await;
                    }
                }
            }

            let base_token = base_token_rx.await.ok().flatten();
            let should_hydrate =
                limit.is_none() && total_count.is_some_and(|expected| base_seen < expected);
            if stream_error {
                let _ = token_tx.send(None);
                return;
            }
            if !should_hydrate {
                let _ = token_tx.send(base_token);
                return;
            }

            let relation_ids = match owner.album_relation_item_ids(&container_id).await {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    let _ = token_tx.send(None);
                    return;
                }
            };
            let mut missing: FxHashSet<String> = relation_ids
                .into_iter()
                .filter(|id| !seen_asset_records.contains(id))
                .collect();
            if missing.is_empty() {
                let _ = token_tx.send(base_token);
                return;
            }

            tracing::info!(
                album = %album_name,
                base_seen,
                missing = missing.len(),
                "Album relation records exceed owner-zone assets; checking bounded cross-zone sources"
            );

            let mut hydrated = 0usize;
            for source in sources {
                if missing.is_empty() {
                    break;
                }
                let missing_before = missing.len();
                match source.matching_assets_from_changes(&mut missing).await {
                    Ok(assets) => {
                        hydrated += missing_before.saturating_sub(missing.len());
                        for asset in assets {
                            if tx.send(Ok(asset)).await.is_err() {
                                let _ = token_tx.send(None);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        let _ = token_tx.send(None);
                        return;
                    }
                }
            }

            if hydrated > 0 {
                tracing::info!(
                    album = %album_name,
                    base_seen,
                    hydrated,
                    unresolved = missing.len(),
                    "Album spans multiple CloudKit zones; hydrated bounded cross-zone members"
                );
            }

            if !missing.is_empty() {
                let sample: Vec<&str> = missing.iter().take(5).map(String::as_str).collect();
                tracing::warn!(
                    album = %album_name,
                    unresolved = missing.len(),
                    sample = ?sample,
                    "Album has unresolved relation records; continuing with visible downloadable assets"
                );
            }

            let _ = token_tx.send(if missing.is_empty() { base_token } else { None });
        });

        (
            Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            token_rx,
        )
    }

    fn photo_stream_with_token_inner_no_cross_zone(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        profile: PhotoStreamProfile,
        preserve_blank_sync_tokens_for_diagnostics: bool,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, tokio::sync::oneshot::Receiver<Option<String>>) {
        let (token_tx, token_rx) = tokio::sync::oneshot::channel();
        let fetcher_sync_tokens = Arc::new(FetcherSyncTokenCapture::default());

        let (stream, handles) = self.photo_stream_inner(
            limit,
            total_count,
            profile,
            Some(fetcher_sync_tokens.clone()),
            preserve_blank_sync_tokens_for_diagnostics,
            treat_empty_tail_as_error,
        );
        let album_name = Arc::clone(&self.name);

        tokio::spawn(async move {
            let fetcher_panicked = await_fetcher_handles(handles).await;
            let final_token = if fetcher_panicked {
                None
            } else {
                fetcher_sync_tokens.resolve(&album_name).await
            };
            let _ = token_tx.send(final_token);
        });

        (stream, token_rx)
    }

    fn clone_for_task_without_sources(&self) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::clone(&self.params),
                service_endpoint: Arc::clone(&self.service_endpoint),
                name: Arc::clone(&self.name),
                list_type: Arc::clone(&self.list_type),
                obj_type: Arc::clone(&self.obj_type),
                query_filter: self.query_filter.as_ref().map(Arc::clone),
                page_size: self.page_size,
                zone_id: Arc::clone(&self.zone_id),
                retry_config: self.retry_config,
                container_id: self.container_id.as_ref().map(Arc::clone),
                cross_zone_sources: Vec::new(),
            },
            self.session.clone_box(),
        )
    }

    fn cross_zone_sources_for_task(&self) -> Vec<PhotoAlbum> {
        self.cross_zone_sources
            .iter()
            .map(Self::clone_for_task_without_sources)
            .collect()
    }

    async fn album_relation_item_ids(
        &self,
        container_id: &str,
    ) -> anyhow::Result<FxHashSet<String>> {
        let mut ids = FxHashSet::default();
        self.scan_changes_zone(|record| {
            if record.record_type != "CPLContainerRelation" {
                return true;
            }
            if record.deleted == Some(true) {
                return true;
            }
            let container = record
                .fields
                .get("containerId")
                .and_then(|f| f.get("value"))
                .and_then(Value::as_str);
            if container != Some(container_id) {
                return true;
            }
            if let Some(item_id) = record
                .fields
                .get("itemId")
                .and_then(|f| f.get("value"))
                .and_then(Value::as_str)
            {
                ids.insert(item_id.to_string());
            }
            true
        })
        .await?;
        Ok(ids)
    }

    pub(crate) async fn hydrate_matching_assets_from_changes(
        &self,
        missing_asset_record_names: &mut FxHashSet<String>,
    ) -> anyhow::Result<Vec<PhotoAsset>> {
        let mut matched = self
            .clone_for_task_without_sources()
            .matching_assets_from_changes(missing_asset_record_names)
            .await?;
        for source in self.cross_zone_sources_for_task() {
            if missing_asset_record_names.is_empty() {
                break;
            }
            matched.extend(
                source
                    .matching_assets_from_changes(missing_asset_record_names)
                    .await?,
            );
        }
        Ok(matched)
    }

    /// Recover current `CPLAsset` records for legacy state keyed only by a
    /// `CPLMaster` record name.
    ///
    /// Scans to EOF so the caller sees every current sibling before choosing
    /// one from durable version, size, and checksum evidence.
    pub(crate) async fn hydrate_matching_master_assets_from_changes(
        &self,
        master_record_names: &FxHashSet<String>,
        shutdown_token: &CancellationToken,
    ) -> anyhow::Result<Vec<PhotoAsset>> {
        if master_record_names.is_empty() || shutdown_token.is_cancelled() {
            return Ok(Vec::new());
        }

        fn collect_event(
            event: ChangeEvent,
            source_zone: &Arc<str>,
            master_record_names: &FxHashSet<String>,
            seen_asset_record_names: &mut FxHashSet<String>,
            matched: &mut Vec<PhotoAsset>,
        ) {
            if event.reason != crate::types::ChangeReason::Created {
                return;
            }
            let Some(asset) = event.asset else {
                return;
            };
            let asset = asset.with_source_zone(Arc::clone(source_zone));
            if master_record_names.contains(asset.id())
                && seen_asset_record_names.insert(asset.asset_record_name().to_string())
            {
                matched.push(asset);
            }
        }

        let mut buffer = DeltaRecordBuffer::new();
        let source_zone: Arc<str> = Arc::from(self.zone_name());
        let mut seen_asset_record_names = FxHashSet::default();
        let mut matched = Vec::new();
        self.scan_changes_zone(|record| {
            for event in buffer.process_records(vec![record]) {
                collect_event(
                    event,
                    &source_zone,
                    master_record_names,
                    &mut seen_asset_record_names,
                    &mut matched,
                );
            }
            !shutdown_token.is_cancelled()
        })
        .await?;

        for event in buffer.flush() {
            collect_event(
                event,
                &source_zone,
                master_record_names,
                &mut seen_asset_record_names,
                &mut matched,
            );
        }
        Ok(matched)
    }

    async fn matching_assets_from_changes(
        &self,
        missing_asset_record_names: &mut FxHashSet<String>,
    ) -> anyhow::Result<Vec<PhotoAsset>> {
        let source_zone: Arc<str> = Arc::from(self.zone_name());
        let mut buffer = DeltaRecordBuffer::new();
        let mut matched = Vec::new();
        self.scan_changes_zone(|record| {
            let events = buffer.process_records(vec![record]);
            for event in events {
                let Some(asset) = event.asset else {
                    continue;
                };
                if missing_asset_record_names.remove(asset.asset_record_name()) {
                    let asset = asset.with_source_zone(Arc::clone(&source_zone));
                    matched.push(asset);
                }
            }
            !missing_asset_record_names.is_empty()
        })
        .await?;

        for event in buffer.flush() {
            let Some(asset) = event.asset else {
                continue;
            };
            if missing_asset_record_names.remove(asset.asset_record_name()) {
                let asset = asset.with_source_zone(Arc::clone(&source_zone));
                matched.push(asset);
            }
        }
        Ok(matched)
    }

    async fn scan_changes_zone<F>(&self, mut on_record: F) -> anyhow::Result<()>
    where
        F: FnMut(super::cloudkit::Record) -> bool,
    {
        let url = format!(
            "{}/changes/zone?{}",
            self.service_endpoint,
            encode_params(&self.params)
        );
        let mut current_token: Option<String> = None;

        loop {
            let body = build_changes_zone_request(&self.zone_id, current_token.as_deref(), 200);
            let response = super::session::retry_post(
                self.session.as_ref(),
                &url,
                &body.to_string(),
                &[("Content-type", "text/plain")],
                &self.retry_config,
            )
            .await?;

            let changes_resp: ChangesZoneResponse = serde_json::from_value(response)?;
            let Some(zone_result) = changes_resp.zones.into_iter().next() else {
                anyhow::bail!("Apple changes/zone returned no zones.");
            };
            let zone_name = zone_result.zone_id.zone_name.clone();
            check_changes_zone_error(
                zone_result.server_error_code.as_deref(),
                zone_result.reason.as_deref(),
                &zone_name,
            )?;

            current_token = Some(zone_result.sync_token);
            let more_coming = zone_result.more_coming;
            for record in zone_result.records {
                if !on_record(record) {
                    return Ok(());
                }
            }
            if !more_coming {
                return Ok(());
            }
        }
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
                    break Some(anyhow::anyhow!("Apple changes/zone returned no zones."));
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
    /// When `fetcher_sync_tokens` is `Some`, each fetcher appends its last
    /// observed `syncToken` to the shared token list.
    ///
    /// Returns the stream and all spawned fetcher `JoinHandle`s.
    fn photo_stream_inner(
        &self,
        limit: Option<u32>,
        total_count: Option<u64>,
        profile: PhotoStreamProfile,
        fetcher_sync_tokens: Option<Arc<FetcherSyncTokenCapture>>,
        preserve_blank_sync_tokens_for_diagnostics: bool,
        treat_empty_tail_as_error: bool,
    ) -> (PhotoStream, Vec<JoinHandle<()>>) {
        let plan = build_enumeration_plan(limit, total_count, self.page_size, profile);
        if let Some(capture) = &fetcher_sync_tokens {
            capture.expect(plan.ranges.len());
            if matches!((limit, total_count), (Some(limit), Some(total)) if total > u64::from(limit))
            {
                capture.suppress();
            }
        }
        let (tx, rx) = mpsc::channel::<anyhow::Result<PhotoAsset>>(
            (plan.page_size * plan.channel_fetchers()).min(500),
        );
        let range_record_owners =
            Arc::new(std::sync::Mutex::new(FxHashMap::<String, u64>::default()));
        let mut handles = Vec::with_capacity(plan.ranges.len());

        if effective_total(limit, total_count).is_none() {
            tracing::info!("Fetching photos from iCloud...");
        }

        let allow_unpaired_at_range_boundary = plan.ranges.len() > 1;
        let behavior = FetcherBehavior {
            page_size: plan.page_size,
            preserve_blank_sync_tokens_for_diagnostics,
            allow_unpaired_at_range_boundary,
            treat_empty_tail_as_error,
        };
        for range in plan.ranges {
            handles.push(self.spawn_fetcher(
                tx.clone(),
                range,
                Arc::clone(&range_record_owners),
                fetcher_sync_tokens.clone(),
                behavior,
            ));
        }
        // Drop our sender so channel closes when all fetchers finish.
        drop(tx);

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
    /// If `fetcher_sync_tokens` is provided, the fetcher appends the last
    /// non-None `syncToken` it observed. The monitor task compares every
    /// fetcher's final token before allowing token advancement.
    fn spawn_fetcher(
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
            let mut pending_masters: FxHashMap<String, super::cloudkit::Record> =
                FxHashMap::default();
            let mut pending_assets: FxHashMap<String, Vec<super::cloudkit::Record>> =
                FxHashMap::default();
            let mut paired_masters: FxHashMap<String, super::cloudkit::Record> =
                FxHashMap::default();
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

                let query: super::cloudkit::QueryResponse = match serde_json::from_value(response) {
                    Ok(q) => q,
                    Err(e) => {
                        tracing::warn!(
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
                        tracing::warn!(
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
                        // Promoted to info! so an enumeration that
                        // terminates after probing past empty pages is
                        // visible in normal logs — operators chasing a
                        // suspected silent truncation should see the
                        // probe count and total_sent here.
                        tracing::info!(
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
                    tracing::debug!(
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
                let mut page_assets: FxHashMap<String, Vec<super::cloudkit::Record>> =
                    FxHashMap::default();
                let mut page_masters: Vec<super::cloudkit::Record> = Vec::new();
                let mut masters_seen_on_page = false;
                let mut limit_reached = false;
                let mut page_emitted = 0u64;

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
                    tracing::warn!(
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

                tracing::debug!(
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
                        tracing::info!(
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
                tracing::warn!(
                    masters = pending_masters.len(),
                    assets = pending_assets.values().map(Vec::len).sum::<usize>(),
                    "Unpaired CPLMaster/CPLAsset records after full enumeration"
                );
                for id in pending_masters.keys() {
                    tracing::debug!(master_id = %id, "Unpaired CPLMaster");
                }
                for (id, records) in &pending_assets {
                    tracing::debug!(master_id = %id, count = records.len(), "Unpaired CPLAsset");
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

/// Panic-on-call `PhotosSession` for tests that inspect a `PhotoAlbum` by
/// name/metadata only. Any actual network call is a test bug.
#[cfg(test)]
pub(crate) struct StubSession;

#[cfg(test)]
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

#[cfg(test)]
impl PhotoAlbum {
    /// Construct a `PhotoAlbum` with the given name for cross-module unit
    /// tests. Wires [`StubSession`], so the album is only safe to inspect by
    /// name/metadata - any network call panics.
    pub(crate) fn stub_for_test(name: Arc<str>) -> Self {
        Self::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name,
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(serde_json::json!({"zoneName": "PrimarySync"})),
                retry_config: RetryConfig::default(),
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(StubSession),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{
        DynamicRecentPhotosSession, MockPhotosFlow, MockPhotosSession, mock_photo_query_page,
    };
    use serde_json::json;
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
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            Box::new(StubSession),
        )
    }

    fn default_zone() -> Value {
        json!({"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner", "zoneType": "REGULAR_CUSTOM_ZONE"})
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
    fn download_stream_page_size_stays_near_worker_pool() {
        assert_eq!(download_stream_page_size(100, 1), 2);
        assert_eq!(download_stream_page_size(100, 4), 8);
        assert_eq!(download_stream_page_size(100, 50), 100);
        assert_eq!(download_stream_page_size(0, 4), 1);
    }

    #[test]
    fn download_profile_plan_covers_entire_recent_window_with_reduced_pages() {
        let plan = build_enumeration_plan(
            Some(1000),
            Some(5000),
            100,
            PhotoStreamProfile::BackpressuredDownload {
                download_concurrency: 10,
            },
        );

        assert_eq!(plan.page_size, 20);
        assert_eq!(
            plan.ranges,
            vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: Some(1000),
                role: FetcherRangeRole::LimitProbe,
            }]
        );
        assert!(
            plan.covers_prefix(1000),
            "download profile must keep complete recent-window coverage"
        );
    }

    #[test]
    fn fast_profile_uses_ordered_limit_probe_for_recent_window() {
        let plan = build_enumeration_plan(
            Some(1000),
            Some(5000),
            100,
            PhotoStreamProfile::FastEnumeration { concurrency: 10 },
        );

        assert_eq!(plan.page_size, 100);
        assert_eq!(plan.ranges.len(), 1);
        assert_eq!(plan.ranges[0].role, FetcherRangeRole::LimitProbe);
        assert!(
            plan.covers_prefix(1000),
            "parallel fast profile must cover the same recent window"
        );
    }

    #[test]
    fn limit_probe_does_not_trust_equal_count_as_eof() {
        let plan = build_enumeration_plan(
            Some(100),
            Some(100),
            100,
            PhotoStreamProfile::FastEnumeration { concurrency: 1 },
        );

        assert_eq!(
            plan.ranges,
            vec![FetcherRange {
                start: 0,
                end: u64::MAX,
                limit: Some(100),
                role: FetcherRangeRole::LimitProbe,
            }]
        );
    }

    #[test]
    fn parallel_recent_plan_still_uses_one_ordered_limit_probe() {
        let plan = build_enumeration_plan(
            Some(100),
            Some(100),
            100,
            PhotoStreamProfile::FastEnumeration { concurrency: 10 },
        );

        assert_eq!(plan.ranges.len(), 1);
        assert_eq!(plan.ranges[0].start, 0);
        assert_eq!(plan.ranges[0].end, u64::MAX);
        assert_eq!(plan.ranges[0].role, FetcherRangeRole::LimitProbe);
        assert!(plan.covers_prefix(100));
    }

    #[test]
    fn unbounded_parallel_plan_partitions_data_and_appends_tail_owner() {
        let plan = build_enumeration_plan(
            None,
            Some(5000),
            100,
            PhotoStreamProfile::FastEnumeration { concurrency: 10 },
        );

        assert!(
            plan.ranges
                .iter()
                .any(|range| range.start == 0 && range.role == FetcherRangeRole::Data),
            "the count prefix must keep count-partitioned data work"
        );
        assert_eq!(
            plan.ranges.last(),
            Some(&FetcherRange {
                start: 5000,
                end: u64::MAX,
                limit: None,
                role: FetcherRangeRole::TailProof,
            })
        );
        assert!(plan.covers_prefix(5000));
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
                container_id: None,
                cross_zone_sources: Vec::new(),
            },
            session,
        )
    }

    fn lookup_request(state_id: &str, master: &str, asset: &str) -> RecordLookupRequest {
        RecordLookupRequest::paired(
            ProviderRecordId::new(state_id),
            ProviderRecordId::new(master),
            ProviderRecordId::new(asset),
        )
    }

    /// Live Phase 0 capability probe for the private CloudKit database.
    ///
    /// Run single-threaded with the maintainer test account:
    /// `cargo test live_targeted_record_lookup_distinguishes_present_and_missing -- --ignored --test-threads=1`
    #[tokio::test]
    #[ignore = "requires live iCloud credentials and a trusted session"]
    async fn live_targeted_record_lookup_distinguishes_present_and_missing() {
        let _ = dotenvy::from_filename(".env");
        let username = std::env::var("ICLOUD_USERNAME").expect("ICLOUD_USERNAME must be set");
        let password = std::env::var("ICLOUD_PASSWORD").expect("ICLOUD_PASSWORD must be set");
        let cookie_dir = std::env::var_os("ICLOUD_TEST_COOKIE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(".test-cookies"));
        let password_provider: crate::password::PasswordProvider =
            Arc::new(move || Some(crate::password::SecretString::from(password.clone())));
        let auth = crate::auth::authenticate(
            &cookie_dir,
            &username,
            &password_provider,
            "com",
            None,
            None,
            None,
        )
        .await
        .expect("authenticate live lookup probe");
        let (_session, mut service) = crate::commands::init_photos_service(
            auth,
            RetryConfig::default(),
            crate::personality::Mode::Off,
            None,
        )
        .await
        .expect("initialize live Photos service");
        let album = service
            .get_library(crate::icloud::photos::PRIMARY_ZONE_NAME)
            .await
            .expect("resolve primary library")
            .all();
        let asset = album
            .photos(Some(1))
            .await
            .expect("enumerate one live asset")
            .into_iter()
            .next()
            .expect("live account must contain at least one photo");
        let missing_id = "kei-phase0-record-that-does-not-exist";
        let master_only_state_id = "live-master-only";
        let requests = [
            lookup_request(asset.id(), asset.id(), asset.asset_record_name()),
            RecordLookupRequest::asset_only(ProviderRecordId::new(asset.asset_record_name())),
            RecordLookupRequest::master_only(
                ProviderRecordId::new(master_only_state_id),
                ProviderRecordId::new(asset.id()),
            ),
            RecordLookupRequest::master_only(
                ProviderRecordId::new(missing_id),
                ProviderRecordId::new(missing_id),
            ),
        ];

        let result = album.resolve_records(&requests).await;

        assert!(
            !result.complete,
            "a live master-only lookup still needs its CPLAsset pair"
        );
        assert!(matches!(
            result
                .results
                .iter()
                .find(|(id, _)| id.as_str() == asset.id()),
            Some((_, RecordResolution::Present(_)))
        ));
        assert!(matches!(
            result.results.iter().find(|(id, _)|
                id.as_str() == asset.asset_record_name()
            ),
            Some((
                _,
                RecordResolution::AssetPresent { master_record_name }
            )) if master_record_name.as_str() == asset.id()
        ));
        assert!(matches!(
            result
                .results
                .iter()
                .find(|(id, _)| id.as_str() == master_only_state_id),
            Some((_, RecordResolution::MasterPresent))
        ));
        assert!(matches!(
            result
                .results
                .iter()
                .find(|(id, _)| id.as_str() == missing_id),
            Some((
                _,
                RecordResolution::Deleted {
                    deleted_at: None,
                    master_family: true,
                }
            ))
        ));
    }

    #[tokio::test]
    async fn targeted_record_lookup_scopes_zone_at_request_level() {
        #[derive(Clone)]
        struct CaptureLookupSession {
            body: Arc<Mutex<Option<Value>>>,
        }

        #[async_trait::async_trait]
        impl PhotosSession for CaptureLookupSession {
            async fn post(
                &self,
                url: &str,
                body: String,
                _headers: &[(&str, &str)],
            ) -> anyhow::Result<Value> {
                assert!(url.contains("/records/lookup?"));
                *self.body.lock().unwrap() = Some(serde_json::from_str(&body)?);
                Ok(json!({"records": []}))
            }

            fn clone_box(&self) -> Box<dyn PhotosSession> {
                Box::new(self.clone())
            }
        }

        let captured = Arc::new(Mutex::new(None));
        let album = make_album_with_session(
            100,
            Box::new(CaptureLookupSession {
                body: Arc::clone(&captured),
            }),
        );
        album
            .resolve_records(&[lookup_request("master", "master", "asset")])
            .await;

        let body = captured.lock().unwrap().clone().expect("lookup body");
        assert_eq!(body["zoneID"], default_zone());
        let records = body["records"].as_array().expect("records array");
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|record| record.get("zoneID").is_none()));
    }

    #[tokio::test]
    async fn targeted_master_only_lookup_resolves_explicit_legacy_deletion() {
        let response = json!({
            "records": [{
                "recordName": "legacy-master",
                "serverErrorCode": "UNKNOWN_ITEM",
                "reason": "record not found"
            }]
        });
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

        let batch = album
            .resolve_records(&[RecordLookupRequest::master_only(
                ProviderRecordId::new("legacy-master"),
                ProviderRecordId::new("legacy-master"),
            )])
            .await;

        assert!(batch.complete);
        assert!(matches!(
            batch.results.as_slice(),
            [(
                _,
                RecordResolution::Deleted {
                    master_family: true,
                    ..
                }
            )]
        ));
    }

    #[tokio::test]
    async fn targeted_master_only_lookup_distinguishes_live_legacy_master() {
        let response = json!({
            "records": [test_master_record("legacy-master")]
        });
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

        let batch = album
            .resolve_records(&[RecordLookupRequest::master_only(
                ProviderRecordId::new("legacy-state"),
                ProviderRecordId::new("legacy-master"),
            )])
            .await;

        assert!(!batch.complete);
        assert!(matches!(
            batch.results.as_slice(),
            [(_, RecordResolution::MasterPresent)]
        ));
    }

    #[tokio::test]
    async fn asset_only_delta_targeted_lookup_recovers_master_identity() {
        let response = json!({
            "records": [test_asset_record_for("asset-present", "master-present")]
        });
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

        let batch = album
            .resolve_records(&[RecordLookupRequest::asset_only(ProviderRecordId::new(
                "asset-present",
            ))])
            .await;

        assert!(
            !batch.complete,
            "identity lookup still needs the master pair"
        );
        assert!(matches!(
            batch.results.as_slice(),
            [(
                _,
                RecordResolution::AssetPresent { master_record_name }
            )] if master_record_name.as_str() == "master-present"
        ));
    }

    #[tokio::test]
    async fn targeted_record_lookup_distinguishes_present_deleted_and_omitted() {
        let response = json!({
            "records": [
                test_master_record("master-present"),
                test_asset_record_for("asset-present", "master-present"),
                test_master_record("master-deleted"),
                {
                    "recordName": "asset-deleted",
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                }
            ]
        });
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));
        let batch = album
            .resolve_records(&[
                lookup_request("master-present", "master-present", "asset-present"),
                lookup_request("master-deleted", "master-deleted", "asset-deleted"),
                lookup_request("master-unknown", "master-unknown", "asset-unknown"),
            ])
            .await;

        assert!(!batch.complete, "an omitted batch member is inconclusive");
        assert!(
            matches!(batch.results[0].1, RecordResolution::Present(_)),
            "unexpected lookup results: {:?}",
            batch.results
        );
        assert!(matches!(
            batch.results[1].1,
            RecordResolution::Deleted { .. }
        ));
        assert!(matches!(batch.results[2].1, RecordResolution::Unknown));
    }

    #[tokio::test]
    async fn targeted_record_lookup_present_sibling_keeps_shared_master_state() {
        let response = json!({
            "records": [
                test_master_record("master-shared"),
                {
                    "recordName": "asset-a-deleted",
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                },
                test_asset_record_for("asset-b-present", "master-shared")
            ]
        });
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

        let batch = album
            .resolve_records(&[
                lookup_request("master-shared", "master-shared", "asset-a-deleted"),
                lookup_request("master-shared", "master-shared", "asset-b-present"),
            ])
            .await;

        assert!(batch.complete);
        assert_eq!(batch.results.len(), 1);
        assert!(matches!(batch.results[0].1, RecordResolution::Present(_)));
    }

    #[tokio::test]
    async fn targeted_record_lookup_omitted_sibling_keeps_shared_master_state_unknown() {
        let response = json!({
            "records": [
                test_master_record("master-shared"),
                {
                    "recordName": "asset-a-deleted",
                    "serverErrorCode": "UNKNOWN_ITEM",
                    "reason": "record not found"
                }
            ]
        });
        let album = make_album_with_session(100, Box::new(MockPhotosSession::new().ok(response)));

        let batch = album
            .resolve_records(&[
                lookup_request("master-shared", "master-shared", "asset-a-deleted"),
                lookup_request("master-shared", "master-shared", "asset-b-omitted"),
            ])
            .await;

        assert!(!batch.complete);
        assert_eq!(batch.results.len(), 1);
        assert!(matches!(batch.results[0].1, RecordResolution::Unknown));
    }

    #[tokio::test]
    async fn targeted_record_lookup_retains_transient_failure() {
        let mut session = MockPhotosSession::new();
        for _ in 0..8 {
            session = session.err("temporary lookup failure");
        }
        let album = make_album_with_session(100, Box::new(session));

        let batch = album
            .resolve_records(&[lookup_request("master", "master", "asset")])
            .await;

        assert!(!batch.complete);
        assert!(matches!(
            batch.results[0].1,
            RecordResolution::TransientFailure(ProviderLookupError::Request(_))
        ));
    }

    #[test]
    fn targeted_record_lookup_preserves_typed_http_failures() {
        let error: anyhow::Error = super::super::session::HttpStatusError {
            status: 429,
            url: "https://example.com/records/lookup".to_string(),
            retry_after: None,
            body: None,
        }
        .into();
        assert!(matches!(
            classify_provider_lookup_error(&error),
            ProviderLookupError::RateLimited { status: 429, .. }
        ));

        let error: anyhow::Error = super::super::session::HttpStatusError {
            status: 421,
            url: "https://example.com/records/lookup".to_string(),
            retry_after: None,
            body: None,
        }
        .into();
        assert!(matches!(
            classify_provider_lookup_error(&error),
            ProviderLookupError::Authentication { status: 421, .. }
        ));
    }

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

    async fn drain_photo_stream_count(stream: PhotoStream) -> u32 {
        use tokio_stream::StreamExt;

        tokio::pin!(stream);
        let mut count = 0u32;
        while let Some(result) = stream.next().await {
            result.expect("photo asset should be Ok");
            count += 1;
        }
        count
    }

    async fn drain_photo_stream_ids(stream: PhotoStream) -> Vec<String> {
        use tokio_stream::StreamExt;

        tokio::pin!(stream);
        let mut ids = Vec::new();
        while let Some(result) = stream.next().await {
            ids.push(result.expect("photo asset should be Ok").id().to_string());
        }
        ids
    }

    async fn drain_photo_stream(stream: PhotoStream) -> (Vec<String>, Vec<String>) {
        use tokio_stream::StreamExt;

        tokio::pin!(stream);
        let mut ids = Vec::new();
        let mut errors = Vec::new();
        while let Some(result) = stream.next().await {
            match result {
                Ok(asset) => ids.push(asset.id().to_string()),
                Err(e) => errors.push(e.to_string()),
            }
        }
        (ids, errors)
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
    async fn offline_replay_incremental_changes_fixture() {
        use tokio_stream::StreamExt;

        let mock = MockPhotosFlow::new()
            .changes_photo_page("master-replay-change", "token-incremental", false)
            .build();
        let album = make_album_with_session(100, Box::new(mock));

        let (stream, token_rx) = album.changes_stream("token-before");
        tokio::pin!(stream);
        let mut events = Vec::new();
        while let Some(result) = stream.next().await {
            events.push(result.expect("change event"));
        }

        assert_eq!(events.len(), 1);
        assert_eq!(&*events[0].record_name, "master-replay-change");
        assert!(events[0].asset.is_some());
        assert_eq!(
            token_rx.await.expect("sync token sender"),
            "token-incremental"
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

    fn test_records(record_name: &str) -> Vec<Value> {
        vec![
            test_master_record(record_name),
            test_asset_record(record_name),
        ]
    }

    fn test_master_record(record_name: &str) -> Value {
        json!({
            "recordName": record_name,
            "recordType": "CPLMaster",
            "fields": {
                "filenameEnc": {"value": "photo.jpg", "type": "STRING"},
                "resOriginalRes": {
                    "value": {
                        "downloadURL": "https://p01.icloud-content.com/photo.jpg",
                        "size": 1024,
                        "fileChecksum": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
                    }
                },
                "resOriginalFileType": {"value": "public.jpeg"},
                "itemType": {"value": "public.jpeg"}
            }
        })
    }

    fn test_asset_record(record_name: &str) -> Value {
        test_asset_record_for(&format!("asset-{record_name}"), record_name)
    }

    fn test_asset_record_for(asset_record_name: &str, master_record_name: &str) -> Value {
        json!({
            "recordName": asset_record_name,
            "recordType": "CPLAsset",
            "fields": {
                "masterRef": {
                    "value": {
                        "recordName": master_record_name,
                        "zoneID": {"zoneName": "PrimarySync"}
                    },
                    "type": "REFERENCE"
                },
                "assetDate": {"value": 1700000000000i64, "type": "TIMESTAMP"}
            }
        })
    }

    fn container_relation_record(container_id: &str, item_id: &str) -> Value {
        json!({
            "recordName": format!("relation-{item_id}"),
            "recordType": "CPLContainerRelation",
            "fields": {
                "containerId": {"value": container_id, "type": "STRING"},
                "itemId": {"value": item_id, "type": "STRING"}
            },
            "recordChangeTag": "ct-relation"
        })
    }

    fn changes_page_for_zone(records: Vec<Value>, zone_name: &str, sync_token: &str) -> Value {
        json!({
            "zones": [{
                "zoneID": {"zoneName": zone_name, "ownerRecordName": "_defaultOwner"},
                "syncToken": sync_token,
                "moreComing": false,
                "records": records
            }]
        })
    }

    #[derive(Clone, Copy, Debug)]
    enum CrossZoneSessionKind {
        Owner,
        Source,
        EmptySource,
    }

    #[derive(Clone, Debug)]
    struct CrossZoneSession {
        kind: CrossZoneSessionKind,
        owner_query_calls: Arc<std::sync::atomic::AtomicUsize>,
        owner_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
        source_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CrossZoneSession {
        fn new(
            kind: CrossZoneSessionKind,
            owner_query_calls: Arc<std::sync::atomic::AtomicUsize>,
            owner_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
            source_changes_calls: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                kind,
                owner_query_calls,
                owner_changes_calls,
                source_changes_calls,
            }
        }
    }

    #[async_trait::async_trait]
    impl PhotosSession for CrossZoneSession {
        async fn post(
            &self,
            url: &str,
            _body: String,
            _headers: &[(&str, &str)],
        ) -> anyhow::Result<Value> {
            if url.contains("/records/query") {
                assert!(
                    matches!(self.kind, CrossZoneSessionKind::Owner),
                    "only the owner album should use records/query"
                );
                let call = self
                    .owner_query_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    return Ok(mock_photo_query_page("master-owner", Some("owner-token")));
                }
                return Ok(json!({"records": [], "syncToken": "owner-token"}));
            }

            if url.contains("/changes/zone") {
                return match self.kind {
                    CrossZoneSessionKind::Owner => {
                        self.owner_changes_calls
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(changes_page_for_zone(
                            vec![
                                container_relation_record("album-container", "asset-master-owner"),
                                container_relation_record("album-container", "asset-master-shared"),
                            ],
                            "PrimarySync",
                            "owner-changes-token",
                        ))
                    }
                    CrossZoneSessionKind::Source => {
                        self.source_changes_calls
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(changes_page_for_zone(
                            test_records("master-shared"),
                            "SharedSync-abc",
                            "source-changes-token",
                        ))
                    }
                    CrossZoneSessionKind::EmptySource => {
                        self.source_changes_calls
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(changes_page_for_zone(
                            Vec::new(),
                            "SharedSync-abc",
                            "source-changes-token",
                        ))
                    }
                };
            }

            anyhow::bail!("unexpected URL: {url}")
        }

        fn clone_box(&self) -> Box<dyn PhotosSession> {
            Box::new(self.clone())
        }
    }

    fn make_cross_zone_album(
        zone_name: &str,
        session: CrossZoneSession,
        container_id: Option<Arc<str>>,
        cross_zone_sources: Vec<PhotoAlbum>,
    ) -> PhotoAlbum {
        PhotoAlbum::new(
            PhotoAlbumConfig {
                params: Arc::new(HashMap::new()),
                service_endpoint: Arc::from("https://example.com"),
                name: Arc::from("TestAlbum"),
                list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
                obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
                query_filter: None,
                page_size: 100,
                zone_id: Arc::new(json!({"zoneName": zone_name})),
                retry_config: RetryConfig::default(),
                container_id,
                cross_zone_sources,
            },
            Box::new(session),
        )
    }

    type CrossZoneTestSetup = (
        PhotoAlbum,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    );

    fn make_cross_zone_owner_album() -> CrossZoneTestSetup {
        make_cross_zone_owner_album_with_source_kind(CrossZoneSessionKind::Source)
    }

    fn make_cross_zone_owner_album_with_source_kind(
        source_kind: CrossZoneSessionKind,
    ) -> CrossZoneTestSetup {
        let owner_query_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner_changes_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source_changes_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let owner_session = CrossZoneSession::new(
            CrossZoneSessionKind::Owner,
            Arc::clone(&owner_query_calls),
            Arc::clone(&owner_changes_calls),
            Arc::clone(&source_changes_calls),
        );
        let source_session = CrossZoneSession::new(
            source_kind,
            Arc::clone(&owner_query_calls),
            Arc::clone(&owner_changes_calls),
            Arc::clone(&source_changes_calls),
        );
        let source = make_cross_zone_album("SharedSync-abc", source_session, None, Vec::new());
        let owner = make_cross_zone_album(
            "PrimarySync",
            owner_session,
            Some(Arc::from("album-container")),
            vec![source],
        );
        (
            owner,
            owner_query_calls,
            owner_changes_calls,
            source_changes_calls,
        )
    }

    #[tokio::test]
    async fn photo_stream_hydrates_named_album_members_from_other_zones() {
        use tokio_stream::StreamExt;

        let (owner, owner_query_calls, owner_changes_calls, source_changes_calls) =
            make_cross_zone_owner_album();

        let (stream, token_rx) = owner.photo_stream_with_token(None, Some(2), 1);
        tokio::pin!(stream);

        let mut assets = Vec::new();
        while let Some(result) = stream.next().await {
            assets.push(result.expect("photo asset should be Ok"));
        }

        assert_eq!(assets.len(), 2);
        assert!(assets.iter().any(|asset| asset.id() == "master-owner"
            && asset.asset_record_name() == "asset-master-owner"
            && asset.source_zone().is_none()));
        assert!(assets.iter().any(|asset| asset.id() == "master-shared"
            && asset.asset_record_name() == "asset-master-shared"
            && asset.source_zone() == Some("SharedSync-abc")));
        assert_eq!(
            token_rx.await.expect("sync token sender").as_deref(),
            Some("owner-token"),
            "fully resolved cross-zone hydration can keep the owner-zone token"
        );
        assert_eq!(
            owner_query_calls.load(std::sync::atomic::Ordering::SeqCst),
            6,
            "base enumeration should stop after the owner page plus finite empty-tail proof"
        );
        assert_eq!(
            owner_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "owner zone relation scan should run only when base enumeration is short"
        );
        assert_eq!(
            source_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "source zone scan should be bounded to missing relation members"
        );
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    async fn photo_stream_warns_but_continues_for_unresolved_relation_members() {
        use tokio_stream::StreamExt;

        let (owner, _owner_query_calls, owner_changes_calls, source_changes_calls) =
            make_cross_zone_owner_album_with_source_kind(CrossZoneSessionKind::EmptySource);

        let (stream, token_rx) = owner.photo_stream_with_token(None, Some(2), 1);
        tokio::pin!(stream);

        let mut assets = Vec::new();
        while let Some(result) = stream.next().await {
            assets.push(result.expect("unresolved relation records should warn, not error"));
        }

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].id(), "master-owner");
        assert_eq!(
            token_rx.await.expect("sync token sender"),
            None,
            "unresolved relation records suppress owner-zone token advancement"
        );
        assert_eq!(
            owner_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "owner relation scan should still run"
        );
        assert_eq!(
            source_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "source zone scan should try to resolve relation members"
        );
        assert!(logs_contain("unresolved=1"));
        assert!(logs_contain("Album has unresolved relation records"));
    }

    #[tokio::test]
    async fn photo_stream_recent_limit_does_not_cross_zone_over_hydrate() {
        use tokio_stream::StreamExt;

        let (owner, owner_query_calls, owner_changes_calls, source_changes_calls) =
            make_cross_zone_owner_album();

        let (stream, token_rx) = owner.photo_stream_with_token(Some(1), Some(2), 1);
        tokio::pin!(stream);

        let mut assets = Vec::new();
        while let Some(result) = stream.next().await {
            assets.push(result.expect("photo asset should be Ok"));
        }

        assert_eq!(assets.len(), 1);
        assert_eq!(assets[0].id(), "master-owner");
        assert_eq!(
            token_rx.await.expect("sync token sender"),
            None,
            "recent-limited streams stop before the full owner-zone checkpoint"
        );
        assert_eq!(
            owner_query_calls.load(std::sync::atomic::Ordering::SeqCst),
            6,
            "recent limit should stop after the requested item and finite owner-zone probe"
        );
        assert_eq!(
            owner_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "recent-limited streams should not widen into full relation scans"
        );
        assert_eq!(
            source_changes_calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "recent-limited streams should not scan source zones"
        );
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

    // --- limit / --recent edge case tests ---

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

    // --- pagination edge case tests ---

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

    // --- changes_stream tests ---

    /// Build a canned `ChangesZoneResponse` JSON with the given records,
    /// syncToken, and moreComing flag.
    fn canned_changes_page(records: &[Value], sync_token: &str, more_coming: bool) -> Value {
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
    async fn hydrate_matching_assets_from_changes_stops_after_all_targets_match() {
        let records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let mock = MockPhotosSession::new().ok(canned_changes_page(&records, "token-page1", true));
        let album = make_album_with_session(100, Box::new(mock));
        let mut missing = FxHashSet::default();
        missing.insert("asset-1".to_string());

        let matched = album
            .hydrate_matching_assets_from_changes(&mut missing)
            .await
            .expect("matching hydrate should stop without fetching the next page");

        assert!(missing.is_empty());
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].asset_record_name(), "asset-1");
    }

    #[tokio::test]
    async fn hydrate_matching_master_assets_collects_every_live_sibling() {
        let records = vec![
            changes_master("master-target"),
            changes_asset("asset-target-a", "master-target"),
            changes_asset("asset-target-b", "master-target"),
            changes_master("master-other"),
            changes_asset("asset-other", "master-other"),
        ];
        let mock = MockPhotosSession::new().ok(canned_changes_page(&records, "token-final", false));
        let album = make_album_with_session(100, Box::new(mock));
        let masters = FxHashSet::from_iter(["master-target".to_string()]);

        let matched = album
            .hydrate_matching_master_assets_from_changes(&masters, &CancellationToken::new())
            .await
            .expect("master hydration");

        assert_eq!(matched.len(), 2);
        assert!(matched.iter().all(|asset| asset.id() == "master-target"));
        assert!(
            matched
                .iter()
                .all(|asset| asset.source_zone() == Some("PrimarySync"))
        );
        let asset_record_names: FxHashSet<&str> =
            matched.iter().map(PhotoAsset::asset_record_name).collect();
        assert_eq!(
            asset_record_names,
            FxHashSet::from_iter(["asset-target-a", "asset-target-b"])
        );
    }

    #[tokio::test]
    async fn hydrate_matching_master_assets_scans_later_sibling_pages() {
        let page1 = vec![
            changes_master("master-target"),
            changes_asset("asset-target-a", "master-target"),
        ];
        let page2 = vec![changes_asset("asset-target-b", "master-target")];
        let mock = MockPhotosSession::new()
            .ok(canned_changes_page(&page1, "token-page1", true))
            .ok(canned_changes_page(&page2, "token-final", false));
        let album = make_album_with_session(100, Box::new(mock));
        let masters = FxHashSet::from_iter(["master-target".to_string()]);

        let matched = album
            .hydrate_matching_master_assets_from_changes(&masters, &CancellationToken::new())
            .await
            .expect("master hydration should scan every page");

        let asset_record_names: FxHashSet<&str> =
            matched.iter().map(PhotoAsset::asset_record_name).collect();
        assert_eq!(
            asset_record_names,
            FxHashSet::from_iter(["asset-target-a", "asset-target-b"])
        );
    }

    #[tokio::test]
    async fn test_changes_stream_single_page() {
        use tokio_stream::StreamExt;

        let records = vec![
            changes_master("master-1"),
            changes_asset("asset-1", "master-1"),
        ];
        let mock = MockPhotosFlow::new()
            .changes_zone_page(records, "token-final", false)
            .build();
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
        let mock = MockPhotosFlow::new()
            .changes_zone_page(page1_records, "token-page1", true)
            .changes_zone_page(page2_records, "token-page2", false)
            .build();
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
        let mock = MockPhotosFlow::new()
            .changes_zone_page(Vec::new(), "token-empty", true)
            .changes_zone_page(page2_records, "token-final", false)
            .build();
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

        let mock = MockPhotosFlow::new()
            .changes_zone_error("BAD_REQUEST", "Unknown sync continuation type", "")
            .build();
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
            err_msg.contains("sync token is no longer valid"),
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

        let mock = MockPhotosFlow::new()
            .changes_zone_error("RETRY_LATER", "temporary backend issue", "")
            .build();
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
            .ok(canned_changes_page(&page1_records, "token-page1", true))
            .ok(canned_changes_page(&page2_records, "token-page2", true))
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
            MockPhotosSession::new().ok(canned_changes_page(&records, "token-after-delete", false));
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
}
