use crate::state;
use crate::sync_loop::precheck::{WatchPrecheck, include_pending_local_work};
use crate::sync_loop::test_support::make_run_cycle_library_state;

#[tokio::test]
async fn baseline_watch_ordinary_retry_wakes_only_selected_debt_without_changing_tokens() {
    for failed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let key = crate::sync_cycle::sync_token_key("PrimarySync");
        {
            let db = state::SqliteStateDb::open(&path).await.unwrap();
            for (library, id) in [
                ("PrimarySync", "selected"),
                ("SharedSync-unselected", "unselected"),
            ] {
                let record = crate::test_helpers::TestAssetRecord::new(id)
                    .library(library)
                    .filename("existing.jpg")
                    .build();
                db.upsert_seen(&record).await.unwrap();
                if failed {
                    db.mark_failed(library, id, "original", "transfer interrupted")
                        .await
                        .unwrap();
                } else {
                    state::DownloadStateStore::defer_baseline_match(&db, &record)
                        .await
                        .unwrap();
                }
                db.set_metadata_capture_revision_for_test(
                    library,
                    id,
                    state::METADATA_CAPTURE_REVISION,
                );
            }
            db.set_metadata(&key, "v27-opaque-cursor").await.unwrap();
            db.set_metadata("sync_token:PrimarySync", "rollback-old-cursor")
                .await
                .unwrap();
        }
        for _ in 0..3 {
            let db = state::SqliteStateDb::open(&path).await.unwrap();
            assert!(
                !db.has_metadata_capture_work(&["PrimarySync"], state::METADATA_CAPTURE_REVISION)
                    .await
                    .unwrap()
            );
            assert!(
                db.get_pending_metadata_rewrites_page(Some(&["PrimarySync"]), 0, 1)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let conn = rusqlite::Connection::open(&path).unwrap();
            for table in [
                "provider_work_retries",
                "provider_work_obligations",
                "provider_selection_generations",
                "provider_active_generations",
                "provider_shadow_pages",
            ] {
                let count: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap();
                assert_eq!(count, 0, "watch must wake without incidental {table} debt");
            }
            let selected = make_run_cycle_library_state("PrimarySync", &key, "v27-opaque-cursor");
            let quiet = make_run_cycle_library_state(
                "SharedSync-quiet",
                &crate::sync_cycle::sync_token_key("SharedSync-quiet"),
                "quiet-cursor",
            );
            let mut precheck = WatchPrecheck::SkipAll;
            include_pending_local_work(&mut precheck, &db, &[selected, quiet]).await;
            assert!(
                precheck.should_sync_zone("PrimarySync"),
                "ordinary retry debt must override SkipAll"
            );
            assert!(!precheck.should_sync_zone("SharedSync-quiet"));
            assert!(!precheck.should_sync_zone("SharedSync-unselected"));
            assert!(precheck.db_sync_token_after_success().is_none());
            assert_eq!(
                db.get_metadata(&key).await.unwrap().as_deref(),
                Some("v27-opaque-cursor")
            );
            assert_eq!(
                db.get_metadata("sync_token:PrimarySync")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("rollback-old-cursor")
            );
            let status: String = conn
                .query_row(
                    "SELECT status FROM assets WHERE library='PrimarySync' AND id='selected'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(status, if failed { "failed" } else { "pending" });
            if !failed {
                let reason:String=conn.query_row("SELECT reason FROM asset_verifications WHERE library='PrimarySync' AND id='selected'",[],|r|r.get(0)).unwrap();
                assert_eq!(reason, "baseline_match_uncertain");
            }
        }
    }
}
