//! iCloud album configuration and stable entry points.
//!
//! Lookup owns targeted identity evidence; counts owns count queries. Planning
//! owns rank ranges, fetch owns page parsing and emission, and completion owns
//! token evidence. Enumeration starts fetchers. Hydration composes enumeration
//! and change scans; changes owns sequential provider deltas.
//!
//! Tests retain their names under `<owner>::tests`; shared fixtures are in
//! `test_support`. All external paths and tracing targets remain unchanged.

mod changes;
mod completion;
mod counts;
mod enumeration;
mod fetch;
mod hydration;
mod lookup;
mod planning;

#[cfg(test)]
mod test_support;

use super::asset::{ChangeEvent, PhotoAsset};
use super::session::PhotosSession;
use crate::retry::RetryConfig;
use completion::{FetcherSyncTokenCapture, await_fetcher_handles};
use planning::PhotoStreamProfile;
use rustc_hash::FxHashSet;
use serde_json::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio_stream::Stream;
use tokio_util::sync::CancellationToken;

pub(crate) use lookup::{
    ProviderLookupError, ProviderRecordId, RecordLookupRequest, RecordResolution,
    RecordResolutionBatch,
};
#[cfg(test)]
pub(crate) use test_support::StubSession;

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

/// A boxed, pinned stream of change event results.
type ChangeStream = Pin<Box<dyn Stream<Item = anyhow::Result<ChangeEvent>> + Send + 'static>>;

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
        self.fetch_len().await
    }

    /// Return item counts for a same-library pass set with one
    /// `/internal/records/query/batch` call. Falls back to per-album count
    /// calls if the albums do not share the same endpoint/params context.
    pub(crate) async fn len_many(albums: &[&Self]) -> Vec<anyhow::Result<u64>> {
        Self::fetch_len_many(albums).await
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
        self.lookup_records(requests).await
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

    pub(crate) async fn hydrate_matching_assets_from_changes(
        &self,
        missing_asset_record_names: &mut FxHashSet<String>,
    ) -> anyhow::Result<Vec<PhotoAsset>> {
        self.hydrate_assets(missing_asset_record_names).await
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
        self.hydrate_masters(master_record_names, shutdown_token)
            .await
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
        self.stream_changes(sync_token)
    }
}

impl std::fmt::Display for PhotoAlbum {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
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
