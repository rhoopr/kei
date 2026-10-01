//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::sync::Arc;

use crate::state::db::{
    ReconciliationContent, ReconciliationPathKey, ReconciliationReservation,
    ReconciliationStateStore, SqliteStateDb,
};
use crate::state::types::VersionSizeKey;

#[tokio::test]
async fn reconciliation_reservations_round_trip_and_reject_conflicts_atomically() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("state.db");
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    let destination = root.path().join("photo-é.jpg");
    let reservation = ReconciliationReservation {
        content: Some(ReconciliationContent {
            checksum: "provider".into(),
            size: 1024,
        }),
        library: Arc::from("PrimarySync"),
        asset_id: "asset".into(),
        version_size: VersionSizeKey::Original,
        requested_path_key: ReconciliationPathKey("requested-photo".into()),
        destination_path_key: ReconciliationPathKey(destination.to_str().unwrap().into()),
        destination_path: destination,
    };
    db.reserve_reconciliation_paths(std::slice::from_ref(&reservation))
        .await
        .unwrap();
    db.reserve_reconciliation_paths(std::slice::from_ref(&reservation))
        .await
        .unwrap();
    drop(db);
    let db = SqliteStateDb::open(&db_path).await.unwrap();
    assert_eq!(
        db.get_reconciliation_reservations().await.unwrap(),
        vec![reservation.clone()]
    );

    let mut new_choice = reservation.clone();
    new_choice.requested_path_key = ReconciliationPathKey("new-request".into());
    new_choice.destination_path_key = ReconciliationPathKey("new-destination".into());
    new_choice.destination_path = root.path().join("new.jpg");
    let mut foreign = reservation.clone();
    foreign.library = Arc::from("SharedSync");
    assert!(
        db.reserve_reconciliation_paths(&[new_choice.clone(), foreign])
            .await
            .is_err()
    );
    assert_eq!(
        db.get_reconciliation_reservations().await.unwrap(),
        vec![reservation.clone()],
        "failed batches leave no partial claims"
    );

    let mut changed_choice = new_choice;
    changed_choice.requested_path_key = reservation.requested_path_key.clone();
    assert!(
        db.reserve_reconciliation_paths(&[changed_choice])
            .await
            .is_err()
    );
    let mut foreign_version = reservation.clone();
    foreign_version.version_size = VersionSizeKey::Medium;
    assert!(
        db.reserve_reconciliation_paths(&[foreign_version])
            .await
            .is_err()
    );
    assert_eq!(
        db.get_reconciliation_reservations().await.unwrap(),
        vec![reservation.clone()]
    );

    // A later template can request a path already owned by this rendition.
    // Retain both request mappings without weakening foreign-owner checks.
    let mut changed_template = reservation;
    changed_template.requested_path_key = ReconciliationPathKey("changed-template".into());
    db.reserve_reconciliation_paths(&[changed_template])
        .await
        .unwrap();
    assert_eq!(db.get_reconciliation_reservations().await.unwrap().len(), 2);
}

#[tokio::test]
async fn reconciliation_reservations_keep_each_content_generation_immutable() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("state.db");
    let db = SqliteStateDb::open(&path).await.unwrap();
    let legacy = ReconciliationReservation {
        library: Arc::from("PrimarySync"),
        asset_id: "asset".into(),
        version_size: VersionSizeKey::Adjusted,
        content: None,
        requested_path_key: ReconciliationPathKey("requested".into()),
        destination_path_key: ReconciliationPathKey("legacy".into()),
        destination_path: root.path().join("legacy.jpg"),
    };
    db.reserve_reconciliation_paths(std::slice::from_ref(&legacy))
        .await
        .unwrap();
    let mut choices = vec![legacy.clone()];
    for (checksum, size) in [("first", 1024), ("second", 1024), ("second", 2048)] {
        let mut choice = legacy.clone();
        choice.content = Some(ReconciliationContent {
            checksum: checksum.into(),
            size,
        });
        // Neither a legacy claim nor an older known generation is reusable.
        for old in &choices {
            choice.destination_path_key = old.destination_path_key.clone();
            choice.destination_path = old.destination_path.clone();
            assert!(
                db.reserve_reconciliation_paths(std::slice::from_ref(&choice))
                    .await
                    .is_err()
            );
        }
        choice.destination_path = root.path().join(format!("{checksum}-{size}.jpg"));
        choice.destination_path_key =
            ReconciliationPathKey(choice.destination_path.to_str().unwrap().into());
        db.reserve_reconciliation_paths(std::slice::from_ref(&choice))
            .await
            .unwrap();
        db.reserve_reconciliation_paths(std::slice::from_ref(&choice))
            .await
            .unwrap();
        let mut moved = choice.clone();
        moved.destination_path_key = ReconciliationPathKey("different".into());
        moved.destination_path = root.path().join("different.jpg");
        assert!(db.reserve_reconciliation_paths(&[moved]).await.is_err());
        choices.push(choice);
    }
    let mut oversized = legacy;
    oversized.content = Some(ReconciliationContent {
        checksum: "oversized".into(),
        size: u64::MAX,
    });
    assert!(db.reserve_reconciliation_paths(&[oversized]).await.is_err());
    drop(db);
    let db = SqliteStateDb::open(&path).await.unwrap();
    let saved = db.get_reconciliation_reservations().await.unwrap();
    assert_eq!(saved.len(), choices.len());
    assert!(choices.iter().all(|choice| saved.contains(choice)));
}
