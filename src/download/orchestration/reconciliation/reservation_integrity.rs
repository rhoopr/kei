//! Reservation integrity without the removed automatic layout planner.
use super::{
    RecoveryFixture, RecoveryLookupSession, album_with_session, build_pending_retry_download_tasks,
    expected_hidden_original_request_key, incremental_photo_records_with_url, issue_770_pass,
    issue_770_seed_records,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download::orchestration::models::DownloadControls;
use crate::download::{DownloadConfig, DownloadRunMode, SyncMode};
use crate::state::{
    ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    ReconciliationStateStore, SqliteStateDb, VersionSizeKey,
};
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn collision_fixture(
    server: &MockServer,
    authenticated: bool,
) -> (RecoveryFixture, std::path::PathBuf) {
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/media/pattern.jpg"
    ))
    .to_vec();
    let mut records = incremental_photo_records_with_url(
        "PENDING",
        "pending.jpg",
        &format!("{}/pending.jpg", server.uri()),
        bytes.len() as u64,
    );
    records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
        json!(data_encoding::BASE64.encode(&[0xa7_u8; 32]));
    let root = tempfile::tempdir().unwrap();
    if authenticated {
        SqliteStateDb::open_owned(&root.path().join("state.db"), &repair_owner())
            .await
            .unwrap();
    }
    let (mut config, records, unsuffixed) = issue_770_seed_records(&root, records).await;
    let destination = unsuffixed.with_file_name("pending-PENDING.jpg");
    let db = Arc::new(
        SqliteStateDb::open(&root.path().join("state.db"))
            .await
            .unwrap(),
    );
    db.mark_downloaded(
        "PrimarySync",
        "PENDING",
        "original",
        &destination,
        "historical-local-checksum",
        None,
    )
    .await
    .unwrap();
    db.mark_failed(
        "PrimarySync",
        "PENDING",
        "original",
        "synthetic earlier transfer failure",
    )
    .await
    .unwrap();
    db.prepare_for_retry(
        Some("PrimarySync"),
        crate::state::RetryErrorRetention::Clear,
    )
    .await
    .unwrap();
    let key = crate::fs_util::confined_path_key(&unsuffixed).unwrap();
    db.reserve_reconciliation_paths(&[ReconciliationReservation {
        library: Arc::from("PrimarySync"),
        asset_id: "FOREIGN".into(),
        version_size: VersionSizeKey::Original,
        content: Some(ReconciliationContent {
            checksum: "foreign-provider-generation".into(),
            size: bytes.len() as u64,
        }),
        requested_path_key: ReconciliationPathKey(key.clone()),
        destination_path_key: ReconciliationPathKey(key),
        destination_path: unsuffixed.clone(),
    }])
    .await
    .unwrap();
    config.state_db = Some(db.clone());
    let tasks = build_pending_retry_download_tasks(
        &[issue_770_pass(PassKind::SmartFolder, &records)],
        &config,
        DownloadRunMode::Download,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(tasks.tasks.len(), 1);
    assert_eq!(tasks.tasks[0].download_path, destination);
    let ledger = db.get_reconciliation_reservations().await.unwrap();
    let request = expected_hidden_original_request_key(&config, &records);
    assert!(ledger.iter().any(|row| row.asset_id.as_ref() == "PENDING"
        && row.requested_path_key.0 == request
        && row.destination_path == destination));
    (
        RecoveryFixture {
            root,
            db,
            config,
            records,
            destination,
            ledger,
            bytes,
        },
        unsuffixed,
    )
}

fn repair_owner() -> crate::state::db::account::AccountOwner {
    crate::state::db::account::AccountOwner::authenticated(
        "repair@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"repair-fixture"}})).unwrap(),
    )
    .unwrap()
}

async fn run_authenticated_repair(
    f: &RecoveryFixture,
    kinds: &[PassKind],
) -> crate::download::SyncResult {
    use crate::icloud::photos::{PhotoAlbum, PhotoAlbumConfig, inbox::ShadowCapture};
    let passes: Vec<_> = kinds
        .iter()
        .map(|kind| {
            let mut album = PhotoAlbum::new(
                PhotoAlbumConfig {
                    params: Arc::new(std::collections::HashMap::new()),
                    service_endpoint:
                        "https://synthetic.example/database/1/photos/production/private".into(),
                    name: if *kind == PassKind::SmartFolder {
                        "Hidden".into()
                    } else {
                        "".into()
                    },
                    list_type: "CPLAssetAndMasterByAssetDateWithoutHiddenOrDeleted".into(),
                    obj_type: "CPLAssetByAssetDateWithoutHiddenOrDeleted".into(),
                    query_filter: None,
                    page_size: 2,
                    zone_id: Arc::new(
                        json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
                    ),
                    retry_config: crate::retry::RetryConfig::default(),
                    container_id: None,
                    cross_zone_sources: Vec::new(),
                },
                Box::new(RecoveryLookupSession {
                    records: Arc::new(f.records.clone()),
                    enumerate: false,
                }),
            );
            album.set_shadow_capture(
                ShadowCapture::new(f.db.clone(), repair_owner(), "com"),
                "private".into(),
            );
            AlbumPass {
                kind: *kind,
                album,
                exclude_ids: Default::default(),
            }
        })
        .collect();
    crate::download::orchestration::dispatch::download_photos_with_sync(
        &reqwest::Client::new(),
        &passes,
        Arc::new(f.config.clone()),
        DownloadControls::download_hidden(),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn v027_reserved_collision_explicit_truncation_repairs_only_saved_owned_destination() {
    let server = MockServer::start().await;
    let (mut f, foreign) = collision_fixture(&server, true).await;
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(f.bytes.clone()))
        .expect(2)
        .mount(&server)
        .await;
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    let initial = run_authenticated_repair(&f, &kinds).await;
    assert_eq!(
        initial.stats.downloaded,
        1,
        "fixture publication must succeed before explicit truncation: {initial:?}; failures={:?}",
        f.db.get_failed().await.unwrap()
    );
    std::fs::write(&foreign, b"foreign media must not change").unwrap();
    let stamp = std::fs::metadata(&foreign).unwrap().modified().unwrap();
    std::fs::write(&f.destination, &f.bytes[..4]).unwrap();
    f.db.mark_failed(
        "PrimarySync",
        "PENDING",
        "original",
        crate::commands::reconcile::FILE_TRUNCATED_REASON,
    )
    .await
    .unwrap();
    f.config.repair_truncated = true;
    f.reopen().await;
    let (capture, _guard) = crate::test_helpers::TracingCapture::install();
    let repaired = run_authenticated_repair(&f, &kinds).await;
    assert_eq!(
        repaired.stats.downloaded,
        1,
        "explicit marked repair must reach its saved destination: {repaired:?}; failures={:?}; GETs={:?}; events={:?}",
        f.db.get_failed().await.unwrap(),
        server.received_requests().await.unwrap().len(),
        capture.events()
    );
    assert_eq!(std::fs::read(&f.destination).unwrap(), f.bytes);
    f.assert_preserved().await;
    f.reopen().await;
    assert_eq!(
        run_authenticated_repair(&f, &kinds).await.stats.downloaded,
        0
    );
    assert_eq!(
        std::fs::read(&foreign).unwrap(),
        b"foreign media must not change"
    );
    assert_eq!(
        std::fs::metadata(&foreign).unwrap().modified().unwrap(),
        stamp
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn v027_unscoped_prepared_lineage_blocks_repair_without_losing_owned_work() {
    use crate::state::TempFileOwnershipStore;
    let server = MockServer::start().await;
    let (mut f, foreign) = collision_fixture(&server, false).await;
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(f.bytes.clone()))
        .expect(1)
        .mount(&server)
        .await;
    let kinds = [PassKind::SmartFolder, PassKind::Unfiled];
    assert_eq!(
        f.run(&kinds, CancellationToken::new())
            .await
            .stats
            .downloaded,
        1
    );
    let proof =
        f.db.get_prepared_publications("PrimarySync", "PENDING", VersionSizeKey::Original)
            .await
            .unwrap();
    assert_eq!(proof.len(), 1);
    assert!(proof[0].binding.is_none());
    std::fs::write(&f.destination, &f.bytes[..4]).unwrap();
    f.db.mark_failed(
        "PrimarySync",
        "PENDING",
        "original",
        crate::commands::reconcile::FILE_TRUNCATED_REASON,
    )
    .await
    .unwrap();
    f.config.repair_truncated = true;
    f.reopen().await;
    let result = f.run(&kinds, CancellationToken::new()).await;
    assert_eq!(result.stats.downloaded, 0);
    assert!(result.stats.failed > 0);
    assert_eq!(
        f.db.get_prepared_publications("PrimarySync", "PENDING", VersionSizeKey::Original)
            .await
            .unwrap(),
        proof
    );
    assert_eq!(std::fs::read(&f.destination).unwrap(), f.bytes[..4]);
    assert!(!foreign.exists());
    assert!(
        !f.db.get_failed().await.unwrap().is_empty()
            || !f.db.get_pending().await.unwrap().is_empty()
    );
    f.assert_preserved().await;
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn v027_identical_foreign_reserved_leaf_never_satisfies_current_copy_or_gets_overwritten() {
    let server = MockServer::start().await;
    let (mut f, foreign) = collision_fixture(&server, false).await;
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&foreign, &f.bytes).unwrap();
    let stamp = std::fs::metadata(&foreign).unwrap().modified().unwrap();
    Mock::given(method("GET"))
        .and(path("/pending.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(f.bytes.clone()))
        .mount(&server)
        .await;
    for _ in 0..2 {
        f.reopen().await;
        let result = f
            .run(
                &[PassKind::SmartFolder, PassKind::Unfiled],
                CancellationToken::new(),
            )
            .await;
        let rows = f.db.get_downloaded_page(0, 10).await.unwrap();
        assert!(
            rows.iter()
                .all(|row| row.local_path.as_ref() != Some(&foreign)),
            "foreign reservation cannot become current ownership: {result:?}"
        );
        if f.destination.exists() {
            assert_eq!(std::fs::read(&f.destination).unwrap(), f.bytes);
            assert!(rows.iter().any(|row| row.id.as_ref() == "PENDING"
                && row.local_path.as_ref() == Some(&f.destination)));
        } else {
            // Until discovery excludes durably foreign owners, uncertainty is
            // acceptable only when independent pending work remains visible.
            assert!(rows.is_empty());
            assert!(
                !f.db.get_pending().await.unwrap().is_empty()
                    || !f.db.get_failed().await.unwrap().is_empty()
            );
            assert!(server.received_requests().await.unwrap().is_empty());
        }
        f.assert_preserved().await;
        assert_eq!(std::fs::read(&foreign).unwrap(), f.bytes);
        assert_eq!(
            std::fs::metadata(&foreign).unwrap().modified().unwrap(),
            stamp
        );
    }
}

#[tokio::test]
async fn v027_full_and_collecting_reservation_write_failure_precedes_get_and_retains_checkpoint() {
    for collecting in [false, true] {
        let server = MockServer::start().await;
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("state.db");
        let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/media/pattern.jpg"
        ));
        let mut records = incremental_photo_records_with_url(
            "NEW",
            "new.jpg",
            &format!("{}/new.jpg", server.uri()),
            bytes.len() as u64,
        );
        records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"] =
            json!(data_encoding::BASE64.encode(&[0xa7_u8; 32]));
        let held = root.path().join("media/held.jpg");
        let key = crate::fs_util::confined_path_key(&held).unwrap();
        db.reserve_reconciliation_paths(&[ReconciliationReservation {
            library: Arc::from("PrimarySync"),
            asset_id: "UNRELATED".into(),
            version_size: VersionSizeKey::Original,
            content: Some(ReconciliationContent {
                checksum: "held".into(),
                size: 1,
            }),
            requested_path_key: ReconciliationPathKey(key.clone()),
            destination_path_key: ReconciliationPathKey(key),
            destination_path: held,
        }])
        .await
        .unwrap();
        let before = db.get_reconciliation_reservations().await.unwrap();
        db.set_metadata("sync_token:PrimarySync", "checkpoint-before")
            .await
            .unwrap();
        db.acquire_lock("inject current reservation admission failure").unwrap().execute_batch("CREATE TEMP TRIGGER reservation_integrity_failure BEFORE INSERT ON reconciliation_paths WHEN NEW.id != 'UNRELATED' BEGIN SELECT RAISE(FAIL, 'synthetic reservation admission failure'); END;").unwrap();
        let mut config = DownloadConfig::test_default();
        config.directory = root.path().join("media").into();
        config.folder_structure = "none".into();
        config.state_db = Some(db.clone());
        config.sync_mode = SyncMode::Full;
        Mock::given(method("GET"))
            .and(path("/new.jpg"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.as_slice()))
            .expect(1)
            .mount(&server)
            .await;
        for cycle in 0..2 {
            if cycle == 1 {
                config.state_db = Some(Arc::new(SqliteStateDb::open(&db_path).await.unwrap()));
            }
            let session: Box<dyn crate::icloud::photos::PhotosSession> = if collecting {
                Box::new(
                    crate::test_helpers::MockPhotosFlow::new()
                        .changes_zone_page(records.clone(), "checkpoint-after", false)
                        .build(),
                )
            } else {
                Box::new(RecoveryLookupSession {
                    records: Arc::new(records.clone()),
                    enumerate: true,
                })
            };
            let passes = [AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session("PrimarySync", "", session),
                exclude_ids: Default::default(),
            }];
            let configured = Arc::new(config.clone());
            let result = if collecting {
                crate::download::orchestration::incremental::download_photos_incremental_collecting_inner(
                    &reqwest::Client::new(), &passes, &configured, "checkpoint-before", DownloadControls::download_hidden(), CancellationToken::new(), std::time::Duration::MAX,
                ).await
            } else {
                crate::download::orchestration::dispatch::download_photos_with_sync(
                    &reqwest::Client::new(),
                    &passes,
                    configured,
                    DownloadControls::download_hidden(),
                    CancellationToken::new(),
                )
                .await
            };
            let current = config.state_db.as_ref().unwrap();
            if cycle == 0 {
                match &result {
                    Ok(result) => {
                        assert!(
                            result.stats.state_write_failures > 0 || result.stats.failed > 0,
                            "{result:?}"
                        );
                        assert!(
                            result.sync_token.is_none(),
                            "unadmitted work cannot release a successor: {result:?}"
                        );
                    }
                    Err(error) => {
                        assert!(collecting, "unexpected full-run refusal: {error:#}");
                        assert!(
                            format!("{error:#}")
                                .contains("synthetic reservation admission failure"),
                            "unexpected collecting error: {error:#}"
                        );
                    }
                }
                assert!(server.received_requests().await.unwrap().is_empty());
                assert!(current.get_downloaded_page(0, 10).await.unwrap().is_empty());
                assert_eq!(
                    current.get_reconciliation_reservations().await.unwrap(),
                    before
                );
                assert_eq!(
                    current
                        .get_metadata("sync_token:PrimarySync")
                        .await
                        .unwrap()
                        .as_deref(),
                    Some("checkpoint-before")
                );
            } else {
                let result = result.expect("reopened admission must recover without error");
                assert_eq!(
                    result.stats.downloaded, 1,
                    "reopened admission must recover: {result:?}"
                );
                let rows = current.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(
                    std::fs::read(rows[0].local_path.as_ref().unwrap()).unwrap(),
                    bytes
                );
                let after = current.get_reconciliation_reservations().await.unwrap();
                assert!(before.iter().all(|choice| after.contains(choice)));
            }
        }
    }
}
