use std::fs;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use super::recover_current_pending_publication;
use crate::download::DownloadConfig;
use crate::download::filter::derive_expected_paths;
use crate::download::pipeline::adoption::{PendingOnDiskAdoption, asset_record_for_derived_path};
use crate::download::planner::TaskPlanner;
use crate::icloud::photos::PhotoAsset;
use crate::state::{RetryErrorRetention, SqliteStateDb, VersionSizeKey};
use crate::test_helpers::TestPhotoAsset;

const MOVIE: &[u8] = b"\0\0\0\x14ftypqt  \0\0\0\0qt  ";

async fn fixture() -> (
    TempDir,
    SqliteStateDb,
    DownloadConfig,
    PhotoAsset,
    std::path::PathBuf,
) {
    let dir = TempDir::new().unwrap();
    let db = SqliteStateDb::open(&dir.path().join("state.db"))
        .await
        .unwrap();
    let asset = TestPhotoAsset::new("L1")
        .filename("IMG_0001.HEIC")
        .item_type("public.heic")
        .orig_file_type("public.heic")
        .orig_url("http://127.0.0.1/unused-still")
        .orig_size(2000)
        .live_photo("http://127.0.0.1/unused", "provider", MOVIE.len() as u64)
        .build();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path().join("media"));
    config.folder_structure = "{album}".to_owned();
    config.album_name = Some(Arc::from("Album"));
    let derived = derive_expected_paths(&asset, &config)
        .into_iter()
        .find(|p| p.version_size == VersionSizeKey::LiveOriginal)
        .unwrap();
    let file = derived
        .path
        .parent()
        .unwrap()
        .join("IMG_0001-L1-28_HEVC-L1-689.MOV");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, MOVIE).unwrap();
    let record = asset_record_for_derived_path(Arc::from("PrimarySync"), &asset, &derived, &config);
    let local = data_encoding::HEXLOWER.encode(&Sha256::digest(MOVIE));
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "L1",
        "live_original",
        &file,
        &local,
        Some(&local),
    )
    .await
    .unwrap();
    db.mark_failed(
        "PrimarySync",
        "L1",
        "live_original",
        "synthetic HTTP 410 for -690",
    )
    .await
    .unwrap();
    db.prepare_for_retry(Some("PrimarySync"), RetryErrorRetention::Clear)
        .await
        .unwrap();
    (dir, db, config, asset, file)
}

fn status(db: &SqliteStateDb) -> String {
    db.acquire_lock("current publication test status")
        .unwrap()
        .query_row(
            "SELECT status FROM assets WHERE id='L1' AND version_size='live_original'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[tokio::test]
async fn current_pending_publication_keeps_legacy_path_and_serializes_concurrent_recovery() {
    let (_dir, db, config, asset, file) = fixture().await;
    let mut first = TaskPlanner::for_download(Some(&db)).await.unwrap();
    let mut second = TaskPlanner::for_download(Some(&db)).await.unwrap();
    let shutdown = CancellationToken::new();
    let (a, b) = tokio::join!(
        recover_current_pending_publication(
            &db,
            &config,
            &asset,
            &mut first,
            VersionSizeKey::LiveOriginal,
            &shutdown
        ),
        recover_current_pending_publication(
            &db,
            &config,
            &asset,
            &mut second,
            VersionSizeKey::LiveOriginal,
            &shutdown
        )
    );
    assert_eq!(
        a.unwrap(),
        Some(PendingOnDiskAdoption::Adopted(file.clone()))
    );
    assert_eq!(
        b.unwrap(),
        Some(PendingOnDiskAdoption::Adopted(file.clone()))
    );
    assert_eq!(status(&db), "downloaded");
    assert_eq!(fs::read(&file).unwrap(), MOVIE);
    assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
    assert_eq!(
        first
            .verified_downloaded_path(&asset, &config, VersionSizeKey::LiveOriginal)
            .await,
        Some(file)
    );
}

#[tokio::test]
async fn current_pending_publication_refuses_changed_bytes_content_and_filename_scope() {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Case {
        SameSizeCorruption,
        Short,
        Missing,
        ProviderChecksum,
        ProviderSize,
        WrongFamily,
        WrongParent,
        HistoricalOnly,
    }

    for case in [
        Case::SameSizeCorruption,
        Case::Short,
        Case::Missing,
        Case::ProviderChecksum,
        Case::ProviderSize,
        Case::WrongFamily,
        Case::WrongParent,
        Case::HistoricalOnly,
    ] {
        let (_dir, db, config, asset, file) = fixture().await;
        match case {
            Case::SameSizeCorruption => {
                let mut bytes = MOVIE.to_vec();
                bytes[0] = 1;
                fs::write(&file, bytes).unwrap();
            }
            Case::Short => {
                fs::write(&file, b"short").unwrap();
            }
            Case::Missing => {
                fs::remove_file(&file).unwrap();
            }
            Case::ProviderChecksum => {
                db.acquire_lock("provider changed")
                    .unwrap()
                    .execute_batch("UPDATE assets SET checksum='new-content'")
                    .unwrap();
            }
            Case::ProviderSize => {
                db.acquire_lock("provider size changed")
                    .unwrap()
                    .execute_batch("UPDATE assets SET size_bytes=size_bytes+1")
                    .unwrap();
            }
            Case::WrongFamily | Case::WrongParent | Case::HistoricalOnly => {
                let target = if case == Case::WrongParent {
                    config
                        .directory
                        .join("OtherAlbum")
                        .join(file.file_name().unwrap())
                } else {
                    file.with_file_name("OTHER.MOV")
                };
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::write(&target, MOVIE).unwrap();
                let conn = db.acquire_lock("different current path").unwrap();
                conn.execute(
                    "UPDATE assets SET local_path=?1",
                    [target.to_str().unwrap()],
                )
                .unwrap();
                if case != Case::HistoricalOnly {
                    conn.execute(
                        "UPDATE asset_metadata_paths SET local_path=?1",
                        [target.to_str().unwrap()],
                    )
                    .unwrap();
                }
            }
        }
        let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
        assert_eq!(
            recover_current_pending_publication(
                &db,
                &config,
                &asset,
                &mut planner,
                VersionSizeKey::LiveOriginal,
                &CancellationToken::new()
            )
            .await
            .unwrap(),
            None,
            "{case:?}"
        );
        assert_eq!(status(&db), "pending", "{case:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn current_pending_publication_refuses_leaf_and_parent_symlinks() {
    for parent in [false, true] {
        let (dir, db, config, asset, file) = fixture().await;
        if parent {
            let real = dir.path().join("real-parent");
            fs::rename(file.parent().unwrap(), &real).unwrap();
            std::os::unix::fs::symlink(&real, file.parent().unwrap()).unwrap();
        } else {
            let real = dir.path().join("real.MOV");
            fs::rename(&file, &real).unwrap();
            std::os::unix::fs::symlink(&real, &file).unwrap();
        }
        let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
        assert!(
            recover_current_pending_publication(
                &db,
                &config,
                &asset,
                &mut planner,
                VersionSizeKey::LiveOriginal,
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(status(&db), "pending");
        assert_eq!(fs::read(&file).unwrap(), MOVIE);
    }
}

#[tokio::test]
async fn current_pending_publication_cancel_while_waiting_keeps_file_and_state() {
    let (_dir, db, config, asset, file) = fixture().await;
    let guard = crate::download::file::lock_download_destination(&file, &CancellationToken::new())
        .await
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
    let shutdown = CancellationToken::new();
    let mut recovery = Box::pin(recover_current_pending_publication(
        &db,
        &config,
        &asset,
        &mut planner,
        VersionSizeKey::LiveOriginal,
        &shutdown,
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), &mut recovery)
            .await
            .is_err()
    );
    shutdown.cancel();
    assert!(recovery.await.is_err());
    drop(guard);
    assert_eq!(status(&db), "pending");
    assert_eq!(fs::read(&file).unwrap(), MOVIE);
}

#[tokio::test]
async fn current_pending_publication_failed_finalization_blocks_download_fallback() {
    let (_dir, db, config, asset, file) = fixture().await;
    db.acquire_lock("inject recovery write failure")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_recovery BEFORE UPDATE ON assets WHEN NEW.status='downloaded' \
         BEGIN SELECT RAISE(ABORT,'synthetic finalization failure'); END;",
        )
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
    assert_eq!(
        recover_current_pending_publication(
            &db,
            &config,
            &asset,
            &mut planner,
            VersionSizeKey::LiveOriginal,
            &CancellationToken::new()
        )
        .await
        .unwrap(),
        Some(PendingOnDiskAdoption::StateWriteFailed(file.clone()))
    );
    assert_eq!(status(&db), "pending");
    assert_eq!(fs::read(&file).unwrap(), MOVIE);
}

#[tokio::test]
async fn current_pending_publication_managed_handover_keeps_retry_debt() {
    let (_dir, db, config, asset, file) = fixture().await;
    let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
    db.acquire_lock("begin managed handover after planner snapshot").unwrap().execute(
        "INSERT INTO primary_layout_claims(path_key,family,operation,library,child,version,provider_checksum,native_path) \
         VALUES(?1,'managed-family','pending-handover','PrimarySync','L1','live_original','provider',X'00')",
        [crate::state::db::primary_layout::path_key(&file).unwrap()],
    ).unwrap();
    for _ in 0..2 {
        let result = recover_current_pending_publication(
            &db,
            &config,
            &asset,
            &mut planner,
            VersionSizeKey::LiveOriginal,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            Some(PendingOnDiskAdoption::StateWriteFailed(file.clone()))
        );
        assert_eq!(status(&db), "pending");
        assert_eq!(fs::read(&file).unwrap(), MOVIE);
        assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
    }
}
#[tokio::test]
async fn managed_pending_plan_keeps_current_publication_with_its_layout_owner() {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig};
    use crate::state::db::DownloadContextStateStore as _;
    use crate::test_helpers::{MockPhotosSession, mock_photo_records_for_zone_with_filename};
    use crate::types::EditedNaming;
    use serde_json::json;

    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
    let provider_records =
        mock_photo_records_for_zone_with_filename("L1", "PrimarySync", "IMG_0001.JPG");
    let asset = PhotoAsset::new(provider_records[0].clone(), provider_records[1].clone())
        .with_source_zone(Arc::from("PrimarySync"))
        .with_state_record_name(Arc::from("asset-L1"));
    let provider = MockPhotosSession::new()
        .ok(json!({"records": [provider_records[1].clone()]}))
        .ok(json!({"records": provider_records.clone()}));
    let album = PhotoAlbum::new(
        PhotoAlbumConfig {
            params: Arc::default(),
            service_endpoint: Arc::from("https://example.com"),
            name: Arc::from("TestAlbum"),
            list_type: Arc::from("CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted"),
            obj_type: Arc::from("CPLAssetByAssetDateWithoutHiddenOrDeleted"),
            query_filter: None,
            page_size: 100,
            zone_id: Arc::new(json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"})),
            retry_config: crate::retry::RetryConfig::default(),
            container_id: None,
            cross_zone_sources: Vec::new(),
        },
        Box::new(provider),
    );
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path().join("media"));
    config.folder_structure = String::new();
    config.edited = true;
    config.edited_naming = EditedNaming::Primary;
    config.primary_layout_active = true;
    config.primary_layout_source = Some(Arc::new(album));
    config.state_db = Some(db.clone());
    let derived = derive_expected_paths(&asset, &config)
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Original)
        .unwrap();
    let file = derived.path.clone();
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    let bytes = vec![1_u8; 1024];
    fs::write(&file, &bytes).unwrap();
    let record = asset_record_for_derived_path(Arc::from("PrimarySync"), &asset, &derived, &config);
    let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&bytes));
    db.upsert_seen(&record).await.unwrap();
    db.mark_downloaded(
        "PrimarySync",
        "asset-L1",
        "original",
        &file,
        &hash,
        Some(&hash),
    )
    .await
    .unwrap();
    db.mark_failed(
        "PrimarySync",
        "asset-L1",
        "original",
        "synthetic later failure",
    )
    .await
    .unwrap();
    db.prepare_for_retry(Some("PrimarySync"), RetryErrorRetention::Clear)
        .await
        .unwrap();
    db.set_asset_verification(
        "PrimarySync",
        "asset-L1",
        "original",
        crate::state::AssetVerificationState::TransientFailure,
        "retained retry evidence",
    )
    .await
    .unwrap();
    let before = db
        .get_pending_publication("PrimarySync", "asset-L1", VersionSizeKey::Original)
        .await
        .unwrap()
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
    planner
        .plan_pending_retry_asset(&asset, &config)
        .await
        .unwrap();
    assert!(planner.managed_layout_session().is_some());
    assert!(
        !db.primary_layout_operations("PrimarySync".to_owned())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        recover_current_pending_publication(
            db.as_ref(),
            &config,
            &asset,
            &mut planner,
            VersionSizeKey::Original,
            &CancellationToken::new()
        )
        .await
        .unwrap(),
        None
    );
    assert_eq!(
        db.get_pending_publication("PrimarySync", "asset-L1", VersionSizeKey::Original)
            .await
            .unwrap(),
        Some(before.clone())
    );
    assert_eq!(fs::read(&file).unwrap(), bytes);
    drop(planner);
    drop(config);
    drop(db);
    let reopened = SqliteStateDb::open(&db_path).await.unwrap();
    assert_eq!(
        reopened
            .get_pending_publication("PrimarySync", "asset-L1", VersionSizeKey::Original)
            .await
            .unwrap(),
        Some(before)
    );
    let verification: String = reopened.acquire_lock("managed retry remains owned").unwrap()
        .query_row("SELECT state FROM asset_verifications WHERE library='PrimarySync' AND id='asset-L1' AND version_size='original'", [], |row| row.get(0)).unwrap();
    assert_eq!(verification, "transient_failure");
    assert_eq!(fs::read(&file).unwrap(), bytes);
}
