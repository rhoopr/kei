//! Tests moved from `state::db::tests`, with their original names and assertions.
use std::collections::HashSet;

use crate::state::db::SqliteStateDb;

#[tokio::test]
async fn asset_master_mapping_is_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    db.upsert_asset_master_mapping("PrimarySync", "asset-a", "master-primary")
        .await
        .unwrap();
    db.upsert_asset_master_mapping("SharedSync-AAAA", "asset-a", "master-shared")
        .await
        .unwrap();

    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-a")
            .await
            .unwrap()
            .as_deref(),
        Some("master-primary")
    );
    assert_eq!(
        db.get_master_record_name_for_asset("SharedSync-AAAA", "asset-a")
            .await
            .unwrap()
            .as_deref(),
        Some("master-shared")
    );
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "missing")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db.get_asset_master_mappings().await.unwrap(),
        HashSet::from([
            (
                "PrimarySync".to_string(),
                "asset-a".to_string(),
                "master-primary".to_string(),
            ),
            (
                "SharedSync-AAAA".to_string(),
                "asset-a".to_string(),
                "master-shared".to_string(),
            ),
        ])
    );
}

#[tokio::test]
async fn legacy_master_state_owner_claim_is_atomic_and_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();

    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "master", "asset-a")
            .await
            .unwrap()
    );
    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "master", "asset-a")
            .await
            .unwrap()
    );
    assert!(
        !db.claim_legacy_master_state_owner("PrimarySync", "master", "asset-b")
            .await
            .unwrap()
    );
    assert!(
        db.claim_legacy_master_state_owner("SharedSync-AAAA", "master", "asset-b")
            .await
            .unwrap()
    );
    assert_eq!(
        db.get_legacy_master_state_owners().await.unwrap(),
        HashSet::from([
            (
                "PrimarySync".to_string(),
                "master".to_string(),
                "asset-a".to_string(),
            ),
            (
                "SharedSync-AAAA".to_string(),
                "master".to_string(),
                "asset-b".to_string(),
            ),
        ])
    );
}

#[tokio::test]
async fn asset_master_mapping_backfill_uses_unambiguous_album_history() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (library, container) in [
        ("PrimarySync", "album-a"),
        ("PrimarySync", "album-b"),
        ("SharedSync-AAAA", "album-a"),
    ] {
        db.upsert_album_container(library, container, "Album", "album")
            .await
            .unwrap();
    }

    for (library, container, asset, master) in [
        ("PrimarySync", "album-a", "asset-a", Some("master-a")),
        ("PrimarySync", "album-b", "asset-a", Some("master-a")),
        (
            "SharedSync-AAAA",
            "album-a",
            "asset-a",
            Some("master-shared"),
        ),
        (
            "PrimarySync",
            "album-a",
            "asset-ambiguous",
            Some("master-one"),
        ),
        (
            "PrimarySync",
            "album-b",
            "asset-ambiguous",
            Some("master-two"),
        ),
        ("PrimarySync", "album-a", "asset-missing", None),
    ] {
        db.upsert_album_membership_delta(library, container, asset, master, "icloud")
            .await
            .unwrap();
    }
    db.mark_album_membership_deleted("PrimarySync", "album-b", "asset-a")
        .await
        .unwrap();

    assert_eq!(
        db.backfill_asset_master_mappings_from_album_memberships()
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-a")
            .await
            .unwrap()
            .as_deref(),
        Some("master-a")
    );
    assert_eq!(
        db.get_master_record_name_for_asset("SharedSync-AAAA", "asset-a")
            .await
            .unwrap()
            .as_deref(),
        Some("master-shared")
    );
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-ambiguous")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db.get_master_record_name_for_asset("PrimarySync", "asset-missing")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        db.backfill_asset_master_mappings_from_album_memberships()
            .await
            .unwrap(),
        0
    );
}
