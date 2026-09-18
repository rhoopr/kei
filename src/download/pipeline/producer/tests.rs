use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use futures_util::stream;
use reqwest::Client;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "xmp")]
use crate::download::DownloadOutcome;
use crate::download::filter::derive_expected_paths;
use crate::download::pipeline::adoption::asset_record_for_derived_path;
use crate::download::pipeline::outcome::build_download_outcome;
use crate::download::pipeline::producer::{
    AssetDisposition, BatchForecast, FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES, ProducerSkipSummary,
    batch_forecast_decision, should_resnapshot_free_space,
};
use crate::download::pipeline::streaming::{
    StreamRuntime, stream_and_download_from_stream, stream_and_download_from_stream_with_context,
};
#[cfg(feature = "xmp")]
use crate::download::pipeline::test_support::{MINIMAL_JPEG, sidecar_path_for};
use crate::download::planner::{
    ADD_ASSET_ALBUM_MAX_RETRIES, TaskPlanner, add_asset_album_with_retry,
};
use crate::download::{DownloadConfig, DownloadControls, planner, preload_download_context};
use crate::icloud::photos::PhotoAsset;
use crate::state::error::StateError;
use crate::state::{AssetRecord, MembershipStore, VersionSizeKey};
use crate::test_helpers::TestPhotoAsset;

// ── batch_forecast_decision unit tests ─────────────────────────────────

#[test]
fn batch_forecast_decision_none_free_always_continues() {
    let queued = AtomicU64::new(0);
    let warn = AtomicBool::new(false);
    let (decision, total) = batch_forecast_decision(10_000, None, &queued, &warn);
    assert_eq!(decision, BatchForecast::Continue);
    assert_eq!(total, 10_000);
    // Even huge sizes must not emit warn/bail when free-space probe failed
    let (decision, _) = batch_forecast_decision(u64::MAX - 10_000, None, &queued, &warn);
    assert_eq!(decision, BatchForecast::Continue);
}

#[test]
fn batch_forecast_decision_below_warn_threshold_continues() {
    let queued = AtomicU64::new(0);
    let warn = AtomicBool::new(false);
    // free = 1000, 50% of free queued → below 90% threshold
    let (decision, total) = batch_forecast_decision(500, Some(1000), &queued, &warn);
    assert_eq!(decision, BatchForecast::Continue);
    assert_eq!(total, 500);
    assert!(!warn.load(Ordering::Relaxed));
}

#[test]
fn batch_forecast_decision_crossing_90pct_warns_once() {
    let queued = AtomicU64::new(0);
    let warn = AtomicBool::new(false);
    // First call crosses 90% threshold
    let (decision, total) = batch_forecast_decision(900, Some(1000), &queued, &warn);
    assert_eq!(decision, BatchForecast::Warn);
    assert_eq!(total, 900);
    assert!(warn.load(Ordering::Relaxed));
    // Subsequent calls that stay below 100% must NOT re-warn
    let (decision, total) = batch_forecast_decision(50, Some(1000), &queued, &warn);
    assert_eq!(decision, BatchForecast::Continue);
    assert_eq!(total, 950);
}

#[test]
fn batch_forecast_decision_crossing_100pct_bails() {
    let queued = AtomicU64::new(800);
    let warn = AtomicBool::new(true); // already warned at 800
    // 800 + 250 = 1050 ≥ 1000 → bail
    let (decision, total) = batch_forecast_decision(250, Some(1000), &queued, &warn);
    assert_eq!(decision, BatchForecast::Bail);
    assert_eq!(total, 1050);
}

#[test]
fn batch_forecast_decision_prefers_bail_over_warn_at_100pct_first_call() {
    // If the very first queued task already exceeds free space, we should
    // bail (not warn). This is the 2TB-into-300GB-disk scenario.
    let queued = AtomicU64::new(0);
    let warn = AtomicBool::new(false);
    let (decision, total) =
        batch_forecast_decision(2_000_000_000, Some(300_000_000), &queued, &warn);
    assert_eq!(decision, BatchForecast::Bail);
    assert_eq!(total, 2_000_000_000);
    // warn flag should NOT have been set — bail short-circuits
    assert!(!warn.load(Ordering::Relaxed));
}

#[test]
fn batch_forecast_decision_zero_size_is_a_noop() {
    let queued = AtomicU64::new(500);
    let warn = AtomicBool::new(false);
    let (decision, total) = batch_forecast_decision(0, Some(1000), &queued, &warn);
    assert_eq!(decision, BatchForecast::Continue);
    assert_eq!(total, 500);
}

// ── Free-space re-snapshot cadence ───────────────────────────────

/// A re-snapshot is required once the running queued total has
/// crossed `interval` bytes past the last snapshot point. Pinning the
/// happy path so a future tweak doesn't regress to the stale-snapshot
/// behaviour the fix was added to repair.
#[test]
fn should_resnapshot_free_space_fires_at_interval_boundary() {
    let interval = FREE_SPACE_RESNAPSHOT_INTERVAL_BYTES;
    // Just below the interval: no resnapshot.
    assert!(!should_resnapshot_free_space(interval - 1, 0, interval));
    // Exactly at the interval: resnapshot required.
    assert!(should_resnapshot_free_space(interval, 0, interval));
    // Past the interval: resnapshot still required (caller updates
    // last_snapshot to suppress repeats; this helper is stateless).
    assert!(should_resnapshot_free_space(interval * 2, 0, interval));
}

/// When the interval is 0 the helper must return `false` for any
/// total. Guards against accidentally turning the cadence into "every
/// call" if a constant is ever zeroed out.
#[test]
fn should_resnapshot_free_space_zero_interval_never_fires() {
    for total in [0u64, 1, 1_000, u64::MAX] {
        assert!(
            !should_resnapshot_free_space(total, 0, 0),
            "interval=0 must never request a resnapshot (total={total})"
        );
    }
}

/// Drives the helper through a sequence where the simulated free
/// space drops between snapshots and asserts the bail decision picks up
/// the fresher snapshot. Pre-fix, the bail rode on the stale 1 TiB
/// snapshot and never fired even though the FS had only 1 GiB left.
#[test]
fn batch_forecast_resnapshot_picks_up_fs_filling_mid_sync() {
    let queued = AtomicU64::new(0);
    let warn = AtomicBool::new(false);

    // Snapshot 1: 1 TiB free. Queue 500 GiB → still well under 90%, no
    // warn or bail.
    let initial_free = 1024u64 * 1024 * 1024 * 1024; // 1 TiB
    let snapshot1_total = 500u64 * 1024 * 1024 * 1024; // 500 GiB
    let (decision, total) =
        batch_forecast_decision(snapshot1_total, Some(initial_free), &queued, &warn);
    assert_eq!(
        decision,
        BatchForecast::Continue,
        "500 GiB / 1 TiB stays under 90%"
    );
    assert_eq!(total, snapshot1_total);

    // Between snapshots an unrelated process consumed almost all the
    // disk. Refresh free-space to 1 GiB. Even though `queued` already
    // counts 500 GiB, the next forecast call should see the fresher
    // 1 GiB free figure and bail.
    let refreshed_free = 1024u64 * 1024 * 1024; // 1 GiB
    let (decision, total) =
        batch_forecast_decision(1024 * 1024, Some(refreshed_free), &queued, &warn);
    assert_eq!(
        decision,
        BatchForecast::Bail,
        "after re-snapshot to 1 GiB free, queued 500 GiB+ must trigger Bail"
    );
    assert!(total > refreshed_free, "queued total dwarfs refreshed free");
}

// ── add_asset_album retry tests ─────────────────────────────────

/// Membership store stub whose `add_asset_album` returns `LockPoisoned` for the
/// first `fail_first` calls, then succeeds. Tracks total call count so
/// tests can pin the retry-attempt accounting. Other methods panic
/// because the test path never reaches them.
struct AlbumRetryStubDb {
    remaining_failures: AtomicUsize,
    calls: AtomicUsize,
    recorded: std::sync::Mutex<Vec<(String, String, String)>>,
}

impl AlbumRetryStubDb {
    fn new(fail_first: usize) -> Self {
        Self {
            remaining_failures: AtomicUsize::new(fail_first),
            calls: AtomicUsize::new(0),
            recorded: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl MembershipStore for AlbumRetryStubDb {
    async fn add_asset_album(
        &self,
        _library: &str,
        asset_id: &str,
        album_name: &str,
        source: &str,
    ) -> Result<(), StateError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let prev = self.remaining_failures.fetch_sub(1, Ordering::Relaxed);
        if prev > 0 {
            Err(StateError::LockPoisoned("simulated SQLite busy".into()))
        } else {
            self.remaining_failures.store(0, Ordering::Relaxed);
            self.recorded.lock().unwrap().push((
                asset_id.to_string(),
                album_name.to_string(),
                source.to_string(),
            ));
            Ok(())
        }
    }

    async fn get_all_asset_albums(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
        unimplemented!()
    }

    async fn get_all_asset_people(&self, _: &str) -> Result<Vec<(String, String)>, StateError> {
        unimplemented!()
    }
}

/// A single transient `LockPoisoned` from `add_asset_album` must
/// be retried so the album-membership row lands. Pre-fix this caller
/// logged at `warn!` and dropped the row, leaving downstream consumers
/// (EXIF keywords, Immich albums) with incomplete data until the next
/// full enumeration repopulated it.
#[tokio::test]
async fn add_asset_album_retry_recovers_after_one_transient_failure() {
    let stub = AlbumRetryStubDb::new(1); // fail once, succeed on retry
    let result =
        add_asset_album_with_retry(&stub, "PrimarySync", "ASSET_A", "Favorites", "icloud").await;
    assert!(
        result.is_ok(),
        "transient SQLite-busy must be retried, not surfaced as Err"
    );
    let recorded = stub.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1, "exactly one row must be recorded");
    assert_eq!(
        recorded[0],
        ("ASSET_A".into(), "Favorites".into(), "icloud".into())
    );
    assert_eq!(
        stub.calls.load(Ordering::Relaxed),
        2,
        "must hit the DB twice: one fail + one success"
    );
}

/// When failures persist beyond the retry cap, the error must
/// surface so the caller can log at `warn!` (existing behaviour) — a
/// genuinely-wedged DB shouldn't be silently retried forever.
#[tokio::test]
async fn add_asset_album_retry_surfaces_persistent_failure() {
    // Fail more times than the retry cap.
    let stub = AlbumRetryStubDb::new((ADD_ASSET_ALBUM_MAX_RETRIES + 5) as usize);
    let result =
        add_asset_album_with_retry(&stub, "PrimarySync", "ASSET_B", "Trip", "icloud").await;
    assert!(
        result.is_err(),
        "persistent failure must propagate so the caller's warn! fires"
    );
    assert_eq!(
        stub.calls.load(Ordering::Relaxed) as u32,
        ADD_ASSET_ALBUM_MAX_RETRIES,
        "must attempt exactly ADD_ASSET_ALBUM_MAX_RETRIES times before giving up"
    );
    assert!(
        stub.recorded.lock().unwrap().is_empty(),
        "no row must be recorded when all attempts fail"
    );
}

/// A first-call success must not pay the retry cost. Pinning so
/// the retry loop doesn't accidentally turn into "retry on every call".
#[tokio::test]
async fn add_asset_album_retry_no_op_on_first_success() {
    let stub = AlbumRetryStubDb::new(0);
    let result =
        add_asset_album_with_retry(&stub, "PrimarySync", "ASSET_C", "Holiday", "icloud").await;
    assert!(result.is_ok());
    assert_eq!(
        stub.calls.load(Ordering::Relaxed),
        1,
        "first-call success must hit the DB exactly once"
    );
}

#[test]
fn batch_forecast_decision_saturating_mul_never_overflows_warn_threshold() {
    // Near-u64::MAX free values must compute warn_threshold without
    // overflowing and without spuriously warning at tiny totals.
    let queued = AtomicU64::new(0);
    let warn = AtomicBool::new(false);
    let (decision, _total) = batch_forecast_decision(1_000, Some(u64::MAX), &queued, &warn);
    assert_eq!(decision, BatchForecast::Continue);
}

#[test]
fn test_producer_skip_summary_total() {
    let skips = ProducerSkipSummary {
        by_state: 10,
        on_disk: 5,
        ampm_variant: 2,
        by_media_type: 1,
        by_date_range: 0,
        by_live_photo: 0,
        by_filename: 0,
        by_excluded_album: 0,
        duplicates: 3,
        retry_exhausted: 4,
        retry_only: 0,
    };
    assert_eq!(skips.total(), 25);
}

#[test]
fn test_producer_skip_summary_add_assign() {
    let mut a = ProducerSkipSummary {
        by_state: 10,
        on_disk: 5,
        ampm_variant: 2,
        by_media_type: 1,
        by_date_range: 0,
        by_live_photo: 0,
        by_filename: 0,
        by_excluded_album: 0,
        duplicates: 3,
        retry_exhausted: 4,
        retry_only: 0,
    };
    let b = ProducerSkipSummary {
        by_state: 1,
        on_disk: 2,
        ampm_variant: 3,
        by_media_type: 2,
        by_date_range: 1,
        by_live_photo: 1,
        by_filename: 0,
        by_excluded_album: 0,
        duplicates: 5,
        retry_exhausted: 6,
        retry_only: 7,
    };
    a += b;
    assert_eq!(a.by_state, 11);
    assert_eq!(a.on_disk, 7);
    assert_eq!(a.ampm_variant, 5);
    assert_eq!(a.by_media_type, 3);
    assert_eq!(a.by_date_range, 1);
    assert_eq!(a.by_live_photo, 1);
    assert_eq!(a.by_filename, 0);
    assert_eq!(a.by_excluded_album, 0);
    assert_eq!(a.duplicates, 8);
    assert_eq!(a.retry_exhausted, 10);
    assert_eq!(a.retry_only, 7);
    assert_eq!(a.total(), 53);
}

#[test]
fn producer_skip_summary_records_every_filter_reason() {
    let mut skips = ProducerSkipSummary::default();

    skips.record_filter_reason(crate::download::filter::FilterReason::MalformedAsset);
    skips.record_filter_reason(crate::download::filter::FilterReason::ExcludedAlbum);
    skips.record_filter_reason(crate::download::filter::FilterReason::MediaType);
    skips.record_filter_reason(crate::download::filter::FilterReason::LivePhoto);
    skips.record_filter_reason(crate::download::filter::FilterReason::DateRange);
    skips.record_filter_reason(crate::download::filter::FilterReason::Filename);

    assert_eq!(skips.by_filename, 2);
    assert_eq!(skips.by_excluded_album, 1);
    assert_eq!(skips.by_media_type, 1);
    assert_eq!(skips.by_live_photo, 1);
    assert_eq!(skips.by_date_range, 1);
    assert_eq!(skips.total(), 6);
}

#[test]
fn producer_skip_summary_converts_to_public_skip_breakdown() {
    let skips = ProducerSkipSummary {
        by_state: 1,
        on_disk: 2,
        ampm_variant: 3,
        by_media_type: 4,
        by_date_range: 5,
        by_live_photo: 6,
        by_filename: 7,
        by_excluded_album: 8,
        duplicates: 9,
        retry_exhausted: 10,
        retry_only: 11,
    };

    let breakdown = crate::download::SkipBreakdown::from(skips);
    assert_eq!(breakdown.by_state, 1);
    assert_eq!(breakdown.on_disk, 2);
    assert_eq!(breakdown.ampm_variant, 3);
    assert_eq!(breakdown.by_media_type, 4);
    assert_eq!(breakdown.by_date_range, 5);
    assert_eq!(breakdown.by_live_photo, 6);
    assert_eq!(breakdown.by_filename, 7);
    assert_eq!(breakdown.by_excluded_album, 8);
    assert_eq!(breakdown.duplicates, 9);
    assert_eq!(breakdown.retry_exhausted, 10);
    assert_eq!(breakdown.retry_only, 11);
}

#[test]
fn test_producer_skip_summary_default_is_zero() {
    let skips = ProducerSkipSummary::default();
    assert_eq!(skips.total(), 0);
}

/// The producer relies on `AssetDisposition` ordering via `.max()` to
/// pick the highest-priority outcome when an asset has mixed task results.
/// If variant order changes, `.max()` silently picks the wrong winner.
#[test]
fn test_asset_disposition_ordering() {
    use AssetDisposition::{
        AmpmVariant, Forwarded, OnDisk, RetryExhausted, RetryOnly, StateSkip, Unresolved,
    };
    assert!(Forwarded > OnDisk);
    assert!(OnDisk > AmpmVariant);
    assert!(AmpmVariant > StateSkip);
    assert!(StateSkip > RetryExhausted);
    assert!(RetryExhausted > RetryOnly);
    assert!(RetryOnly > Unresolved);

    // .max() picks the highest priority
    assert_eq!(Unresolved.max(Forwarded), Forwarded);
    assert_eq!(OnDisk.max(RetryExhausted), OnDisk);
    assert_eq!(RetryOnly.max(RetryExhausted), RetryExhausted);
}

/// T-11: When the API returns the same asset ID on two different pages,
/// the dedup logic (seen_ids) ensures only one download task is created.
#[test]
fn test_duplicate_asset_id_detected() {
    use rustc_hash::FxHashSet;

    // Simulate the producer's seen_ids dedup logic
    let mut seen_ids: FxHashSet<Box<str>> = FxHashSet::default();

    let asset1_id: Box<str> = "DUPLICATE_ASSET".into();
    let asset2_id: Box<str> = "DUPLICATE_ASSET".into();
    let asset3_id: Box<str> = "UNIQUE_ASSET".into();

    // First occurrence: insert succeeds
    assert!(
        seen_ids.insert(asset1_id),
        "first occurrence should be accepted"
    );

    // Duplicate on second page: insert returns false
    assert!(
        !seen_ids.insert(asset2_id),
        "duplicate asset ID should be detected and skipped"
    );

    // Different asset: insert succeeds
    assert!(
        seen_ids.insert(asset3_id),
        "unique asset should be accepted"
    );

    assert_eq!(seen_ids.len(), 2, "only 2 unique IDs should be tracked");
}

/// End-to-end regression for issue #211. A pending row carried over from
/// a prior sync, combined with a filter that excludes the asset in the
/// current sync, must not have its `last_seen_at` bumped by the producer.
/// Combined with the flipped `promote_pending_to_failed` gate, this
/// guarantees the ghost loop (pending -> failed -> pending) can't recur.
#[tokio::test]
async fn ghost_loop_regression_filtered_pending_asset_survives_sync() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::{MediaType, SqliteStateDb};
    use chrono::TimeZone;
    use futures_util::stream;
    use std::sync::Arc;

    fn ghost_asset() -> PhotoAsset {
        TestPhotoAsset::new("GHOST")
            .filename("ghost.mov")
            .item_type("com.apple.quicktime-movie")
            .orig_file_type("com.apple.quicktime-movie")
            .orig_size(4096)
            .orig_url("http://127.0.0.1:1/ghost.mov")
            .orig_checksum("ck_ghost")
            .build()
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());

    let prior_seen_at = chrono::Utc::now().timestamp() - 86400;
    let record = AssetRecord::new_pending(
        "PrimarySync".into(),
        "GHOST".into(),
        VersionSizeKey::Original,
        "ck_ghost".into(),
        "ghost.mov".into(),
        chrono::Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        None,
        4096,
        MediaType::Video,
    );
    db.upsert_seen(&record).await.unwrap();
    db.backdate_last_seen("GHOST", prior_seen_at);

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.media.videos = false;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let client = reqwest::Client::new();
    let sync_started_at = chrono::Utc::now().timestamp();
    let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(ghost_asset())]);
    stream_and_download_from_stream(
        &client,
        stream1,
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    let pending = db.get_pending().await.unwrap();
    assert_eq!(pending.len(), 1, "asset must remain the only pending row");
    assert_eq!(&*pending[0].id, "GHOST");
    assert_eq!(
        pending[0].last_seen_at.timestamp(),
        prior_seen_at,
        "producer must NOT bump last_seen_at on a filtered asset"
    );

    let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
    assert_eq!(promoted, 0, "filtered asset must not be promoted");

    // Second cycle locks in stability across repeated syncs.
    let sync_2_start = chrono::Utc::now().timestamp();
    let stream2 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(ghost_asset())]);
    stream_and_download_from_stream(
        &client,
        stream2,
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("second sync must complete");
    let promoted2 = db.promote_pending_to_failed(sync_2_start).await.unwrap();
    assert_eq!(promoted2, 0, "second sync must also leave row untouched");

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn producer_plans_distinct_same_path_same_size_assets() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use futures_util::stream;
    use std::sync::Arc;

    let asset_a = TestPhotoAsset::new("SAME_SIZE_A")
        .filename("IMG_0001.JPG")
        .orig_size(5000)
        .orig_url("https://p01.icloud-content.com/a.jpg")
        .orig_checksum("ck_same_size_a")
        .build();
    let asset_b = TestPhotoAsset::new("SAME_SIZE_B")
        .filename("IMG_0001.JPG")
        .orig_size(5000)
        .orig_url("https://p01.icloud-content.com/b.jpg")
        .orig_checksum("ck_same_size_b")
        .build();

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    let config = Arc::new(config);

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![
            Ok::<PhotoAsset, anyhow::Error>(asset_a),
            Ok::<PhotoAsset, anyhow::Error>(asset_b),
        ]),
        &config,
        DownloadControls::dry_run_hidden(),
        2,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("same-size collision planning should complete");

    assert_eq!(result.downloaded, 2, "unexpected result: {result:?}");
    assert!(result.failed.is_empty());
    assert!(
        fs::read_dir(dir.path()).unwrap().next().is_none(),
        "dry-run planning must not create files"
    );
}

/// Explicit metadata refresh updates historical versions before current
/// resolution and filename filters are applied.
#[tokio::test]
async fn refresh_metadata_updates_historical_version_before_filename_filter() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;
    use futures_util::stream;
    use std::sync::Arc;

    fn existing_asset() -> PhotoAsset {
        TestPhotoAsset::new("REFRESH_FILTERED")
            .asset_date(1_700_000_000_123.0)
            .filename("filtered.jpg")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(1234)
            .orig_url("https://p01.icloud-content.com/filtered.jpg")
            .orig_checksum("ck_filtered")
            .build()
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.refresh_metadata = true;
    config.capture_timestamp_repair =
        crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
    config.metadata.set_exif_datetime = true;
    config.filename_exclude =
        Arc::from(vec![glob::Pattern::new("*.jpg").expect("filename pattern")]);
    let config = Arc::new(config);

    let historical_path = dir.path().join("historical-medium.jpg");
    fs::write(
        &historical_path,
        [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ],
    )
    .unwrap();
    let record = crate::test_helpers::TestAssetRecord::new("REFRESH_FILTERED")
        .version_size(crate::state::VersionSizeKey::Medium)
        .checksum("historical-medium")
        .filename("historical-medium.jpg")
        .size(1234)
        .metadata(AssetMetadata {
            title: Some("stale".to_string()),
            metadata_hash: Some("stale-hash".to_string()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "REFRESH_FILTERED",
        "medium",
        &historical_path,
        "sha256",
        None,
    )
    .await
    .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(existing_asset())]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(
        result.downloaded, 0,
        "filename-filtered asset should not be downloaded"
    );
    let rewrites = db.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(rewrites.len(), 1);
    assert_eq!(
        rewrites[0].version_size,
        crate::state::VersionSizeKey::Medium
    );
    assert_ne!(
        rewrites[0].metadata.metadata_hash.as_deref(),
        Some("stale-hash")
    );
    assert_eq!(rewrites[0].created_at, existing_asset().created());
    assert_eq!(rewrites[0].added_at, Some(existing_asset().added_date()));
    assert_eq!(rewrites[0].metadata.title, None);
    let capture_repairs = db
        .get_pending_metadata_rewrites_page_for_queue(
            crate::state::db::MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            10,
        )
        .await
        .unwrap();
    assert_eq!(capture_repairs.len(), 1);
    assert_eq!(
        capture_repairs[0].asset.version_size,
        crate::state::VersionSizeKey::Medium
    );
}

/// #707 review: the pre-plan refresh makes embedded rewrites reachable for
/// a filtered downloaded asset. The drain rewrites the media, so the row
/// must end holding a checksum of the bytes that are now on disk, and the
/// pre-rewrite hash must survive as the provider download checksum.
#[tokio::test]
async fn filtered_embed_rewrite_keeps_stored_checksums_current() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use crate::state::types::AssetMetadata;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.metadata.set_exif_rating = true;
    config.filename_exclude =
        Arc::from(vec![glob::Pattern::new("*.jpg").expect("filename pattern")]);
    let config = Arc::new(config);

    let media_path = dir.path().join("filtered-embed.jpg");
    fs::write(
        &media_path,
        [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xD9,
        ],
    )
    .unwrap();
    let seeded_checksum = crate::download::file::compute_sha256(&media_path)
        .await
        .expect("hash the seeded media");

    // Stored without the favourite, so the provider edit below raises the
    // rating and gives the embedded writer real work to do.
    let record = crate::test_helpers::TestAssetRecord::new("FILTERED_EMBED")
        .checksum("ck_filtered_embed")
        .filename("filtered-embed.jpg")
        .size(1234)
        .metadata(AssetMetadata {
            metadata_hash: Some("stale-hash".to_string()),
            ..AssetMetadata::default()
        })
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "FILTERED_EMBED",
        "original",
        &media_path,
        &seeded_checksum,
        None,
    )
    .await
    .unwrap();

    let edited: PhotoAsset = TestPhotoAsset::new("FILTERED_EMBED")
        .filename("filtered-embed.jpg")
        .item_type("public.jpeg")
        .orig_file_type("public.jpeg")
        .orig_size(1234)
        .orig_url("https://p01.icloud-content.com/filtered-embed.jpg")
        .orig_checksum("ck_filtered_embed")
        .favorite(true)
        .build();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(result.downloaded, 0, "the filter must skip the media task");
    let on_disk = crate::download::file::compute_sha256(&media_path)
        .await
        .expect("hash the rewritten media");
    assert_ne!(
        on_disk, seeded_checksum,
        "precondition: the embedded rewrite must change the media bytes"
    );

    let row = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
    assert_eq!(
        row.local_checksum.as_deref(),
        Some(on_disk.as_str()),
        "the row must record the bytes the rewrite left on disk"
    );
    assert_eq!(
        row.download_checksum.as_deref(),
        Some(seeded_checksum.as_str()),
        "the pre-rewrite hash must survive as the provider download checksum"
    );
}

/// A `PhotoAsset` backed by `MINIMAL_JPEG`, so a metadata write against its
/// file exercises the real writer rather than failing to decode.
#[cfg(feature = "xmp")]
fn jpeg_asset(id: &str, filename: &str, favorite: bool) -> crate::icloud::photos::PhotoAsset {
    TestPhotoAsset::new(id)
        .filename(filename)
        .orig_size(MINIMAL_JPEG.len() as u64)
        .orig_checksum(&format!("ck_{id}"))
        .favorite(favorite)
        .build()
}

/// Seeds a downloaded asset whose on-disk file is a real JPEG.
#[cfg(feature = "xmp")]
async fn seed_downloaded_jpeg_asset(
    db: &Arc<crate::state::SqliteStateDb>,
    config: &DownloadConfig,
    stored: &crate::icloud::photos::PhotoAsset,
) -> crate::download::filter::DerivedPath {
    let derived = derive_expected_paths(stored, config)
        .into_iter()
        .next()
        .unwrap();
    fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
    fs::write(&derived.path, MINIMAL_JPEG).unwrap();
    let record = asset_record_for_derived_path(Arc::from("PrimarySync"), stored, &derived, config);
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        stored.state_id(),
        derived.version_size.as_str(),
        &derived.path,
        "local-checksum",
        None,
    )
    .await
    .unwrap();
    derived
}

/// A stored/edited `PhotoAsset` pair sharing identical bytes but differing
/// provider metadata, which is the shape of a metadata-only iCloud edit.
fn drifted_asset_pair(
    id: &str,
    filename: &str,
) -> (
    crate::icloud::photos::PhotoAsset,
    crate::icloud::photos::PhotoAsset,
) {
    let checksum = format!("ck_{id}");
    let stored = TestPhotoAsset::new(id)
        .filename(filename)
        .orig_size(1000)
        .orig_checksum(&checksum)
        .favorite(true)
        .build();
    let edited = TestPhotoAsset::new(id)
        .filename(filename)
        .orig_size(1000)
        .orig_checksum(&checksum)
        .favorite(false)
        .build();
    (stored, edited)
}

/// Seed a downloaded asset with its file on disk, as a prior sync would
/// have left it, and return the derived path it was recorded against.
async fn seed_downloaded_drift_asset(
    db: &Arc<crate::state::SqliteStateDb>,
    config: &DownloadConfig,
    stored: &crate::icloud::photos::PhotoAsset,
) -> crate::download::filter::DerivedPath {
    let derived = derive_expected_paths(stored, config)
        .into_iter()
        .next()
        .unwrap();
    fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
    fs::write(&derived.path, vec![0u8; 1000]).unwrap();
    let record = asset_record_for_derived_path(Arc::from("PrimarySync"), stored, &derived, config);
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        stored.state_id(),
        derived.version_size.as_str(),
        &derived.path,
        "local-checksum",
        None,
    )
    .await
    .unwrap();
    derived
}

/// #707: with local metadata outputs disabled the catalogue must still be
/// refreshed, and no rewrite may be queued. Backup fidelity of the
/// manifest does not depend on the user opting into sidecars or EXIF.
#[tokio::test]
async fn metadata_drift_without_writers_refreshes_catalogue_and_queues_nothing() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    // No metadata writers enabled.
    let config = Arc::new(config);

    let (stored, edited) = drifted_asset_pair("NOWRITERS", "nowriters.jpg");
    let derived = seed_downloaded_drift_asset(&db, &config, &stored).await;
    let before = fs::metadata(&derived.path).unwrap().len();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0);
    assert_eq!(result.state_write_failures, 0);
    let refreshed = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
    assert!(
        !refreshed.metadata.is_favorite,
        "the catalogue must refresh even with local metadata outputs disabled"
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "no rewrite may be queued when no metadata writer is enabled"
    );
    assert_eq!(
        fs::metadata(&derived.path).unwrap().len(),
        before,
        "local files must be untouched"
    );
}

#[tokio::test]
async fn raw_policy_rendition_metadata_tracks_bytes_across_refresh_and_config_drift() {
    use crate::types::RawPolicy;
    use base64::Engine as _;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    for policy in [RawPolicy::PreferRaw, RawPolicy::PreferJpeg] {
        for adopt_existing in [false, true] {
            let server = crate::start_wiremock_or_skip!();
            let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF\0".as_slice();
            let raw = b"II*\0\x08\0\0\0\0\0".as_slice();
            let (original, alternative) = if policy == RawPolicy::PreferRaw {
                ((jpeg, "public.jpeg"), (raw, "com.adobe.raw-image"))
            } else {
                ((raw, "com.adobe.raw-image"), (jpeg, "public.jpeg"))
            };
            let mut master = json!({"recordName": "RAW_METADATA", "fields": {
                "filenameEnc": {"value": "pair.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalWidth": {"value": 4000},
                "resOriginalHeight": {"value": 3000},
                "resOriginalAltWidth": {"value": 6000},
                "resOriginalAltHeight": {"value": 4500},
            }});
            for (prefix, name, (body, uti)) in [
                ("resOriginal", "/original", original),
                ("resOriginalAlt", "/alternative", alternative),
            ] {
                Mock::given(method("GET"))
                    .and(path(name))
                    .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                    .expect(if adopt_existing { 0 } else { 1 })
                    .mount(&server)
                    .await;
                master["fields"][format!("{prefix}Res")] = json!({"value": {
                    "downloadURL": format!("{}{name}", server.uri()),
                    "size": body.len(),
                    "fileChecksum": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body)),
                }});
                master["fields"][format!("{prefix}FileType")] = json!({"value": uti});
            }
            let fields = json!({"recordName": "asset-RAW_METADATA", "fields": {
                "assetDate": {"value": 1736899200000.0},
            }});
            let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
            let dir = TempDir::new().unwrap();
            let mut config = DownloadConfig::test_default();
            config.directory = Arc::from(dir.path());
            config.raw_policy = policy;
            config.alternative = true;
            config.state_db = Some(db.clone());
            if adopt_existing {
                let asset = PhotoAsset::new(master.clone(), fields.clone())
                    .with_state_record_name(Arc::from("asset-RAW_METADATA"));
                let plan = TaskPlanner::new().plan_asset(&asset, &config).await;
                assert_eq!(plan.tasks.len(), 2);
                for task in plan.tasks {
                    planner::upsert_seen_for_task(db.as_ref(), &config, &asset, &task)
                        .await
                        .unwrap();
                    let body = if task.version_size == VersionSizeKey::Original {
                        alternative.0
                    } else {
                        original.0
                    };
                    fs::create_dir_all(task.download_path.parent().unwrap()).unwrap();
                    fs::write(&task.download_path, body).unwrap();
                }
            }
            let mut initial = Vec::<AssetRecord>::new();
            let mut refreshed = Vec::new();
            for cycle in 0..4 {
                if cycle == 1 {
                    master["fields"]["resOriginalAltWidth"] = json!({"value": 6200});
                }
                if cycle == 2 {
                    // A filtered historical row survives policy drift. Refresh
                    // must identify its bytes even without downloadable URLs.
                    config.raw_policy = RawPolicy::AsIs;
                    config.exclude_asset_ids =
                        Arc::new(["RAW_METADATA".into()].into_iter().collect());
                    for prefix in ["resOriginal", "resOriginalAlt"] {
                        master["fields"][format!("{prefix}Res")]["value"]
                            .as_object_mut()
                            .unwrap()
                            .remove("downloadURL");
                    }
                    master["fields"]["resOriginalAltHeight"] = json!({"value": 4600});
                }
                if cycle == 3 {
                    db.fail_provider_metadata_refresh_for_test();
                }
                let asset = PhotoAsset::new(master.clone(), fields.clone());
                let result = stream_and_download_from_stream(
                    &Client::new(),
                    stream::iter(vec![Ok::<_, anyhow::Error>(asset.clone())]),
                    &Arc::new(config.clone()),
                    DownloadControls::download_hidden(),
                    1,
                    CancellationToken::new(),
                    StreamRuntime::new(None, None),
                )
                .await
                .unwrap();
                assert_eq!(
                    result.state_write_failures, 0,
                    "{policy:?} cycle {cycle}: {result:?}"
                );
                assert!(result.failed.is_empty());
                assert_eq!(
                    result.downloaded,
                    if cycle == 0 && !adopt_existing { 2 } else { 0 }
                );
                let rows = db.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), 2);
                for row in &rows {
                    let (provider_key, body, width, height) =
                        if row.version_size == VersionSizeKey::Original {
                            (
                                VersionSizeKey::Alternative,
                                alternative.0,
                                if cycle == 0 { 6000 } else { 6200 },
                                if cycle < 2 { 4500 } else { 4600 },
                            )
                        } else {
                            (VersionSizeKey::Original, original.0, 4000, 3000)
                        };
                    assert_eq!(
                        (row.metadata.width, row.metadata.height),
                        (Some(width), Some(height))
                    );
                    assert_eq!(
                        row.metadata.metadata_hash,
                        asset.metadata_arc(provider_key).metadata_hash
                    );
                    assert_eq!(
                        row.metadata.metadata_hash,
                        Some(row.metadata.compute_hash())
                    );
                    assert_eq!(fs::read(row.local_path.as_ref().unwrap()).unwrap(), body);
                    if cycle > 0 {
                        let before = initial
                            .iter()
                            .find(|before| before.version_size == row.version_size)
                            .unwrap();
                        assert_eq!(row.local_path, before.local_path);
                        assert_eq!(row.checksum, before.checksum);
                        assert_eq!(row.local_checksum, before.local_checksum);
                        assert_eq!(row.downloaded_at, before.downloaded_at);
                    }
                }
                assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
                let hashes: Vec<_> = rows
                    .iter()
                    .map(|row| row.metadata.metadata_hash.clone())
                    .collect();
                if cycle == 0 {
                    initial = rows;
                }
                if cycle == 2 {
                    refreshed = hashes.clone();
                }
                if cycle == 3 {
                    assert_eq!(hashes, refreshed);
                }
            }
            // Unknown historical bytes retain shared metadata but no measurements.
            db.acquire_lock("allow raw metadata refresh")
                .unwrap()
                .execute_batch("DROP TRIGGER fail_provider_metadata_refresh")
                .unwrap();
            master["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("replacement");
            master["fields"]["resOriginalAltWidth"] = json!({"value": 9999});
            let result = stream_and_download_from_stream(
                &Client::new(),
                stream::iter(vec![Ok::<_, anyhow::Error>(PhotoAsset::new(
                    master, fields,
                ))]),
                &Arc::new(config),
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(result.state_write_failures, 0);
            let rows = db.get_downloaded_page(0, 10).await.unwrap();
            for row in rows {
                if row.version_size == VersionSizeKey::Original {
                    assert_eq!(row.metadata.width, Some(9999));
                } else {
                    assert_eq!(
                        (
                            row.metadata.width,
                            row.metadata.height,
                            row.metadata.duration_secs
                        ),
                        (None, None, None)
                    );
                }
                let before = initial
                    .iter()
                    .find(|before| before.version_size == row.version_size)
                    .unwrap();
                assert_eq!(row.checksum, before.checksum);
                assert_eq!(row.local_checksum, before.local_checksum);
                assert_eq!(row.local_path, before.local_path);
            }
        }
    }
}

#[tokio::test]
async fn filtered_replacement_renditions_capture_shared_metadata_without_guessing() {
    use serde_json::json;

    for (key, prefix) in [
        (VersionSizeKey::LiveOriginal, "resOriginalVidCompl"),
        (VersionSizeKey::Medium, "resJPEGMed"),
        (VersionSizeKey::LiveMedium, "resVidMed"),
        (VersionSizeKey::Original, "resOriginal"),
    ] {
        let mut master = json!({"recordName": "REPLACED", "fields": {
            "filenameEnc": {"value": "replaced.jpg", "type": "STRING"},
            "itemType": {"value": "public.jpeg"},
            "resOriginalRes": {"value": {"size": 1000, "fileChecksum": "original", "downloadURL": "https://p01.icloud-content.com/original"}},
            "resOriginalFileType": {"value": "public.jpeg"},
        }});
        master["fields"][format!("{prefix}Res")] = json!({"value": {
            "size": 1000, "fileChecksum": "previous", "downloadURL": "https://p01.icloud-content.com/unused",
        }});
        master["fields"][format!("{prefix}FileType")] = json!({"value": if key.is_live_photo_motion() { "com.apple.quicktime-movie" } else { "public.jpeg" }});
        master["fields"][format!("{prefix}Width")] = json!({"value": 1920});
        master["fields"][format!("{prefix}Height")] = json!({"value": 1080});
        if key == VersionSizeKey::Original {
            // Equal current RAW/JPEG measurements do not identify historical bytes.
            master["fields"]["resOriginalAltRes"] =
                json!({"value": {"fileChecksum": "alternative"}});
            master["fields"]["resOriginalAltWidth"] = json!({"value": 1920});
            master["fields"]["resOriginalAltHeight"] = json!({"value": 1080});
        }
        let mut fields = json!({"recordName": "asset-REPLACED", "fields": {
            "assetDate": {"value": 1736899200000.0},
            "duration": {"value": 2.3},
            "vidComplDurValue": {"value": 23},
            "vidComplDurScale": {"value": 10},
        }});
        let stored = PhotoAsset::new(master.clone(), fields.clone())
            .with_state_record_name(Arc::from("asset-REPLACED"));
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        if key == VersionSizeKey::Medium {
            config.resolution = crate::types::PhotoResolution::Medium;
        }
        if key == VersionSizeKey::LiveMedium {
            config.live_resolution = crate::types::AssetVersionSize::LiveMedium;
        }
        let derived = derive_expected_paths(&stored, &config)
            .into_iter()
            .find(|path| path.version_size == key)
            .unwrap();
        fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
        let bytes = vec![0u8; 1000];
        fs::write(&derived.path, &bytes).unwrap();
        let local_checksum = crate::download::file::compute_sha256(&derived.path)
            .await
            .unwrap();
        let record =
            asset_record_for_derived_path(Arc::from("PrimarySync"), &stored, &derived, &config);
        assert_eq!(record.metadata.width, Some(1920));
        db.upsert_seen(&record).await.unwrap();
        db.mark_downloaded(
            "PrimarySync",
            stored.state_id(),
            key.as_str(),
            &derived.path,
            &local_checksum,
            None,
        )
        .await
        .unwrap();
        let before = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
        config.exclude_asset_ids = Arc::new(["asset-REPLACED".into()].into_iter().collect());
        master["fields"][format!("{prefix}Res")]["value"]["fileChecksum"] = json!("replacement");
        fields["fields"]["isFavorite"] = json!({"value": 1});
        let mut refreshed_hash = None;
        for cycle in 0..2 {
            if cycle == 1 {
                db.fail_provider_metadata_refresh_for_test();
            }
            let result = stream_and_download_from_stream(
                &Client::new(),
                stream::iter(vec![Ok::<_, anyhow::Error>(PhotoAsset::new(
                    master.clone(),
                    fields.clone(),
                ))]),
                &Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                StreamRuntime::new(None, None),
            )
            .await
            .unwrap();
            assert_eq!(
                result.state_write_failures, 0,
                "{key:?} cycle {cycle}: {result:?}"
            );
            assert_eq!(result.downloaded, 0);
            assert!(result.failed.is_empty());
            let row = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
            assert!(row.metadata.is_favorite);
            assert_eq!(
                (
                    row.metadata.width,
                    row.metadata.height,
                    row.metadata.duration_secs
                ),
                (None, None, None)
            );
            assert_eq!(
                row.metadata.metadata_hash,
                Some(row.metadata.compute_hash())
            );
            assert_eq!(row.local_path, before.local_path);
            assert_eq!(row.status, before.status);
            assert_eq!(row.checksum, before.checksum);
            assert_eq!(row.local_checksum, before.local_checksum);
            assert_eq!(row.download_checksum, before.download_checksum);
            assert_eq!(row.downloaded_at, before.downloaded_at);
            assert_eq!(fs::read(&derived.path).unwrap(), bytes);
            assert_eq!(
                fs::read_dir(derived.path.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
            assert!(db.get_pending().await.unwrap().is_empty());
            assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
            if cycle == 0 {
                refreshed_hash = row.metadata.metadata_hash.clone();
            } else {
                assert_eq!(row.metadata.metadata_hash, refreshed_hash);
            }
        }
    }
}

#[tokio::test]
async fn forced_metadata_refresh_resolves_completed_sibling_with_stale_preload() {
    use crate::types::RawPolicy;
    use base64::Engine as _;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF\0".as_slice();
    let raw = b"II*\0\x08\0\0\0\0\0".as_slice();
    let mut master = json!({"recordName": "STALE_PAIR", "fields": {
        "filenameEnc": {"value": "pair.jpg", "type": "STRING"},
        "itemType": {"value": "public.jpeg"},
        "resOriginalWidth": {"value": 4000},
        "resOriginalHeight": {"value": 3000},
        "resOriginalAltWidth": {"value": 6000},
        "resOriginalAltHeight": {"value": 4500},
    }});
    for (prefix, body, uti) in [
        ("resOriginal", jpeg, "public.jpeg"),
        ("resOriginalAlt", raw, "com.adobe.raw-image"),
    ] {
        master["fields"][format!("{prefix}Res")] = json!({"value": {
            "downloadURL": format!("https://p01.icloud-content.com/{prefix}"),
            "size": body.len(),
            "fileChecksum": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(body)),
        }});
        master["fields"][format!("{prefix}FileType")] = json!({"value": uti});
    }
    let mut fields = json!({"recordName": "asset-STALE_PAIR", "fields": {
        "assetDate": {"value": 1736899200000.0},
    }});
    let asset = PhotoAsset::new(master.clone(), fields.clone())
        .with_state_record_name(Arc::from("asset-STALE_PAIR"));
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.raw_policy = RawPolicy::PreferRaw;
    config.alternative = true;
    config.refresh_metadata = true;
    let plan = TaskPlanner::new().plan_asset(&asset, &config).await;
    assert_eq!(plan.tasks.len(), 2);
    for task in &plan.tasks {
        planner::upsert_seen_for_task(db.as_ref(), &config, &asset, task)
            .await
            .unwrap();
        let body = if task.version_size == VersionSizeKey::Original {
            raw
        } else {
            jpeg
        };
        fs::create_dir_all(task.download_path.parent().unwrap()).unwrap();
        fs::write(&task.download_path, body).unwrap();
        if task.version_size == VersionSizeKey::Original {
            let checksum = crate::download::file::compute_sha256(&task.download_path)
                .await
                .unwrap();
            db.mark_downloaded(
                "PrimarySync",
                asset.state_id(),
                "original",
                &task.download_path,
                &checksum,
                None,
            )
            .await
            .unwrap();
        }
    }
    let stale = preload_download_context(&config).await;
    assert_eq!(db.get_pending().await.unwrap().len(), 1);
    // Reject even an intermediate refresh that uses provider keys instead of
    // the current downloaded checksums. Final-state assertions alone miss it.
    db.acquire_lock("guard swapped metadata")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER guard_swapped_metadata BEFORE UPDATE OF width ON assets
         WHEN (NEW.version_size = 'original' AND NEW.width IS NOT 6000)
           OR (NEW.version_size = 'alternative' AND NEW.width IS NOT 4000)
         BEGIN SELECT RAISE(ABORT, 'wrong rendition metadata'); END;",
        )
        .unwrap();
    for cycle in 0..3 {
        if cycle == 1 {
            config.raw_policy = RawPolicy::AsIs;
            config.media.photos = false;
            fields["fields"]["isFavorite"] = json!({"value": 1});
        }
        let result = stream_and_download_from_stream_with_context(
            &Client::new(),
            stream::iter(vec![Ok::<_, anyhow::Error>(PhotoAsset::new(
                master.clone(),
                fields.clone(),
            ))]),
            &Arc::new(config.clone()),
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::with_context(None, None, Some(stale.clone())),
        )
        .await
        .unwrap();
        assert_eq!(result.state_write_failures, 0, "cycle {cycle}: {result:?}");
        assert_eq!(result.downloaded, 0);
        assert!(result.failed.is_empty());
        assert!(db.get_pending().await.unwrap().is_empty());
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            let task = plan
                .tasks
                .iter()
                .find(|task| task.version_size == row.version_size)
                .unwrap();
            let (width, body) = if row.version_size == VersionSizeKey::Original {
                (6000, raw)
            } else {
                (4000, jpeg)
            };
            assert_eq!(row.metadata.width, Some(width));
            assert_eq!(row.metadata.is_favorite, cycle > 0);
            assert_eq!(row.checksum.as_ref(), task.checksum.as_ref());
            assert_eq!(row.local_path.as_ref(), Some(&task.download_path));
            assert_eq!(fs::read(&task.download_path).unwrap(), body);
            assert_eq!(
                row.metadata.metadata_hash,
                Some(row.metadata.compute_hash())
            );
        }
        assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn live_photo_streaming_rendition_metadata_is_catalogue_only_and_idempotent() {
    use base64::Engine as _;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let server = crate::start_wiremock_or_skip!();
    let still = b"\xff\xd8\xff\xe0\0\x10JFIF\0";
    let motion = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";
    for (name, body, content_type) in [
        ("/live.jpg", still.as_slice(), "image/jpeg"),
        ("/live.MOV", motion.as_slice(), "video/quicktime"),
    ] {
        Mock::given(method("GET"))
            .and(path(name))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(body)
                    .insert_header("content-type", content_type),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let make_asset = |favorite: bool| {
        PhotoAsset::new(
            json!({"recordName": "LIVE_METADATA", "fields": {
                "filenameEnc": {"value": "live.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {
                    "downloadURL": format!("{}/live.jpg", server.uri()),
                    "size": still.len(),
                    "fileChecksum": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(still)),
                }},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resOriginalWidth": {"value": 5712},
                "resOriginalHeight": {"value": 4284},
                "resOriginalVidComplRes": {"value": {
                    "downloadURL": format!("{}/live.MOV", server.uri()),
                    "size": motion.len(),
                    "fileChecksum": base64::engine::general_purpose::STANDARD.encode(Sha256::digest(motion)),
                }},
                "resOriginalVidComplFileType": {"value": "com.apple.quicktime-movie"},
                "resOriginalVidComplWidth": {"value": 1744},
                "resOriginalVidComplHeight": {"value": 1308},
            }}),
            json!({"recordName": "asset-LIVE_METADATA", "fields": {
                "assetDate": {"value": 1736899200000.0},
                "duration": {"value": 0},
                "vidComplDurValue": {"value": 2300000000_u64},
                "vidComplDurScale": {"value": 1000000000},
                "isFavorite": {"value": i64::from(favorite)},
            }}),
        )
    };
    let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);
    let mut initial = Vec::<AssetRecord>::new();
    let mut refreshed_hashes = Vec::new();

    for cycle in 0..3 {
        if cycle == 2 {
            db.fail_provider_metadata_refresh_for_test();
        }
        let asset = make_asset(cycle > 0);
        let result = stream_and_download_from_stream(
            &reqwest::Client::new(),
            stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset.clone())]),
            &config,
            DownloadControls::download_hidden(),
            1,
            CancellationToken::new(),
            StreamRuntime::new(None, None),
        )
        .await
        .unwrap();
        assert!(result.failed.is_empty(), "cycle {cycle}: {result:?}");
        assert_eq!(result.state_write_failures, 0, "cycle {cycle}");
        assert_eq!(result.downloaded, if cycle == 0 { 2 } else { 0 });
        let rows = db.get_downloaded_page(0, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(
            rows[0].metadata.metadata_hash,
            rows[1].metadata.metadata_hash
        );
        for row in &rows {
            let (width, height, duration, body) = match row.version_size {
                VersionSizeKey::Original => (5712, 4284, 0.0, still.as_slice()),
                VersionSizeKey::LiveOriginal => (1744, 1308, 2.3, motion.as_slice()),
                other => panic!("unexpected rendition {other:?}"),
            };
            assert_eq!(
                (
                    row.metadata.width,
                    row.metadata.height,
                    row.metadata.duration_secs
                ),
                (Some(width), Some(height), Some(duration))
            );
            assert_eq!(
                row.metadata.metadata_hash,
                asset.metadata_arc(row.version_size).metadata_hash
            );
            assert_eq!(row.metadata.is_favorite, cycle > 0);
            let media_path = row.local_path.as_ref().unwrap();
            assert_eq!(fs::read(media_path).unwrap(), body);
            assert_eq!(
                fs::read_dir(media_path.parent().unwrap()).unwrap().count(),
                2
            );
            if cycle > 0 {
                let before = initial
                    .iter()
                    .find(|before| before.version_size == row.version_size)
                    .unwrap();
                assert_eq!(row.status, before.status);
                assert_eq!(row.local_path, before.local_path);
                assert_eq!(row.checksum, before.checksum);
                assert_eq!(row.local_checksum, before.local_checksum);
                assert_eq!(row.download_checksum, before.download_checksum);
                assert_ne!(row.metadata.metadata_hash, before.metadata.metadata_hash);
            }
        }
        assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
        let hashes: Vec<_> = rows
            .iter()
            .map(|row| row.metadata.metadata_hash.clone())
            .collect();
        match cycle {
            0 => initial = rows,
            1 => refreshed_hashes = hashes,
            _ => assert_eq!(hashes, refreshed_hashes),
        }
    }
}

/// #707: a downloaded asset whose media task is filtered out must still
/// have its provider metadata applied. The refresh runs before filtering
/// and path planning, so an excluded asset cannot strand a stale
/// catalogue row.
#[tokio::test]
async fn metadata_drift_refreshes_when_media_task_is_filtered() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.set_exif_rating = true;
    config.state_db = Some(db.clone());

    let (stored, edited) = drifted_asset_pair("FILTERED", "filtered.jpg");
    // Filter the asset out of media planning entirely.
    config.exclude_asset_ids = Arc::new(std::iter::once(stored.id().to_string()).collect());
    let config = Arc::new(config);

    seed_downloaded_drift_asset(&db, &config, &stored).await;

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0);
    assert_eq!(result.state_write_failures, 0);
    let refreshed = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
    assert!(
        !refreshed.metadata.is_favorite,
        "a filtered media task must not prevent the provider metadata refresh"
    );
}

/// #707: a photo the provider deleted and the user then restored must
/// still be recognised as already held. The exclusion for deleted rows
/// belongs in the staleness checks, not in the shared downloaded list:
/// filtering that list sends the asset down the fast path, which skips the
/// on-disk check, so the default naming policy files a copy beside the
/// original.
#[tokio::test]
async fn restored_soft_deleted_asset_is_skipped_without_downloading() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let (stored, _) = drifted_asset_pair("RESTORED", "restored.jpg");
    let derived = seed_downloaded_drift_asset(&db, &config, &stored).await;
    db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
        .await
        .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(stored.clone())]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0);
    assert!(
        result.failed.is_empty(),
        "a restored photo must not be queued for download"
    );
    assert_eq!(result.skip_summary.on_disk, 1);
    assert_eq!(
        fs::read_dir(derived.path.parent().unwrap())
            .unwrap()
            .count(),
        1,
        "no duplicate may be written beside the original"
    );
}

/// #707: a restored photo that was also edited must not report unsafe
/// provider state. Its rows cannot be refreshed while the deletion stands,
/// and reporting that every cycle would hold the checkpoint forever.
#[tokio::test]
async fn restored_and_edited_asset_does_not_report_unsafe_state() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.set_exif_rating = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let (stored, edited) = drifted_asset_pair("RESTORED_EDIT", "restored_edit.jpg");
    seed_downloaded_drift_asset(&db, &config, &stored).await;
    db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
        .await
        .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(
        result.state_write_failures, 0,
        "a deleted row that cannot be refreshed is not a durability failure"
    );
    assert_eq!(result.downloaded, 0);
}

/// #707 acceptance criterion 1, sidecar half: the rewrite must run from
/// the refreshed catalogue and land the new value on disk. The direction
/// matters. Un-favouriting clears the rating and writes nothing, so only
/// favouriting proves the fresh value reached the file.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn metadata_drift_writes_fresh_value_into_the_sidecar() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let stored = jpeg_asset("SIDECAR", "sidecar.jpg", false);
    let edited = jpeg_asset("SIDECAR", "sidecar.jpg", true);
    let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0, "a metadata edit must not fetch media");
    assert_eq!(result.exif_failures, 0);

    let sidecar = fs::read_to_string(sidecar_path_for(&derived.path))
        .expect("sidecar written next to the media file");
    assert!(
        sidecar.contains("<xmp:Rating>5</xmp:Rating>"),
        "the sidecar must carry the favourite from the refreshed catalogue: {sidecar}"
    );
}

/// #707: a rewrite marker that predates a provider deletion must not
/// drain against the tombstoned row. Its metadata is frozen at the values
/// held before the deletion, so writing it would publish stale data.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn marker_predating_a_deletion_is_not_drained() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let stored = jpeg_asset("OLDMARKER", "oldmarker.jpg", false);
    let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
    db.record_metadata_write_failure(
        "PrimarySync",
        stored.state_id(),
        derived.version_size.as_str(),
    )
    .await
    .unwrap();
    db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
        .await
        .unwrap();

    stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(jpeg_asset(
            "OLDMARKER",
            "oldmarker.jpg",
            true,
        ))]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert!(
        !sidecar_path_for(&derived.path).exists(),
        "a marker on a deleted row must not write metadata"
    );
}

/// #707: a tombstoned row cannot be refreshed, so it must not be queued
/// for rewrite either. Doing so would publish the stale values the issue
/// exists to stop.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn restored_asset_is_not_rewritten_from_the_stale_catalogue() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let stored = jpeg_asset("TOMBSTONE", "tombstone.jpg", false);
    let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
    db.mark_soft_deleted("PrimarySync", stored.state_id(), None)
        .await
        .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(jpeg_asset(
            "TOMBSTONE",
            "tombstone.jpg",
            true,
        ))]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0);
    assert!(
        !sidecar_path_for(&derived.path).exists(),
        "a tombstoned asset must not have metadata written from its stale row"
    );
    assert!(
        db.get_metadata_retry_markers().await.unwrap().is_empty(),
        "a tombstoned asset must not be queued for a later rewrite either"
    );
}

/// #707: when the catalogue write fails, the prior metadata must survive
/// intact and the failure must reach `state_write_failures`, which is the
/// signal `sync_cycle` uses to hold the zone checkpoint.
#[tokio::test]
async fn metadata_drift_refresh_failure_preserves_state_and_reports_not_durable() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.set_exif_rating = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let (stored, edited) = drifted_asset_pair("DBFAIL", "dbfail.jpg");
    seed_downloaded_drift_asset(&db, &config, &stored).await;

    db.fail_provider_metadata_refresh_for_test();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert!(
        result.state_write_failures > 0,
        "a failed catalogue refresh must be reported as non-durable provider state"
    );
    let unchanged = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
    assert!(
        unchanged.metadata.is_favorite,
        "a failed refresh must leave the prior catalogue metadata intact"
    );
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "a failed refresh must not queue a rewrite, which would publish the stale row"
    );
}

/// The skip helper must compare the same unknown-measurement projection as
/// drift detection. Defer draining so an unnecessary marker stays observable.
#[tokio::test]
async fn unknown_measurements_idle_cycle_does_not_queue_metadata_rewrites() {
    use serde_json::json;

    for checksum in ["", "historical"] {
        let db = Arc::new(crate::state::SqliteStateDb::open_in_memory().unwrap());
        let dir = TempDir::new().unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path());
        config.state_db = Some(db.clone());
        config.metadata.set_exif_rating = true;
        config.file_match_policy = crate::types::FileMatchPolicy::NameId7;
        let asset = PhotoAsset::new(
            json!({"recordName": "UNKNOWN_IDLE", "fields": {
                "filenameEnc": {"value": "idle.jpg", "type": "STRING"},
                "itemType": {"value": "public.jpeg"},
                "resOriginalRes": {"value": {"size": 1000, "fileChecksum": "current", "downloadURL": "https://p01.icloud-content.com/current"}},
                "resOriginalFileType": {"value": "public.jpeg"},
                "resOriginalWidth": {"value": 1920},
                "resOriginalHeight": {"value": 1080},
            }}),
            json!({"recordName": "asset-UNKNOWN_IDLE", "fields": {
                "assetDate": {"value": 1736899200000.0},
                "duration": {"value": 2.3},
            }}),
        ).with_state_record_name(Arc::from("asset-UNKNOWN_IDLE"));
        let derived = derive_expected_paths(&asset, &config).remove(0);
        fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
        let bytes = vec![0u8; 1000];
        fs::write(&derived.path, &bytes).unwrap();
        let mut record =
            asset_record_for_derived_path(Arc::from("PrimarySync"), &asset, &derived, &config);
        record.checksum = checksum.into();
        record.metadata = Arc::new(
            crate::download::filter::metadata_capture(&asset)
                .resolve(VersionSizeKey::Original, checksum),
        );
        db.upsert_seen(&record).await.unwrap();
        let local_checksum = crate::download::file::compute_sha256(&derived.path)
            .await
            .unwrap();
        db.mark_downloaded(
            "PrimarySync",
            asset.state_id(),
            "original",
            &derived.path,
            &local_checksum,
            None,
        )
        .await
        .unwrap();
        assert!(db.get_pending().await.unwrap().is_empty());
        let context = preload_download_context(&config).await;
        assert!(!context.has_provider_metadata_drift(
            "PrimarySync",
            asset.state_id(),
            &crate::download::filter::metadata_capture(&asset)
        ));
        db.fail_provider_metadata_refresh_for_test();
        for _ in 0..2 {
            let result = stream_and_download_from_stream_with_context(
                &Client::new(),
                stream::iter(vec![Ok::<_, anyhow::Error>(asset.clone())]),
                &Arc::new(config.clone()),
                DownloadControls::download_hidden(),
                1,
                CancellationToken::new(),
                StreamRuntime::new(None, None).deferring_metadata_drain(),
            )
            .await
            .unwrap();
            assert_eq!(
                result.state_write_failures, 0,
                "checksum {checksum:?}: {result:?}"
            );
            assert_eq!(result.downloaded, 0);
            assert_eq!(result.skip_summary.on_disk, 1);
            assert!(result.failed.is_empty());
            assert!(db.get_metadata_retry_markers().await.unwrap().is_empty());
            let row = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
            assert_eq!(row.metadata.metadata_hash, record.metadata.metadata_hash);
            assert_eq!(
                (
                    row.metadata.width,
                    row.metadata.height,
                    row.metadata.duration_secs
                ),
                (None, None, None)
            );
            assert_eq!(row.checksum, record.checksum);
            assert_eq!(row.local_checksum.as_deref(), Some(local_checksum.as_str()));
            assert_eq!(row.local_path.as_ref(), Some(&derived.path));
            assert_eq!(fs::read(&derived.path).unwrap(), bytes);
            assert_eq!(
                fs::read_dir(derived.path.parent().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        }
    }
}

/// #707: unchanged provider metadata is a no-op. The failure trigger
/// would turn any attempted catalogue write into an error, so a clean
/// result proves no write was attempted and no rewrite was queued.
#[tokio::test]
async fn unchanged_metadata_performs_no_catalogue_write() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.set_exif_rating = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let (stored, _) = drifted_asset_pair("SAME", "same.jpg");
    seed_downloaded_drift_asset(&db, &config, &stored).await;

    db.fail_provider_metadata_refresh_for_test();

    // Re-enumerate the identical asset: same bytes, same metadata.
    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(stored.clone())]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(
        result.state_write_failures, 0,
        "unchanged metadata must not attempt a catalogue write"
    );
    assert_eq!(result.downloaded, 0);
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty(),
        "unchanged metadata must not queue a rewrite"
    );
}

/// #707: a retry marker must trigger a refresh even when the stored hash
/// matches the provider. `mark_hidden_at_source` sets `is_hidden` without
/// recomputing `metadata_hash`, so after an unhide the hashes agree while
/// the row still reads hidden. Drift alone cannot see that.
#[tokio::test]
async fn retry_marker_refreshes_row_whose_hash_matches_but_columns_are_stale() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.set_exif_rating = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let (visible, _) = drifted_asset_pair("UNHIDE", "unhide.jpg");
    let derived = seed_downloaded_drift_asset(&db, &config, &visible).await;

    // Hidden at source: the column flips, the stored hash does not.
    db.mark_hidden_at_source("PrimarySync", visible.state_id())
        .await
        .unwrap();
    db.record_metadata_write_failure(
        "PrimarySync",
        visible.state_id(),
        derived.version_size.as_str(),
    )
    .await
    .unwrap();
    assert!(
        db.get_downloaded_page(0, 1).await.unwrap()[0]
            .metadata
            .is_hidden,
        "precondition: the stored row reads hidden"
    );

    // The provider now reports it visible again, matching the stored hash.
    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(visible.clone())]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0);
    assert_eq!(result.state_write_failures, 0);
    assert!(
        !db.get_downloaded_page(0, 1).await.unwrap()[0]
            .metadata
            .is_hidden,
        "the retry marker must drive a refresh that clears the stale hidden flag"
    );
}

/// #707: when the queued rewrite cannot complete, its marker must survive
/// for retry and the surviving row must hold the fresh provider metadata,
/// so the retry cannot replay stale values.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn rewrite_failure_retains_marker_holding_fresh_metadata() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let (stored, edited) = drifted_asset_pair("REWRITEFAIL", "rewritefail.jpg");
    seed_downloaded_drift_asset(&db, &config, &stored).await;
    db.fail_metadata_marker_clear_for_test();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(edited)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(
        result.state_write_failures, 0,
        "a local rewrite failure is not a provider-state failure"
    );
    let surviving = db.get_pending_metadata_rewrites(10).await.unwrap();
    assert_eq!(
        surviving.len(),
        1,
        "a failed rewrite must remain visible for retry"
    );
    assert!(
        !surviving[0].metadata.is_favorite,
        "the retained marker must point at the fresh catalogue metadata"
    );

    // A metadata-only edit downloads nothing, so the zero-download branch
    // is the one that has to report the failure.
    assert_eq!(result.downloaded, 0);
    assert!(result.exif_failures > 0);
    let (outcome, stats) = build_download_outcome(
        &reqwest::Client::new(),
        &[],
        &config,
        DownloadControls::download_hidden(),
        result,
        Instant::now(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { .. }),
        "a failed rewrite must not report a clean sync, got {outcome:?}"
    );
    assert!(stats.exif_failures > 0);
    assert_eq!(
        stats.state_write_failures, 0,
        "the checkpoint may still advance once the marker is durable"
    );
}

/// #741: the normal pending-rewrite owner must treat an unparsable
/// third-party sidecar as visible retryable work, not successful output.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn contract_xmp_sidecar_rewrite_requires_stable_input() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let db = Arc::new(
        SqliteStateDb::open(&dir.path().join("state.db"))
            .await
            .unwrap(),
    );
    let download_dir = dir.path().join("downloads");
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(download_dir.as_path());
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let stored = jpeg_asset("UNPARSABLE_SIDECAR", "unparsable.jpg", true);
    let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
    db.record_metadata_write_failure(
        "PrimarySync",
        stored.state_id(),
        derived.version_size.as_str(),
    )
    .await
    .unwrap();
    let sidecar_path = sidecar_path_for(&derived.path);
    let original = b"<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF";
    fs::write(&sidecar_path, original).unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::empty::<Result<PhotoAsset, anyhow::Error>>(),
        &config,
        DownloadControls::download_hidden(),
        0,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0);
    assert_eq!(result.exif_failures, 1);
    assert_eq!(fs::read(&sidecar_path).unwrap(), original);
    assert_eq!(
        db.get_pending_metadata_rewrites(10).await.unwrap().len(),
        1,
        "failed sidecar rewrite must keep its durable marker"
    );

    let (outcome, stats) = build_download_outcome(
        &reqwest::Client::new(),
        &[],
        &config,
        DownloadControls::download_hidden(),
        result,
        Instant::now(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, DownloadOutcome::PartialFailure { .. }));
    assert_eq!(stats.exif_failures, 1);
    assert_eq!(
        serde_json::to_value(&stats).unwrap()["exif_failures"],
        1,
        "sync report statistics must expose the sidecar failure"
    );
}

/// #752: ambiguous bytes retained from a refused sidecar publication must
/// not block the durable marker from succeeding on a later drain.
#[cfg(feature = "xmp")]
#[tokio::test]
async fn retained_sidecar_temp_does_not_block_pending_rewrite_retry() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let db = Arc::new(
        SqliteStateDb::open(&dir.path().join("state.db"))
            .await
            .unwrap(),
    );
    let download_dir = dir.path().join("downloads");
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(download_dir.as_path());
    config.metadata.xmp_sidecar = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let stored = jpeg_asset("RETAINED_SIDECAR_TEMP", "retained.jpg", true);
    let derived = seed_downloaded_jpeg_asset(&db, &config, &stored).await;
    db.record_metadata_write_failure(
        "PrimarySync",
        stored.state_id(),
        derived.version_size.as_str(),
    )
    .await
    .unwrap();

    let retained_path = derived.path.parent().unwrap().join(format!(
        ".kei-xmp-{}-{}.kei-tmp",
        std::process::id(),
        u64::MAX
    ));
    let retained_bytes = b"ambiguous bytes from an earlier publication";
    fs::write(&retained_path, retained_bytes).unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::empty::<Result<PhotoAsset, anyhow::Error>>(),
        &config,
        DownloadControls::download_hidden(),
        0,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .unwrap();

    assert_eq!(result.downloaded, 0);
    assert_eq!(result.exif_failures, 0);
    assert!(
        db.get_pending_metadata_rewrites(10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fs::read(&retained_path).unwrap(), retained_bytes);
    let sidecar = fs::read_to_string(sidecar_path_for(&derived.path)).unwrap();
    assert!(sidecar.contains("<xmp:Rating>5</xmp:Rating>"), "{sidecar}");
}

#[tokio::test]
async fn free_space_forecast_cancel_before_stream_exhaustion_is_partial_failure() {
    use crate::download::{DownloadConfig, DownloadOutcome};
    use crate::icloud::photos::PhotoAsset;
    use crate::retry::RetryConfig;
    use futures_util::stream;
    use std::sync::Arc;

    let dir = TempDir::new().unwrap();
    let oversized = TestPhotoAsset::new("TOO_BIG_FOR_DISK")
        .filename("too-big.jpg")
        .orig_size(8_000_000_000_000)
        .orig_url("https://p01.icloud-content.com/too-big.jpg")
        .orig_checksum("not-valid-base64")
        .build();
    let undiscovered = TestPhotoAsset::new("UNDISCOVERED_AFTER_CANCEL")
        .filename("undiscovered.jpg")
        .orig_url("https://p01.icloud-content.com/undiscovered.jpg")
        .orig_checksum("BAUG")
        .build();
    let stream = stream::iter(vec![
        Ok::<PhotoAsset, anyhow::Error>(oversized),
        Ok::<PhotoAsset, anyhow::Error>(undiscovered),
    ]);

    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.retry = RetryConfig {
        max_retries: 0,
        base_delay_secs: 0,
        max_delay_secs: 0,
    };
    let config = Arc::new(config);
    let client = reqwest::Client::new();
    let controls = DownloadControls::download_hidden();

    let streaming_result = stream_and_download_from_stream(
        &client,
        stream,
        &config,
        controls,
        0,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("free-space forecast cancellation should return a streaming result");

    assert_eq!(
        streaming_result.assets_seen, 1,
        "producer should stop before reading the second stream item"
    );
    assert!(
        !streaming_result.enumeration_complete,
        "forecast cancellation must leave enumeration incomplete"
    );

    let (outcome, stats) = build_download_outcome(
        &client,
        &[],
        &config,
        controls,
        streaming_result,
        Instant::now(),
        CancellationToken::new(),
    )
    .await
    .expect("outcome should build after forecast cancellation");

    assert!(
        matches!(outcome, DownloadOutcome::PartialFailure { .. }),
        "forecast cancellation must not report clean success, got {outcome:?}"
    );
    assert!(stats.enumeration_incomplete);
    assert_eq!(
        stats.sync_token_blocked_reason,
        Some(crate::download::PRODUCER_ENUMERATION_INCOMPLETE_REASON)
    );
    assert!(!crate::sync_cycle::should_store_sync_token(&outcome, false));
}
