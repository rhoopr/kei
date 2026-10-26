use super::{asset_record_for_derived_path, validate_enumerated_reserved_pending};
use crate::download::baseline_matching::{BaselineMatch, commit_match};
use crate::download::filter::derive_expected_paths;
use crate::download::planner::TaskPlanner;
use crate::download::{DownloadConfig, DownloadContext};
use crate::state::SqliteStateDb;
use crate::state::db::{
    DownloadStateStore, ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    ReconciliationStateStore,
};
use crate::test_helpers::TestPhotoAsset;
use sha1::{Digest, Sha1};
use sha2::Sha256;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn v027_cross_parent_validation_defers_content_proof_to_atomic_matcher() {
    for stale_history in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let db = Arc::new(SqliteStateDb::open_in_memory().unwrap());
        let original = b"raw-provider-bytes";
        let mut provider = Sha1::new();
        provider.update(b"com.apple.XattrObjectSalt\0com.apple.DataObjectSalt\0");
        provider.update(original);
        let signature =
            data_encoding::BASE64.encode(&[vec![1], provider.finalize().to_vec()].concat());
        let asset = TestPhotoAsset::new("cross-parent")
            .filename("photo.JPG")
            .orig_size(original.len() as u64)
            .orig_checksum(&signature)
            .build();
        let mut config = DownloadConfig::test_default();
        config.directory = root.path().to_path_buf().into();
        config.folder_structure = "new".into();
        config.state_db = Some(db.clone());
        let derived = derive_expected_paths(&asset, &config).remove(0);
        let old = root.path().join("old/photo.JPG");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, original).unwrap();
        let record =
            asset_record_for_derived_path(config.library.clone(), &asset, &derived, &config);
        db.upsert_seen(&record).await.unwrap();
        let source = format!("{:x}", Sha256::digest(original));
        if stale_history {
            db.mark_verified_download(
                &record.library,
                &record.id,
                record.version_size.as_str(),
                &old,
                &source,
                Some(&source),
                false,
            )
            .await
            .unwrap();
            // Model the old generic writer replacing the local hash while
            // retaining unrelated genuine source provenance.
            let wrong = vec![b'x'; original.len()];
            std::fs::write(&old, &wrong).unwrap();
            let local = format!("{:x}", Sha256::digest(&wrong));
            db.mark_downloaded(
                &record.library,
                &record.id,
                record.version_size.as_str(),
                &old,
                &local,
                Some(&source),
            )
            .await
            .unwrap();
        }
        db.acquire_lock("retain cross-parent pending owner")
            .unwrap()
            .execute(
                "UPDATE assets SET status='pending' WHERE library=?1 AND id=?2",
                rusqlite::params![record.library.as_ref(), record.id.as_ref()],
            )
            .unwrap();
        db.reserve_reconciliation_paths(&[ReconciliationReservation {
            library: record.library.clone(),
            asset_id: record.id.clone(),
            version_size: record.version_size,
            requested_path_key: ReconciliationPathKey(
                crate::fs_util::confined_path_key(&derived.path).unwrap(),
            ),
            destination_path_key: ReconciliationPathKey(
                crate::fs_util::confined_path_key(&old).unwrap(),
            ),
            destination_path: old.clone(),
            content: Some(ReconciliationContent {
                checksum: record.checksum.clone(),
                size: record.size_bytes,
            }),
        }])
        .await
        .unwrap();
        let before = std::fs::read(&old).unwrap();
        let mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
        let ctx = DownloadContext::load(db.as_ref(), false).await;
        let mut planner = TaskPlanner::for_download(Some(db.as_ref())).await.unwrap();
        assert!(
            validate_enumerated_reserved_pending(db.as_ref(), &config, &asset, &ctx, &planner)
                .await
                .unwrap(),
            "ownership validation must not require historical byte receipts"
        );
        let plan = planner
            .plan_pending_retry_asset(&asset, &config)
            .await
            .unwrap();
        let task = plan
            .tasks
            .iter()
            .find(|task| task.version_size == record.version_size)
            .unwrap();
        assert_eq!(
            task.download_path, old,
            "replay keeps the recorded native destination"
        );
        let decision = planner.match_existing(&asset, &config, task).await;
        if stale_history {
            assert!(matches!(decision, BaselineMatch::Uncertain(_)));
            let pending: i64 = db
                .acquire_lock("uncertain remains pending")
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM assets WHERE status='pending'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(pending, 1);
        } else {
            assert!(matches!(decision, BaselineMatch::ProviderPresent { .. }));
            // A changed retained file cannot finalize, even after a positive match.
            std::fs::write(&old, vec![b'z'; original.len()]).unwrap();
            assert!(
                commit_match(decision, &config, &asset, task, &CancellationToken::new())
                    .await
                    .is_err()
            );
            let pending: i64 = db
                .acquire_lock("failed callback retains owner")
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM assets WHERE status='pending'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(pending, 1);
            std::fs::write(&old, original).unwrap();
            let retry = planner.match_existing(&asset, &config, task).await;
            assert!(matches!(retry, BaselineMatch::ProviderPresent { .. }));
            let retry_mtime = std::fs::metadata(&old).unwrap().modified().unwrap();
            commit_match(retry, &config, &asset, task, &CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(
                std::fs::metadata(&old).unwrap().modified().unwrap(),
                retry_mtime
            );
        }
        assert_eq!(std::fs::read(&old).unwrap(), before);
        if stale_history {
            assert_eq!(std::fs::metadata(&old).unwrap().modified().unwrap(), mtime);
        }
        assert!(
            !derived.path.exists(),
            "proof must not create a replacement destination"
        );
    }
}
