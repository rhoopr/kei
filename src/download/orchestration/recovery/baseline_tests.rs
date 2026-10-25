use super::super::test_support::{
    PendingLookupSession, album_with_session, incremental_photo_records_with_url, test_config,
};
use crate::commands::{AlbumPass, PassKind};
use crate::download::retry::build_pending_retry_download_tasks;
use crate::download::{DownloadRunMode, filter};
use crate::icloud::photos::PhotoAsset;
use crate::state::SqliteStateDb;
use crate::test_helpers::TestAssetRecord;
use rustc_hash::FxHashSet;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn baseline_quiet_retry_retains_uncertain_occupied_paths_across_reopen() {
    let server = crate::start_wiremock_or_skip!();
    wiremock::Mock::given(wiremock::matchers::any())
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_bytes(b"correct!"))
        .expect(0)
        .mount(&server)
        .await;
    for renamed in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let root = dir.path().join("photos");
        std::fs::create_dir_all(&root).unwrap();
        let records = incremental_photo_records_with_url(
            "UNCERTAIN",
            "uncertain.jpg",
            &format!("{}/must-not-download", server.uri()),
            8,
        );
        let asset = PhotoAsset::new(records[0].clone(), records[1].clone());
        let checksum = records[0]["fields"]["resOriginalRes"]["value"]["fileChecksum"]
            .as_str()
            .unwrap();
        let mut cfg = test_config();
        cfg.directory = Arc::from(root.as_path());
        cfg.folder_structure = "none".into();
        let requested = filter::expected_paths_for(&asset, &cfg).remove(0).path;
        let occupied = if renamed {
            root.join("old-renamed.jpg")
        } else {
            requested
        };
        std::fs::write(&occupied, b"wrong!!!").unwrap();
        let modified = std::fs::metadata(&occupied).unwrap().modified().unwrap();
        let record = TestAssetRecord::new(asset.state_id())
            .filename("uncertain.jpg")
            .checksum(checksum)
            .size(8)
            .added_at(asset.added_date())
            .build();
        {
            let db = SqliteStateDb::open(&db_path).await.unwrap();
            db.upsert_seen(&record).await.unwrap();
            db.mark_downloaded(
                "PrimarySync",
                asset.state_id(),
                "original",
                &occupied,
                &"a".repeat(64),
                None,
            )
            .await
            .unwrap();
            db.defer_baseline_match(&record).await.unwrap();
            db.upsert_asset_master_mapping(
                "PrimarySync",
                asset.asset_record_name(),
                asset.state_id(),
            )
            .await
            .unwrap();
        }
        for _ in 0..3 {
            let db = Arc::new(SqliteStateDb::open(&db_path).await.unwrap());
            cfg.state_db = Some(db);
            let passes = vec![AlbumPass {
                kind: PassKind::Unfiled,
                album: album_with_session(
                    "PrimarySync",
                    "",
                    Box::new(PendingLookupSession {
                        records: Arc::new(records.clone()),
                    }),
                ),
                exclude_ids: Arc::new(FxHashSet::default()),
            }];
            let plan = build_pending_retry_download_tasks(
                &passes,
                &cfg,
                DownloadRunMode::Download,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                plan.tasks.is_empty(),
                "uncertainty must never become a transfer"
            );
            cfg.sync_mode = crate::download::orchestration::models::SyncMode::Incremental {
                zone_sync_token: "quiet-provider-token".into(),
            };
            let quiet = super::super::dispatch::download_photos_with_sync(
                &reqwest::Client::new(),
                &passes,
                Arc::new(cfg.clone()),
                crate::download::DownloadControls::new(
                    DownloadRunMode::Download,
                    crate::download::DownloadReporting::hidden(),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert!(
                !quiet.full_enumeration_ran,
                "quiet provider retry must not force inventory"
            );

            let conn = rusqlite::Connection::open(&db_path).unwrap();
            let status: String = conn
                .query_row(
                    "SELECT status FROM assets WHERE id=?1",
                    [asset.state_id()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                status, "pending",
                "same-size bytes cannot certify completion"
            );
            assert_eq!(std::fs::read(&occupied).unwrap(), b"wrong!!!");
            assert_eq!(
                std::fs::metadata(&occupied).unwrap().modified().unwrap(),
                modified
            );
            cfg.state_db = None;
        }
    }
    server.verify().await;
}
