//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::path::PathBuf;
use std::sync::Arc;

use crate::state::db::{
    ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    ReconciliationStateStore, SqliteStateDb,
};
use crate::state::types::{AssetRecord, AssetStatus, VersionSizeKey};
use crate::test_helpers::TestAssetRecord;

// ── import_adopt: atomic upsert + mark-downloaded ───────────────────

#[tokio::test]
async fn import_adopt_respects_reservation_ownership() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("photo.jpg");
    let owner = TestAssetRecord::new("owner")
        .checksum("provider")
        .size(1024)
        .build();
    let reservation = ReconciliationReservation {
        content: Some(ReconciliationContent {
            checksum: owner.checksum.clone(),
            size: owner.size_bytes,
        }),
        library: owner.library.clone(),
        asset_id: owner.id.clone(),
        version_size: owner.version_size,
        requested_path_key: ReconciliationPathKey("requested".into()),
        destination_path_key: ReconciliationPathKey(
            crate::fs_util::confined_path_key(&destination).unwrap(),
        ),
        destination_path: destination.clone(),
    };
    let candidates = [
        owner.clone(),
        AssetRecord {
            id: "other".into(),
            ..owner.clone()
        },
        AssetRecord {
            library: Arc::from("SharedSync"),
            ..owner.clone()
        },
        AssetRecord {
            version_size: VersionSizeKey::Medium,
            ..owner.clone()
        },
        AssetRecord {
            checksum: "changed".into(),
            ..owner.clone()
        },
        AssetRecord {
            size_bytes: 2048,
            ..owner.clone()
        },
        AssetRecord {
            size_bytes: u64::MAX,
            ..owner.clone()
        },
    ];
    for legacy in [false, true] {
        for (index, candidate) in candidates.iter().enumerate() {
            let db = SqliteStateDb::open_in_memory().unwrap();
            let mut reservation = reservation.clone();
            if legacy {
                reservation.content = None;
            }
            db.reserve_reconciliation_paths(std::slice::from_ref(&reservation))
                .await
                .unwrap();
            let allowed = index == 0 && !legacy;
            // Equivalent root spelling must not bypass the ownership check.
            let alias = root.path().join(".").join("photo.jpg");
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            let alias = alias.with_file_name("PHOTO.JPG");
            let adopted = db
                .import_adopt(candidate, &alias, "local", 1024, Some(1))
                .await;
            assert_eq!(
                adopted.is_ok(),
                allowed,
                "legacy={legacy}, candidate={index}"
            );
            assert_eq!(
                db.get_summary().await.unwrap().total_assets,
                u64::from(allowed)
            );
            assert_eq!(
                db.get_reconciliation_catalog_paths().await.unwrap().len(),
                usize::from(allowed)
            );
            assert_eq!(
                db.get_reconciliation_reservations().await.unwrap(),
                vec![reservation]
            );
            if allowed {
                db.import_adopt(candidate, &alias, "local", 1024, Some(1))
                    .await
                    .unwrap();
                assert_eq!(db.get_summary().await.unwrap().total_assets, 1);
            } else {
                // Refusal must also preserve an existing imported row and its historical path.
                let previous = root.path().join("previous.jpg");
                db.import_adopt(candidate, &previous, "previous-hash", 1024, Some(2))
                    .await
                    .unwrap();
                assert!(
                    db.import_adopt(candidate, &alias, "replacement", 1024, Some(3))
                        .await
                        .is_err()
                );
                let rows = db.get_downloaded_page(0, 10).await.unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].local_path.as_ref(), Some(&previous));
                assert_eq!(rows[0].local_checksum.as_deref(), Some("previous-hash"));
                let paths = db.get_reconciliation_catalog_paths().await.unwrap();
                assert_eq!(paths.len(), 1);
                assert_eq!(paths[0].path, previous);
            }
        }
    }
}

#[tokio::test]
async fn import_adopt_persists_downloaded_row_in_one_call() {
    // The whole point of import_adopt: one transactional call that
    // leaves the row fully `downloaded` with `local_path` set. No
    // separate upsert_seen + mark_downloaded sequence at the call site.
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ADOPT_ONE")
        .checksum("ck_adopt")
        .filename("photo.jpg")
        .size(2048)
        .build();
    let local_path = PathBuf::from("/tmp/photos/photo.jpg");

    db.import_adopt(
        &record,
        &local_path,
        "local-ck-abc",
        2048,
        Some(1_700_000_000),
    )
    .await
    .expect("import_adopt should succeed on a fresh row");

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 1);
    assert_eq!(summary.downloaded, 1);
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.failed, 0);

    let pages = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(pages.len(), 1);
    let row = &pages[0];
    assert_eq!(&*row.id, "ADOPT_ONE");
    assert_eq!(row.status, AssetStatus::Downloaded);
    assert_eq!(row.local_path.as_deref(), Some(local_path.as_path()));
    assert_eq!(row.local_checksum.as_deref(), Some("local-ck-abc"));
}

#[tokio::test]
async fn import_adopt_is_idempotent_on_repeat_calls() {
    // Re-running an import scan over the same on-disk files must not
    // duplicate rows or drop the downloaded state — the second adopt
    // should see the existing downloaded row and leave it healthy.
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ADOPT_IDEMP")
        .checksum("ck_idemp")
        .filename("idemp.jpg")
        .size(1024)
        .build();
    let local_path = PathBuf::from("/tmp/photos/idemp.jpg");

    for _ in 0..2 {
        db.import_adopt(
            &record,
            &local_path,
            "local-ck-idemp",
            1024,
            Some(1_700_000_001),
        )
        .await
        .expect("import_adopt should be idempotent");
    }

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.total_assets, 1, "no duplicate row");
    assert_eq!(summary.downloaded, 1);
}

#[tokio::test]
async fn import_adopt_promotes_existing_pending_row_to_downloaded() {
    // A prior interrupted scan may have left a pending row (pre-PR-5
    // behavior). Re-running the import must recover by upserting the
    // metadata and flipping the row to downloaded in one transaction.
    let db = SqliteStateDb::open_in_memory().unwrap();

    let record = TestAssetRecord::new("ADOPT_RESUME")
        .checksum("ck_resume")
        .filename("resume.jpg")
        .size(4096)
        .build();
    db.upsert_seen(&record).await.unwrap();

    // Sanity: row is pending with no local_path.
    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 1);
    assert_eq!(summary.downloaded, 0);

    let local_path = PathBuf::from("/tmp/photos/resume.jpg");
    db.import_adopt(
        &record,
        &local_path,
        "local-ck-resume",
        4096,
        Some(1_700_000_002),
    )
    .await
    .expect("import_adopt should promote pending → downloaded");

    let summary = db.get_summary().await.unwrap();
    assert_eq!(summary.pending, 0);
    assert_eq!(summary.downloaded, 1);

    let pages = db.get_downloaded_page(0, 10).await.unwrap();
    assert_eq!(pages[0].local_path.as_deref(), Some(local_path.as_path()));
    assert_eq!(pages[0].local_checksum.as_deref(), Some("local-ck-resume"));
}
