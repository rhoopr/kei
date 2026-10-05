#![cfg(test)]

use super::shadow_tests::{BodySession, album, capture, collect, open, owner};
use crate::icloud::photos::album::test_support::{
    changes_asset, changes_master, make_album_with_session,
};
use crate::state::SqliteStateDb;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn body(zone: &str, revision: &str) -> Vec<u8> {
    let value = json!({"zones":[{"zoneID":{"zoneName":zone,"ownerRecordName":"_defaultOwner"},
    "syncToken":"successor","moreComing":false,"records":[
        changes_master("known-master"), changes_asset("known-asset","known-master"),
        {"recordName":"album","recordType":"CPLAlbum","fields":{"albumName":{"value":"Kept"}}},
        {"recordName":"membership","recordType":"CPLContainerRelation","fields":{"containerId":{"value":"album"},"itemId":{"value":"known-asset"}}},
        {"recordName":"future","recordType":"FutureRecord","revision":revision,"preciseNumber":0,
            "fields":{"linked~/peer":{"value":{"recordName":"external-target","zoneID":{"zoneName":"other-zone","ownerRecordName":"other-owner"}}}}},
        {"recordName":"tomb","recordType":null,"deleted":true,"unknownPayload":[false,7]},
        {"recordName":"broken-relation","recordType":"CPLContainerRelation","fields":{"containerId":{"value":42}}}
    ]}]});
    String::from_utf8(serde_json::to_vec(&value).unwrap())
        .unwrap()
        .replace(
            "\"preciseNumber\":0",
            "\"preciseNumber\":9007199254740993.0",
        )
        .into_bytes()
}

async fn retain(
    db: &Arc<SqliteStateDb>,
    raw: Vec<u8>,
    database: &str,
) -> crate::state::db::provider_inbox::CapturedPageId {
    let value: Value = serde_json::from_slice(&raw).unwrap();
    let zone = value.pointer("/zones/0/zoneID").unwrap();
    let scope = capture(db).scope(database, zone).unwrap();
    let page = super::catalog_observed_page(raw, &scope, "saved").unwrap();
    db.capture_shadow_page(owner(), page, 512 * 1024 * 1024)
        .await
        .unwrap()
}

fn counts(db: &SqliteStateDb) -> Vec<i64> {
    let conn = db.acquire_lock("catalog fixture counts").unwrap();
    [
        "provider_catalog_records",
        "provider_catalog_references",
        "provider_catalog_debt",
        "provider_catalog_pages",
    ]
    .iter()
    .map(|table| {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    })
    .collect()
}

fn snapshot(db: &SqliteStateDb) -> Vec<Vec<String>> {
    let conn = db.acquire_lock("independent catalog snapshot").unwrap();
    [
        "SELECT printf('%d|%d|%s|%s|%d|%s',page_id,ordinal,record_name,record_type,deleted,kind) FROM provider_catalog_records ORDER BY page_id,ordinal",
        "SELECT printf('%d|%d|%s|%s|%s|%s',page_id,ordinal,field_path,target_record_name,target_zone_name,target_zone_owner) FROM provider_catalog_references ORDER BY page_id,ordinal,field_path",
        "SELECT printf('%d|%d|%s|%s',page_id,ordinal,field_path,reason) FROM provider_catalog_debt ORDER BY page_id,ordinal,field_path,reason",
        "SELECT printf('%d|%d|%s|%d|%d|%d|%d',page_id,projector_version,body_hash,record_count,reference_count,debt_count,charged_bytes) FROM provider_catalog_pages ORDER BY page_id",
    ].iter().map(|sql|conn.prepare(sql).unwrap().query_map([],|row|row.get(0)).unwrap().collect::<Result<Vec<String>,_>>().unwrap()).collect()
}

fn seed(db: &SqliteStateDb) {
    db.acquire_lock("catalog fixture historical state").unwrap().execute_batch(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,last_seen_at,status) VALUES ('PrimarySync','kept','original','synthetic','kept.jpg',1000,14,'photo',1000,'downloaded'); INSERT INTO metadata(key,value) VALUES('sync_token:PrimarySync','saved'),('pending_sync_token:old:PrimarySync','old-debt'); CREATE TABLE future_unknown(payload BLOB); INSERT INTO future_unknown VALUES(X'00FF07');"
    ).unwrap();
}

async fn unchanged(db: &SqliteStateDb, media: &std::path::Path) {
    assert_eq!(std::fs::read(media).unwrap(), b"existing-media");
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved")
    );
    assert_eq!(
        db.get_metadata("pending_sync_token:old:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("old-debt")
    );
    assert_eq!(db.get_summary().await.unwrap().downloaded, 1);
    assert_eq!(
        db.acquire_lock("unknown bytes remain")
            .unwrap()
            .query_row::<Vec<u8>, _, _>("SELECT payload FROM future_unknown", [], |row| row.get(0))
            .unwrap(),
        [0, 255, 7]
    );
}

#[tokio::test]
async fn catalog_projection_replays_schema29_sources_losslessly_through_two_quiet_reopens() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let media = directory.path().join("kept.jpg");
    std::fs::write(&media, b"existing-media").unwrap();
    let db = open(&path).await;
    seed(&db);
    let original = body("PrimarySync", "v1");
    retain(&db, original.clone(), "private").await;
    assert_eq!(
        counts(&db),
        [0, 0, 0, 0],
        "capture alone is the pre-projection control"
    );
    db.acquire_lock("synthetic schema29 pending source").unwrap().execute_batch("DROP TABLE provider_catalog_pages; DROP TABLE provider_catalog_debt; DROP TABLE provider_catalog_references; DROP TABLE provider_catalog_records; PRAGMA user_version=29;").unwrap();
    drop(db);
    let mut frozen = None;
    for _ in 0..2 {
        let db = open(&path).await;
        capture(&db)
            .replay_catalog(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(counts(&db), [7, 4, 7, 1]);
        let current = snapshot(&db);
        if let Some(expected) = &frozen {
            assert_eq!(&current, expected);
        } else {
            frozen = Some(current);
        }
        {
            let conn = db
                .acquire_lock("independent source and reference oracles")
                .unwrap();
            let raw: Vec<u8> = conn
                .query_row("SELECT body FROM provider_shadow_pages", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(raw, original);
            assert!(String::from_utf8_lossy(&raw).contains("9007199254740993.0"));
            let kinds: Vec<(String, String, bool)> = conn
            .prepare(
                "SELECT record_name,kind,deleted FROM provider_catalog_records ORDER BY ordinal",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
            assert_eq!(
                kinds,
                [
                    ("known-master".into(), "master".into(), false),
                    ("known-asset".into(), "asset".into(), false),
                    ("album".into(), "album".into(), false),
                    ("membership".into(), "relation".into(), false),
                    ("future".into(), "unknown".into(), false),
                    ("tomb".into(), "unknown".into(), true),
                    ("broken-relation".into(), "relation".into(), false)
                ]
            );
            let target:(String,String,String,String)=conn.query_row("SELECT field_path,target_record_name,target_zone_name,target_zone_owner FROM provider_catalog_references WHERE target_record_name='external-target'",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
            assert_eq!(
                target,
                (
                    "/fields/linked~0~1peer/value".into(),
                    "external-target".into(),
                    "other-zone".into(),
                    "other-owner".into()
                )
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_catalog_debt WHERE reason='unclassified_record'",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                2
            );
            assert_eq!(
                conn.query_row::<i64, _, _>(
                    "SELECT count(*) FROM provider_catalog_debt WHERE reason='incomplete_relation'",
                    [],
                    |row| row.get(0)
                )
                .unwrap(),
                1
            );
        }
        unchanged(&db, &media).await;
    }
}

#[tokio::test]
async fn catalog_projection_transaction_failure_retains_source_then_recovers_live_parity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let media = directory.path().join("kept.jpg");
    std::fs::write(&media, b"existing-media").unwrap();
    let db = open(&path).await;
    seed(&db);
    db.acquire_lock("receipt fault after real indexing").unwrap().execute_batch("CREATE TRIGGER catalog_fault BEFORE INSERT ON provider_catalog_pages BEGIN SELECT RAISE(ABORT,'synthetic catalog receipt fault'); END;").unwrap();
    let raw = body("PrimarySync", "v1");
    let bound = album(raw.clone(), capture(&db));
    let result = collect(&bound).await;
    assert!(result.0.is_empty());
    assert_eq!(result.2, "saved");
    assert!(result.1[0].contains("synthetic catalog receipt fault"));
    assert_eq!(counts(&db), [0, 0, 0, 0]);
    assert_eq!(
        db.acquire_lock("retained failed source")
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM provider_shadow_pages", [], |row| row
                .get(0))
            .unwrap(),
        1
    );
    unchanged(&db, &media).await;
    drop(bound);
    drop(db);
    let db = open(&path).await;
    assert!(
        capture(&db)
            .replay_catalog(&CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(counts(&db), [0, 0, 0, 0]);
    unchanged(&db, &media).await;
    db.acquire_lock("fault-only removal")
        .unwrap()
        .execute_batch("DROP TRIGGER catalog_fault")
        .unwrap();
    capture(&db)
        .replay_catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(counts(&db), [7, 4, 7, 1]);
    let frozen = snapshot(&db);
    let baseline = collect(&make_album_with_session(
        100,
        Box::new(BodySession(raw.clone())),
    ))
    .await;
    assert_eq!(collect(&album(raw, capture(&db))).await, baseline);
    assert_eq!(snapshot(&db), frozen);
    unchanged(&db, &media).await;
    drop(db);
    for _ in 0..2 {
        let db = open(&path).await;
        capture(&db)
            .replay_catalog(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snapshot(&db), frozen);
        unchanged(&db, &media).await;
    }
}

#[tokio::test]
async fn catalog_projection_scopes_revisions_and_old_replay_never_overwrite_facts() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    retain(&db, body("PrimarySync", "v1"), "private").await;
    retain(&db, body("PrimarySync", "v2"), "private").await;
    retain(&db, body("PrimarySync", "v1"), "shared").await;
    capture(&db)
        .replay_catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(counts(&db), [21, 12, 21, 3]);
    let frozen = snapshot(&db);
    let old = super::catalog_observed_page(
        body("PrimarySync", "v1"),
        &capture(&db)
            .scope(
                "private",
                &json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
            )
            .unwrap(),
        "saved",
    )
    .unwrap();
    capture(&db).capture(old).await.unwrap();
    assert_eq!(snapshot(&db), frozen);
    assert_eq!(db.acquire_lock("scope separation oracle").unwrap().query_row::<i64,_,_>("SELECT count(DISTINCT p.scope) FROM provider_catalog_records c JOIN provider_shadow_pages p ON p.id=c.page_id",[],|row|row.get(0)).unwrap(),2);
}

#[tokio::test]
async fn catalog_projection_capacity_replays_existing_and_recovers_pending_page() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let id = retain(&db, body("PrimarySync", "v1"), "private").await;
    let mut full = capture(&db);
    full.catalog_capacity = 0;
    assert!(
        full.replay_catalog(&CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("projection is full")
    );
    assert_eq!(counts(&db), [0, 0, 0, 0]);
    capture(&db)
        .replay_catalog(&CancellationToken::new())
        .await
        .unwrap();
    let frozen = snapshot(&db);
    let source = db
        .catalog_source(owner(), id, 16 * 1024 * 1024)
        .await
        .unwrap();
    let plan = crate::icloud::photos::projection::plan(&full, source).unwrap();
    db.project_catalog_page(owner(), plan, 0).await.unwrap();
    assert_eq!(snapshot(&db), frozen);
    retain(&db, body("PrimarySync", "v2"), "private").await;
    assert!(
        full.replay_catalog(&CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(snapshot(&db), frozen);
    capture(&db)
        .replay_catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(counts(&db), [14, 8, 14, 2]);
}

#[tokio::test]
async fn catalog_projection_revalidates_owner_source_metadata_and_ordinals() {
    for (fault, sql) in [
        (
            "owner",
            "UPDATE account_owner SET provider_key='wrong-provider'",
        ),
        (
            "provider",
            "UPDATE provider_shadow_pages SET provider_key='wrong-provider'",
        ),
        (
            "hash",
            "UPDATE provider_shadow_pages SET body_hash='wrong-hash'",
        ),
        (
            "successor",
            "UPDATE provider_shadow_pages SET successor='wrong-successor'",
        ),
        (
            "identity",
            "UPDATE provider_shadow_records SET record_name='wrong-name' WHERE ordinal=0",
        ),
        (
            "ordinal",
            "UPDATE provider_shadow_records SET ordinal=99 WHERE ordinal=0",
        ),
        ("scope", "UPDATE provider_shadow_pages SET scope='{}'"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let db = open(&directory.path().join("owned.db")).await;
        retain(&db, body("PrimarySync", "v1"), "private").await;
        db.acquire_lock("synthetic retained source mutation")
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        assert!(
            capture(&db)
                .replay_catalog(&CancellationToken::new())
                .await
                .is_err(),
            "{fault}"
        );
        assert_eq!(counts(&db), [0, 0, 0, 0], "{fault}");
    }
}

#[tokio::test]
async fn catalog_projection_revalidates_source_after_planning_before_transaction() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let id = retain(&db, body("PrimarySync", "v1"), "private").await;
    let source = db
        .catalog_source(owner(), id, 16 * 1024 * 1024)
        .await
        .unwrap();
    let plan = crate::icloud::photos::projection::plan(&capture(&db), source).unwrap();
    db.acquire_lock("source changes between read and write")
        .unwrap()
        .execute_batch("UPDATE provider_shadow_pages SET successor='newer-source'")
        .unwrap();
    assert!(
        db.project_catalog_page(owner(), plan, 512 * 1024 * 1024)
            .await
            .is_err()
    );
    assert_eq!(counts(&db), [0, 0, 0, 0]);
}

#[tokio::test]
async fn catalog_projection_cancelled_replay_leaves_pending_source_for_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let db = open(&path).await;
    retain(&db, body("PrimarySync", "v1"), "private").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    capture(&db).replay_catalog(&cancel).await.unwrap();
    assert_eq!(counts(&db), [0, 0, 0, 0]);
    drop(db);
    let db = open(&path).await;
    capture(&db)
        .replay_catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(counts(&db), [7, 4, 7, 1]);
}

#[tokio::test]
async fn catalog_projection_migration_conflict_preserves_schema29_source_and_unknown_history() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let media = directory.path().join("kept.jpg");
    std::fs::write(&media, b"existing-media").unwrap();
    let db = open(&path).await;
    seed(&db);
    let original = body("PrimarySync", "v1");
    retain(&db, original.clone(), "private").await;
    db.acquire_lock("synthetic catalog migration conflict").unwrap().execute_batch(
        "DROP TABLE provider_catalog_pages; DROP TABLE provider_catalog_debt; DROP TABLE provider_catalog_references; DROP TABLE provider_catalog_records; CREATE TABLE provider_catalog_pages(page_id INTEGER,projector_version INTEGER,body_hash TEXT,record_count INTEGER,reference_count INTEGER,debt_count INTEGER,charged_bytes INTEGER,future_blob BLOB); INSERT INTO provider_catalog_pages(future_blob) VALUES(X'0011FF'); PRAGMA user_version=29;"
    ).unwrap();
    drop(db);
    for _ in 0..2 {
        assert!(
            crate::state::SqliteStateDb::open_owned(&path, &owner())
                .await
                .is_err()
        );
        let conn = rusqlite::Connection::open(&path).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            29
        );
        for table in [
            "provider_catalog_records",
            "provider_catalog_references",
            "provider_catalog_debt",
        ] {
            assert!(
                !conn
                    .query_row::<bool, _, _>(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
                        [table],
                        |row| row.get(0)
                    )
                    .unwrap()
            );
        }
        assert_eq!(
            conn.query_row::<Vec<u8>, _, _>(
                "SELECT future_blob FROM provider_catalog_pages",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            [0, 17, 255]
        );
        assert_eq!(
            conn.query_row::<Vec<u8>, _, _>("SELECT body FROM provider_shadow_pages", [], |row| {
                row.get(0)
            })
            .unwrap(),
            original
        );
        assert_eq!(
            conn.query_row::<Vec<u8>, _, _>("SELECT payload FROM future_unknown", [], |row| row
                .get(0))
                .unwrap(),
            [0, 255, 7]
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT value FROM metadata WHERE key='sync_token:PrimarySync'",
                [],
                |row| row.get(0)
            )
            .unwrap(),
            "saved"
        );
        assert_eq!(std::fs::read(&media).unwrap(), b"existing-media");
    }
    // Remove only the injected fixture conflict, never production unknown data.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE provider_catalog_pages")
        .unwrap();
    for _ in 0..2 {
        let db = open(&path).await;
        capture(&db)
            .replay_catalog(&CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(counts(&db), [7, 4, 7, 1]);
        unchanged(&db, &media).await;
    }
}

#[tokio::test]
async fn catalog_projection_page_budget_refuses_expanding_reference_paths_without_partial_index() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let db = open(&path).await;
    db.set_metadata("sync_token:PrimarySync", "saved")
        .await
        .unwrap();
    let targets: Vec<Value> = (0..3000)
        .map(|n| json!({"recordName":format!("target-{n}")}))
        .collect();
    let raw=serde_json::to_vec(&json!({"zones":[{"zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},"syncToken":"successor","moreComing":false,
        "records":[{"recordName":"future","recordType":"FutureRecord","fields":{("k".repeat(4000)):{"value":targets}}}]}]})).unwrap();
    assert!(
        raw.len() < 16 * 1024 * 1024,
        "raw page fits; normalized pointer paths cause the refusal"
    );
    let result = collect(&album(raw, capture(&db))).await;
    assert!(result.0.is_empty());
    assert_eq!(result.2, "saved");
    assert!(result.1[0].contains("projection is full"), "{:?}", result.1);
    assert_eq!(counts(&db), [0, 0, 0, 0]);
    assert_eq!(
        db.acquire_lock("retained oversized projection source")
            .unwrap()
            .query_row::<i64, _, _>("SELECT count(*) FROM provider_shadow_pages", [], |row| row
                .get(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved")
    );
}

#[tokio::test]
async fn catalog_projection_completed_replay_refuses_inconsistent_receipt_or_facts() {
    for (fault, sql) in [
        (
            "receipt",
            "UPDATE provider_catalog_pages SET record_count=record_count+1",
        ),
        (
            "record",
            "UPDATE provider_catalog_records SET record_name='wrong-source' WHERE ordinal=0",
        ),
        (
            "reference",
            "UPDATE provider_catalog_references SET target_record_name='wrong-target' WHERE ordinal=1",
        ),
        ("debt", "DELETE FROM provider_catalog_debt WHERE ordinal=4"),
        (
            "version",
            "UPDATE provider_catalog_pages SET projector_version=99",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let db = open(&directory.path().join("owned.db")).await;
        let id = retain(&db, body("PrimarySync", "v1"), "private").await;
        capture(&db)
            .replay_catalog(&CancellationToken::new())
            .await
            .unwrap();
        db.acquire_lock("synthetic completed receipt corruption")
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        let frozen = snapshot(&db);
        let source = db
            .catalog_source(owner(), id, 16 * 1024 * 1024)
            .await
            .unwrap();
        let plan = crate::icloud::photos::projection::plan(&capture(&db), source).unwrap();
        assert!(
            db.project_catalog_page(owner(), plan, 0).await.is_err(),
            "{fault}"
        );
        assert_eq!(
            snapshot(&db),
            frozen,
            "refusal must not rewrite corrupted evidence: {fault}"
        );
    }
}
