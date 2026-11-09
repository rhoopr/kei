use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
use super::completed_link_test_hook;
use super::recover_current_pending_publication;
use crate::download::DownloadConfig;
use crate::download::file::ExistingFileFingerprint;
use crate::download::filter::derive_expected_paths;
use crate::download::pipeline::adoption::{PendingOnDiskAdoption, asset_record_for_derived_path};
use crate::download::planner::TaskPlanner;
use crate::download::retry::PreparedReplaceAuthorization;
use crate::icloud::photos::PhotoAsset;
use crate::state::db::{
    PreparedPublication, PublicationBinding, TempFileOwnershipStore, account::AccountOwner,
    provider_selection::SelectionPath,
};
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
    // This lower-level fixture bypasses dispatch, so establish its real database check.
    config.ownership_check = Some(
        crate::download::planner::OwnershipCheck::inspect(&db)
            .await
            .unwrap(),
    );
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
            None,
            &shutdown
        ),
        recover_current_pending_publication(
            &db,
            &config,
            &asset,
            &mut second,
            VersionSizeKey::LiveOriginal,
            None,
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
                None,
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
                None,
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
        None,
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
            None,
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
            None,
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
async fn prepared_first_publication_reopens_before_rename_without_http_or_metadata_rewrite() {
    use crate::state::db::{
        DownloadContextStateStore, PreparedPublication, TempFileOwnershipStore,
        provider_selection::SelectionPath,
    };

    for (changed_part, incomplete, published) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, true, true),
    ] {
        let dir = TempDir::new().unwrap();
        let database = dir.path().join("state.db");
        let mut config = DownloadConfig::test_default();
        config.directory = Arc::from(dir.path().join("media"));
        config.folder_structure = "none".to_owned();
        let asset = TestPhotoAsset::new("prepared-first")
            .filename("prepared.MOV")
            .orig_url("http://127.0.0.1:1/must-not-download")
            .orig_size(MOVIE.len() as u64)
            .build();
        let derived = derive_expected_paths(&asset, &config)
            .into_iter()
            .find(|path| path.version_size == VersionSizeKey::Original)
            .unwrap();
        fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
        let temporary = derived.path.with_extension("part");
        fs::write(&temporary, MOVIE).unwrap();
        let mtime = fs::metadata(&temporary).unwrap().modified().unwrap();
        let local = data_encoding::HEXLOWER.encode(&Sha256::digest(MOVIE));
        let source = data_encoding::HEXLOWER.encode(&Sha256::digest(b"raw bytes before metadata"));
        let record =
            asset_record_for_derived_path(Arc::from("PrimarySync"), &asset, &derived, &config);
        let proof = PreparedPublication {
            binding: None,
            library: "PrimarySync".into(),
            id: asset.state_id().into(),
            version: "original".into(),
            provider_checksum: derived.checksum.to_string(),
            provider_size: derived.size,
            temporary: SelectionPath::from_path(&temporary),
            destination: SelectionPath::from_path(&derived.path),
            local_checksum: local.clone(),
            source_checksum: source.clone(),
        };
        {
            let db = SqliteStateDb::open(&database).await.unwrap();
            db.upsert_seen(&record).await.unwrap();
            db.claim_temp_file(&temporary).await.unwrap();
            db.prepare_temp_publication(&temporary, &proof)
                .await
                .unwrap();
        }
        if changed_part {
            fs::write(&temporary, b"foreign altered temporary bytes").unwrap();
        }
        let db = SqliteStateDb::open(&database).await.unwrap();
        config.ownership_check = Some(
            crate::download::planner::OwnershipCheck::inspect(&db)
                .await
                .unwrap(),
        );
        if incomplete {
            let legacy = crate::test_helpers::TestAssetRecord::new("legacy-malformed").build();
            db.upsert_seen(&legacy).await.unwrap();
            db.acquire_lock("synthetic historical claim")
                .unwrap()
                .execute(
                    "UPDATE assets SET local_path='old/../history.JPG' WHERE id='legacy-malformed'",
                    [],
                )
                .unwrap();
        }
        if published {
            fs::rename(&temporary, &derived.path).unwrap();
        }
        let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
        let recovered = recover_current_pending_publication(
            &db,
            &config,
            &asset,
            &mut planner,
            VersionSizeKey::Original,
            None,
            &CancellationToken::new(),
        )
        .await;
        if incomplete {
            assert!(recovered.unwrap().is_none());
            assert_eq!(derived.path.exists(), published);
            assert_eq!(
                fs::read(if published { &derived.path } else { &temporary }).unwrap(),
                MOVIE
            );
            assert_eq!(
                db.get_prepared_publications(
                    "PrimarySync",
                    asset.state_id(),
                    VersionSizeKey::Original
                )
                .await
                .unwrap(),
                vec![proof.clone()]
            );
            drop(planner);
            drop(db);
            let reopened = SqliteStateDb::open(&database).await.unwrap();
            let mut planner = TaskPlanner::for_download(Some(&reopened)).await.unwrap();
            assert!(
                recover_current_pending_publication(
                    &reopened,
                    &config,
                    &asset,
                    &mut planner,
                    VersionSizeKey::Original,
                    None,
                    &CancellationToken::new()
                )
                .await
                .unwrap()
                .is_none()
            );
            assert_eq!(
                reopened
                    .get_prepared_publications(
                        "PrimarySync",
                        asset.state_id(),
                        VersionSizeKey::Original
                    )
                    .await
                    .unwrap(),
                vec![proof]
            );
            assert!(reopened.get_baseline_receipts().await.unwrap().is_empty());
            continue;
        }
        if changed_part {
            assert!(recovered.is_err());
            assert!(!derived.path.exists());
            assert_eq!(
                fs::read(&temporary).unwrap(),
                b"foreign altered temporary bytes"
            );
            assert_eq!(
                db.get_prepared_publications(
                    "PrimarySync",
                    asset.state_id(),
                    VersionSizeKey::Original
                )
                .await
                .unwrap(),
                vec![proof]
            );
        } else {
            assert_eq!(
                recovered.unwrap(),
                Some(PendingOnDiskAdoption::Adopted(derived.path.clone()))
            );
            assert!(!temporary.exists());
            assert_eq!(fs::read(&derived.path).unwrap(), MOVIE);
            assert_eq!(
                fs::metadata(&derived.path).unwrap().modified().unwrap(),
                mtime
            );
            // This historical proof has no account/provider binding. Recovery
            // must retain its only modified-byte lineage until a validated
            // compact successor exists, rather than inventing that binding.
            assert_eq!(
                db.get_prepared_publications(
                    "PrimarySync",
                    asset.state_id(),
                    VersionSizeKey::Original
                )
                .await
                .unwrap(),
                vec![proof.clone()]
            );
            drop(planner);
            drop(db);
            let db = SqliteStateDb::open(&database).await.unwrap();
            let mut planner = TaskPlanner::for_download(Some(&db)).await.unwrap();
            assert_eq!(
                recover_current_pending_publication(
                    &db,
                    &config,
                    &asset,
                    &mut planner,
                    VersionSizeKey::Original,
                    None,
                    &CancellationToken::new(),
                )
                .await
                .unwrap(),
                Some(PendingOnDiskAdoption::Adopted(derived.path.clone()))
            );
            assert_eq!(fs::read(&derived.path).unwrap(), MOVIE);
            assert_eq!(
                fs::metadata(&derived.path).unwrap().modified().unwrap(),
                mtime
            );
            assert_eq!(
                db.get_prepared_publications(
                    "PrimarySync",
                    asset.state_id(),
                    VersionSizeKey::Original
                )
                .await
                .unwrap(),
                vec![proof]
            );
            assert!(
                db.get_owned_temp_files_before(i64::MAX)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let receipts = db.get_baseline_receipts().await.unwrap();
            let receipt = receipts
                .iter()
                .find(|receipt| receipt.id == asset.state_id())
                .unwrap();
            assert_eq!(receipt.local_checksum.as_deref(), Some(local.as_str()));
            assert_eq!(receipt.source_checksum.as_deref(), Some(source.as_str()));
        }
    }
}

const REPAIR_MEDIA: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0\x01\x01\0\0\x01\0\x01\0\0\xff\xd9";
const REPAIR_OLD: &[u8] = b"\xff\xd8\xff";

fn repair_hash(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn repair_fingerprint(bytes: &[u8]) -> ExistingFileFingerprint {
    ExistingFileFingerprint {
        size: bytes.len() as u64,
        sha256: Sha256::digest(bytes).into(),
    }
}

fn repair_owner() -> AccountOwner {
    AccountOwner::authenticated(
        "prepared-repair@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"prepared-repair"}})).unwrap(),
    )
    .unwrap()
}

fn repair_status(db: &SqliteStateDb) -> String {
    db.acquire_lock("read prepared repair status")
        .unwrap()
        .query_row(
            "SELECT status FROM assets WHERE library='PrimarySync' AND id='prepared-repair' AND version_size='original'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

struct RepairFixture {
    dir: TempDir,
    db: SqliteStateDb,
    config: DownloadConfig,
    asset: PhotoAsset,
    destination: PathBuf,
    part: PathBuf,
    proof: PreparedPublication,
    auth: PreparedReplaceAuthorization,
}

async fn repair_fixture() -> RepairFixture {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let owner = repair_owner();
    let db = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path().join("media"));
    config.folder_structure = "none".to_owned();
    config.repair_truncated = true;
    let asset = TestPhotoAsset::new("prepared-repair")
        .filename("prepared.jpg")
        .orig_file_type("public.jpeg")
        .orig_url("http://127.0.0.1:1/must-not-download")
        .orig_size(REPAIR_MEDIA.len() as u64)
        .orig_checksum(&data_encoding::BASE64.encode(&[0xab; 32]))
        .build();
    let derived = derive_expected_paths(&asset, &config)
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Original)
        .unwrap();
    fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
    let destination = derived.path.clone();
    fs::write(&destination, REPAIR_OLD).unwrap();
    let record = asset_record_for_derived_path(Arc::from("PrimarySync"), &asset, &derived, &config);
    db.upsert_seen(&record).await.unwrap();
    let media_hash = repair_hash(REPAIR_MEDIA);
    db.mark_downloaded(
        "PrimarySync",
        asset.state_id(),
        "original",
        &destination,
        &media_hash,
        Some(&media_hash),
    )
    .await
    .unwrap();
    db.mark_failed(
        "PrimarySync",
        asset.state_id(),
        "original",
        crate::commands::reconcile::FILE_TRUNCATED_REASON,
    )
    .await
    .unwrap();
    db.prepare_for_retry(
        Some("PrimarySync"),
        RetryErrorRetention::Preserve(crate::commands::reconcile::FILE_TRUNCATED_REASON),
    )
    .await
    .unwrap();
    let part = destination.with_file_name("prepared-repair.kei-tmp");
    fs::write(&part, REPAIR_MEDIA).unwrap();
    db.claim_temp_file(&part).await.unwrap();
    let scope = serde_json::to_string(&json!({
        "format": 1,
        "realm": "com",
        "container": "com.apple.photos.cloud",
        "environment": "production",
        "database": "private",
        "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
    }))
    .unwrap();
    let binding = db
        .bind_publication(
            owner,
            PublicationBinding {
                account_key: String::new(),
                provider_key: String::new(),
                scope,
                library: "PrimarySync".into(),
                child: asset.asset_record_name().to_owned(),
                master: asset.id().to_owned(),
                state_id: asset.state_id().to_owned(),
                origin_pass: "selected-pass".into(),
            },
        )
        .await
        .unwrap();
    let proof = PreparedPublication {
        binding: Some(binding.clone()),
        library: "PrimarySync".into(),
        id: asset.state_id().into(),
        version: "original".into(),
        provider_checksum: derived.checksum.to_string(),
        provider_size: derived.size,
        temporary: SelectionPath::from_path(&part),
        destination: SelectionPath::from_path(&destination),
        local_checksum: media_hash.clone(),
        source_checksum: media_hash,
    };
    db.prepare_temp_publication(&part, &proof).await.unwrap();
    config.ownership_check = Some(
        crate::download::planner::OwnershipCheck::inspect(&db)
            .await
            .unwrap(),
    );
    let auth = PreparedReplaceAuthorization {
        library: Arc::from("PrimarySync"),
        asset_id: Arc::from(asset.state_id()),
        version_size: VersionSizeKey::Original,
        native_path: SelectionPath::from_path(&destination),
        provider_checksum: derived.checksum.to_string(),
        provider_size: derived.size,
        expected: repair_fingerprint(REPAIR_OLD),
        binding,
    };
    RepairFixture {
        dir,
        db,
        config,
        asset,
        destination,
        part,
        proof,
        auth,
    }
}

#[tokio::test]
async fn prepared_replace_authorization_completes_exact_truncated_repair_without_http() {
    let fixture = repair_fixture().await;
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        Some(&fixture.auth),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert!(!fixture.part.exists());
    assert_eq!(repair_status(&fixture.db), "downloaded");
    assert!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap()
            .is_empty()
    );
    drop(planner);
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let reopened = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    drop(fixture.dir);
}

#[tokio::test]
async fn prepared_replace_authorization_refuses_unauthorized_evidence_and_preserves_claim() {
    #[derive(Clone, Copy, Debug)]
    enum Case {
        MissingAuthorization,
        RepairsDisabled,
        WrongNativePath,
        WrongBinding,
        WrongExpected,
        ChangedOccupant,
        WrongResource,
        Cancelled,
    }

    for case in [
        Case::MissingAuthorization,
        Case::RepairsDisabled,
        Case::WrongNativePath,
        Case::WrongBinding,
        Case::WrongExpected,
        Case::ChangedOccupant,
        Case::WrongResource,
        Case::Cancelled,
    ] {
        let fixture = repair_fixture().await;
        let mut config = fixture.config.clone();
        let mut auth = fixture.auth.clone();
        let shutdown = CancellationToken::new();
        let mut use_auth = true;
        let expected_occupant = match case {
            Case::MissingAuthorization => {
                use_auth = false;
                REPAIR_OLD.to_vec()
            }
            Case::RepairsDisabled => {
                config.repair_truncated = false;
                REPAIR_OLD.to_vec()
            }
            Case::WrongNativePath => {
                auth.native_path =
                    SelectionPath::from_path(&fixture.destination.with_file_name("other.jpg"));
                REPAIR_OLD.to_vec()
            }
            Case::WrongBinding => {
                auth.binding.origin_pass = "other-pass".into();
                REPAIR_OLD.to_vec()
            }
            Case::WrongExpected => {
                auth.expected = repair_fingerprint(b"different old bytes");
                REPAIR_OLD.to_vec()
            }
            Case::ChangedOccupant => {
                fs::write(&fixture.destination, b"changed occupant").unwrap();
                b"changed occupant".to_vec()
            }
            Case::WrongResource => {
                auth.provider_checksum = "other-provider-resource".into();
                REPAIR_OLD.to_vec()
            }
            Case::Cancelled => {
                shutdown.cancel();
                REPAIR_OLD.to_vec()
            }
        };
        let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
        let result = recover_current_pending_publication(
            &fixture.db,
            &config,
            &fixture.asset,
            &mut planner,
            VersionSizeKey::Original,
            use_auth.then_some(&auth),
            &shutdown,
        )
        .await;
        if matches!(case, Case::Cancelled) {
            assert!(result.is_err(), "{case:?}");
        } else {
            assert_eq!(result.unwrap(), None, "{case:?}");
        }
        assert_eq!(
            fs::read(&fixture.destination).unwrap(),
            expected_occupant,
            "{case:?}"
        );
        assert_eq!(fs::read(&fixture.part).unwrap(), REPAIR_MEDIA, "{case:?}");
        assert_eq!(
            fixture
                .db
                .get_prepared_publications(
                    "PrimarySync",
                    fixture.asset.state_id(),
                    VersionSizeKey::Original
                )
                .await
                .unwrap(),
            vec![fixture.proof.clone()],
            "{case:?}"
        );
        assert_eq!(repair_status(&fixture.db), "pending", "{case:?}");
        drop(fixture.dir);
    }
}

#[tokio::test]
async fn prepared_replace_authorization_finalize_failure_retains_reopen_recovery() {
    let fixture = repair_fixture().await;
    fixture
        .db
        .acquire_lock("inject prepared repair finalization failure")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_prepared_repair BEFORE UPDATE ON assets WHEN NEW.status='downloaded' \
             BEGIN SELECT RAISE(ABORT,'synthetic prepared repair finalization failure'); END;",
        )
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        Some(&fixture.auth),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::StateWriteFailed(
            fixture.destination.clone()
        ))
    );
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert_eq!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap(),
        vec![fixture.proof.clone()]
    );
    assert_eq!(repair_status(&fixture.db), "pending");
    fixture
        .db
        .acquire_lock("remove prepared repair finalization failure")
        .unwrap()
        .execute_batch("DROP TRIGGER reject_prepared_repair")
        .unwrap();
    drop(planner);
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let reopened = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        reopened,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert_eq!(repair_status(&fixture.db), "downloaded");
    assert!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap()
            .is_empty()
    );
    drop(planner);
    drop(fixture.dir);
}

#[derive(Clone, Copy, Debug)]
enum CompletedLinkBinding {
    Exact,
    WrongChild,
    Missing,
}

struct CompletedLinkFixture {
    dir: TempDir,
    db_path: PathBuf,
    owner: AccountOwner,
    db: SqliteStateDb,
    config: DownloadConfig,
    asset: PhotoAsset,
    destination: PathBuf,
    part: PathBuf,
    proof: PreparedPublication,
}

fn completed_link_status(db: &SqliteStateDb, id: &str) -> String {
    db.acquire_lock("read completed link status")
        .unwrap()
        .query_row(
            "SELECT status FROM assets WHERE library='PrimarySync' AND id=?1 AND version_size='original'",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

async fn completed_link_fixture(binding_mode: CompletedLinkBinding) -> CompletedLinkFixture {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("state.db");
    let owner = repair_owner();
    let db = SqliteStateDb::open_owned(&db_path, &owner).await.unwrap();
    let mut config = DownloadConfig::test_default();
    config.directory = Arc::from(dir.path().join("media"));
    config.folder_structure = "none".to_owned();
    config.repair_truncated = true;
    let asset = TestPhotoAsset::new("completed-link")
        .filename("completed-link.jpg")
        .orig_file_type("public.jpeg")
        .orig_url("http://127.0.0.1:1/must-not-download")
        .orig_size(REPAIR_MEDIA.len() as u64)
        .orig_checksum(&data_encoding::BASE64.encode(&[0xef; 32]))
        .build();
    let derived = derive_expected_paths(&asset, &config)
        .into_iter()
        .find(|path| path.version_size == VersionSizeKey::Original)
        .unwrap();
    fs::create_dir_all(derived.path.parent().unwrap()).unwrap();
    let destination = derived.path.clone();
    fs::write(&destination, REPAIR_MEDIA).unwrap();
    let record = asset_record_for_derived_path(Arc::from("PrimarySync"), &asset, &derived, &config);
    db.upsert_seen(&record).await.unwrap();
    let media_hash = repair_hash(REPAIR_MEDIA);
    db.mark_downloaded(
        "PrimarySync",
        asset.state_id(),
        "original",
        &destination,
        &media_hash,
        Some(&media_hash),
    )
    .await
    .unwrap();
    let part = destination.with_file_name("completed-link.kei-tmp");
    fs::hard_link(&destination, &part).unwrap();
    db.claim_temp_file(&part).await.unwrap();
    let scope = serde_json::to_string(&json!({
        "format": 1,
        "realm": "com",
        "container": "com.apple.photos.cloud",
        "environment": "production",
        "database": "private",
        "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
    }))
    .unwrap();
    let binding = match binding_mode {
        CompletedLinkBinding::Exact => Some(
            db.bind_publication(
                owner.clone(),
                PublicationBinding {
                    account_key: String::new(),
                    provider_key: String::new(),
                    scope,
                    library: "PrimarySync".into(),
                    child: asset.asset_record_name().to_owned(),
                    master: asset.id().to_owned(),
                    state_id: asset.state_id().to_owned(),
                    origin_pass: "selected-pass".into(),
                },
            )
            .await
            .unwrap(),
        ),
        CompletedLinkBinding::WrongChild => Some(
            db.bind_publication(
                owner.clone(),
                PublicationBinding {
                    account_key: String::new(),
                    provider_key: String::new(),
                    scope,
                    library: "PrimarySync".into(),
                    child: "other-child".into(),
                    master: asset.id().to_owned(),
                    state_id: asset.state_id().to_owned(),
                    origin_pass: "selected-pass".into(),
                },
            )
            .await
            .unwrap(),
        ),
        CompletedLinkBinding::Missing => None,
    };
    let proof = PreparedPublication {
        binding,
        library: "PrimarySync".into(),
        id: asset.state_id().into(),
        version: "original".into(),
        provider_checksum: derived.checksum.to_string(),
        provider_size: derived.size,
        temporary: SelectionPath::from_path(&part),
        destination: SelectionPath::from_path(&destination),
        local_checksum: media_hash.clone(),
        source_checksum: media_hash,
    };
    db.prepare_temp_publication(&part, &proof).await.unwrap();
    config.ownership_check = Some(
        crate::download::planner::OwnershipCheck::inspect(&db)
            .await
            .unwrap(),
    );
    CompletedLinkFixture {
        dir,
        db_path,
        owner,
        db,
        config,
        asset,
        destination,
        part,
        proof,
    }
}

#[cfg(target_os = "linux")]
struct ParentSyncFailureGuard;
#[cfg(target_os = "linux")]
impl Drop for ParentSyncFailureGuard {
    fn drop(&mut self) {
        completed_link_test_hook::disarm();
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_link_cleanup_removes_exact_two_link_and_retires_proof() {
    let fixture = completed_link_fixture(CompletedLinkBinding::Exact).await;
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert!(!fixture.part.exists());
    assert_eq!(
        completed_link_status(&fixture.db, fixture.asset.state_id()),
        "downloaded"
    );
    assert!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap()
            .is_empty()
    );
    eprintln!("COMPLETED-LINK phase=cleanup part=absent target=complete proof=retired");
    drop(planner);
    drop(fixture.dir);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_link_cleanup_missing_part_still_syncs_parents_before_finalize() {
    let fixture = completed_link_fixture(CompletedLinkBinding::Exact).await;
    fs::remove_file(&fixture.part).unwrap();
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert!(!fixture.part.exists());
    assert!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap()
            .is_empty()
    );
    eprintln!("COMPLETED-LINK phase=missing-part part=absent target=complete proof=retired");
    drop(planner);
    drop(fixture.dir);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_link_cleanup_refuses_unsafe_parts_and_preserves_proof() {
    #[derive(Clone, Copy, Debug)]
    enum Case {
        LinkCountOne,
        Symlink,
        OtherInodeTwoLinks,
        PairedNamespaceSwap,
        WrongChild,
        Unbound,
    }

    for case in [
        Case::LinkCountOne,
        Case::Symlink,
        Case::OtherInodeTwoLinks,
        Case::PairedNamespaceSwap,
        Case::WrongChild,
        Case::Unbound,
    ] {
        let binding = match case {
            Case::WrongChild => CompletedLinkBinding::WrongChild,
            Case::Unbound => CompletedLinkBinding::Missing,
            _ => CompletedLinkBinding::Exact,
        };
        let fixture = completed_link_fixture(binding).await;
        match case {
            Case::LinkCountOne => {
                fs::remove_file(&fixture.part).unwrap();
                fs::write(&fixture.part, REPAIR_MEDIA).unwrap();
            }
            Case::Symlink => {
                fs::remove_file(&fixture.part).unwrap();
                std::os::unix::fs::symlink(&fixture.destination, &fixture.part).unwrap();
            }
            Case::OtherInodeTwoLinks => {
                fs::remove_file(&fixture.part).unwrap();
                fs::write(&fixture.part, REPAIR_MEDIA).unwrap();
                fs::hard_link(
                    &fixture.part,
                    fixture
                        .destination
                        .with_file_name("completed-link-other.kei-tmp"),
                )
                .unwrap();
            }
            Case::WrongChild | Case::Unbound | Case::PairedNamespaceSwap => {}
        }
        let pause = matches!(case, Case::PairedNamespaceSwap).then(|| {
            completed_link_test_hook::arm_pause(
                &fixture.destination,
                completed_link_test_hook::Stage::CleanupRetained,
            )
        });
        let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
        let shutdown = CancellationToken::new();
        let mut recovery = Box::pin(recover_current_pending_publication(
            &fixture.db,
            &fixture.config,
            &fixture.asset,
            &mut planner,
            VersionSizeKey::Original,
            None,
            &shutdown,
        ));
        let replacement = if let Some((reached, resume)) = pause {
            tokio::select! {
                result = &mut recovery => panic!("recovery missed retained-target handoff: {result:?}"),
                ready = tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified()) => ready.unwrap(),
            }
            let held = fs::File::open(&fixture.destination).unwrap();
            let old = crate::fs_util::file_identity(&held).unwrap();
            fs::remove_file(&fixture.part).unwrap();
            fs::remove_file(&fixture.destination).unwrap();
            fs::write(&fixture.destination, REPAIR_MEDIA).unwrap();
            fs::hard_link(&fixture.destination, &fixture.part).unwrap();
            let new = handoff_identity(&fixture.destination);
            assert_ne!(old, new);
            resume.notify_one();
            Some((held, old, new))
        } else {
            None
        };
        let result = recovery.as_mut().await;
        drop(recovery);
        assert!(result.is_err(), "{case:?}");
        assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
        assert!(fixture.part.exists(), "{case:?}");
        if let Some((mut held, old, new)) = replacement {
            use std::os::unix::fs::MetadataExt;
            for path in [&fixture.destination, &fixture.part] {
                assert_eq!(handoff_identity(path), new);
                assert_eq!(fs::read(path).unwrap(), REPAIR_MEDIA);
                assert_eq!(fs::metadata(path).unwrap().nlink(), 2);
            }
            assert_eq!(crate::fs_util::file_identity(&held).unwrap(), old);
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut held, &mut bytes).unwrap();
            assert_eq!(bytes, REPAIR_MEDIA);
            eprintln!(
                "COMPLETED-LINK phase=paired-swap-refused target=B part=B held=A proof=retained"
            );
        }
        assert_eq!(
            fixture
                .db
                .get_prepared_publications(
                    "PrimarySync",
                    fixture.asset.state_id(),
                    VersionSizeKey::Original
                )
                .await
                .unwrap(),
            vec![fixture.proof.clone()],
            "{case:?}"
        );
        assert_eq!(
            completed_link_status(&fixture.db, fixture.asset.state_id()),
            "downloaded",
            "{case:?}"
        );
        eprintln!("COMPLETED-LINK phase=refused case={case:?} part=retained proof=retained");
        drop(planner);
        drop(fixture.dir);
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_link_cleanup_finalize_failure_reopens_without_part() {
    let fixture = completed_link_fixture(CompletedLinkBinding::Exact).await;
    fixture
        .db
        .acquire_lock("inject completed link finalization failure")
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_completed_link BEFORE UPDATE ON assets WHEN NEW.status='downloaded' \
             BEGIN SELECT RAISE(ABORT,'synthetic completed link finalization failure'); END;",
        )
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::StateWriteFailed(
            fixture.destination.clone()
        ))
    );
    assert!(!fixture.part.exists());
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert_eq!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap(),
        vec![fixture.proof.clone()]
    );
    eprintln!("COMPLETED-LINK phase=unlink-commit-fail part=absent proof=retained");
    fixture
        .db
        .acquire_lock("remove completed link finalization failure")
        .unwrap()
        .execute_batch("DROP TRIGGER reject_completed_link")
        .unwrap();
    drop(planner);
    drop(fixture.db);
    let reopened = SqliteStateDb::open_owned(&fixture.db_path, &fixture.owner)
        .await
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(&reopened)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &reopened,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert!(!fixture.part.exists());
    assert!(
        reopened
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap()
            .is_empty()
    );
    eprintln!("COMPLETED-LINK phase=reopen-finalized part=absent proof=retired");
    drop(planner);
    drop(reopened);
    drop(fixture.dir);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_link_cleanup_parent_sync_failure_requires_reopen_durability() {
    let fixture = completed_link_fixture(CompletedLinkBinding::Exact).await;
    completed_link_test_hook::arm(&fixture.destination);
    let _guard = ParentSyncFailureGuard;
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let result = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await;
    assert!(result.is_err());
    assert!(!fixture.part.exists());
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert_eq!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap(),
        vec![fixture.proof.clone()]
    );
    eprintln!("COMPLETED-LINK phase=unlink-parent-sync-fail part=absent proof=retained");
    drop(planner);
    drop(fixture.db);
    let reopened = SqliteStateDb::open_owned(&fixture.db_path, &fixture.owner)
        .await
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(&reopened)).await.unwrap();
    let retry = recover_current_pending_publication(
        &reopened,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await;
    assert!(retry.is_err());
    assert!(!fixture.part.exists());
    assert_eq!(
        reopened
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap(),
        vec![fixture.proof.clone()]
    );
    eprintln!("COMPLETED-LINK phase=missing-part-fault-still-on proof=retained");
    completed_link_test_hook::disarm();
    drop(planner);
    let mut planner = TaskPlanner::for_download(Some(&reopened)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &reopened,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert!(!fixture.part.exists());
    assert!(
        reopened
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap()
            .is_empty()
    );
    eprintln!("COMPLETED-LINK phase=fault-removed part=absent proof=retired");
    drop(planner);
    drop(reopened);
    drop(fixture.dir);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_link_cleanup_cancellation_before_cleanup_preserves_proof_and_part() {
    let fixture = completed_link_fixture(CompletedLinkBinding::Exact).await;
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let result = recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        None,
        &shutdown,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert!(fixture.part.exists());
    assert_eq!(
        fixture
            .db
            .get_prepared_publications(
                "PrimarySync",
                fixture.asset.state_id(),
                VersionSizeKey::Original
            )
            .await
            .unwrap(),
        vec![fixture.proof.clone()]
    );
    eprintln!("COMPLETED-LINK phase=cancelled-before-cleanup part=retained proof=retained");
    drop(planner);
    drop(fixture.dir);
}

#[cfg(target_os = "linux")]
fn handoff_identity(path: &std::path::Path) -> crate::fs_util::FileIdentity {
    crate::fs_util::file_identity(&fs::File::open(path).unwrap()).unwrap()
}

#[cfg(target_os = "linux")]
async fn cancellation_at_handoff(stage: completed_link_test_hook::Stage) {
    let fixture = repair_fixture().await;
    if stage == completed_link_test_hook::Stage::CleanupPrechecked {
        fs::write(&fixture.destination, REPAIR_MEDIA).unwrap();
        fs::remove_file(&fixture.part).unwrap();
        fs::hard_link(&fixture.destination, &fixture.part).unwrap();
    }
    let paths = [&fixture.destination, &fixture.part];
    let before = paths.map(|path| (fs::read(path).unwrap(), handoff_identity(path)));
    let (reached, resume) = completed_link_test_hook::arm_pause(&fixture.destination, stage);
    let shutdown = CancellationToken::new();
    let mut planner = TaskPlanner::for_download(Some(&fixture.db)).await.unwrap();
    let mut recovery = Box::pin(recover_current_pending_publication(
        &fixture.db,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        Some(&fixture.auth),
        &shutdown,
    ));
    tokio::select! {
        result = &mut recovery => panic!("recovery missed cancellation handoff: {result:?}"),
        ready = tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified()) => ready.unwrap(),
    }
    eprintln!("PREPARED-HANDOFF stage={stage:?} owner=awaiting mutation=not-started");
    shutdown.cancel();
    resume.notify_one();
    assert!(
        recovery.as_mut().await.is_err(),
        "cancelled handoff must refuse mutation/adoption"
    );
    drop(recovery);
    for (path, expected) in paths.into_iter().zip(before) {
        assert_eq!((fs::read(path).unwrap(), handoff_identity(path)), expected);
    }
    let proofs = fixture
        .db
        .get_prepared_publications(
            "PrimarySync",
            fixture.asset.state_id(),
            VersionSizeKey::Original,
        )
        .await
        .unwrap();
    assert_eq!(proofs, vec![fixture.proof.clone()]);
    assert_eq!(repair_status(&fixture.db), "pending");
    drop(planner);
    drop(fixture.db);
    let reopened = SqliteStateDb::open_owned(&fixture.dir.path().join("state.db"), &repair_owner())
        .await
        .unwrap();
    let mut planner = TaskPlanner::for_download(Some(&reopened)).await.unwrap();
    let recovered = recover_current_pending_publication(
        &reopened,
        &fixture.config,
        &fixture.asset,
        &mut planner,
        VersionSizeKey::Original,
        Some(&fixture.auth),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        recovered,
        Some(PendingOnDiskAdoption::Adopted(fixture.destination.clone()))
    );
    assert!(!fixture.part.exists());
    assert_eq!(fs::read(&fixture.destination).unwrap(), REPAIR_MEDIA);
    assert_eq!(repair_status(&reopened), "downloaded");
    assert!(
        reopened
            .get_owned_temp_files_before(i64::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    eprintln!(
        "PREPARED-HANDOFF stage={stage:?} phase=cancelled proof=retained names=stable reopened=GET0-finalized"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn prepared_replace_cancellation_after_reopen_preserves_proof_and_names() {
    cancellation_at_handoff(completed_link_test_hook::Stage::ReplaceReady).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_link_cleanup_cancellation_after_precheck_preserves_proof_and_names() {
    cancellation_at_handoff(completed_link_test_hook::Stage::CleanupPrechecked).await;
}
