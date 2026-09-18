use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::download::filter::derive_expected_paths;
use crate::download::pipeline::adoption::asset_record_for_derived_path;
use crate::download::pipeline::outcome::StreamingResult;
use crate::download::pipeline::streaming::{StreamRuntime, stream_and_download_from_stream};
use crate::download::{DownloadConfig, DownloadControls};
use crate::icloud::photos::PhotoAsset;
use crate::state::VersionSizeKey;
use crate::test_helpers::TestPhotoAsset;

/// Producer-side regression for resolving pending rows when the expected
/// file already exists on disk.
///
/// A pending row carried over from a prior interrupted sync, whose
/// new sync sees the expected file at the natural path, must be adopted as
/// downloaded when the file already exists with the same name and size.
/// Otherwise standard sync resets failed assets to pending, full
/// enumeration routes the path collision through a deterministic alternate
/// path, and the same row is promoted back to failed every run.
#[tokio::test]
async fn producer_adopts_pending_on_disk_skip_as_downloaded() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use crate::test_helpers::TestAssetRecord;
    use futures_util::stream;
    use std::sync::Arc;

    fn carryover_asset() -> PhotoAsset {
        TestPhotoAsset::new("STUCK")
            .filename("stuck.jpg")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(1234)
            .orig_url("https://p01.icloud-content.com/stuck.jpg")
            .orig_checksum("ck_stuck")
            .build()
            .with_source_zone(Arc::from("SharedSync-abc"))
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());

    let prior_seen_at = chrono::Utc::now().timestamp() - 86400;
    let record = TestAssetRecord::new("STUCK")
        .library("SharedSync-abc")
        .checksum("ck_stuck")
        .filename("stuck.jpg")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.backdate_last_seen("STUCK", prior_seen_at);

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    config.refresh_metadata = true;
    config.capture_timestamp_repair =
        crate::download::CaptureTimestampRepair::ReplaceWithCaptureLocal;
    config.metadata.set_exif_datetime = true;
    let config = Arc::new(config);

    // Pre-create the on-disk file at the expected natural path. The path
    // layer now emits an identity collision task for this same-size file,
    // so the producer must still adopt the pending row before forwarding.
    let asset = carryover_asset();
    let target_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&target_path, vec![0u8; 1234]).unwrap();

    let client = reqwest::Client::new();
    let sync_started_at = chrono::Utc::now().timestamp();
    let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(carryover_asset())]);
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

    let promoted = db.promote_pending_to_failed(sync_started_at).await.unwrap();
    assert_eq!(
        promoted, 0,
        "on-disk pending row should be resolved before failed promotion"
    );

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);
    let capture_repairs = db
        .get_pending_metadata_rewrites_page_for_queue(
            crate::state::db::MetadataRewriteQueue::CaptureRepair,
            None,
            0,
            1,
        )
        .await
        .unwrap();
    assert_eq!(capture_repairs.len(), 1);
    assert_eq!(capture_repairs[0].asset.id.as_ref(), "STUCK");
}

async fn run_producer_metadata_rewritten_pending_file(
    pending_checksum: &str,
) -> (
    StreamingResult,
    i64,
    Arc<crate::state::SqliteStateDb>,
    TempDir,
    PathBuf,
    String,
) {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::{SqliteStateDb, VersionSizeKey};
    use crate::test_helpers::TestAssetRecord;
    use futures_util::stream;
    use std::sync::Arc;

    fn metadata_rewritten_asset() -> PhotoAsset {
        TestPhotoAsset::new("METADATA_REWRITTEN_PENDING")
            .filename("rewritten.jpg")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(8)
            .orig_url("https://p01.icloud-content.com/rewritten.jpg")
            .orig_checksum("ck_metadata_rewritten_pending")
            .build()
            .with_source_zone(Arc::from("SharedSync-abc"))
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let asset = metadata_rewritten_asset();
    let target_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&target_path, b"shorter").unwrap();
    let local_checksum = crate::download::file::compute_sha256(&target_path)
        .await
        .expect("hash metadata-rewritten file");

    let record = TestAssetRecord::new(asset.state_id())
        .library("SharedSync-abc")
        .checksum(pending_checksum)
        .filename("rewritten.jpg")
        .size(8)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "SharedSync-abc",
        asset.state_id(),
        VersionSizeKey::Original.as_str(),
        &target_path,
        &local_checksum,
        Some("download-checksum-before-metadata"),
    )
    .await
    .unwrap();
    db.mark_failed(
        "SharedSync-abc",
        asset.state_id(),
        VersionSizeKey::Original.as_str(),
        "prior false truncation",
    )
    .await
    .unwrap();
    db.prepare_for_retry(
        Some("SharedSync-abc"),
        crate::state::RetryErrorRetention::Clear,
    )
    .await
    .unwrap();

    let client = reqwest::Client::new();
    let sync_started_at = chrono::Utc::now().timestamp();
    let assets = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(
        metadata_rewritten_asset(),
    )]);
    let result = stream_and_download_from_stream(
        &client,
        assets,
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must process metadata-rewritten pending file");

    (
        result,
        sync_started_at,
        db,
        dir,
        target_path,
        local_checksum,
    )
}

#[tokio::test]
async fn producer_adopts_smaller_metadata_rewritten_pending_file() {
    let (result, sync_started_at, db, _dir, target_path, local_checksum) =
        run_producer_metadata_rewritten_pending_file("ck_metadata_rewritten_pending").await;

    assert_eq!(
        db.promote_pending_to_failed(sync_started_at).await.unwrap(),
        0
    );
    assert!(result.failed.is_empty());
    assert_eq!(fs::read(&target_path).unwrap(), b"shorter");
    let downloaded = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(downloaded.len(), 1);
    assert_eq!(
        downloaded[0].local_path.as_deref(),
        Some(target_path.as_path())
    );
    assert_eq!(
        downloaded[0].local_checksum.as_deref(),
        Some(local_checksum.as_str())
    );
    assert_eq!(
        downloaded[0].download_checksum.as_deref(),
        Some("download-checksum-before-metadata")
    );
}

#[tokio::test]
async fn producer_does_not_adopt_metadata_rewritten_file_after_provider_change() {
    let (result, _, db, _dir, target_path, _) =
        run_producer_metadata_rewritten_pending_file("old-provider-checksum").await;

    assert_eq!(result.downloaded, 0);
    assert_eq!(result.failed.len(), 1);
    assert_eq!(fs::read(&target_path).unwrap(), b"shorter");
    assert!(db.get_downloaded_page(0, 10).await.unwrap().is_empty());
    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].checksum.as_ref(), "ck_metadata_rewritten_pending");
}

#[tokio::test]
async fn pending_same_size_collision_does_not_adopt_other_assets_bare_file() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use crate::test_helpers::TestAssetRecord;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let asset = TestPhotoAsset::new("SAME_SIZE_B")
        .filename("IMG_0001.JPG")
        .orig_size(5000)
        .orig_url("https://p01.icloud-content.com/b.jpg")
        .orig_checksum("ck_same_size_b")
        .build();

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let bare_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    let identity_path = identity_suffixed_path_for(&bare_path, asset.id());
    fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    fs::write(&bare_path, vec![1u8; 5000]).unwrap();

    let pending = TestAssetRecord::new(asset.id())
        .checksum("ck_same_size_b")
        .filename(
            identity_path
                .file_name()
                .and_then(|name| name.to_str())
                .expect("identity path must have UTF-8 filename"),
        )
        .size(5000)
        .build();
    db.upsert_seen(&pending).await.unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(
        result.skip_summary.on_disk, 0,
        "pending collision row must not adopt another asset's bare file"
    );
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.downloaded, 0);
    assert_eq!(summary.failed, 1);
}

/// v5 metadata backfill regression: a previously downloaded row with a
/// NULL metadata_hash can hit the producer's on-disk-skip branch when
/// the file already exists. That branch must refresh metadata for the
/// existing downloaded row; otherwise the "one-time after upgrade"
/// backfill notice repeats forever.
#[tokio::test]
async fn on_disk_skip_backfills_downloaded_row_metadata_hash() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    fn existing_asset() -> PhotoAsset {
        TestPhotoAsset::new("BACKFILL")
            .filename("backfill.jpg")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(1234)
            .orig_url("https://p01.icloud-content.com/backfill.jpg")
            .orig_checksum("ck_backfill")
            .build()
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let asset = existing_asset();
    let target_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&target_path, vec![0u8; 1234]).unwrap();

    let record = crate::test_helpers::TestAssetRecord::new("BACKFILL")
        .checksum("ck_backfill")
        .filename("backfill.jpg")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "BACKFILL",
        "original",
        &target_path,
        "sha256",
        None,
    )
    .await
    .unwrap();
    db.clear_metadata_hash_for_test("PrimarySync", "BACKFILL", "original");
    assert!(db.has_downloaded_without_metadata_hash().await.unwrap());

    let client = reqwest::Client::new();
    let assets = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(existing_asset())]);
    let result = stream_and_download_from_stream(
        &client,
        assets,
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
        "existing file should not be re-downloaded"
    );
    assert!(
        !db.has_downloaded_without_metadata_hash().await.unwrap(),
        "on-disk skip must backfill metadata_hash for existing downloaded rows"
    );
}

/// #674 regression: a metadata-only edit drifts the source hash from the
/// stored hash. On the on-disk skip the catalogue must be refreshed to the
/// edited source metadata (not left stale) without re-downloading, so a
/// queued rewrite applies corrected values instead of replaying old ones.
#[tokio::test]
async fn on_disk_skip_refreshes_catalogue_on_metadata_drift() {
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

    // Downloaded while a favourite; the source has since dropped it.
    let stored: PhotoAsset = TestPhotoAsset::new("DRIFT")
        .filename("drift.jpg")
        .orig_size(1000)
        .orig_checksum("ck_drift")
        .favorite(true)
        .build();
    let edited: PhotoAsset = TestPhotoAsset::new("DRIFT")
        .filename("drift.jpg")
        .orig_size(1000)
        .orig_checksum("ck_drift")
        .favorite(false)
        .build();

    let derived = derive_expected_paths(&stored, config.as_ref())
        .into_iter()
        .next()
        .unwrap();
    fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
    fs::write(&derived.path, vec![0u8; 1000]).unwrap();
    let seeded_checksum = crate::download::file::compute_sha256(&derived.path)
        .await
        .unwrap();
    let record =
        asset_record_for_derived_path(Arc::from("PrimarySync"), &stored, &derived, &config);
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        stored.state_id(),
        derived.version_size.as_str(),
        &derived.path,
        &seeded_checksum,
        None,
    )
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

    assert_eq!(result.downloaded, 0, "unchanged bytes must not re-download");
    let refreshed = db.get_downloaded_page(0, 1).await.unwrap().remove(0);
    assert!(
        !refreshed.metadata.is_favorite,
        "the on-disk skip must refresh the stale catalogue to the edited source metadata"
    );
    let queued = db.get_pending_metadata_rewrites(10).await.unwrap();
    assert!(
        queued.is_empty(),
        "the queued rewrite must run from the fresh catalogue and clear its marker"
    );
    assert_eq!(result.exif_failures, 0);
    assert_eq!(
        fs::read_dir(derived.path.parent().unwrap())
            .unwrap()
            .count(),
        1,
        "the refresh must not create a suffixed duplicate"
    );
}

/// Data-sacred regression for the trust-state fast-skip removal.
///
/// When the state DB says an asset is `downloaded` and the config_hash
/// matches the prior sync, the producer must still verify the file is on
/// disk. A user-deleted file must be forwarded for re-download, not
/// fast-skipped on the strength of the DB row alone.
///
/// Setup mirrors the prior failure mode: stored config_hash matches the
/// current config (so any past trust-state gate would activate), the row
/// is `downloaded` with a matching checksum, and the file is absent.
/// Asserts `result.failed` contains the asset (download was attempted
/// against a dead URL), proving the producer did not skip via state.
#[tokio::test]
async fn deleted_downloaded_file_is_forwarded_not_state_skipped() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    fn deleted_asset() -> PhotoAsset {
        TestPhotoAsset::new("DELETED")
            .filename("deleted.jpg")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(1234)
            // Allowlisted CDN host so the URL passes version validation
            // and the asset reaches the download phase; the non-base64
            // checksum below makes the download fail before any network
            // I/O, which surfaces as a `failed` row in the state DB.
            .orig_url("https://p01.icloud-content.com/deleted.jpg")
            .orig_checksum("ck_deleted")
            .build()
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let asset = deleted_asset();

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let target_path = crate::download::paths::local_download_path(
        &config.directory,
        &config.folder_structure,
        &asset.created_local(),
        "deleted.jpg",
        None,
    );
    let record = crate::test_helpers::TestAssetRecord::new("DELETED")
        .checksum("ck_deleted")
        .filename("deleted.jpg")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "DELETED",
        "original",
        &target_path,
        "ck_deleted",
        None,
    )
    .await
    .unwrap();
    let config_hash = crate::download::hash_download_config(&config);
    db.set_metadata("config_hash", &config_hash).await.unwrap();

    assert!(!target_path.exists(), "target file must not exist");

    let client = reqwest::Client::new();
    let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(deleted_asset())]);
    let result = stream_and_download_from_stream(
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

    // by_state was the counter on the now-removed trust-state fast-skip;
    // a non-zero value here would mean the gate was reintroduced. A
    // renamed-counter regression would slip past this alone, so we also
    // assert the download phase ran (failed row below).
    assert_eq!(
        result.skip_summary.by_state, 0,
        "deleted-but-DB-downloaded asset must not be state-skipped"
    );
    let failed = db.get_failed().await.unwrap();
    assert_eq!(
        failed.len(),
        1,
        "deleted file must be forwarded for re-download (which fails against the dead URL)"
    );
    assert_eq!(&*failed[0].id, "DELETED");
}

async fn assert_metadata_mutated_downloaded_file_is_not_redownloaded(on_disk_size: usize) {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    fn asset() -> PhotoAsset {
        TestPhotoAsset::new("METADATA_MUTATED")
            .filename("IMG_4123.JPG")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(1234)
            // Allowlisted CDN host so a regression reaches the download
            // phase; the non-base64 checksum then fails locally without
            // doing network I/O.
            .orig_url("https://p01.icloud-content.com/IMG_4123.JPG")
            .orig_checksum("ck_metadata_mutated")
            .build()
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let existing_asset = asset();

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let target_path = crate::download::paths::local_download_path(
        &config.directory,
        &config.folder_structure,
        &existing_asset.created_local(),
        "IMG_4123.JPG",
        None,
    );
    fs::create_dir_all(target_path.parent().unwrap()).unwrap();
    fs::write(&target_path, vec![0u8; on_disk_size]).unwrap();
    let local_checksum = crate::download::file::compute_sha256(&target_path)
        .await
        .expect("hash metadata-mutated file");

    let record = crate::test_helpers::TestAssetRecord::new("METADATA_MUTATED")
        .checksum("ck_metadata_mutated")
        .filename("IMG_4123.JPG")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "METADATA_MUTATED",
        "original",
        &target_path,
        &local_checksum,
        Some("download_checksum_before_metadata_write"),
    )
    .await
    .unwrap();

    let client = reqwest::Client::new();
    let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset())]);
    let result = stream_and_download_from_stream(
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

    assert_eq!(result.downloaded, 0, "asset must not be re-downloaded");
    assert!(
        result.failed.is_empty(),
        "state-backed current path should skip before the dead URL reaches the download phase"
    );
    assert!(
        !target_path.with_file_name("IMG_4123-1234.JPG").exists(),
        "sync must not create a size-dedup duplicate"
    );
    let failed = db.get_failed().await.unwrap();
    assert!(
        failed.is_empty(),
        "metadata-mutated downloaded file should remain downloaded, not failed"
    );
}

#[tokio::test]
async fn metadata_mutated_downloaded_file_is_not_size_dedup_redownloaded() {
    assert_metadata_mutated_downloaded_file_is_not_redownloaded(1500).await;
}

/// Metadata embedding can legitimately make the local file smaller after
/// the downloaded bytes were verified. A later sync must prove the current
/// bytes from the DB row instead of downloading a `-<size>` duplicate.
#[tokio::test]
async fn smaller_metadata_mutated_downloaded_file_is_not_redownloaded() {
    assert_metadata_mutated_downloaded_file_is_not_redownloaded(1200).await;
}

fn identity_suffixed_path_for(bare_path: &Path, asset_id: &str) -> PathBuf {
    let filename = bare_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("test path must have a UTF-8 filename");
    bare_path.with_file_name(crate::download::paths::insert_asset_identity_suffix(
        filename, asset_id,
    ))
}

fn suffixed_collision_asset(id: &str, checksum: &str) -> PhotoAsset {
    TestPhotoAsset::new(id)
        .filename("IMG_1816.HEIC")
        .item_type("public.heic")
        .orig_file_type("public.heic")
        .orig_size(1234)
        .orig_url("https://p01.icloud-content.com/IMG_1816.HEIC")
        .orig_checksum(checksum)
        .build()
}

fn suffixed_ampm_collision_asset(id: &str, checksum: &str) -> PhotoAsset {
    TestPhotoAsset::new(id)
        .filename("Screenshot 2025-01-14 at 1.40.01\u{202F}PM.PNG")
        .item_type("public.png")
        .orig_file_type("public.png")
        .orig_size(1234)
        .orig_url("https://p01.icloud-content.com/Screenshot.PNG")
        .orig_checksum(checksum)
        .build()
}

fn live_photo_collision_asset(id: &str) -> PhotoAsset {
    TestPhotoAsset::new(id)
        .filename("IMG_0001.HEIC")
        .item_type("public.heic")
        .orig_file_type("public.heic")
        .orig_size(2000)
        .orig_url("https://p01.icloud-content.com/IMG_0001.HEIC")
        .orig_checksum("heic_ck")
        .live_photo(
            "https://p01.icloud-content.com/IMG_0001.MOV",
            "mov_ck",
            3000,
        )
        .build()
}

fn path_for_filename(config: &DownloadConfig, asset: &PhotoAsset, filename: &str) -> PathBuf {
    crate::download::paths::local_download_path(
        &config.directory,
        &config.folder_structure,
        &asset.created_local(),
        filename,
        None,
    )
}

async fn record_downloaded_test_version(
    db: &crate::state::SqliteStateDb,
    asset: &PhotoAsset,
    version_size: VersionSizeKey,
    checksum: &str,
    size: u64,
    path: &Path,
) {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("test path must have a UTF-8 filename");
    let record = crate::test_helpers::TestAssetRecord::new(asset.id())
        .checksum(checksum)
        .filename(filename)
        .size(size)
        .version_size(version_size)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        asset.id(),
        version_size.as_str(),
        path,
        "local_checksum",
        None,
    )
    .await
    .unwrap();
}

/// Regression for #594: a downloaded asset stored at an identity-suffixed
/// collision path must be matched by its recorded state path, not only by
/// the bare derived path.
#[tokio::test]
async fn suffixed_downloaded_file_is_on_disk_skipped() {
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

    let asset_b = suffixed_collision_asset("SUFFIXED_B", "ck_suffixed_b");
    let bare_path = crate::download::filter::expected_paths_for(&asset_b, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    let suffixed_path = identity_suffixed_path_for(&bare_path, asset_b.id());
    fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    fs::write(&bare_path, vec![0u8; 1234]).unwrap();
    fs::write(&suffixed_path, vec![1u8; 1234]).unwrap();

    let record = crate::test_helpers::TestAssetRecord::new("SUFFIXED_B")
        .checksum("ck_suffixed_b")
        .filename("IMG_1816.HEIC")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "SUFFIXED_B",
        "original",
        &suffixed_path,
        "local_checksum_for_suffixed_file",
        None,
    )
    .await
    .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset_b)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(result.downloaded, 0, "asset must not be re-downloaded");
    assert_eq!(
        result.skip_summary.on_disk, 1,
        "suffixed downloaded file must count as an on-disk skip"
    );
    assert!(result.failed.is_empty());
    assert!(
        !identity_suffixed_path_for(&bare_path, "SUFFIXED_B-2").exists(),
        "sync must not create an ordinal duplicate for the same asset"
    );
    let failed = db.get_failed().await.unwrap();
    assert!(failed.is_empty(), "suffixed file should remain downloaded");
}

#[tokio::test]
async fn live_photo_motion_with_size_suffixed_primary_stem_is_on_disk_skipped() {
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

    let asset = live_photo_collision_asset("LIVE_SIZE_COLLISION");
    let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
    let stored_primary = path_for_filename(&config, &asset, "IMG_0001-2000.HEIC");
    let stored_motion = path_for_filename(&config, &asset, "IMG_0001-2000_HEVC.MOV");
    fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
    fs::write(&bare_primary, vec![9u8; 1111]).unwrap();
    fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
    fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::Original,
        "heic_ck",
        2000,
        &stored_primary,
    )
    .await;
    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::LiveOriginal,
        "mov_ck",
        3000,
        &stored_motion,
    )
    .await;

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
    .expect("sync must complete");

    assert_eq!(result.downloaded, 0, "Live Photo must not be re-downloaded");
    assert_eq!(
        result.skip_summary.on_disk, 1,
        "asset should count as an on-disk skip when both paths are already present"
    );
    assert!(result.failed.is_empty());
    assert!(
        !path_for_filename(
            &config,
            &asset,
            "IMG_0001-LIVE_SIZE_COLLISION_HEVC-LIVE_SIZE_COLLISION-2.MOV"
        )
        .exists(),
        "sync must not create a compounding motion duplicate"
    );
}

#[tokio::test]
async fn live_photo_motion_with_identity_suffixed_primary_stem_is_on_disk_skipped() {
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

    let asset = live_photo_collision_asset("LIVE_ID_COLLISION");
    let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
    let stored_primary = path_for_filename(&config, &asset, "IMG_0001-LIVE_ID_COLLISION.HEIC");
    let stored_motion = path_for_filename(
        &config,
        &asset,
        "IMG_0001-LIVE_ID_COLLISION_HEVC-LIVE_ID_COLLISION-2.MOV",
    );
    fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
    fs::write(&bare_primary, vec![9u8; 2000]).unwrap();
    fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
    fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::Original,
        "heic_ck",
        2000,
        &stored_primary,
    )
    .await;
    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::LiveOriginal,
        "mov_ck",
        3000,
        &stored_motion,
    )
    .await;

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(result.downloaded, 0, "Live Photo must not be re-downloaded");
    assert_eq!(result.skip_summary.on_disk, 1);
    assert!(result.failed.is_empty());
}

#[tokio::test]
async fn live_photo_motion_with_sanitized_identity_suffix_is_on_disk_skipped() {
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

    let asset = live_photo_collision_asset("LIVE/ID_COLLISION");
    let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
    let stored_primary = path_for_filename(&config, &asset, "IMG_0001-LIVE_ID_COLLISION.HEIC");
    let stored_motion = path_for_filename(
        &config,
        &asset,
        "IMG_0001-LIVE_ID_COLLISION_HEVC-LIVE_ID_COLLISION-22.MOV",
    );
    fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
    fs::write(&bare_primary, vec![9u8; 2000]).unwrap();
    fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
    fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::Original,
        "heic_ck",
        2000,
        &stored_primary,
    )
    .await;
    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::LiveOriginal,
        "mov_ck",
        3000,
        &stored_motion,
    )
    .await;

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
    .expect("sync must complete");

    assert_eq!(
        result.downloaded, 0,
        "Live Photo with sanitized asset-id suffix must not be re-downloaded"
    );
    assert_eq!(result.skip_summary.on_disk, 1);
    assert!(result.failed.is_empty());
    assert!(
        !path_for_filename(
            &config,
            &asset,
            "IMG_0001-LIVE_ID_COLLISION_HEVC-LIVE_ID_COLLISION-23.MOV"
        )
        .exists(),
        "sync must not create the next sanitized ordinal duplicate"
    );
}

#[tokio::test]
async fn live_photo_motion_original_policy_with_primary_collision_is_on_disk_skipped() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use crate::types::LivePhotoMovFilenamePolicy;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.live_photo_mov_filename_policy = LivePhotoMovFilenamePolicy::Original;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let asset = live_photo_collision_asset("LIVE_ORIGINAL_POLICY");
    let bare_primary = path_for_filename(&config, &asset, "IMG_0001.HEIC");
    let stored_primary = path_for_filename(&config, &asset, "IMG_0001-2000.HEIC");
    let stored_motion = path_for_filename(&config, &asset, "IMG_0001-2000.MOV");
    fs::create_dir_all(bare_primary.parent().unwrap()).unwrap();
    fs::write(&bare_primary, vec![9u8; 1111]).unwrap();
    fs::write(&stored_primary, vec![1u8; 2000]).unwrap();
    fs::write(&stored_motion, vec![2u8; 3000]).unwrap();

    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::Original,
        "heic_ck",
        2000,
        &stored_primary,
    )
    .await;
    record_downloaded_test_version(
        db.as_ref(),
        &asset,
        VersionSizeKey::LiveOriginal,
        "mov_ck",
        3000,
        &stored_motion,
    )
    .await;

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(result.downloaded, 0, "Live Photo must not be re-downloaded");
    assert_eq!(result.skip_summary.on_disk, 1);
    assert!(result.failed.is_empty());
}

/// Same state-path family as #594, with the AM/PM whitespace variant that
/// import-existing and normal on-disk probes already treat as equivalent.
#[tokio::test]
async fn ampm_variant_suffixed_downloaded_file_is_on_disk_skipped() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.keep_unicode_in_filenames = true;
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let asset = suffixed_ampm_collision_asset("AMPM_SUFFIXED_B", "ck_ampm_suffixed_b");
    let bare_path = crate::download::filter::expected_paths_for(&asset, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    let regular_space_filename = bare_path
        .file_name()
        .and_then(|name| name.to_str())
        .expect("test path must have a UTF-8 filename")
        .replace('\u{202F}', " ");
    let regular_space_bare_path = bare_path.with_file_name(regular_space_filename);
    let suffixed_path = identity_suffixed_path_for(&regular_space_bare_path, asset.id());
    fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    fs::write(&suffixed_path, vec![1u8; 1234]).unwrap();

    let record = crate::test_helpers::TestAssetRecord::new("AMPM_SUFFIXED_B")
        .checksum("ck_ampm_suffixed_b")
        .filename("Screenshot 2025-01-14 at 1.40.01\u{202F}PM.PNG")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "AMPM_SUFFIXED_B",
        "original",
        &suffixed_path,
        "local_checksum_for_ampm_suffixed_file",
        None,
    )
    .await
    .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(result.downloaded, 0, "asset must not be re-downloaded");
    assert_eq!(
        result.skip_summary.on_disk, 1,
        "AM/PM-equivalent suffixed file must count as an on-disk skip"
    );
    assert!(result.failed.is_empty());
    let failed = db.get_failed().await.unwrap();
    assert!(failed.is_empty(), "suffixed file should remain downloaded");
}

/// Same #594 path, but the recorded suffixed file is too small. The
/// state-backed skip must not hide local truncation.
#[tokio::test]
async fn truncated_suffixed_downloaded_file_is_forwarded_not_on_disk_skipped() {
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

    let asset_b = suffixed_collision_asset("TRUNCATED_SUFFIXED_B", "ck_truncated_suffixed_b");
    let bare_path = crate::download::filter::expected_paths_for(&asset_b, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    let suffixed_path = identity_suffixed_path_for(&bare_path, asset_b.id());
    fs::create_dir_all(bare_path.parent().unwrap()).unwrap();
    fs::write(&bare_path, vec![0u8; 1234]).unwrap();
    fs::write(&suffixed_path, []).unwrap();

    let record = crate::test_helpers::TestAssetRecord::new("TRUNCATED_SUFFIXED_B")
        .checksum("ck_truncated_suffixed_b")
        .filename("IMG_1816.HEIC")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "TRUNCATED_SUFFIXED_B",
        "original",
        &suffixed_path,
        "local_checksum_for_truncated_suffixed_file",
        None,
    )
    .await
    .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset_b)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(
        result.skip_summary.on_disk, 0,
        "truncated suffixed file must not be counted as an on-disk skip"
    );
    let failed = db.get_failed().await.unwrap();
    assert_eq!(
        failed.len(),
        1,
        "truncated suffixed file must be forwarded for re-download"
    );
    assert_eq!(&*failed[0].id, "TRUNCATED_SUFFIXED_B");
}

/// A state-backed identity-suffixed skip is valid only for the current
/// path family. An existing file recorded under an old directory must not
/// satisfy a new configured target after path-affecting config drift.
#[tokio::test]
async fn old_directory_state_path_is_forwarded_not_on_disk_skipped() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let old_dir = TempDir::new().unwrap();
    let new_dir = TempDir::new().unwrap();
    let mut old_config = DownloadConfig::test_default();
    old_config.directory = Arc::from(old_dir.path());
    let old_config = Arc::new(old_config);
    let mut new_config = DownloadConfig::test_default();
    new_config.directory = Arc::from(new_dir.path());
    new_config.state_db = Some(db.clone());
    let new_config = Arc::new(new_config);

    let asset = suffixed_collision_asset("OLD_DIR_SUFFIXED_B", "ck_old_dir_suffixed_b");
    let old_bare_path = crate::download::filter::expected_paths_for(&asset, old_config.as_ref())
        .first()
        .expect("test asset must derive an old expected path")
        .path
        .clone();
    let old_suffixed_path = identity_suffixed_path_for(&old_bare_path, asset.id());
    fs::create_dir_all(old_bare_path.parent().unwrap()).unwrap();
    fs::write(&old_suffixed_path, vec![1u8; 1234]).unwrap();

    let new_bare_path = crate::download::filter::expected_paths_for(&asset, new_config.as_ref())
        .first()
        .expect("test asset must derive a new expected path")
        .path
        .clone();
    assert!(
        !new_bare_path.exists(),
        "new configured target must be absent"
    );

    let record = crate::test_helpers::TestAssetRecord::new("OLD_DIR_SUFFIXED_B")
        .checksum("ck_old_dir_suffixed_b")
        .filename("IMG_1816.HEIC")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "OLD_DIR_SUFFIXED_B",
        "original",
        &old_suffixed_path,
        "local_checksum_for_old_suffixed_file",
        None,
    )
    .await
    .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &new_config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(
        result.skip_summary.on_disk, 0,
        "old directory state path must not count as a current on-disk skip"
    );
    let failed = db.get_failed().await.unwrap();
    assert_eq!(
        failed.len(),
        1,
        "missing new target must be forwarded for re-download"
    );
    assert_eq!(&*failed[0].id, "OLD_DIR_SUFFIXED_B");
}

/// Capture-local derivation can move an asset into a different date
/// folder than a host-local rendering chose. The stored path is then not
/// a current derived path, so a full sweep forwards the asset for
/// download into its capture-local folder, and the earlier copy is left
/// untouched because kei never deletes local media.
#[tokio::test]
async fn capture_local_date_folder_forwards_and_keeps_host_local_copy() {
    use crate::download::DownloadConfig;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    // 2026-01-31T22:31:59Z captured at +11:00 is 2026-02-01 locally.
    let asset = TestPhotoAsset::new("CAPTURE_OFFSET_MOVE")
        .filename("IMG_9001.JPG")
        .orig_size(1234)
        .orig_url("https://p01.icloud-content.com/IMG_9001.JPG")
        .orig_checksum("ck_capture_offset_move")
        .asset_date(1_769_898_719_000.0)
        .timezone_offset(39_600)
        .build();

    let derived = crate::download::filter::expected_paths_for(&asset, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    assert!(derived.parent().is_some_and(|p| p.ends_with("2026/02/01")));

    let host_local_path = dir.path().join("2026/01/31/IMG_9001.JPG");
    fs::create_dir_all(host_local_path.parent().unwrap()).unwrap();
    fs::write(&host_local_path, vec![7u8; 1234]).unwrap();

    let record = crate::test_helpers::TestAssetRecord::new("CAPTURE_OFFSET_MOVE")
        .checksum("ck_capture_offset_move")
        .filename("IMG_9001.JPG")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "CAPTURE_OFFSET_MOVE",
        "original",
        &host_local_path,
        "local_checksum_for_host_local_file",
        None,
    )
    .await
    .unwrap();

    let result = stream_and_download_from_stream(
        &reqwest::Client::new(),
        stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset)]),
        &config,
        DownloadControls::download_hidden(),
        1,
        CancellationToken::new(),
        StreamRuntime::new(None, None),
    )
    .await
    .expect("sync must complete");

    assert_eq!(
        result.skip_summary.on_disk, 0,
        "a host-local stored path must not satisfy the capture-local target"
    );
    let failed = db.get_failed().await.unwrap();
    assert_eq!(failed.len(), 1, "missing capture-local target is forwarded");
    assert_eq!(&*failed[0].id, "CAPTURE_OFFSET_MOVE");
    assert!(
        host_local_path.exists(),
        "the earlier copy must survive; kei does not delete local media"
    );
}

/// Data-sacred regression for state-backed on-disk skips.
///
/// A downloaded DB row with a matching remote checksum is not enough to
/// trust a too-small local file. If the stored current path exists but is
/// shorter than the API-reported size, the producer must route the asset
/// back through the download phase instead of letting the truncated file
/// mask the real media.
#[tokio::test]
async fn truncated_downloaded_file_is_forwarded_not_on_disk_skipped() {
    use crate::download::DownloadConfig;
    use crate::icloud::photos::PhotoAsset;
    use crate::state::SqliteStateDb;
    use futures_util::stream;
    use std::sync::Arc;

    fn asset() -> PhotoAsset {
        TestPhotoAsset::new("TRUNCATED_DOWNLOADED")
            .filename("IMG_TRUNCATED.JPG")
            .item_type("public.jpeg")
            .orig_file_type("public.jpeg")
            .orig_size(1234)
            // Allowlisted CDN host so the asset reaches the download
            // phase; the non-base64 checksum fails locally without
            // doing network I/O.
            .orig_url("https://p01.icloud-content.com/IMG_TRUNCATED.JPG")
            .orig_checksum("ck_truncated_downloaded")
            .build()
    }

    let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
    let existing_asset = asset();

    let dir = TempDir::new().unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = std::sync::Arc::from(dir.path());
    config.state_db = Some(db.clone());
    let config = Arc::new(config);

    let target_path = crate::download::filter::expected_paths_for(&existing_asset, config.as_ref())
        .first()
        .expect("test asset must derive an expected path")
        .path
        .clone();
    fs::create_dir_all(target_path.parent().unwrap()).unwrap();
    fs::write(&target_path, []).unwrap();

    let record = crate::test_helpers::TestAssetRecord::new("TRUNCATED_DOWNLOADED")
        .checksum("ck_truncated_downloaded")
        .filename("IMG_TRUNCATED.JPG")
        .size(1234)
        .build();
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "TRUNCATED_DOWNLOADED",
        "original",
        &target_path,
        "local_checksum_for_truncated_file",
        None,
    )
    .await
    .unwrap();

    let client = reqwest::Client::new();
    let stream1 = stream::iter(vec![Ok::<PhotoAsset, anyhow::Error>(asset())]);
    let result = stream_and_download_from_stream(
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

    assert_eq!(
        result.downloaded, 0,
        "dead test URL should not produce a successful download"
    );
    assert_eq!(
        result.skip_summary.on_disk, 0,
        "truncated downloaded file must not be counted as an on-disk skip"
    );
    let failed = db.get_failed().await.unwrap();
    assert_eq!(
        failed.len(),
        1,
        "truncated file must be forwarded for re-download (which fails against the dead URL)"
    );
    assert_eq!(&*failed[0].id, "TRUNCATED_DOWNLOADED");
}
