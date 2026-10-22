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

fn transport_page(url: &str, size: u64) -> Vec<u8> {
    format!(
        r#"{{"zones":[{{"zoneID":{{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},"syncToken":"successor","moreComing":false,"records":[{{"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":"ct1","fields":{{"resOriginalRes":{{"type":"ASSETID","value":{{"size":{size},"downloadURL":"{url}","fileChecksum":"f","referenceChecksum":"r","wrappingKey":"w"}}}}}}}}]}}]}}"#
    )
    .into_bytes()
}

fn transport_compute_page(url: &str, wrapping_key: &str) -> Vec<u8> {
    format!(
        r#"{{"zones":[{{"zoneID":{{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}},"syncToken":"successor","moreComing":false,"records":[{{"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"ct1","fields":{{"resComputeStateData":{{"type":"ASSETID","value":{{"size":2048,"downloadURL":"{url}","fileChecksum":"f","referenceChecksum":"r","wrappingKey":"{wrapping_key}"}}}}}}}}]}}]}}"#
    )
    .into_bytes()
}

fn transport_scope(db: &Arc<SqliteStateDb>, database: &str) -> String {
    capture(db)
        .scope(
            database,
            &json!({"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"}),
        )
        .unwrap()
}

fn transport_stream_page(
    master_url: &str,
    compute_url: &str,
    size: u64,
    master_tag: &str,
    successor: &str,
    more_coming: bool,
) -> Vec<u8> {
    serde_json::to_vec(&json!({"zones":[{
        "zoneID":{"zoneName":"PrimarySync","ownerRecordName":"_defaultOwner"},
        "syncToken":successor,"moreComing":more_coming,
        "records":[
            {"recordName":"master-1","recordType":"CPLMaster","recordChangeTag":master_tag,
             "fields":{
                "resOriginalRes":{"type":"ASSETID","value":{"size":size,"downloadURL":master_url,
                    "fileChecksum":"f","referenceChecksum":"r","wrappingKey":"w"}},
                "filenameEnc":{"value":"photo.jpg","type":"STRING"},
                "itemType":{"value":"public.jpeg"},
                "resOriginalFileType":{"value":"public.jpeg"}
             }},
            {"recordName":"asset-1","recordType":"CPLAsset","recordChangeTag":"ct2",
             "fields":{
                "masterRef":{"value":{"recordName":"master-1","zoneID":{"zoneName":"PrimarySync"}},"type":"REFERENCE"},
                "assetDate":{"value":1700000000000i64,"type":"TIMESTAMP"},
                "resComputeStateData":{"type":"ASSETID","value":{"size":2048,"downloadURL":compute_url,
                    "fileChecksum":"cf","referenceChecksum":"cr","wrappingKey":"cw"}}
             }}
        ]
    }]})).unwrap()
}

async fn stream_urls(
    album: &crate::icloud::photos::PhotoAlbum,
    request_cursor: &str,
) -> (Vec<String>, Vec<String>, String) {
    use tokio_stream::StreamExt;

    let (stream, token) = album.changes_stream(request_cursor);
    let mut urls = Vec::new();
    let mut errors = Vec::new();
    for item in stream.collect::<Vec<_>>().await {
        match item {
            Ok(event) => {
                if let Some(asset) = event.asset {
                    for (_, version) in asset.versions() {
                        urls.push(version.url.to_string());
                    }
                }
            }
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    (urls, errors, token.await.unwrap())
}

fn transport_alias_counts(db: &SqliteStateDb) -> (i64, i64, i64, i64, i64, i64) {
    let conn = db.acquire_lock("transport stream counts").unwrap();
    conn.query_row(
        "SELECT (SELECT count(*) FROM provider_shadow_pages),(SELECT count(*) FROM provider_transport_canonical),(SELECT count(*) FROM provider_transport_aliases),(SELECT count(*) FROM provider_catalog_debt),(SELECT COALESCE(SUM(charged_bytes),0) FROM provider_shadow_pages),(SELECT COALESCE(SUM(charged_bytes),0) FROM provider_catalog_pages)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    )
    .unwrap()
}

fn transport_page_totals(db: &SqliteStateDb) -> (i64, i64, i64) {
    let conn = db.acquire_lock("transport stream page totals").unwrap();
    conn.query_row(
        "SELECT COALESCE(SUM(record_count),0),COALESCE(SUM(reference_count),0),COALESCE(SUM(debt_count),0) FROM provider_catalog_pages",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

#[tokio::test]
async fn transport_stream_url_rotations_keep_latest_events_and_unchanged_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let media = directory.path().join("kept.jpg");
    std::fs::write(&media, b"existing-media").unwrap();
    let db = open(&path).await;
    seed(&db);

    let first_master = "https://p01.icloud-content.com/original?token=0";
    let first_compute = "https://p01.icloud-content.com/compute?token=0";
    let result = stream_urls(
        &album(
            transport_stream_page(first_master, first_compute, 1024, "ct1", "successor", false),
            capture(&db),
        ),
        "saved",
    )
    .await;
    assert!(result.1.is_empty(), "{:?}", result.1);
    assert_eq!(result.2, "successor");
    assert!(result.0.iter().any(|url| url == first_master));
    let baseline = transport_alias_counts(&db);
    assert_eq!(baseline.0, 1);
    assert_eq!(baseline.1, baseline.0, "one canonical index per class");
    let checkpoint = db.get_metadata("sync_token:PrimarySync").await.unwrap();

    // Repeated old-range captures rotate only the URL tokens. Each downstream
    // event still carries the incoming latest URLs, while the retained class,
    // catalogue facts, debt, charged bytes and checkpoint do not move.
    for index in 1..4 {
        let master_url = format!("https://p{index:02}.icloud-content.com/original?token={index}");
        let compute_url = format!("https://p{index:02}.icloud-content.com/compute?token={index}");
        let result = stream_urls(
            &album(
                transport_stream_page(&master_url, &compute_url, 1024, "ct1", "successor", false),
                capture(&db),
            ),
            "saved",
        )
        .await;
        assert!(result.1.is_empty(), "{:?}", result.1);
        assert_eq!(result.2, "successor");
        assert!(
            result.0.iter().any(|url| url == &master_url),
            "incoming original URL downstream"
        );
        assert!(!result.0.iter().any(|url| url == first_master));
        assert_eq!(transport_alias_counts(&db), baseline);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            checkpoint
        );
    }

    // A fixed-cursor EOF page requests and returns the same successor with
    // `moreComing: false`. The cursor is part of the canonical key, so this is
    // an independent class from the old-range page even though the record
    // bytes differ only in their eligible URLs. Both URL variants rotate
    // together, and repeated equivalent EOF captures stay in this class.
    let eof_master = "https://p11.icloud-content.com/original?token=11";
    let eof_compute = "https://p11.icloud-content.com/compute?token=11";
    let result = stream_urls(
        &album(
            transport_stream_page(eof_master, eof_compute, 1024, "ct1", "successor", false),
            capture(&db),
        ),
        "successor",
    )
    .await;
    assert!(result.1.is_empty(), "{:?}", result.1);
    assert_eq!(result.2, "successor");
    assert!(
        result.0.iter().any(|url| url == eof_master),
        "incoming original URL downstream"
    );
    let eof_baseline = transport_alias_counts(&db);
    assert_eq!(
        eof_baseline.0,
        baseline.0 + 1,
        "fixed-cursor EOF is an independent canonical class"
    );
    assert_eq!(eof_baseline.1, baseline.1 + 1);
    assert_eq!(eof_baseline.2, baseline.2);
    assert_eq!(
        eof_baseline.3,
        baseline.3 * 2,
        "one debt set per class page"
    );
    assert!(
        eof_baseline.4 > baseline.4,
        "the new class is charged in the inbox budget"
    );
    assert!(
        eof_baseline.5 > baseline.5,
        "the new class is charged in the catalogue budget"
    );
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync").await.unwrap(),
        checkpoint
    );

    for index in 1..4 {
        let master_url = format!("https://p1{index}.icloud-content.com/original?token={index}");
        let compute_url = format!("https://p1{index}.icloud-content.com/compute?token={index}");
        let result = stream_urls(
            &album(
                transport_stream_page(&master_url, &compute_url, 1024, "ct1", "successor", false),
                capture(&db),
            ),
            "successor",
        )
        .await;
        assert!(result.1.is_empty(), "{:?}", result.1);
        assert_eq!(result.2, "successor");
        assert!(
            result.0.iter().any(|url| url == &master_url),
            "incoming original URL downstream"
        );
        assert!(!result.0.iter().any(|url| url == eof_master));
        assert_eq!(transport_alias_counts(&db), eof_baseline);
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync").await.unwrap(),
            checkpoint
        );
    }

    let page_facts = transport_page_totals(&db);
    drop(db);
    let db = open(&path).await;
    capture(&db)
        .replay_catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(transport_alias_counts(&db), eof_baseline);
    assert_eq!(transport_page_totals(&db), page_facts);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync").await.unwrap(),
        checkpoint
    );

    // Reopened durable state still serves another equivalent EOF through the
    // actual stream, not only through local catalogue replay.
    let reopened_master = "https://p21.icloud-content.com/original?token=21";
    let reopened_compute = "https://p21.icloud-content.com/compute?token=21";
    let result = stream_urls(
        &album(
            transport_stream_page(
                reopened_master,
                reopened_compute,
                1024,
                "ct1",
                "successor",
                false,
            ),
            capture(&db),
        ),
        "successor",
    )
    .await;
    assert!(result.1.is_empty(), "{:?}", result.1);
    assert_eq!(result.2, "successor");
    assert!(
        result.0.iter().any(|url| url == reopened_master),
        "incoming original URL downstream"
    );
    assert_eq!(transport_alias_counts(&db), eof_baseline);
    assert_eq!(transport_page_totals(&db), page_facts);
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync").await.unwrap(),
        checkpoint
    );
    unchanged(&db, &media).await;

    // A genuine new record version is admitted separately, not aliased.
    let result = collect(&album(
        transport_stream_page(
            "https://p05.icloud-content.com/original?token=5",
            "https://p05.icloud-content.com/compute?token=5",
            1025,
            "ct2",
            "successor",
            false,
        ),
        capture(&db),
    ))
    .await;
    assert!(result.1.is_empty(), "{:?}", result.1);
    let after = transport_alias_counts(&db);
    assert_eq!(after.0, eof_baseline.0 + 1);
    assert_eq!(after.1, eof_baseline.1 + 1);
    assert_eq!(after.2, eof_baseline.2);
}

fn transport_counts(db: &SqliteStateDb) -> (i64, i64, i64) {
    let conn = db.acquire_lock("transport fixture counts").unwrap();
    conn.query_row(
        "SELECT (SELECT count(*) FROM provider_shadow_pages),(SELECT count(*) FROM provider_transport_canonical),(SELECT count(*) FROM provider_catalog_pages)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

#[tokio::test]
async fn transport_replay_reuses_one_canonical_page_before_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let scope = transport_scope(&db, "private");
    let capture = capture(&db);
    let first = super::catalog_observed_page(
        transport_page("https://p01.icloud-content.com/a?token=0", 1024),
        &scope,
        "held",
    )
    .unwrap();
    capture.capture(first).await.unwrap();
    for index in 1..100 {
        let next = super::catalog_observed_page(
            transport_page(
                &format!("https://p02.icloud-content.com/a?token={index}"),
                1024,
            ),
            &scope,
            "held",
        )
        .unwrap();
        capture.capture(next).await.unwrap();
        assert_eq!(transport_counts(&db), (1, 1, 1));
    }
    let changed = super::catalog_observed_page(
        transport_page("https://p01.icloud-content.com/a?token=0", 1025),
        &scope,
        "held",
    )
    .unwrap();
    capture.capture(changed).await.unwrap();
    assert_eq!(transport_counts(&db), (2, 2, 2));
}

#[tokio::test]
async fn compute_state_replay_reuses_one_canonical_page() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let scope = transport_scope(&db, "private");
    let capture = capture(&db);
    let first = super::catalog_observed_page(
        transport_compute_page("https://p01.icloud-content.com/a?token=0", "w"),
        &scope,
        "held",
    )
    .unwrap();
    capture.capture(first).await.unwrap();
    let rotated = super::catalog_observed_page(
        transport_compute_page("https://p09.icloud-content.com/a/much/longer?token=1", "w"),
        &scope,
        "held",
    )
    .unwrap();
    capture.capture(rotated).await.unwrap();
    assert_eq!(transport_counts(&db), (1, 1, 1));

    // A non-URL compute descriptor change is not a transport rotation.
    let changed = super::catalog_observed_page(
        transport_compute_page("https://p09.icloud-content.com/a/much/longer?token=1", "w2"),
        &scope,
        "held",
    )
    .unwrap();
    capture.capture(changed).await.unwrap();
    assert_eq!(transport_counts(&db), (2, 2, 2));
}

#[tokio::test]
async fn same_body_with_different_request_cursor_is_an_independent_class() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let scope = transport_scope(&db, "private");
    let capture = capture(&db);
    let body = transport_page("https://p01.icloud-content.com/a?token=0", 1024);
    for cursor in ["held", "held-next"] {
        let page = super::catalog_observed_page(body.clone(), &scope, cursor).unwrap();
        capture.capture(page).await.unwrap();
    }
    assert_eq!(transport_counts(&db), (2, 2, 2));
    let ids: Vec<i64> = db
        .acquire_lock("independent cursor classes")
        .unwrap()
        .prepare("SELECT page_id FROM provider_transport_canonical ORDER BY page_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test]
async fn shared_scope_stays_raw_while_private_scope_masks() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let capture = capture(&db);
    let body = transport_page("https://p01.icloud-content.com/a?token=0", 1024);
    for database in ["shared", "private"] {
        let scope = transport_scope(&db, database);
        let page = super::catalog_observed_page(body.clone(), &scope, "held").unwrap();
        capture.capture(page).await.unwrap();
    }
    assert_eq!(transport_counts(&db), (2, 1, 2));
}

#[tokio::test]
async fn ownerless_private_scope_is_raw_and_cannot_coalesce() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;

    let missing_owner = json!({
        "format": 1, "realm": "com", "container": "com.apple.photos.cloud",
        "environment": "production", "database": "private",
        "zone": {"zoneName": "PrimarySync"}
    });
    for url in [
        "https://p01.icloud-content.com/a?token=0",
        "https://p02.icloud-content.com/a?token=1",
    ] {
        let page = super::catalog_observed_page(
            transport_page(url, 1024),
            &missing_owner.to_string(),
            "held",
        )
        .unwrap();
        db.capture_shadow_page(owner(), page, 512 * 1024 * 1024)
            .await
            .unwrap();
    }
    // No complete private scope proof means no canonical identity: both
    // captures remain distinct raw observations instead of being rejected or
    // coalesced.
    assert_eq!(transport_counts(&db), (2, 0, 0));

    let unknown = json!({
        "format": 1, "realm": "com", "container": "com.apple.photos.cloud",
        "environment": "staging", "database": "private",
        "zone": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"}
    });
    let page = super::catalog_observed_page(
        transport_page("https://p01.icloud-content.com/a?token=0", 1024),
        &unknown.to_string(),
        "held",
    )
    .unwrap();
    db.capture_shadow_page(owner(), page, 512 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(transport_counts(&db), (3, 0, 0));
}

#[tokio::test]
async fn url_repeat_projects_after_capture_without_projection() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let scope = transport_scope(&db, "private");
    let first = super::catalog_observed_page(
        transport_page("https://p01.icloud-content.com/a?token=0", 1024),
        &scope,
        "held",
    )
    .unwrap();
    let id = db
        .capture_shadow_page(owner(), first, 512 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(transport_counts(&db), (1, 1, 0));

    let repeat = super::catalog_observed_page(
        transport_page("https://p02.icloud-content.com/a?token=1", 1024),
        &scope,
        "held",
    )
    .unwrap();
    capture(&db).capture(repeat).await.unwrap();
    assert_eq!(transport_counts(&db), (1, 1, 1));
    let projected: i64 = db
        .acquire_lock("interrupted projection recovered")
        .unwrap()
        .query_row("SELECT page_id FROM provider_catalog_pages", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(projected, id.0);
}

#[tokio::test]
async fn full_budget_accepts_equivalent_repeat_and_refuses_distinct_change() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let scope = transport_scope(&db, "private");
    let mut capture = capture(&db);
    let first = super::catalog_observed_page(
        transport_page("https://p01.icloud-content.com/a?token=0", 1024),
        &scope,
        "held",
    )
    .unwrap();
    capture.capture(first).await.unwrap();
    capture.capacity = 0;

    let repeat = super::catalog_observed_page(
        transport_page("https://p02.icloud-content.com/a?token=1", 1024),
        &scope,
        "held",
    )
    .unwrap();
    capture.capture(repeat).await.unwrap();
    assert_eq!(transport_counts(&db), (1, 1, 1));

    let changed = super::catalog_observed_page(
        transport_page("https://p02.icloud-content.com/a?token=1", 1025),
        &scope,
        "held",
    )
    .unwrap();
    let error = capture.capture(changed).await.expect_err("full inbox");
    assert!(format!("{error:#}").contains("inbox is full"));
    assert_eq!(transport_counts(&db), (1, 1, 1));
    let receipt: i64 = db
        .acquire_lock("refused change keeps receipt")
        .unwrap()
        .query_row("SELECT page_id FROM provider_shadow_receipts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(receipt, 1);
}

#[tokio::test]
async fn mismatched_canonical_index_entry_is_invalid() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let scope = transport_scope(&db, "private");
    let capture = capture(&db);
    let body = transport_page("https://p01.icloud-content.com/a?token=0", 1024);
    for cursor in ["held", "held-next"] {
        let page = super::catalog_observed_page(body.clone(), &scope, cursor).unwrap();
        capture.capture(page).await.unwrap();
    }
    {
        let conn = db.acquire_lock("tamper canonical index").unwrap();
        let rows: Vec<(String, i64)> = conn
            .prepare(
                "SELECT canonical_sha,page_id FROM provider_transport_canonical ORDER BY page_id",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 2);
        conn.execute(
            "DELETE FROM provider_transport_canonical WHERE canonical_sha=?1",
            [&rows[1].0],
        )
        .unwrap();
        conn.execute(
            "UPDATE provider_transport_canonical SET page_id=?1 WHERE canonical_sha=?2",
            rusqlite::params![rows[1].1, rows[0].0],
        )
        .unwrap();
    }
    let repeat = super::catalog_observed_page(
        transport_page("https://p02.icloud-content.com/a?token=1", 1024),
        &scope,
        "held",
    )
    .unwrap();
    assert!(capture.capture(repeat).await.is_err());
    assert_eq!(transport_counts(&db), (2, 1, 2));
}
