use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chrono::DateTime;
use reqwest::Client;
use rustc_hash::FxHashSet;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::commands::{AlbumPass, PassKind};
use crate::download::{file, filter, paths};
use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, PhotoAsset, PhotosSession};
use crate::retry::RetryConfig;
use crate::state::SqliteStateDb;
use crate::test_helpers::{
    DynamicRecentPhotosSession, MockPhotosFlow, TestAssetRecord, TracingCapture,
    mock_photo_records_for_zone_with_filename,
    mock_photo_records_for_zone_with_filename_and_asset_date,
};
use crate::types::FileMatchPolicy;

use super::super::config::DownloadConfig;
use super::super::models::{
    DATE_BOUNDED_FULL_ENUMERATION_REASON, DownloadControls, DownloadOutcome, DownloadReporting,
    DownloadRunMode, DownloadStore, ICLOUD_ALBUM_COUNT_ERROR_REASON, PassKey,
    RECENT_LIMITED_FULL_ENUMERATION_REASON, SyncResult, sync_token_blocked_explanation,
};
use super::super::test_support::{
    album_with_session, incremental_photo_records_with_url, mock_album, mock_album_with_container,
    mock_asset_record_for, mock_master_record_with_filename, mock_photo_records_with_filename,
    test_config,
};
use super::{
    DEFERRED_UNFILED_HEARTBEAT_ASSETS, PaginationShortfall, PassTokenObservation, PassTokenResult,
    RecentFrontier, TokenGap, ZoneTokenEvidence, build_pass_count_plan,
    classify_pagination_shortfall, classify_zone_token_evidence, download_photos_full_with_token,
    fold_pass_count_results, should_skip_pass_count_fetch, stream_created_lower_bound,
};

#[derive(Clone)]
struct ConcurrentRecordsSession {
    in_flight_records_queries: Arc<AtomicUsize>,
    max_in_flight_records_queries: Arc<AtomicUsize>,
    records_delay: Duration,
}

impl ConcurrentRecordsSession {
    fn new(records_delay: Duration) -> Self {
        Self {
            in_flight_records_queries: Arc::new(AtomicUsize::new(0)),
            max_in_flight_records_queries: Arc::new(AtomicUsize::new(0)),
            records_delay,
        }
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight_records_queries.load(Ordering::SeqCst)
    }

    fn note_records_query_start(&self) {
        let current = self
            .in_flight_records_queries
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        let mut observed = self.max_in_flight_records_queries.load(Ordering::SeqCst);
        while current > observed {
            match self.max_in_flight_records_queries.compare_exchange(
                observed,
                current,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break,
                Err(next) => observed = next,
            }
        }
    }
}

#[async_trait::async_trait]
impl PhotosSession for ConcurrentRecordsSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 0}}}]}]
            }));
        }

        if url.contains("/records/query?") {
            self.note_records_query_start();
            tokio::time::sleep(self.records_delay).await;
            self.in_flight_records_queries
                .fetch_sub(1, Ordering::SeqCst);
            return Ok(json!({
                "records": [],
                "syncToken": "zone-token"
            }));
        }

        Ok(json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn probe_album(name: &str, session: ConcurrentRecordsSession) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            retry_config: RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeferredOrderKind {
    Album,
    Unfiled,
}

#[derive(Clone)]
struct DeferredOrderSession {
    kind: DeferredOrderKind,
    album_done: Arc<AtomicBool>,
    unfiled_started_too_early: Arc<AtomicBool>,
    record_name: Arc<str>,
}

impl DeferredOrderSession {
    fn new_pair() -> (Self, Self) {
        let album_done = Arc::new(AtomicBool::new(false));
        let unfiled_started_too_early = Arc::new(AtomicBool::new(false));
        (
            Self {
                kind: DeferredOrderKind::Album,
                album_done: Arc::clone(&album_done),
                unfiled_started_too_early: Arc::clone(&unfiled_started_too_early),
                record_name: Arc::from("ORDER_ALBUM"),
            },
            Self {
                kind: DeferredOrderKind::Unfiled,
                album_done,
                unfiled_started_too_early,
                record_name: Arc::from("ORDER_UNFILED"),
            },
        )
    }

    fn unfiled_started_too_early(&self) -> bool {
        self.unfiled_started_too_early.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl PhotosSession for DeferredOrderSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
            }));
        }

        if url.contains("/records/query?") {
            match self.kind {
                DeferredOrderKind::Album => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    self.album_done.store(true, Ordering::SeqCst);
                }
                DeferredOrderKind::Unfiled => {
                    if !self.album_done.load(Ordering::SeqCst) {
                        self.unfiled_started_too_early.store(true, Ordering::SeqCst);
                    }
                }
            }
            return Ok(json!({
                "records": mock_photo_records_for_zone_with_filename(
                    &self.record_name,
                    "PrimarySync",
                    &format!("{}.jpg", self.record_name),
                ),
                "syncToken": "zone-token",
            }));
        }

        Ok(json!({"records": []}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone, Debug)]
struct RecoveringPassTokenSession {
    query_calls: Arc<AtomicUsize>,
    records: Arc<Vec<Value>>,
}

#[derive(Clone, Debug)]
struct RecoveringIncompletePassSession {
    query_calls: Arc<AtomicUsize>,
    records: Arc<Vec<Value>>,
}

#[async_trait::async_trait]
impl PhotosSession for RecoveringIncompletePassSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
            }));
        }
        if !url.contains("/records/query?") {
            return Ok(json!({"records": []}));
        }

        let call = self.query_calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Ok(json!({"records": "malformed"}));
        }
        if call == 1 {
            return Ok(json!({
                "records": self.records.as_ref().clone(),
                "syncToken": "zone-token-recovered"
            }));
        }
        Ok(json!({"records": [], "syncToken": "zone-token-recovered"}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[async_trait::async_trait]
impl PhotosSession for RecoveringPassTokenSession {
    async fn post(
        &self,
        url: &str,
        _body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": 1}}}]}]
            }));
        }
        if !url.contains("/records/query?") {
            return Ok(json!({"records": []}));
        }

        let call = self.query_calls.fetch_add(1, Ordering::SeqCst);
        let recovery_round = call >= 6;
        let first_page = call.is_multiple_of(6);
        Ok(match (first_page, recovery_round) {
            (true, false) => json!({"records": self.records.as_ref().clone()}),
            (false, false) => json!({"records": []}),
            (true, true) => json!({
                "records": self.records.as_ref().clone(),
                "syncToken": "zone-token-recovered"
            }),
            (false, true) => {
                json!({"records": [], "syncToken": "zone-token-recovered"})
            }
        })
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[derive(Clone)]
struct RecentScopeAsset {
    id: String,
    asset_date: i64,
}

#[derive(Clone)]
struct RecentScopeSession {
    all_assets: Arc<Vec<RecentScopeAsset>>,
    album_assets: Arc<Vec<RecentScopeAsset>>,
    all_offsets: Arc<std::sync::Mutex<Vec<u64>>>,
    album_offsets: Arc<std::sync::Mutex<Vec<u64>>>,
}

impl RecentScopeSession {
    fn new(all_assets: Vec<RecentScopeAsset>, album_assets: Vec<RecentScopeAsset>) -> Self {
        Self {
            all_assets: Arc::new(all_assets),
            album_assets: Arc::new(album_assets),
            all_offsets: Arc::new(std::sync::Mutex::new(Vec::new())),
            album_offsets: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn album_offsets(&self) -> Vec<u64> {
        self.album_offsets
            .lock()
            .expect("album offsets lock")
            .clone()
    }

    fn page_records(assets: &[RecentScopeAsset], offset: u64, results_limit: u64) -> Vec<Value> {
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        let page_assets = usize::try_from(results_limit / 2).unwrap_or(usize::MAX);
        let end = start.saturating_add(page_assets).min(assets.len());
        let mut records = Vec::with_capacity(end.saturating_sub(start) * 2);
        for asset in assets.get(start..end).unwrap_or_default() {
            records.extend(mock_photo_records_for_zone_with_filename_and_asset_date(
                &asset.id,
                "PrimarySync",
                &format!("{}.jpg", asset.id),
                asset.asset_date,
            ));
        }
        records
    }
}

#[async_trait::async_trait]
impl PhotosSession for RecentScopeSession {
    async fn post(
        &self,
        url: &str,
        body: String,
        _headers: &[(&str, &str)],
    ) -> anyhow::Result<Value> {
        if url.contains("/internal/records/query/batch") {
            return Ok(json!({
                "batch": [{"records": [{"fields": {"itemCount": {"value": self.all_assets.len() as u64}}}]}]
            }));
        }
        if !url.contains("/records/query?") {
            return Ok(json!({"records": []}));
        }

        let request: Value = serde_json::from_str(&body)?;
        let record_type = request["query"]["recordType"].as_str().unwrap_or_default();
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
        let results_limit = request["resultsLimit"].as_u64().unwrap_or(0);

        let assets = if record_type == "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted" {
            self.all_offsets
                .lock()
                .expect("all offsets lock")
                .push(offset);
            self.all_assets.as_ref()
        } else {
            self.album_offsets
                .lock()
                .expect("album offsets lock")
                .push(offset);
            self.album_assets.as_ref()
        };
        let records = Self::page_records(assets, offset, results_limit);
        Ok(json!({"records": records, "syncToken": "zone-token"}))
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn recent_scope_album(name: &str, session: RecentScopeSession) -> PhotoAlbum {
    PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::new(HashMap::new()),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from(name),
            list_type: Arc::from("CPLContainerRelationLiveByAssetDate"),
            obj_type: Arc::from("CPLContainerRelationNotDeletedByAssetDate:test"),
            query_filter: None,
            page_size: 2,
            zone_id: Arc::new(json!({"zoneName": "PrimarySync"})),
            retry_config: RetryConfig::default(),
            container_id: Some(Arc::from("test")),
            cross_zone_sources: Vec::new(),
        },
        Box::new(session),
    )
}

async fn seed_existing_file_for_asset(
    base_config: &mut DownloadConfig,
    pass: &AlbumPass,
    asset: &PhotoAsset,
) {
    base_config.file_match_policy = FileMatchPolicy::NameId7;
    let pass_config = base_config.with_pass(pass);
    let expected_path = filter::expected_paths_for(asset, &pass_config)
        .into_iter()
        .next()
        .expect("mock asset should have an expected path");
    tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
        .await
        .expect("create parent dir");
    tokio::fs::write(&expected_path.path, vec![0u8; 1024])
        .await
        .expect("seed existing file");
    seed_downloaded_state_for_expected_path(base_config, &pass_config, asset, &expected_path).await;
}

async fn seed_downloaded_state_for_expected_path(
    base_config: &mut DownloadConfig,
    pass_config: &DownloadConfig,
    asset: &PhotoAsset,
    expected_path: &filter::ExpectedAssetPath,
) {
    let db = match &base_config.state_db {
        Some(db) => Arc::clone(db),
        None => {
            let db: Arc<dyn DownloadStore> =
                Arc::new(SqliteStateDb::open_in_memory().expect("open state db"));
            base_config.state_db = Some(Arc::clone(&db));
            db
        }
    };
    let library = asset.source_zone().unwrap_or(&pass_config.library);
    let filename = expected_path
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("expected path has filename");
    let record = TestAssetRecord::new(asset.state_id())
        .library(library)
        .checksum(&expected_path.checksum)
        .filename(filename)
        .created_at(asset.created())
        .size(expected_path.size)
        .version_size(expected_path.version_size)
        .build();
    db.upsert_seen(&record).await.expect("seed state row");
    db.mark_downloaded(
        library,
        asset.state_id(),
        expected_path.version_size.as_str(),
        &expected_path.path,
        "seeded-local-sha256",
        None,
    )
    .await
    .expect("mark seeded file downloaded");
    assert!(
        !db.should_download(
            library,
            asset.state_id(),
            expected_path.version_size.as_str(),
            &expected_path.checksum,
            &expected_path.path,
        )
        .await
        .expect("seeded state should be readable"),
        "seeded downloaded state must make the expected file a safe skip"
    );
}

async fn seed_existing_recent_files(
    base_config: &mut DownloadConfig,
    pass: &AlbumPass,
    zone: &str,
    ids: &[String],
    filename_prefix: &str,
) {
    for (index, id) in ids.iter().enumerate() {
        let records = mock_photo_records_for_zone_with_filename(
            id,
            zone,
            &format!("{filename_prefix}-{index:04}.jpg"),
        );
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_existing_file_for_asset(base_config, pass, &asset).await;
    }
}

fn recent_ids(prefix: &str, count: u64) -> Vec<String> {
    (0..count).map(|i| format!("{prefix}-{i:04}")).collect()
}

fn recent_scope_assets(prefix: &str, count: u64, base_date: i64) -> Vec<RecentScopeAsset> {
    (0..count)
        .map(|i| RecentScopeAsset {
            id: format!("{prefix}-{i:04}"),
            asset_date: base_date - i64::try_from(i).expect("test index fits i64") * 1_000,
        })
        .collect()
}

fn recent_scope_photo_asset(asset: &RecentScopeAsset) -> PhotoAsset {
    let records = mock_photo_records_for_zone_with_filename_and_asset_date(
        &asset.id,
        "PrimarySync",
        &format!("{}.jpg", asset.id),
        asset.asset_date,
    );
    PhotoAsset::new(records[0].clone(), records[1].clone())
}

fn unique_ids_in_order(ids: Vec<String>) -> Vec<String> {
    let mut seen = FxHashSet::default();
    ids.into_iter()
        .filter(|id| seen.insert(id.clone()))
        .collect()
}

#[tokio::test]
async fn full_sync_per_pass_streams_overlap_when_paths_are_pass_specific() {
    let session = ConcurrentRecordsSession::new(Duration::from_millis(100));
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: probe_album("album_a", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: probe_album("album_b", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 2;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::dry_run_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("dry-run full sync should succeed");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "empty dry-run enumeration should succeed"
    );
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(
        session.max_in_flight() >= 2,
        "album records/query streams should overlap; max in-flight was {}",
        session.max_in_flight()
    );
}

#[tokio::test]
async fn full_sync_threads_one_keeps_pass_streams_serial() {
    let session = ConcurrentRecordsSession::new(Duration::from_millis(25));
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: probe_album("album_a", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: probe_album("album_b", session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::dry_run_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("dry-run full sync should succeed");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "result: {result:?}"
    );
    assert_eq!(
        session.max_in_flight(),
        1,
        "threads=1 should prevent multi-pass enumeration from consuming multiple cores at once"
    );
}

const WORKER_BUDGET_ASSETS_PER_PASS: usize = 4;

const WORKER_BUDGET_TIMEOUT: Duration = Duration::from_secs(15);

const WORKER_BUDGET_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
    0x00, 0x01, 0x00, 0x00, 0xff, 0xd9,
];

#[derive(Default)]
struct WorkerBudgetProbe {
    active: AtomicUsize,
    peak: AtomicUsize,
    requests: AtomicUsize,
    release: CancellationToken,
}

struct WorkerBudgetServer(tokio::task::JoinHandle<()>);

impl Drop for WorkerBudgetServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn worker_budget_media(
    axum::extract::State(probe): axum::extract::State<Arc<WorkerBudgetProbe>>,
) -> Vec<u8> {
    let active = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
    probe.peak.fetch_max(active, Ordering::SeqCst);
    probe.requests.fetch_add(1, Ordering::SeqCst);
    // Hold the first wave until the test has observed concurrent admission.
    probe.release.cancelled().await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    probe.active.fetch_sub(1, Ordering::SeqCst);
    WORKER_BUDGET_JPEG.to_vec()
}

fn worker_budget_passes(pass_count: usize, url: &str) -> Vec<AlbumPass> {
    use base64::Engine as _;

    (0..pass_count)
        .map(|pass| {
            let records = (0..WORKER_BUDGET_ASSETS_PER_PASS)
                .flat_map(|asset| {
                    let id = format!("budget-{pass}-{asset}");
                    let mut records = incremental_photo_records_with_url(
                        &id,
                        &format!("{id}.jpg"),
                        url,
                        WORKER_BUDGET_JPEG.len() as u64,
                    );
                    // Provider checksums are opaque resource identities,
                    // not content hashes. Give each resource its own temp path.
                    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
                        json!(base64::engine::general_purpose::STANDARD.encode(&id));
                    records
                })
                .collect();
            AlbumPass {
                kind: PassKind::Album,
                album: mock_album(
                    &format!("album-{pass}"),
                    MockPhotosFlow::new()
                        .album_count(WORKER_BUDGET_ASSETS_PER_PASS as u64)
                        .query_page(records, Some("budget-token"))
                        .empty_query_page(Some("budget-token"))
                        .build(),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            }
        })
        .collect()
}

enum WorkerBudgetRun {
    Complete,
    CancelFirstWave,
}

async fn check_full_sync_worker_budget(workers: usize, pass_count: usize, run: WorkerBudgetRun) {
    let probe = Arc::new(WorkerBudgetProbe::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind controlled media endpoint");
    let url = format!("http://{}/media.jpg", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route("/media.jpg", axum::routing::get(worker_budget_media))
        .with_state(Arc::clone(&probe));
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let server = WorkerBudgetServer(server);

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let mut config = test_config();
    config.directory = Arc::from(dir.path().join("media"));
    config.folder_structure_albums = Arc::from("{album}");
    config.concurrent_downloads = workers;
    #[cfg(feature = "xmp")]
    {
        config.metadata.xmp_sidecar = true;
    }
    config.state_db = Some(db.clone());
    let mut config = Arc::new(config);
    let shutdown = CancellationToken::new();
    let client = Client::new();
    let passes = worker_budget_passes(pass_count, &url);
    let download = Box::pin(download_photos_full_with_token(
        &client,
        &passes,
        &config,
        DownloadControls::download_hidden(),
        shutdown.clone(),
    ));
    let control = async {
        let parallel_passes = workers.min(pass_count);
        let first_wave =
            parallel_passes * (workers / parallel_passes).min(WORKER_BUDGET_ASSETS_PER_PASS);
        while probe.active.load(Ordering::SeqCst) < first_wave {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Keep responses blocked so an over-admitting scheduler can expose
        // all its workers instead of hiding them behind fast responses.
        tokio::time::sleep(Duration::from_millis(100)).await;
        if matches!(run, WorkerBudgetRun::CancelFirstWave) {
            shutdown.cancel();
        }
        probe.release.cancel();
    };
    let (result, ()) = tokio::time::timeout(WORKER_BUDGET_TIMEOUT, async {
        tokio::join!(download, control)
    })
    .await
    .expect("full enumeration must not deadlock");
    let result = result.unwrap();
    let peak = probe.peak.load(Ordering::SeqCst);
    assert!(
        peak > 0 && peak <= workers,
        "workers={workers}, passes={pass_count}, peak={peak}"
    );
    let total = pass_count * WORKER_BUDGET_ASSETS_PER_PASS;
    if matches!(run, WorkerBudgetRun::CancelFirstWave) {
        assert_eq!(result.sync_token, None);
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, 0);
        assert!(
            summary.pending + summary.failed > 0,
            "retain dispatched work"
        );
        assert_eq!(summary.total_assets, summary.pending + summary.failed);
    } else {
        assert!(
            matches!(result.outcome, DownloadOutcome::Success),
            "workers={workers}, passes={pass_count}: {result:?}; failed={:?}",
            db.get_failed().await.unwrap()
        );
        assert_eq!(result.stats.downloaded, total);
        assert_eq!(result.sync_token.as_deref(), Some("budget-token"));
        assert_eq!(probe.requests.load(Ordering::SeqCst), total);
    }

    // Reopen file-backed state, recover interrupted work, then prove that
    // an unchanged full cycle does not request or rewrite completed media.
    Arc::get_mut(&mut config).unwrap().state_db = None;
    drop(db);
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    Arc::get_mut(&mut config).unwrap().state_db = Some(db.clone());
    let mut completed_files = Vec::new();
    for cycle in 0..2 {
        let before = probe.requests.load(Ordering::SeqCst);
        let result = tokio::time::timeout(
            WORKER_BUDGET_TIMEOUT,
            Box::pin(download_photos_full_with_token(
                &client,
                &worker_budget_passes(pass_count, &url),
                &config,
                DownloadControls::download_hidden(),
                CancellationToken::new(),
            )),
        )
        .await
        .expect("restarted full enumeration must make progress")
        .unwrap();
        assert!(matches!(result.outcome, DownloadOutcome::Success));
        assert_eq!(result.sync_token.as_deref(), Some("budget-token"));
        if cycle > 0 || matches!(run, WorkerBudgetRun::Complete) {
            assert_eq!(result.stats.downloaded, 0);
            assert_eq!(probe.requests.load(Ordering::SeqCst), before);
        }
        let summary = db.get_summary().await.unwrap();
        assert_eq!(summary.downloaded, total as u64);
        assert_eq!(summary.total_assets, total as u64);
        assert_eq!(summary.pending + summary.failed, 0);
        let albums = std::fs::read_dir(&config.directory).unwrap();
        let mut directory_count = 0;
        for album in albums {
            let entries = std::fs::read_dir(album.unwrap().path()).unwrap();
            assert_eq!(
                entries.count(),
                WORKER_BUDGET_ASSETS_PER_PASS * (1 + usize::from(cfg!(feature = "xmp"))),
                "only completed media and configured sidecars may remain"
            );
            directory_count += 1;
        }
        assert_eq!(directory_count, pass_count);
        let rows = db.get_downloaded_page(0, total as u32).await.unwrap();
        let mut files = Vec::new();
        for row in rows {
            let path = row.local_path.unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), WORKER_BUDGET_JPEG);
            assert_eq!(
                row.local_checksum,
                Some(file::compute_sha256(&path).await.unwrap())
            );
            assert_eq!(row.download_checksum, row.local_checksum);
            let mut paths = vec![path.clone()];
            if cfg!(feature = "xmp") {
                let mut name = path.file_name().unwrap().to_os_string();
                name.push(".xmp");
                let sidecar = path.with_file_name(name);
                assert!(
                    sidecar.exists(),
                    "configured metadata must finish: {sidecar:?}"
                );
                paths.push(sidecar);
            }
            for path in paths {
                files.push((
                    path.clone(),
                    std::fs::metadata(&path).unwrap().modified().unwrap(),
                ));
            }
        }
        files.sort();
        if cycle == 0 {
            completed_files = files;
        } else {
            assert_eq!(files, completed_files);
        }
        assert!(
            db.get_owned_temp_files_before(i64::MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }
    assert!(probe.peak.load(Ordering::SeqCst) <= workers);
    drop(server);
}

#[tokio::test]
async fn full_sync_worker_budget_ten_workers_nine_passes() {
    check_full_sync_worker_budget(10, 9, WorkerBudgetRun::Complete).await;
}

#[tokio::test]
async fn full_sync_worker_budget_ten_workers_six_passes() {
    check_full_sync_worker_budget(10, 6, WorkerBudgetRun::Complete).await;
}

#[tokio::test]
async fn full_sync_worker_budget_bounds_serial_divisible_and_replacement_passes() {
    for (workers, passes) in [(1, 3), (2, 5), (10, 1), (6, 3)] {
        check_full_sync_worker_budget(workers, passes, WorkerBudgetRun::Complete).await;
    }
}

#[tokio::test]
async fn full_sync_worker_budget_cancellation_retains_work_and_recovers() {
    for (workers, passes) in [(10, 9), (2, 5)] {
        check_full_sync_worker_budget(workers, passes, WorkerBudgetRun::CancelFirstWave).await;
    }
}

#[tokio::test]
async fn full_sync_deferred_unfiled_excludes_album_members() {
    let album_session = MockPhotosFlow::new()
        .album_count(1)
        .query_photo_page("MASTER_1", Some("zone-token"))
        .build();
    let unfiled_session = MockPhotosFlow::new()
        .album_count(1)
        .query_photo_page("MASTER_1", Some("zone-token"))
        .build();
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", unfiled_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 2;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::dry_run_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("dry-run full sync should succeed");

    assert_eq!(
        result.stats.downloaded, 1,
        "asset present in an album pass must not also be counted through the unfiled pass"
    );
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "filtered duplicate should not make the run partial"
    );
}

#[tokio::test]
async fn full_sync_deferred_unfiled_write_mode_exclusion_does_not_count_as_shortfall() {
    let records = mock_photo_records_with_filename("MASTER_1", "test.jpg");
    let album_session = MockPhotosFlow::new()
        .album_count(1)
        .query_page(records.clone(), Some("zone-token"))
        .build();
    let unfiled_session = MockPhotosFlow::new()
        .album_count(1)
        .query_page(records.clone(), Some("zone-token"))
        .build();
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", unfiled_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 2;
    config.file_match_policy = FileMatchPolicy::NameId7;

    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let album_config = config.with_pass(&passes[0]);
    let expected_path = filter::expected_paths_for(&asset, &album_config)
        .into_iter()
        .next()
        .expect("mock asset should have an expected path");
    tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
        .await
        .expect("create parent dir");
    tokio::fs::write(&expected_path.path, vec![0u8; 1024])
        .await
        .expect("seed existing file");
    seed_downloaded_state_for_expected_path(&mut config, &album_config, &asset, &expected_path)
        .await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("write-mode full sync should succeed");

    assert_eq!(
        result.stats.enumeration_errors, 0,
        "deferred unfiled exclusions are intentional and must not be counted as pagination undercount"
    );
    assert_eq!(
        result.stats.api_total_at_start,
        Some(1),
        "reliable full-enumeration count should be persisted for cross-cycle inventory checks"
    );
    assert_eq!(
        result.sync_token.as_deref(),
        Some("zone-token"),
        "clean write-mode enumeration should still advance the agreed sync token"
    );
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_expected_receivers, Some(2));
    assert_eq!(result.stats.sync_token_receivers_with_token, Some(2));
    assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
    assert_eq!(result.stats.sync_token_receivers_blank, Some(0));
    assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
    assert_eq!(result.stats.sync_token_unique_values, Some(1));
    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "filtered duplicate should not make the write-mode run partial"
    );
}

#[tokio::test]
async fn full_sync_deferred_unfiled_excludes_empty_album_count_from_pagination_total() {
    let records = mock_photo_records_with_filename("MASTER_UNFILED", "unfiled.jpg");
    let album_session = MockPhotosFlow::new()
        .album_count(1)
        .empty_query_page(Some("zone-token"))
        .build();
    let unfiled_session = MockPhotosFlow::new()
        .album_count(1)
        .query_page(records.clone(), Some("zone-token"))
        .build();
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: mock_album("Vacation", album_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: mock_album("", unfiled_session),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 2;
    config.file_match_policy = FileMatchPolicy::NameId7;

    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    seed_existing_file_for_asset(&mut config, &passes[1], &asset).await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("write-mode full sync should succeed");

    assert_eq!(
        result.stats.api_total_at_start,
        Some(1),
        "empty album-side count must not be added on top of the library-wide unfiled stream"
    );
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.pagination_shortfall_assets, 0);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
}

#[tokio::test]
async fn full_sync_deferred_unfiled_opens_stream_after_album_passes_finish() {
    let (album_session, unfiled_session) = DeferredOrderSession::new_pair();
    let unfiled_probe = unfiled_session.clone();
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(unfiled_session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 2;
    for (pass, record_name) in [(&passes[0], "ORDER_ALBUM"), (&passes[1], "ORDER_UNFILED")] {
        let records = mock_photo_records_for_zone_with_filename(
            record_name,
            "PrimarySync",
            &format!("{record_name}.jpg"),
        );
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        seed_existing_file_for_asset(&mut config, pass, &asset).await;
    }

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("write-mode full sync should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert!(
        !unfiled_probe.unfiled_started_too_early(),
        "deferred unfiled must not enumerate URL-bearing assets while album passes are still running"
    );
    assert_eq!(
        result.stats.assets_seen, 2,
        "album and unfiled assets should both be processed after exclusions are known"
    );
}

#[tokio::test]
async fn full_sync_deferred_unfiled_logs_heartbeat_and_completion() {
    let (capture, _guard) = TracingCapture::install();
    let album_session =
        DynamicRecentPhotosSession::from_ids(vec!["album-heartbeat-0000".to_string()])
            .with_filename_prefix("album-heartbeat")
            .with_token("zone-token");
    let unfiled_count = DEFERRED_UNFILED_HEARTBEAT_ASSETS + 1;
    let unfiled_session =
        DynamicRecentPhotosSession::from_ids(recent_ids("unfiled-heartbeat", unfiled_count))
            .with_filename_prefix("unfiled-heartbeat")
            .with_token("zone-token");
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(unfiled_session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 10;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::dry_run_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("dry-run full sync should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let events = capture.events();
    let start = events
        .iter()
        .find(|event| event.message() == Some("Deferred unfiled enumeration started"))
        .unwrap_or_else(|| panic!("missing deferred unfiled start event: {events:?}"));
    assert_eq!(start.field("library"), Some("PrimarySync"));
    assert_eq!(start.field("pass_type"), Some("unfiled"));
    assert_eq!(start.field("assets_enumerated"), Some("0"));

    let progress = events
        .iter()
        .find(|event| event.message() == Some("Deferred unfiled enumeration progress"))
        .unwrap_or_else(|| panic!("missing deferred unfiled progress event: {events:?}"));
    assert_eq!(progress.field("library"), Some("PrimarySync"));
    assert_eq!(progress.field("pass_type"), Some("unfiled"));
    let progress_assets = DEFERRED_UNFILED_HEARTBEAT_ASSETS.to_string();
    assert_eq!(
        progress.field("assets_enumerated"),
        Some(progress_assets.as_str())
    );
    assert!(
        progress
            .field("expected_assets")
            .is_some_and(|value| value.contains(&unfiled_count.to_string())),
        "progress event should include expected asset count: {progress:?}"
    );
    assert!(
        progress.field("elapsed").is_some(),
        "progress event should include elapsed time: {progress:?}"
    );

    let complete = events
        .iter()
        .find(|event| event.message() == Some("Deferred unfiled enumeration complete"))
        .unwrap_or_else(|| panic!("missing deferred unfiled completion event: {events:?}"));
    assert_eq!(complete.field("library"), Some("PrimarySync"));
    assert_eq!(complete.field("pass_type"), Some("unfiled"));
    let completion_assets = unfiled_count.to_string();
    assert_eq!(
        complete.field("assets_enumerated"),
        Some(completion_assets.as_str())
    );
    assert!(
        complete.field("elapsed").is_some(),
        "completion event should include elapsed time: {complete:?}"
    );
}

#[tokio::test]
async fn full_sync_unfiled_only_does_not_log_deferred_unfiled_heartbeat() {
    let (capture, _guard) = TracingCapture::install();
    let unfiled_session = DynamicRecentPhotosSession::from_ids(recent_ids(
        "unfiled-only-heartbeat",
        DEFERRED_UNFILED_HEARTBEAT_ASSETS + 1,
    ))
    .with_filename_prefix("unfiled-only-heartbeat")
    .with_token("zone-token");
    let passes = vec![AlbumPass {
        kind: PassKind::Unfiled,
        album: album_with_session("PrimarySync", "", Box::new(unfiled_session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 10;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::dry_run_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("dry-run full sync should succeed");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    let events = capture.events();
    assert!(
        !events.iter().any(|event| event
            .message()
            .is_some_and(|message| message.starts_with("Deferred unfiled enumeration"))),
        "unfiled-only sync should not emit deferred unfiled heartbeat events: {events:?}"
    );
}

#[tokio::test]
async fn full_album_pass_records_complete_snapshot_before_planning_skips() {
    let filtered_records = mock_photo_records_for_zone_with_filename_and_asset_date(
        "MASTER_FILTERED",
        "PrimarySync",
        "filtered.jpg",
        1_700_000_000_000,
    );
    let on_disk_records = mock_photo_records_for_zone_with_filename_and_asset_date(
        "MASTER_ON_DISK",
        "PrimarySync",
        "on-disk.jpg",
        1_699_000_000_000,
    );
    let mut records = filtered_records.clone();
    records.extend(on_disk_records.clone());
    let album_session = MockPhotosFlow::new()
        .album_count(2)
        .query_page(records, Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album_with_container("Vacation", "container-vacation", album_session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.enum_config_hash = Some(Arc::from("hash-pr3"));
    config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::from_timestamp_millis(1_699_999_999_000).expect("valid timestamp"),
    ));
    let on_disk_asset = PhotoAsset::new(on_disk_records[0].clone(), on_disk_records[1].clone());
    seed_existing_file_for_asset(&mut config, &passes[0], &on_disk_asset).await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("full album sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert!(
        db.selected_album_containers_have_complete_snapshots(
            "PrimarySync",
            &["container-vacation"]
        )
        .await
        .unwrap(),
        "clean full pass should mark the album snapshot complete"
    );
    for (asset_record_name, master_record_name) in [
        ("asset-MASTER_FILTERED", "MASTER_FILTERED"),
        ("asset-MASTER_ON_DISK", "MASTER_ON_DISK"),
    ] {
        let memberships = db
            .get_live_selected_album_memberships_for_asset(
                "PrimarySync",
                asset_record_name,
                &["container-vacation"],
            )
            .await
            .unwrap();
        assert_eq!(memberships.len(), 1, "{asset_record_name} membership");
        assert_eq!(
            memberships[0].master_record_name.as_deref(),
            Some(master_record_name)
        );
    }
}

#[tokio::test]
async fn failed_album_pass_leaves_previous_complete_snapshot_trusted() {
    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    db.upsert_album_container("PrimarySync", "container-vacation", "Vacation", "album")
        .await
        .unwrap();
    let previous = db
        .start_album_membership_snapshot("PrimarySync", "container-vacation", Some("hash-old"))
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "container-vacation",
        previous,
        "asset-OLD",
        Some("MASTER_OLD"),
        "icloud",
    )
    .await
    .unwrap();
    db.complete_album_membership_snapshot("PrimarySync", "container-vacation", previous)
        .await
        .unwrap();

    let album_session = MockPhotosFlow::new()
        .album_count(1)
        .error("album stream failed")
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album_with_container("Vacation", "container-vacation", album_session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.enum_config_hash = Some(Arc::from("hash-new"));

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("stream errors should be reported in the sync result");

    assert!(result.stats.enumeration_errors > 0);
    assert!(
        db.selected_album_containers_have_complete_snapshots(
            "PrimarySync",
            &["container-vacation"]
        )
        .await
        .unwrap(),
        "failed replacement snapshot must not invalidate the old complete generation"
    );
    let memberships = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-OLD",
            &["container-vacation"],
        )
        .await
        .unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(
        memberships[0].master_record_name.as_deref(),
        Some("MASTER_OLD")
    );
}

#[tokio::test]
async fn interrupted_album_pass_does_not_complete_snapshot() {
    let album_session = MockPhotosFlow::new()
        .album_count(1)
        .query_photo_page("MASTER_INTERRUPTED", Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album_with_container("Vacation", "container-vacation", album_session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.state_db = Some(Arc::clone(&db) as Arc<dyn DownloadStore>);
    config.enum_config_hash = Some(Arc::from("hash-interrupted"));
    let shutdown = CancellationToken::new();
    shutdown.cancel();

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        shutdown,
    )
    .await
    .expect("interrupted stream should return a sync result");

    assert!(result.full_enumeration_ran);
    assert!(
        !db.selected_album_containers_have_complete_snapshots(
            "PrimarySync",
            &["container-vacation"]
        )
        .await
        .unwrap(),
        "interrupted album pass must leave the new snapshot incomplete"
    );
}

#[tokio::test]
async fn full_enumeration_shortfall_warns_but_allows_sync_token() {
    let records = mock_photo_records_with_filename("MASTER_SHORTFALL", "shortfall.jpg");
    let session = MockPhotosFlow::new()
        .album_count(2)
        .query_page(records.clone(), Some("zone-token"))
        .empty_query_page(Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 2;
    config.file_match_policy = FileMatchPolicy::NameId7;

    // Seed the destination so the single enumerated asset is skipped
    // on-disk and the test isolates count-only shortfall behavior.
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("count shortfall should not error");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "count-only shortfall should not be reported as partial failure"
    );
    assert_eq!(result.stats.failed, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(result.stats.pagination_shortfall_warnings, 1);
    assert_eq!(result.stats.pagination_shortfall_assets, 1);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_blocked_reason, None);
    assert_eq!(result.stats.sync_token_blocked_source, None);
    assert_eq!(result.stats.sync_token_blocked_explanation, None);
    assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
    assert_eq!(result.stats.sync_token_receivers_with_token, Some(1));
    assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
    assert_eq!(result.stats.sync_token_receivers_blank, Some(0));
    assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
    assert_eq!(result.stats.sync_token_unique_values, Some(1));
    assert_eq!(
        result.sync_token.as_deref(),
        Some("zone-token"),
        "count-side-channel shortfall must stay diagnostic when records/query completed cleanly"
    );
}

#[tokio::test]
async fn malformed_album_count_is_diagnostic_when_tail_proves_eof() {
    let records = mock_photo_records_with_filename("MASTER_BEFORE_GAP", "before-gap.jpg");
    let later_records = mock_photo_records_with_filename("MASTER_AFTER_GAP", "after-gap.jpg");
    let session = MockPhotosFlow::new()
        .album_count_response(json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": "not-a-count"}}}]}]
        }))
        .query_page(records.clone(), Some("zone-token"))
        .empty_query_page(Some("zone-token"))
        .empty_query_page(Some("zone-token"))
        .empty_query_page(Some("zone-token"))
        .empty_query_page(Some("zone-token"))
        .empty_query_page(Some("zone-token"))
        .query_page(later_records, Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.file_match_policy = FileMatchPolicy::NameId7;

    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("malformed album count should produce a sync result");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.stats.assets_seen, 1,
        "the finite empty-page ceiling is the natural EOF proof"
    );
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(result.stats.count_probe_failures, 1);
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.pagination_shortfall_assets, 0);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_blocked_reason, None);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
}

#[tokio::test]
async fn missing_album_count_is_diagnostic_when_empty_inventory_proves_eof() {
    let session = MockPhotosFlow::new()
        .album_count_response(json!({
            "batch": [{"records": [{"fields": {}}]}]
        }))
        .empty_query_page(Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("missing album count should produce a sync result");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(result.stats.count_probe_failures, 1);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_blocked_reason, None);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
}

#[tokio::test]
async fn malformed_album_count_still_blocks_when_stream_errors() {
    let session = MockPhotosFlow::new()
        .album_count_response(json!({
            "batch": [{"records": [{"fields": {"itemCount": {"value": "not-a-count"}}}]}]
        }))
        .error("stream failed")
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("stream error should produce a sync result");

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { failed_count: 1 }
    ));
    assert_eq!(result.stats.enumeration_errors, 1);
    assert_eq!(result.stats.count_probe_failures, 1);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(ICLOUD_ALBUM_COUNT_ERROR_REASON)
    );
    assert_eq!(result.sync_token, None);
}

#[tokio::test]
async fn well_formed_zero_album_count_allows_empty_token_capture() {
    let session = MockPhotosFlow::new()
        .album_count(0)
        .empty_query_page(Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("well-formed zero count should complete cleanly");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.assets_seen, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
}

#[tokio::test]
async fn full_enumeration_duplicate_asset_ids_do_not_block_sync_token() {
    let records = mock_photo_records_with_filename("MASTER_DUPLICATE", "duplicate.jpg");
    let session = MockPhotosFlow::new()
        .album_count(2)
        .query_page(records.clone(), Some("zone-token"))
        .query_page(records.clone(), Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.file_match_policy = FileMatchPolicy::NameId7;

    // Seed the destination so the unique asset is skipped on-disk and the
    // test isolates duplicate API asset-id accounting.
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let pass_config = config.with_pass(&passes[0]);
    let expected_path = filter::expected_paths_for(&asset, &pass_config)
        .into_iter()
        .next()
        .expect("mock asset should have an expected path");
    tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
        .await
        .expect("create parent dir");
    tokio::fs::write(&expected_path.path, vec![0u8; 1024])
        .await
        .expect("seed existing file");
    seed_downloaded_state_for_expected_path(&mut config, &pass_config, &asset, &expected_path)
        .await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("duplicate asset-id shortfall should not error");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "duplicate asset IDs should be treated as producer skips, not partial failure"
    );
    assert_eq!(result.stats.assets_seen, 1);
    assert_eq!(result.stats.skipped.duplicates, 0);
    assert_eq!(result.stats.pagination_shortfall_warnings, 1);
    assert_eq!(result.stats.pagination_shortfall_assets, 1);
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
    assert_eq!(result.stats.sync_token_receivers_with_token, Some(1));
    assert_eq!(result.stats.sync_token_unique_values, Some(1));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
}

#[tokio::test]
async fn full_sync_page_split_sibling_cplassets_keep_identity_and_download_nothing() {
    let records = vec![
        mock_asset_record_for("asset-sibling-a", "MASTER_SIBLING"),
        mock_master_record_with_filename("MASTER_SIBLING", "sibling.jpg"),
        mock_asset_record_for("asset-sibling-b", "MASTER_SIBLING"),
    ];
    let session = MockPhotosFlow::new()
        .album_count(2)
        .query_page(records.clone(), Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.file_match_policy = FileMatchPolicy::NameId7;

    let pass_config = config.with_pass(&passes[0]);
    let first_asset = PhotoAsset::new(records[1].clone(), records[0].clone());
    let second_asset = PhotoAsset::new(records[1].clone(), records[2].clone())
        .with_state_record_name(Arc::from("asset-sibling-b"));
    let mut seeded_paths = Vec::new();
    for (index, asset) in [&first_asset, &second_asset].into_iter().enumerate() {
        let mut expected_path = filter::expected_paths_for(asset, &pass_config)
            .into_iter()
            .next()
            .expect("mock sibling asset should have an expected path");
        if index > 0 {
            let filename = expected_path
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("expected path has filename");
            expected_path
                .path
                .set_file_name(paths::insert_asset_identity_suffix(
                    filename,
                    asset.state_id(),
                ));
        }
        tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
            .await
            .expect("create parent dir");
        tokio::fs::write(&expected_path.path, vec![0u8; 1024])
            .await
            .expect("seed existing file");
        seed_downloaded_state_for_expected_path(&mut config, &pass_config, asset, &expected_path)
            .await;
        seeded_paths.push(expected_path.path);
    }

    let config = Arc::new(config);
    let first = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &config,
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("sibling CPLAssets should complete");

    assert!(matches!(first.outcome, DownloadOutcome::Success));
    assert_eq!(first.stats.assets_seen, 2);
    assert_eq!(first.stats.downloaded, 0);
    assert_eq!(first.stats.failed, 0);
    assert_eq!(first.stats.skipped.duplicates, 0);
    assert_eq!(first.stats.pagination_shortfall_warnings, 0);
    assert_eq!(first.stats.pagination_shortfall_assets, 0);
    assert!(!first.stats.sync_token_blocked);
    assert_eq!(first.sync_token.as_deref(), Some("zone-token"));

    let split_session = MockPhotosFlow::new()
        .album_count(2)
        .query_page(
            vec![records[2].clone(), records[1].clone()],
            Some("zone-token-split"),
        )
        .query_page(vec![records[0].clone()], Some("zone-token-split"))
        .build();
    let split_passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", split_session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let second = download_photos_full_with_token(
        &Client::new(),
        &split_passes,
        &config,
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("page-split sibling sync should reuse both downloaded identities");

    assert!(matches!(second.outcome, DownloadOutcome::Success));
    assert_eq!(second.stats.assets_seen, 2);
    assert_eq!(second.stats.downloaded, 0);
    assert_eq!(second.stats.failed, 0);
    assert_eq!(second.stats.skipped.duplicates, 0);
    assert_eq!(second.stats.pagination_shortfall_warnings, 0);
    assert_eq!(second.stats.pagination_shortfall_assets, 0);
    assert!(!second.stats.sync_token_blocked);
    assert_eq!(second.sync_token.as_deref(), Some("zone-token-split"));
    assert!(seeded_paths.iter().all(|path| path.exists()));
    let downloaded_ids = config
        .state_db
        .as_ref()
        .expect("test state db")
        .get_downloaded_ids()
        .await
        .expect("downloaded state IDs");
    assert!(
        downloaded_ids
            .iter()
            .any(|(_, id, _)| id == "MASTER_SIBLING")
    );
    assert!(
        downloaded_ids
            .iter()
            .any(|(_, id, _)| id == "asset-sibling-b")
    );
    assert!(
        !downloaded_ids
            .iter()
            .any(|(_, id, _)| id == "asset-sibling-a")
    );
}

#[tokio::test]
async fn full_sync_filtered_sibling_cannot_displace_legacy_master_owner_next_cycle() {
    let asset_a_record = mock_asset_record_for("asset-owner-a", "MASTER_OWNER");
    let asset_b_record = mock_asset_record_for("asset-owner-b", "MASTER_OWNER");
    let master_record = mock_master_record_with_filename("MASTER_OWNER", "owner.jpg");
    let excluded_ids = Arc::new(["asset-owner-b".to_string()].into_iter().collect());
    let first_session = MockPhotosFlow::new()
        .album_count(2)
        .query_page(
            vec![
                asset_b_record.clone(),
                master_record.clone(),
                asset_a_record.clone(),
            ],
            Some("zone-token-first"),
        )
        .build();
    let first_passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", first_session),
        exclude_ids: Arc::clone(&excluded_ids),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.file_match_policy = FileMatchPolicy::NameId7;
    let db = Arc::new(SqliteStateDb::open_in_memory().expect("state db"));
    config.state_db = Some(db.clone());
    let legacy_asset = PhotoAsset::new(master_record.clone(), asset_a_record.clone());
    seed_existing_file_for_asset(&mut config, &first_passes[0], &legacy_asset).await;
    db.fail_next_legacy_master_state_owner_claims(1);
    let config = Arc::new(config);

    let first = download_photos_full_with_token(
        &Client::new(),
        &first_passes,
        &config,
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("first owner-claim cycle should complete");
    assert!(matches!(first.outcome, DownloadOutcome::Success));
    assert_eq!(first.stats.downloaded, 0);
    assert_eq!(first.stats.skipped.by_excluded_album, 1);
    assert_eq!(
        db.remaining_legacy_master_state_owner_claim_failures(),
        0,
        "the first owner claim must hit the injected write failure"
    );
    assert_eq!(
        config
            .state_db
            .as_ref()
            .expect("test state db")
            .get_legacy_master_state_owners()
            .await
            .expect("legacy owner rows"),
        std::collections::HashSet::from([(
            "PrimarySync".to_string(),
            "MASTER_OWNER".to_string(),
            "asset-owner-a".to_string(),
        )])
    );

    let second_session = MockPhotosFlow::new()
        .album_count(2)
        .query_page(
            vec![asset_a_record, master_record, asset_b_record],
            Some("zone-token-second"),
        )
        .build();
    let second_passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", second_session),
        exclude_ids: excluded_ids,
    }];
    let second = download_photos_full_with_token(
        &Client::new(),
        &second_passes,
        &config,
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("second owner-reuse cycle should complete");

    assert!(matches!(second.outcome, DownloadOutcome::Success));
    assert_eq!(second.stats.downloaded, 0);
    assert_eq!(second.stats.failed, 0);
    assert_eq!(second.stats.skipped.by_excluded_album, 1);
    let downloaded_ids = config
        .state_db
        .as_ref()
        .expect("test state db")
        .get_downloaded_ids()
        .await
        .expect("downloaded state IDs");
    assert!(downloaded_ids.iter().any(|(_, id, _)| id == "MASTER_OWNER"));
    assert!(
        !downloaded_ids
            .iter()
            .any(|(_, id, _)| id == "asset-owner-a")
    );
}

#[tokio::test]
async fn full_sync_blank_query_sync_token_blocks_advancement() {
    let records = mock_photo_records_with_filename("MASTER_BLANK_TOKEN", "blank-token.jpg");
    let session = MockPhotosFlow::new()
        .album_count(1)
        .query_page(records.clone(), Some(""))
        .empty_query_page(Some(""))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Hidden", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 2;
    config.file_match_policy = FileMatchPolicy::NameId7;

    // Seed the destination so the enumerated asset is skipped on-disk
    // and this test isolates token-capture behavior.
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let pass_config = config.with_pass(&passes[0]);
    let expected_path = filter::expected_paths_for(&asset, &pass_config)
        .into_iter()
        .next()
        .expect("mock asset should have an expected path");
    tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
        .await
        .expect("create parent dir");
    tokio::fs::write(&expected_path.path, vec![0u8; 1024])
        .await
        .expect("seed existing file");
    seed_downloaded_state_for_expected_path(&mut config, &pass_config, &asset, &expected_path)
        .await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("blank sync token should not error");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "blank token should not force partial failure"
    );
    assert_eq!(result.stats.failed, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.pagination_shortfall_assets, 0);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some("icloud_blank_sync_token")
    );
    assert_eq!(result.stats.sync_token_blocked_source, Some("icloud"));
    assert_eq!(
        result.stats.sync_token_blocked_explanation,
        Some(sync_token_blocked_explanation("icloud_blank_sync_token"))
    );
    assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
    assert_eq!(result.stats.sync_token_receivers_with_token, Some(0));
    assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
    assert_eq!(result.stats.sync_token_receivers_blank, Some(1));
    assert_eq!(result.stats.sync_token_receivers_dropped, Some(0));
    assert_eq!(result.stats.sync_token_unique_values, Some(0));
    assert_eq!(result.sync_token, None, "blank token must not be persisted");
    assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
    assert_eq!(result.stats.same_cycle_recovery_successes, 0);
}

#[tokio::test]
async fn full_sync_repairs_missing_pass_token_in_same_cycle() {
    let records = mock_photo_records_with_filename("MASTER_RECOVER", "recover.jpg");
    let query_calls = Arc::new(AtomicUsize::new(0));
    let session = RecoveringPassTokenSession {
        query_calls: Arc::clone(&query_calls),
        records: Arc::new(records.clone()),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session("PrimarySync", "Recovery", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.file_match_policy = FileMatchPolicy::NameId7;

    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("same-cycle token recovery should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-recovered"));
    assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
    assert_eq!(result.stats.same_cycle_recovery_successes, 1);
    assert_eq!(result.stats.sync_token_receivers_with_token, Some(1));
    assert_eq!(result.stats.sync_token_receivers_missing, Some(0));
    assert_eq!(query_calls.load(Ordering::SeqCst), 12);
}

#[tokio::test]
async fn full_sync_repairs_incomplete_pass_in_same_cycle() {
    let records = mock_photo_records_with_filename("MASTER_REPAIR", "repair.jpg");
    let query_calls = Arc::new(AtomicUsize::new(0));
    let session = RecoveringIncompletePassSession {
        query_calls: Arc::clone(&query_calls),
        records: Arc::new(records.clone()),
    };
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session("PrimarySync", "Recovery", Box::new(session)),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.file_match_policy = FileMatchPolicy::NameId7;
    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("incomplete pass should repair once in-cycle");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token-recovered"));
    assert_eq!(result.stats.same_cycle_recovery_attempts, 1);
    assert_eq!(result.stats.same_cycle_recovery_successes, 1);
    assert_eq!(query_calls.load(Ordering::SeqCst), 7);
}

/// A transient `pass.album.len()` failure must not
/// reduce to a 0-count that silently advances the sync token. Folding
/// the per-pass results must surface the failure as an error count so
/// downstream gates can suppress token advancement.
#[test]
fn fold_pass_count_results_counts_errors_and_zeroes_failed_passes() {
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoAlbum;
    use rustc_hash::FxHashSet;
    use std::sync::Arc;

    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("album_c")),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    // First and third pass succeed; second pass fails (transient len()
    // error). The failed pass must contribute 0 to the counts vector and
    // increment the error count by exactly 1.
    let results = vec![
        Ok(100),
        Err(anyhow::anyhow!("simulated transient len() failure")),
        Ok(50),
    ];

    let (counts, errors) = fold_pass_count_results(results, &passes);

    assert_eq!(counts, vec![100, 0, 50]);
    assert_eq!(
        errors, 1,
        "exactly one len() error must surface so token advancement is blocked"
    );
}

/// Pin the all-failures case: every pass's `len()` errors out → counts
/// are all zero AND the error count equals the pass count, so the cycle
/// cannot be mistaken for a clean empty enumeration.
#[test]
fn fold_pass_count_results_all_errors_yields_full_error_count() {
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoAlbum;
    use rustc_hash::FxHashSet;
    use std::sync::Arc;

    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let results = vec![
        Err(anyhow::anyhow!("first failure")),
        Err(anyhow::anyhow!("second failure")),
    ];

    let (counts, errors) = fold_pass_count_results(results, &passes);

    assert_eq!(counts, vec![0, 0]);
    assert_eq!(errors, 2);
}

#[test]
fn recent_runs_skip_pass_count_fetch() {
    let mut config = test_config();
    config.recent = Some(25);

    assert!(
        should_skip_pass_count_fetch(&config),
        "recent-limited runs are not complete enumerations, so the \
         full-pass count is not an exact pagination bound"
    );
}

#[test]
fn skip_created_before_runs_skip_pass_count_fetch() {
    let mut config = test_config();
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::from_timestamp_millis(1_700_000_000_000).expect("valid test timestamp"),
    ));

    assert!(
        should_skip_pass_count_fetch(&config),
        "lower-date-bounded runs stop before the full pass is drained, so \
         the full-pass count is not an exact pagination bound"
    );
}

#[test]
fn stream_created_lower_bound_uses_the_stricter_date_or_recent_frontier() {
    let capture_date = chrono::NaiveDate::from_ymd_opt(2025, 2, 1).unwrap();
    let date_bound =
        capture_date.and_time(chrono::NaiveTime::MIN).and_utc() - chrono::Duration::days(1);
    let mut config = test_config();
    config.skip_created_before = Some(crate::config::CreatedDateFilter::CaptureDate(capture_date));

    for (name, frontier_bound, expected) in [
        (
            "date bound",
            date_bound - chrono::Duration::hours(1),
            date_bound,
        ),
        (
            "recent frontier",
            date_bound + chrono::Duration::hours(1),
            date_bound + chrono::Duration::hours(1),
        ),
    ] {
        let frontier = RecentFrontier {
            asset_ids: Arc::new(FxHashSet::default()),
            oldest_created: Some(frontier_bound),
        };
        assert_eq!(
            stream_created_lower_bound(&config, Some(&frontier)),
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn skip_created_after_runs_keep_pass_count_fetch() {
    let mut config = test_config();
    config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::from_timestamp_millis(1_700_000_000_000).expect("valid test timestamp"),
    ));

    assert!(
        !should_skip_pass_count_fetch(&config),
        "upper-date filters must still drain the stream because older \
         assets after the skipped prefix can still match"
    );
}

#[test]
fn unbounded_runs_keep_pass_count_fetch() {
    let mut config = test_config();
    config.recent = None;

    assert!(
        !should_skip_pass_count_fetch(&config),
        "unbounded runs still use exact counts for progress bounds and \
         pagination-underflow detection"
    );
}

#[tokio::test]
async fn build_pass_count_plan_uses_recent_bound_without_exact_counts_for_read_only_runs() {
    use crate::commands::{AlbumPass, PassKind};
    use crate::icloud::photos::PhotoAlbum;
    use rustc_hash::FxHashSet;
    use std::sync::Arc;

    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("album_a")),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Album,
            album: PhotoAlbum::stub_for_test(Arc::from("album_b")),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];
    let mut config = test_config();
    config.recent = Some(10);

    let plan = build_pass_count_plan(&passes, &config, DownloadControls::dry_run_hidden()).await;

    assert_eq!(plan.display_counts, vec![10, 10]);
    assert_eq!(plan.stream_total_counts, vec![None, None]);
    assert_eq!(plan.exact_total, None);
    assert_eq!(plan.len_errors, 0);
}

#[tokio::test]
async fn full_sync_recent_album_passes_use_scope_frontier() {
    let all_assets = recent_scope_assets("frontier", 300, 1_700_000_000_000);
    let mut album_assets = vec![all_assets[0].clone()];
    album_assets.extend(recent_scope_assets("old-album", 500, 1_699_000_000_000));
    let asset = recent_scope_photo_asset(&all_assets[0]);
    let session = RecentScopeSession::new(all_assets, album_assets);

    let passes: Vec<AlbumPass> = (0..10)
        .map(|index| AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album(&format!("Album {index}"), session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        })
        .collect();

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.recent = Some(300);
    for pass in &passes {
        seed_existing_file_for_asset(&mut config, pass, &asset).await;
    }

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("scope-frontier recent sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.stats.assets_seen,
        10,
        "each album should plan only assets inside the library-wide recent frontier; offsets={:?}",
        session.album_offsets()
    );
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(
        session.album_offsets().len() < 100,
        "album enumeration should stop near the frontier boundary instead of \
         applying recent=300 to every album pass"
    );
    assert_eq!(result.sync_token, None);
}

#[tokio::test]
async fn full_sync_recent_per_filter_scope_limits_each_pass_independently() {
    let all_assets = recent_scope_assets("frontier", 6, 1_700_000_000_000);
    let mut album_assets = vec![all_assets[0].clone()];
    album_assets.extend(recent_scope_assets(
        "old-per-filter-album",
        20,
        1_699_000_000_000,
    ));
    let expected_assets = album_assets
        .iter()
        .take(6)
        .map(recent_scope_photo_asset)
        .collect::<Vec<_>>();
    let session = RecentScopeSession::new(all_assets, album_assets);

    let passes: Vec<AlbumPass> = (0..3)
        .map(|index| AlbumPass {
            kind: PassKind::Album,
            album: recent_scope_album(&format!("Album {index}"), session.clone()),
            exclude_ids: Arc::new(FxHashSet::default()),
        })
        .collect();

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.recent = Some(6);
    config.recent_scope = crate::cli::RecentScope::PerFilter;

    for pass in &passes {
        for asset in &expected_assets {
            seed_existing_file_for_asset(&mut config, pass, asset).await;
        }
    }

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("per-filter recent sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.stats.assets_seen, 18,
        "per-filter recent scope should take the recent limit from each album pass"
    );
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(
        session.album_offsets().len() >= 9,
        "per-filter scope should enumerate each album's recent window, not stop at the global frontier"
    );
    assert_eq!(result.sync_token, None);
}

#[tokio::test]
async fn full_sync_recent_single_album_filters_library_frontier() {
    let all_assets = recent_scope_assets("frontier", 300, 1_700_000_000_000);
    let mut album_assets = vec![all_assets[0].clone()];
    album_assets.extend(recent_scope_assets(
        "old-single-album",
        500,
        1_699_000_000_000,
    ));
    let asset = recent_scope_photo_asset(&all_assets[0]);
    let session = RecentScopeSession::new(all_assets, album_assets);
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: recent_scope_album("Vacation", session.clone()),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.recent = Some(300);
    seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("single-album scope-frontier recent sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.stats.assets_seen, 1,
        "album filter should pare down the library-wide recent frontier"
    );
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(
        session.album_offsets().len() < 10,
        "single album enumeration should stop at the frontier boundary"
    );
    assert_eq!(result.sync_token, None);
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(RECENT_LIMITED_FULL_ENUMERATION_REASON)
    );
}

#[tokio::test]
async fn full_sync_skip_created_before_stops_at_date_boundary() {
    let newer_assets = recent_scope_assets("date-new", 5, 1_700_000_000_000);
    let older_assets = recent_scope_assets("date-old", 20, 1_699_000_000_000);
    let mut album_assets = newer_assets.clone();
    album_assets.extend(older_assets);
    let session = RecentScopeSession::new(album_assets.clone(), album_assets);
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: recent_scope_album("Vacation", session.clone()),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::from_timestamp_millis(1_699_999_000_000).expect("valid test timestamp"),
    ));
    for asset in newer_assets.iter().map(recent_scope_photo_asset) {
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;
    }

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("date-bounded full sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.stats.assets_seen, 5,
        "lower-date-bound enumeration should stop before older assets are \
         handed to the download pipeline"
    );
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(
        session.album_offsets().len() <= 5,
        "lower-date-bound enumeration should stop near the first old page; offsets={:?}",
        session.album_offsets()
    );
    assert_eq!(
        result.sync_token, None,
        "date-bounded full sync must not advance a zone token"
    );
    assert!(result.stats.sync_token_blocked);
    assert_eq!(
        result.stats.sync_token_blocked_reason,
        Some(DATE_BOUNDED_FULL_ENUMERATION_REASON)
    );
}

#[tokio::test]
async fn full_sync_skip_created_before_saves_token_when_boundary_does_not_truncate() {
    let newer_assets = recent_scope_assets("date-newer-only", 5, 1_700_000_000_000);
    let session = RecentScopeSession::new(newer_assets.clone(), newer_assets.clone());
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: recent_scope_album("Vacation", session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.skip_created_before = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::from_timestamp_millis(1_699_000_000_000).expect("valid test timestamp"),
    ));
    for asset in newer_assets.iter().map(recent_scope_photo_asset) {
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;
    }

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("non-truncating date-bounded full sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.assets_seen, 5);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
    assert!(!result.stats.sync_token_blocked);
}

#[tokio::test]
async fn full_sync_skip_created_after_drains_past_newer_prefix() {
    let newer_assets = recent_scope_assets("date-after-new", 5, 1_700_000_000_000);
    let older_assets = recent_scope_assets("date-after-old", 5, 1_699_000_000_000);
    let mut album_assets = newer_assets;
    album_assets.extend(older_assets.clone());
    let session = RecentScopeSession::new(album_assets.clone(), album_assets);
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: recent_scope_album("Vacation", session.clone()),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.skip_created_after = Some(crate::config::CreatedDateFilter::Instant(
        DateTime::from_timestamp_millis(1_699_999_000_000).expect("valid test timestamp"),
    ));
    for asset in older_assets.iter().map(recent_scope_photo_asset) {
        seed_existing_file_for_asset(&mut config, &passes[0], &asset).await;
    }

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("upper-date-filtered full sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.stats.assets_seen, 10,
        "upper-date filters skip the newer prefix but must keep enumerating \
         because older assets can still match"
    );
    assert_eq!(result.stats.pagination_shortfall_warnings, 0);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert!(
        session.album_offsets().len() > 3,
        "upper-date filters must not stop near the newer prefix; offsets={:?}",
        session.album_offsets()
    );
    assert_eq!(result.sync_token.as_deref(), Some("zone-token"));
}

#[tokio::test]
async fn full_sync_recent_download_saves_token_when_cap_does_not_bind() {
    let records = mock_photo_records_with_filename("MASTER_RECENT", "recent.jpg");
    let album_session = MockPhotosFlow::new()
        .query_page(records.clone(), Some("zone-token"))
        .empty_query_page(Some("zone-token"))
        .build();
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: mock_album("Vacation", album_session),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.recent = Some(100);
    config.file_match_policy = FileMatchPolicy::NameId7;

    let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
    let pass_config = config.with_pass(&passes[0]);
    let expected_path = filter::expected_paths_for(&asset, &pass_config)
        .into_iter()
        .next()
        .expect("mock asset should have an expected path");
    tokio::fs::create_dir_all(expected_path.path.parent().expect("path has parent"))
        .await
        .expect("create parent dir");
    tokio::fs::write(&expected_path.path, vec![0u8; 1024])
        .await
        .expect("seed existing file");
    seed_downloaded_state_for_expected_path(&mut config, &pass_config, &asset, &expected_path)
        .await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("recent full sync should complete");

    assert!(
        matches!(result.outcome, DownloadOutcome::Success),
        "a sparse recent window must not be treated as pagination undercount"
    );
    assert_eq!(
        result.stats.enumeration_errors, 0,
        "recent-limited count shortfalls are not exact enumeration errors"
    );
    assert_eq!(
        result.stats.api_total_at_start, None,
        "recent-limited runs must not seed comparable inventory totals"
    );
    assert_eq!(
        result.sync_token.as_deref(),
        Some("zone-token"),
        "non-binding recent cap should still produce a complete-zone token"
    );
    assert!(!result.stats.sync_token_blocked);
    assert_eq!(result.stats.sync_token_expected_receivers, Some(1));
}

#[tokio::test]
async fn full_sync_recent_download_drains_multiple_reduced_pages() {
    let ids = recent_ids("recent-prod", 100);
    let session = DynamicRecentPhotosSession::from_ids(ids.clone())
        .with_filename_prefix("recent-prod")
        .with_token("zone-token");
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session("PrimarySync", "Vacation", Box::new(session.clone())),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 10;
    config.recent = Some(100);
    seed_existing_recent_files(&mut config, &passes[0], "PrimarySync", &ids, "recent-prod").await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("recent full sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.assets_seen, 100);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(
        result.sync_token.as_deref(),
        Some("zone-token"),
        "a recent run that still drains the selected stream to EOF should advance the zone token"
    );
    assert!(!result.stats.sync_token_blocked);
    assert!(
        session.offsets().len() >= 5,
        "write-mode full sync should drain every reduced download page"
    );
}

async fn run_recent_mode_for_ids(
    mode: DownloadRunMode,
    ids: &[String],
    filename_prefix: &str,
) -> (SyncResult, Vec<String>) {
    let session = DynamicRecentPhotosSession::from_ids(ids.to_vec())
        .with_filename_prefix(filename_prefix)
        .with_token("zone-token");
    let passes = vec![AlbumPass {
        kind: PassKind::Album,
        album: album_with_session("PrimarySync", "Vacation", Box::new(session.clone())),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 1;
    config.recent = Some(ids.len().try_into().expect("test id count fits u32"));
    if matches!(mode, DownloadRunMode::Download) {
        seed_existing_recent_files(&mut config, &passes[0], "PrimarySync", ids, filename_prefix)
            .await;
    }

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::new(mode, DownloadReporting::hidden()),
        CancellationToken::new(),
    )
    .await
    .expect("recent mode sync should complete");

    (result, unique_ids_in_order(session.emitted_ids()))
}

#[tokio::test]
async fn full_sync_recent_run_modes_enumerate_same_asset_ids() {
    let ids = recent_ids("mode-parity", 6);

    let (print_result, print_ids) =
        run_recent_mode_for_ids(DownloadRunMode::PrintFilenames, &ids, "mode-parity").await;
    let (dry_result, dry_ids) =
        run_recent_mode_for_ids(DownloadRunMode::DryRun, &ids, "mode-parity").await;
    let (download_result, download_ids) =
        run_recent_mode_for_ids(DownloadRunMode::Download, &ids, "mode-parity").await;

    assert!(matches!(print_result.outcome, DownloadOutcome::Success));
    assert!(matches!(dry_result.outcome, DownloadOutcome::Success));
    assert!(matches!(download_result.outcome, DownloadOutcome::Success));
    assert_eq!(print_ids, ids);
    assert_eq!(dry_ids, ids);
    assert_eq!(download_ids, ids);
    assert_eq!(print_ids, dry_ids);
    assert_eq!(dry_ids, download_ids);
    assert_eq!(download_result.stats.assets_seen, ids.len() as u64);
    assert_eq!(print_result.sync_token, None);
    assert_eq!(dry_result.sync_token, None);
    assert_eq!(
        download_result.sync_token.as_deref(),
        Some("zone-token"),
        "download mode may advance when the recent cap is exactly the selected stream"
    );
}

#[tokio::test]
async fn full_sync_recent_deferred_unfiled_filters_album_members_after_multi_page_stream() {
    let album_ids = recent_ids("album-member", 40);
    let unfiled_only_ids = recent_ids("unfiled-only", 20);
    let mut unfiled_ids = album_ids.clone();
    unfiled_ids.extend(unfiled_only_ids.clone());

    let album_session = DynamicRecentPhotosSession::from_ids(album_ids.clone())
        .with_filename_prefix("album-member")
        .with_token("zone-token");
    let unfiled_session = DynamicRecentPhotosSession::from_ids(unfiled_ids.clone())
        .with_filename_prefix("unfiled-mixed")
        .with_token("zone-token");
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(unfiled_session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 10;
    config.recent = Some(60);
    seed_existing_recent_files(
        &mut config,
        &passes[0],
        "PrimarySync",
        &album_ids,
        "album-member",
    )
    .await;
    seed_existing_recent_files(
        &mut config,
        &passes[1],
        "PrimarySync",
        &unfiled_ids,
        "unfiled-mixed",
    )
    .await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("recent album plus unfiled sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(
        result.stats.assets_seen, 60,
        "40 album assets plus 20 non-album unfiled assets should be counted"
    );
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(
        result.sync_token.as_deref(),
        Some("zone-token"),
        "recent deferred-unfiled sync may advance when the global frontier does not truncate"
    );
    assert_eq!(
        unfiled_session.emitted_ids().len(),
        120,
        "recent deferred unfiled should build the global frontier and then re-open a fresh stream for download URLs"
    );
}

#[tokio::test]
async fn full_sync_recent_smart_folder_drains_multiple_reduced_pages() {
    let ids = recent_ids("smart-recent", 60);
    let session = DynamicRecentPhotosSession::from_ids(ids.clone())
        .with_filename_prefix("smart-recent")
        .with_token("zone-token");
    let passes = vec![AlbumPass {
        kind: PassKind::SmartFolder,
        album: album_with_session("PrimarySync", "Favorites", Box::new(session.clone())),
        exclude_ids: Arc::new(FxHashSet::default()),
    }];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 10;
    config.recent = Some(60);
    seed_existing_recent_files(&mut config, &passes[0], "PrimarySync", &ids, "smart-recent").await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("recent smart-folder sync should complete");

    assert!(matches!(result.outcome, DownloadOutcome::Success));
    assert_eq!(result.stats.assets_seen, 60);
    assert_eq!(result.stats.enumeration_errors, 0);
    assert_eq!(
        result.sync_token.as_deref(),
        Some("zone-token"),
        "smart-folder recent sync may advance when the cap does not truncate"
    );
    assert!(
        session.offsets().len() >= 3,
        "smart-folder recent sync should drain every reduced page"
    );
}

#[tokio::test]
async fn full_sync_deferred_unfiled_waits_when_album_enumeration_errors() {
    let album_ids = recent_ids("album-error", 40);
    let unfiled_ids = recent_ids("unfiled-after-error", 20);
    let mut library_ids = album_ids.clone();
    library_ids.extend(unfiled_ids.clone());
    let album_session = DynamicRecentPhotosSession::from_ids(album_ids.clone())
        .with_filename_prefix("album-error")
        .with_error_at_offset(20);
    let unfiled_session = DynamicRecentPhotosSession::from_ids(library_ids)
        .with_filename_prefix("unfiled-after-error");
    let passes = vec![
        AlbumPass {
            kind: PassKind::Album,
            album: album_with_session("PrimarySync", "Vacation", Box::new(album_session)),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
        AlbumPass {
            kind: PassKind::Unfiled,
            album: album_with_session("PrimarySync", "", Box::new(unfiled_session.clone())),
            exclude_ids: Arc::new(FxHashSet::default()),
        },
    ];

    let mut config = test_config();
    let dir = TempDir::new().expect("temp dir");
    config.directory = Arc::from(dir.path());
    config.concurrent_downloads = 10;
    config.recent = Some(40);
    seed_existing_recent_files(
        &mut config,
        &passes[0],
        "PrimarySync",
        &album_ids,
        "album-error",
    )
    .await;
    seed_existing_recent_files(
        &mut config,
        &passes[1],
        "PrimarySync",
        &unfiled_ids,
        "unfiled-after-error",
    )
    .await;

    let result = download_photos_full_with_token(
        &Client::new(),
        &passes,
        &Arc::new(config),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .expect("album enumeration error should be reported as partial result");

    assert!(matches!(
        result.outcome,
        DownloadOutcome::PartialFailure { .. }
    ));
    assert_eq!(result.stats.enumeration_errors, 1);
    assert_eq!(
        result.stats.assets_seen, 20,
        "unfiled assets must not be processed when album exclusions are incomplete"
    );
    assert_eq!(result.sync_token, None);
}

#[test]
fn zone_token_evidence_is_complete_when_all_passes_agree() {
    let observations = [PassKind::Album, PassKind::Unfiled]
        .into_iter()
        .enumerate()
        .map(|(index, kind)| PassTokenObservation {
            pass: PassKey {
                index,
                kind,
                label: format!("pass-{index}"),
            },
            result: PassTokenResult::Present("zone-token".to_string()),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        classify_zone_token_evidence(&observations),
        ZoneTokenEvidence::Complete {
            token: "zone-token".to_string()
        }
    );
}

#[test]
fn zone_token_evidence_retries_all_passes_after_mismatch() {
    let observations = ["zone-token-a", "zone-token-b"]
        .into_iter()
        .enumerate()
        .map(|(index, token)| PassTokenObservation {
            pass: PassKey {
                index,
                kind: PassKind::Album,
                label: format!("pass-{index}"),
            },
            result: PassTokenResult::Present(token.to_string()),
        })
        .collect::<Vec<_>>();

    let ZoneTokenEvidence::Recoverable { passes, reason } =
        classify_zone_token_evidence(&observations)
    else {
        panic!("mismatched pass tokens must be recoverable once");
    };
    assert_eq!(reason, TokenGap::Mismatch);
    assert_eq!(passes.len(), 2);
}

#[test]
fn zone_token_evidence_retries_only_passes_with_gaps() {
    for (gap, expected_reason) in [
        (PassTokenResult::Missing, TokenGap::Missing),
        (PassTokenResult::Blank, TokenGap::Blank),
        (PassTokenResult::ReceiverDropped, TokenGap::ReceiverDropped),
        (
            PassTokenResult::EnumerationIncomplete,
            TokenGap::EnumerationIncomplete,
        ),
    ] {
        let observations = vec![
            PassTokenObservation {
                pass: PassKey {
                    index: 0,
                    kind: PassKind::Album,
                    label: "complete".to_string(),
                },
                result: PassTokenResult::Present("zone-token".to_string()),
            },
            PassTokenObservation {
                pass: PassKey {
                    index: 1,
                    kind: PassKind::Unfiled,
                    label: "gap".to_string(),
                },
                result: gap,
            },
        ];

        let ZoneTokenEvidence::Recoverable { passes, reason } =
            classify_zone_token_evidence(&observations)
        else {
            panic!("pass gap must receive one bounded recovery round");
        };
        assert_eq!(reason, expected_reason);
        assert_eq!(passes, vec![observations[1].pass.clone()]);
    }
}

#[test]
fn contract_source_checkpoint_requires_durable_recovery_token_evidence_exhaustive() {
    const RESULT_KINDS: usize = 6;
    for len in 0..=3u32 {
        for encoded in 0..RESULT_KINDS.pow(len) {
            let mut value = encoded;
            let mut observations = Vec::with_capacity(len as usize);
            for index in 0..len as usize {
                let result = match value % RESULT_KINDS {
                    0 => PassTokenResult::Present("token-a".to_string()),
                    1 => PassTokenResult::Present("token-b".to_string()),
                    2 => PassTokenResult::Missing,
                    3 => PassTokenResult::Blank,
                    4 => PassTokenResult::ReceiverDropped,
                    _ => PassTokenResult::EnumerationIncomplete,
                };
                value /= RESULT_KINDS;
                observations.push(PassTokenObservation {
                    pass: PassKey {
                        index,
                        kind: PassKind::Album,
                        label: format!("pass-{index}"),
                    },
                    result,
                });
            }

            let first_token = observations.first().and_then(|observation| {
                if let PassTokenResult::Present(token) = &observation.result {
                    Some(token.as_str())
                } else {
                    None
                }
            });
            let complete_expected = first_token.is_some()
                && observations.iter().all(|observation| {
                    matches!(
                        &observation.result,
                        PassTokenResult::Present(token)
                            if Some(token.as_str()) == first_token
                    )
                });
            let evidence = classify_zone_token_evidence(&observations);
            assert_eq!(
                matches!(evidence, ZoneTokenEvidence::Complete { .. }),
                complete_expected,
                "unexpected complete classification: {observations:?}"
            );
            assert_eq!(
                matches!(evidence, ZoneTokenEvidence::Incomplete { .. }),
                observations.is_empty(),
                "only an empty observation set may be incomplete"
            );
        }
    }
}

/// Per-pass mode AND-folds `enumeration_complete` across passes.
/// The first pass that aborts must drop the cycle's flag to false. The
/// `&&=` semantics are subtle (especially around the empty-passes case)
/// so this test pins the truth table.
#[test]
fn enum_progress_marker_per_pass_and_fold_semantics() {
    // All passes complete → cycle complete.
    let mut combined = true;
    for pass_complete in [true, true, true] {
        combined = combined && pass_complete;
    }
    assert!(combined, "all passes complete → marker clears");

    // One pass aborted mid-stream → cycle incomplete.
    let mut combined = true;
    for pass_complete in [true, false, true] {
        combined = combined && pass_complete;
    }
    assert!(
        !combined,
        "one pass aborted → marker must stay set even if siblings finished"
    );

    // Empty passes: combined stays at the initializer (false in the
    // production code so a no-pass cycle doesn't accidentally clear
    // the marker for a zone the cycle didn't actually enumerate).
    let combined: bool = []
        .iter()
        .fold(false, |acc, pass_complete: &bool| acc && *pass_complete);
    assert!(!combined, "no passes → marker stays set");
}

/// Pagination undercount classifier — exact match returns Match.
/// Token must advance silently when the producer saw at least as many
/// assets as the API reported.
#[test]
fn classify_pagination_shortfall_exact_match_is_silent() {
    let decision = classify_pagination_shortfall(1000, 1000, 0);
    assert_eq!(decision, PaginationShortfall::Match);
}

/// Duplicate API asset IDs can explain a count gap because they are
/// included in the pre-enumeration count but suppressed by the producer
/// before `assets_seen` advances.
#[test]
fn classify_pagination_shortfall_duplicate_compensated_allows_token() {
    let decision = classify_pagination_shortfall(23555, 23549, 6);
    assert_eq!(
        decision,
        PaginationShortfall::DuplicateCompensated { shortfall: 6 }
    );
}

/// A 1% undercount is still reported when duplicate asset IDs do not
/// explain it.
#[test]
fn classify_pagination_shortfall_one_percent_below_reports_shortfall() {
    let decision = classify_pagination_shortfall(1000, 990, 0);
    assert_eq!(
        decision,
        PaginationShortfall::Shortfall { shortfall: 10 },
        "unexplained shortfalls should remain visible"
    );
}

/// Regression fixture for issue #498: expected=1578, seen=1533
/// (shortfall=45, ~2.85%). This remains visible as an endpoint-drift
/// diagnostic.
#[test]
fn classify_pagination_shortfall_issue_498_fixture_reports_shortfall() {
    let decision = classify_pagination_shortfall(1578, 1533, 0);
    assert_eq!(decision, PaginationShortfall::Shortfall { shortfall: 45 });
}

/// Regression fixture from downstream k8s-gitops mitigation:
/// expected=31_000, seen=30_959 (shortfall=41, ~0.13%).
#[test]
fn classify_pagination_shortfall_billimek_sharedsync_fixture_reports_shortfall() {
    let decision = classify_pagination_shortfall(31_000, 30_959, 0);
    assert_eq!(decision, PaginationShortfall::Shortfall { shortfall: 41 });
}
