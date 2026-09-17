//! Tests moved from `state::db::tests`, with their original names and assertions.
use crate::state::db::SqliteStateDb;
use crate::state::db::membership::ALBUM_GROUPING_STATE_IDS_SQL;
use crate::test_helpers::TestAssetRecord;

#[tokio::test]
async fn album_grouping_projection_has_bounded_sql_work() {
    use rusqlite::StatementStatus;

    const SINGLE_MEMBER_STEP_LIMIT: i32 = 300;
    const STEPS_PER_MEMBER_LIMIT: i32 = 150;
    for members in [128, 512] {
        let db = SqliteStateDb::open_in_memory().unwrap();
        db.upsert_album_container("PrimarySync", "trip", "Trip", "album")
            .await
            .unwrap();
        {
            let mut conn = db.acquire_lock("seed_populated_album").unwrap();
            let tx = conn.transaction().unwrap();
            for index in 0..members {
                let child = format!("child-{index}");
                let master = format!("master-{index}");
                for id in [&child, &master] {
                    tx.execute(
                        "INSERT INTO assets (library, id, version_size, checksum, filename, created_at, size_bytes, media_type, status, last_seen_at) VALUES ('PrimarySync', ?1, 'original', 'provider', 'image.jpg', 0, 10, 'photo', 'downloaded', 0)",
                        [id],
                    ).unwrap();
                }
                tx.execute(
                    "INSERT INTO legacy_master_state_owners VALUES ('PrimarySync', ?1, ?2, 0)",
                    [&master, &child],
                )
                .unwrap();
                // Missing master hints must not hide the durable legacy owner.
                tx.execute("INSERT INTO asset_album_memberships VALUES ('PrimarySync', ?1, NULL, 'trip', 1, 0, 'icloud', 0)", [&child]).unwrap();
            }
            tx.commit().unwrap();
        }
        let steps = || {
            db.acquire_lock("projection_sql_work")
                .unwrap()
                .prepare_cached(ALBUM_GROUPING_STATE_IDS_SQL)
                .unwrap()
                .reset_status(StatementStatus::VmStep)
        };
        let dirty = || {
            let conn = db.acquire_lock("projection_dirty_rows").unwrap();
            conn.query_row(
                "SELECT COUNT(*) FROM assets WHERE metadata_write_failed_at IS NOT NULL",
                [],
                |row| row.get::<_, i32>(0),
            )
            .unwrap()
        };
        steps();
        for expected_dirty in [2, 0] {
            db.upsert_album_membership_delta("PrimarySync", "trip", "child-0", None, "icloud")
                .await
                .unwrap();
            let work = steps();
            assert!(
                work > 0 && work <= SINGLE_MEMBER_STEP_LIMIT,
                "{members} members: {work} single-member VM steps"
            );
            assert_eq!(
                db.get_asset_groupings("PrimarySync", &["child-0", "master-0"])
                    .await
                    .unwrap()
                    .albums,
                [
                    ("child-0".into(), "Trip".into()),
                    ("master-0".into(), "Trip".into())
                ]
            );
            assert_eq!(dirty(), expected_dirty);
            for id in ["child-0", "master-0"] {
                db.clear_metadata_write_failure("PrimarySync", id, "original")
                    .await
                    .unwrap();
            }
        }
        for expected_dirty in [members * 2, 0] {
            db.upsert_album_container("PrimarySync", "trip", "Renamed", "album")
                .await
                .unwrap();
            let work = steps();
            assert!(
                work > 0 && work <= members * STEPS_PER_MEMBER_LIMIT,
                "{members} members: {work} container VM steps"
            );
            let groups = db.get_all_asset_albums("PrimarySync").await.unwrap();
            assert_eq!(groups.len(), usize::try_from(members * 2).unwrap());
            assert!(groups.iter().all(|(_, name)| name == "Renamed"));
            assert_eq!(
                dirty(),
                expected_dirty,
                "unchanged projection must not queue work"
            );
            db.acquire_lock("clear_projection_debt")
                .unwrap()
                .execute("UPDATE assets SET metadata_write_failed_at = NULL", [])
                .unwrap();
        }
        assert_eq!(dirty(), 0);
    }
}

#[tokio::test]
async fn album_grouping_projection_respects_legacy_state_ownership() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for id in ["master", "child-b"] {
        db.upsert_seen(&TestAssetRecord::new(id).build())
            .await
            .unwrap();
    }
    assert!(
        db.claim_legacy_master_state_owner("PrimarySync", "master", "child-a")
            .await
            .unwrap()
    );
    db.upsert_album_container("PrimarySync", "trip", "Trip", "album")
        .await
        .unwrap();
    db.upsert_album_membership_delta("PrimarySync", "trip", "child-b", Some("master"), "icloud")
        .await
        .unwrap();
    assert!(
        db.get_asset_groupings("PrimarySync", &["master"])
            .await
            .unwrap()
            .albums
            .is_empty()
    );
    assert_eq!(
        db.get_asset_groupings("PrimarySync", &["child-b"])
            .await
            .unwrap()
            .albums,
        [("child-b".into(), "Trip".into())]
    );
    db.upsert_album_membership_delta("PrimarySync", "trip", "child-a", Some("master"), "icloud")
        .await
        .unwrap();
    assert_eq!(
        db.get_asset_groupings("PrimarySync", &["master"])
            .await
            .unwrap()
            .albums,
        [("master".into(), "Trip".into())]
    );
    db.mark_album_membership_deleted("PrimarySync", "trip", "child-b")
        .await
        .unwrap();
    assert!(
        db.get_asset_groupings("PrimarySync", &["child-b"])
            .await
            .unwrap()
            .albums
            .is_empty()
    );
    assert_eq!(
        db.get_asset_groupings("PrimarySync", &["master"])
            .await
            .unwrap()
            .albums,
        [("master".into(), "Trip".into())]
    );
}

#[tokio::test]
async fn album_grouping_mutations_keep_atomic_retry_evidence() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (library, id) in [
        ("PrimarySync", "child"),
        ("PrimarySync", "sibling"),
        ("SharedSync", "child"),
    ] {
        db.upsert_seen(&TestAssetRecord::new(id).library(library).build())
            .await
            .unwrap();
    }
    db.add_asset_album("PrimarySync", "child", "External", "external-import")
        .await
        .unwrap();
    db.upsert_album_container("PrimarySync", "trip", "Trip", "album")
        .await
        .unwrap();
    let generation = db
        .start_album_membership_snapshot("PrimarySync", "trip", None)
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "trip",
        generation,
        "child",
        Some("master"),
        "icloud",
    )
    .await
    .unwrap();
    let groups = db
        .get_asset_groupings("PrimarySync", &["child"])
        .await
        .unwrap();
    assert_eq!(
        groups.albums,
        [
            ("child".into(), "External".into()),
            ("child".into(), "Trip".into())
        ]
    );
    db.clear_metadata_write_failure("PrimarySync", "child", "original")
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "trip",
        generation,
        "child",
        Some("master"),
        "icloud",
    )
    .await
    .unwrap();
    db.complete_album_membership_snapshot("PrimarySync", "trip", generation)
        .await
        .unwrap();
    let dirty = || {
        db.acquire_lock("test_grouping_markers")
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM assets WHERE metadata_write_failed_at IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    assert_eq!(dirty(), 0, "replayed membership must not queue a rewrite");
    db.acquire_lock("test_grouping_marker_failure").unwrap().execute_batch(
        "CREATE TEMP TRIGGER fail_grouping_marker BEFORE UPDATE OF metadata_write_failed_at ON assets \
             BEGIN SELECT RAISE(ABORT, 'grouping marker failure'); END;",
    ).unwrap();
    assert!(
        db.mark_album_membership_deleted("PrimarySync", "trip", "child")
            .await
            .is_err()
    );
    assert_eq!(
        db.get_asset_groupings("PrimarySync", &["child"])
            .await
            .unwrap()
            .albums,
        groups.albums
    );
    assert_eq!(
        db.get_live_selected_album_memberships_for_asset("PrimarySync", "child", &["trip"])
            .await
            .unwrap()
            .len(),
        1
    );
    db.acquire_lock("test_grouping_marker_recovery")
        .unwrap()
        .execute_batch("DROP TRIGGER fail_grouping_marker")
        .unwrap();
    db.mark_album_membership_deleted("PrimarySync", "trip", "child")
        .await
        .unwrap();
    assert_eq!(
        dirty(),
        1,
        "only the changed child in this library is dirty"
    );
    assert_eq!(
        db.get_asset_groupings("PrimarySync", &["child"])
            .await
            .unwrap()
            .albums,
        [("child".into(), "External".into())]
    );
    db.clear_metadata_write_failure("PrimarySync", "child", "original")
        .await
        .unwrap();
    db.mark_album_membership_deleted("PrimarySync", "trip", "child")
        .await
        .unwrap();
    assert_eq!(dirty(), 0, "replayed tombstones are a no-op");

    db.upsert_album_membership_delta("PrimarySync", "trip", "child", Some("master"), "icloud")
        .await
        .unwrap();
    db.clear_metadata_write_failure("PrimarySync", "child", "original")
        .await
        .unwrap();
    let next = db
        .start_album_membership_snapshot("PrimarySync", "trip", None)
        .await
        .unwrap();
    assert_eq!(
        db.get_asset_groupings("PrimarySync", &["child"])
            .await
            .unwrap()
            .albums,
        groups.albums,
        "an interrupted empty snapshot cannot remove the prior membership"
    );
    assert_eq!(dirty(), 0);
    db.complete_album_membership_snapshot("PrimarySync", "trip", next)
        .await
        .unwrap();
    assert_eq!(
        db.get_asset_groupings("PrimarySync", &["child"])
            .await
            .unwrap()
            .albums,
        [("child".into(), "External".into())]
    );
    assert_eq!(
        dirty(),
        1,
        "completed snapshot removal must queue a rewrite"
    );
    db.clear_metadata_write_failure("PrimarySync", "child", "original")
        .await
        .unwrap();
    let steady = db
        .start_album_membership_snapshot("PrimarySync", "trip", None)
        .await
        .unwrap();
    db.complete_album_membership_snapshot("PrimarySync", "trip", steady)
        .await
        .unwrap();
    assert_eq!(dirty(), 0);
}

#[tokio::test]
async fn add_asset_album_is_idempotent() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
        .await
        .unwrap();
    db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
        .await
        .unwrap();
    let conn = db.acquire_lock("test").unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_albums WHERE asset_id = 'A1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn add_asset_album_respects_source_namespace() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
        .await
        .unwrap();
    db.add_asset_album("PrimarySync", "A1", "Favorites", "external-import")
        .await
        .unwrap();
    let conn = db.acquire_lock("test").unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_albums WHERE asset_id = 'A1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
}

/// v9 PK adds `library`: same `(asset_id, album_name, source)` triple
/// in two libraries must round-trip as two distinct rows. Pre-v9 this
/// silently collapsed via `INSERT OR IGNORE`. Assertions filter by
/// `library` so a regression that wrote both rows under the same zone
/// (the exact bug v9 prevents) cannot pass with COUNT(*) = 2.
#[tokio::test]
async fn add_asset_album_keeps_distinct_rows_per_library() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.add_asset_album("PrimarySync", "SHARED_ID", "Favorites", "icloud")
        .await
        .unwrap();
    db.add_asset_album("SharedSync-A1B2C3D4", "SHARED_ID", "Favorites", "icloud")
        .await
        .unwrap();
    let conn = db.acquire_lock("test").unwrap();
    let primary_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_albums \
                 WHERE asset_id = 'SHARED_ID' AND library = 'PrimarySync'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let shared_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM asset_albums \
                 WHERE asset_id = 'SHARED_ID' AND library = 'SharedSync-A1B2C3D4'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        primary_count, 1,
        "PrimarySync must hold exactly one row for SHARED_ID"
    );
    assert_eq!(
        shared_count, 1,
        "SharedSync-A1B2C3D4 must hold exactly one row for SHARED_ID"
    );
}

#[tokio::test]
async fn get_all_asset_people_returns_every_pair() {
    // asset_people has no production writer yet; insert test rows via raw
    // SQL so this covers the read path without adding a trait method that
    // would sit unused in production builds.
    let db = SqliteStateDb::open_in_memory().unwrap();
    {
        let conn = db.acquire_lock("seed").unwrap();
        for (aid, person) in [("A1", "Alice"), ("A1", "Bob"), ("A2", "Alice")] {
            conn.execute(
                "INSERT INTO asset_people (library, asset_id, person_name) \
                     VALUES ('PrimarySync', ?1, ?2)",
                rusqlite::params![aid, person],
            )
            .unwrap();
        }
    }
    let rows = db.get_all_asset_people("PrimarySync").await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.contains(&("A1".into(), "Alice".into())));
    assert!(rows.contains(&("A1".into(), "Bob".into())));
    assert!(rows.contains(&("A2".into(), "Alice".into())));
}

#[tokio::test]
async fn get_all_asset_albums_returns_every_pair() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.add_asset_album("PrimarySync", "A1", "Favorites", "icloud")
        .await
        .unwrap();
    db.add_asset_album("PrimarySync", "A1", "Trip", "icloud")
        .await
        .unwrap();
    db.add_asset_album("PrimarySync", "A2", "Favorites", "icloud")
        .await
        .unwrap();
    let rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows.contains(&("A1".into(), "Favorites".into())));
    assert!(rows.contains(&("A1".into(), "Trip".into())));
    assert!(rows.contains(&("A2".into(), "Favorites".into())));
}

/// Reads must also be library-scoped: a SharedSync row with the same
/// asset_id must NOT bleed into PrimarySync's grouping load.
#[tokio::test]
async fn get_all_asset_albums_is_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.add_asset_album("PrimarySync", "ID", "Vacation", "icloud")
        .await
        .unwrap();
    db.add_asset_album("SharedSync-AB", "ID", "Family", "icloud")
        .await
        .unwrap();

    let primary = db.get_all_asset_albums("PrimarySync").await.unwrap();
    let shared = db.get_all_asset_albums("SharedSync-AB").await.unwrap();
    assert_eq!(primary, vec![("ID".into(), "Vacation".into())]);
    assert_eq!(shared, vec![("ID".into(), "Family".into())]);
}

#[tokio::test]
async fn get_asset_groupings_is_bounded_and_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (library, asset_id, album) in [
        ("PrimarySync", "TARGET", "Vacation"),
        ("PrimarySync", "OTHER", "Work"),
        ("SharedSync-AB", "TARGET", "Family"),
    ] {
        db.add_asset_album(library, asset_id, album, "icloud")
            .await
            .unwrap();
    }
    {
        let conn = db.acquire_lock("seed people").unwrap();
        for (library, asset_id, person) in [
            ("PrimarySync", "TARGET", "Alice"),
            ("PrimarySync", "OTHER", "Bob"),
            ("SharedSync-AB", "TARGET", "Casey"),
        ] {
            conn.execute(
                "INSERT INTO asset_people (library, asset_id, person_name) \
                     VALUES (?1, ?2, ?3)",
                rusqlite::params![library, asset_id, person],
            )
            .unwrap();
        }
    }

    let rows = db
        .get_asset_groupings("PrimarySync", &["TARGET"])
        .await
        .unwrap();
    assert_eq!(rows.albums, vec![("TARGET".into(), "Vacation".into())]);
    assert_eq!(rows.people, vec![("TARGET".into(), "Alice".into())]);
}

#[tokio::test]
async fn album_membership_snapshot_complete_prunes_only_own_container() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (library, container, album) in [
        ("PrimarySync", "container-a", "Vacation"),
        ("PrimarySync", "container-b", "Family"),
        ("SharedSync-AB", "container-a", "Shared Vacation"),
    ] {
        db.upsert_album_container(library, container, album, "album")
            .await
            .unwrap();
        let gen1 = db
            .start_album_membership_snapshot(library, container, Some("hash-1"))
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            library,
            container,
            gen1,
            "asset-record-old",
            Some("master-old"),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot(library, container, gen1)
            .await
            .unwrap();
    }

    let gen2 = db
        .start_album_membership_snapshot("PrimarySync", "container-a", Some("hash-2"))
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "container-a",
        gen2,
        "asset-record-new",
        Some("master-new"),
        "icloud",
    )
    .await
    .unwrap();
    db.complete_album_membership_snapshot("PrimarySync", "container-a", gen2)
        .await
        .unwrap();

    let primary_a_old = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-record-old",
            &["container-a"],
        )
        .await
        .unwrap();
    assert!(
        primary_a_old.is_empty(),
        "completing generation 2 must prune stale generation 1 rows for the same container",
    );

    let primary_b_old = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-record-old",
            &["container-b"],
        )
        .await
        .unwrap();
    assert_eq!(
        primary_b_old.len(),
        1,
        "pruning container-a must not delete container-b memberships",
    );

    let shared_old = db
        .get_live_selected_album_memberships_for_asset(
            "SharedSync-AB",
            "asset-record-old",
            &["container-a"],
        )
        .await
        .unwrap();
    assert_eq!(
        shared_old.len(),
        1,
        "pruning PrimarySync must not delete another library's same container id",
    );
}

#[tokio::test]
async fn incomplete_album_snapshot_leaves_previous_complete_snapshot_trusted() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
        .await
        .unwrap();
    let gen1 = db
        .start_album_membership_snapshot("PrimarySync", "container-a", Some("hash-1"))
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "container-a",
        gen1,
        "asset-record-old",
        Some("master-old"),
        "icloud",
    )
    .await
    .unwrap();
    db.complete_album_membership_snapshot("PrimarySync", "container-a", gen1)
        .await
        .unwrap();

    let gen2 = db
        .start_album_membership_snapshot("PrimarySync", "container-a", Some("hash-2"))
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "container-a",
        gen2,
        "asset-record-new",
        Some("master-new"),
        "icloud",
    )
    .await
    .unwrap();

    assert!(
        db.selected_album_containers_have_complete_snapshots("PrimarySync", &["container-a"])
            .await
            .unwrap(),
        "a running replacement snapshot must not hide the previous complete generation",
    );
    let conn = db.acquire_lock("test").unwrap();
    let statuses: Vec<String> = conn
        .prepare(
            "SELECT status FROM album_membership_snapshots \
                 WHERE library = 'PrimarySync' AND container_id = 'container-a' \
                 ORDER BY generation",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        statuses,
        vec!["complete".to_string(), "running".to_string()]
    );
}

#[tokio::test]
async fn album_membership_lookups_are_library_scoped() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    for (library, container, master) in [
        ("PrimarySync", "container-a", "master-primary"),
        ("SharedSync-AB", "container-a", "master-shared"),
    ] {
        db.upsert_album_container(library, container, "Vacation", "album")
            .await
            .unwrap();
        let generation = db
            .start_album_membership_snapshot(library, container, None)
            .await
            .unwrap();
        db.add_album_membership_to_snapshot(
            library,
            container,
            generation,
            "same-asset-record",
            Some(master),
            "icloud",
        )
        .await
        .unwrap();
        db.complete_album_membership_snapshot(library, container, generation)
            .await
            .unwrap();
    }

    let primary = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "same-asset-record",
            &["container-a"],
        )
        .await
        .unwrap();
    let shared = db
        .get_live_selected_album_memberships_for_asset(
            "SharedSync-AB",
            "same-asset-record",
            &["container-a"],
        )
        .await
        .unwrap();
    assert_eq!(primary.len(), 1);
    assert_eq!(shared.len(), 1);
    assert_eq!(
        primary[0].master_record_name.as_deref(),
        Some("master-primary")
    );
    assert_eq!(
        shared[0].master_record_name.as_deref(),
        Some("master-shared")
    );
}

#[tokio::test]
async fn album_relation_delta_add_and_delete_update_live_membership() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
        .await
        .unwrap();

    let known = db
        .upsert_album_membership_delta(
            "PrimarySync",
            "container-a",
            "asset-record-a",
            Some("master-a"),
            "icloud",
        )
        .await
        .unwrap();
    assert!(known, "selected album container should be known");
    let live = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-record-a",
            &["container-a"],
        )
        .await
        .unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].master_record_name.as_deref(), Some("master-a"));

    let known = db
        .mark_album_membership_deleted("PrimarySync", "container-a", "asset-record-a")
        .await
        .unwrap();
    assert!(known, "delete should still know the album container");
    let live = db
        .get_live_selected_album_memberships_for_asset(
            "PrimarySync",
            "asset-record-a",
            &["container-a"],
        )
        .await
        .unwrap();
    assert!(live.is_empty(), "relation delete must hide live membership");
}

#[tokio::test]
async fn album_delta_delete_invalidates_snapshot() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.upsert_album_container("PrimarySync", "container-a", "Vacation", "album")
        .await
        .unwrap();
    let generation = db
        .start_album_membership_snapshot("PrimarySync", "container-a", None)
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "container-a",
        generation,
        "asset-record-a",
        Some("master-a"),
        "icloud",
    )
    .await
    .unwrap();
    db.complete_album_membership_snapshot("PrimarySync", "container-a", generation)
        .await
        .unwrap();
    assert!(
        db.selected_album_containers_have_complete_snapshots("PrimarySync", &["container-a"])
            .await
            .unwrap()
    );

    db.mark_album_container_deleted("PrimarySync", "container-a")
        .await
        .unwrap();
    db.invalidate_album_membership_snapshot("PrimarySync", "container-a")
        .await
        .unwrap();

    assert!(
        !db.selected_album_containers_have_complete_snapshots("PrimarySync", &["container-a"])
            .await
            .unwrap(),
        "deleted album containers must not remain trusted"
    );
}

#[tokio::test]
async fn unmaterialized_relations_do_not_create_compatibility_groupings() {
    let db = SqliteStateDb::open_in_memory().unwrap();
    db.add_asset_album("PrimarySync", "master-old", "Legacy Album", "icloud")
        .await
        .unwrap();
    db.upsert_album_container("PrimarySync", "container-a", "Trusted Album", "album")
        .await
        .unwrap();
    let generation = db
        .start_album_membership_snapshot("PrimarySync", "container-a", None)
        .await
        .unwrap();
    db.add_album_membership_to_snapshot(
        "PrimarySync",
        "container-a",
        generation,
        "asset-record-new",
        Some("master-new"),
        "icloud",
    )
    .await
    .unwrap();
    db.complete_album_membership_snapshot("PrimarySync", "container-a", generation)
        .await
        .unwrap();

    let legacy_rows = db.get_all_asset_albums("PrimarySync").await.unwrap();
    assert_eq!(
        legacy_rows,
        vec![("master-old".to_string(), "Legacy Album".to_string())],
        "relations without a catalogued state identity must not create compatibility groupings",
    );
}
