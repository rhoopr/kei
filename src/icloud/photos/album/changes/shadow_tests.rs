use crate::icloud::photos::album::test_support::make_album_with_session;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::session::PhotosSession;
use crate::state::SqliteStateDb;
use crate::state::db::account::AccountOwner;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_stream::StreamExt;

#[derive(Clone)]
struct BodySession(Vec<u8>);

#[async_trait::async_trait]
impl PhotosSession for BodySession {
    async fn post(&self, _: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        Ok(serde_json::from_slice(&self.0)?)
    }

    async fn post_changes_body(
        &self,
        _: &str,
        _: String,
        _: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        Ok(self.0.clone())
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn unknown_page() -> Vec<u8> {
    serde_json::to_vec(&json!({"zones": [{
        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
        "syncToken": "successor", "moreComing": false,
        "records": [{"recordName": "future-source", "recordType": "FutureRecordType",
            "futureEnvelope": {"source": "unknown", "revision": "v1"},
            "fields": {"linkedShare": {"value": {"recordName": "target",
                "zoneID": {"zoneName": "other-zone", "ownerRecordName": "other-owner"}}}}}]
    }]}))
    .unwrap()
}

#[tokio::test]
async fn legacy_unknown_delta_loses_source_before_successor_control() {
    let album = make_album_with_session(100, Box::new(BodySession(unknown_page())));
    let (stream, token) = album.changes_stream("saved");
    let events: Vec<_> = stream.collect().await;
    assert!(
        events.is_empty(),
        "unknown records have no legacy projection"
    );
    assert_eq!(token.await.unwrap(), "successor");
}

fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap()
}

async fn open(path: &std::path::Path) -> Arc<SqliteStateDb> {
    Arc::new(SqliteStateDb::open_owned(path, &owner()).await.unwrap())
}

fn capture(db: &Arc<SqliteStateDb>) -> ShadowCapture {
    ShadowCapture::new(Arc::clone(db), owner(), "com")
}

fn album(body: Vec<u8>, capture: ShadowCapture) -> crate::icloud::photos::PhotoAlbum {
    let mut album = make_album_with_session(100, Box::new(BodySession(body)));
    album.set_shadow_capture(capture, Arc::from("private"));
    album
}

fn counts(db: &SqliteStateDb) -> (i64, i64, i64) {
    let conn = db.acquire_lock("shadow fixture observation").unwrap();
    (
        conn.query_row("SELECT count(*) FROM provider_shadow_pages", [], |r| {
            r.get(0)
        })
        .unwrap(),
        conn.query_row("SELECT count(*) FROM provider_shadow_records", [], |r| {
            r.get(0)
        })
        .unwrap(),
        conn.query_row("SELECT count(*) FROM provider_shadow_receipts", [], |r| {
            r.get(0)
        })
        .unwrap(),
    )
}

async fn collect(album: &crate::icloud::photos::PhotoAlbum) -> (Vec<String>, Vec<String>, String) {
    let (stream, token) = album.changes_stream("saved");
    let mut events = Vec::new();
    let mut errors = Vec::new();
    for item in stream.collect::<Vec<_>>().await {
        match item {
            Ok(event) => events.push(format!("{event:?}")),
            Err(error) => errors.push(format!("{error:#}")),
        }
    }
    (events, errors, token.await.unwrap())
}

#[tokio::test]
async fn shadow_source_payload_roundtrip_reopens_losslessly_and_replays_legacy_parity() {
    use crate::icloud::photos::album::test_support::{changes_asset, changes_master};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let media = directory.path().join("kept.jpg");
    std::fs::write(&media, b"preexisting-media").unwrap();
    let db = open(&path).await;
    db.set_metadata("sync_token:PrimarySync", "saved")
        .await
        .unwrap();
    db.set_metadata("pending_sync_token:old-epoch:PrimarySync", "retained-debt")
        .await
        .unwrap();
    db.acquire_lock("preexisting downloaded state").unwrap().execute_batch(
        "INSERT INTO assets(library,id,version_size,checksum,filename,created_at,size_bytes,media_type,last_seen_at,status) VALUES ('PrimarySync','kept','original','synthetic','kept.jpg',1000,17,'photo',1000,'downloaded')").unwrap();
    let mut page: Value = serde_json::from_slice(&unknown_page()).unwrap();
    let records = page["zones"][0]["records"].as_array_mut().unwrap();
    records.extend([
        json!({"recordName":"tombstone","deleted":true,"unknownTombstoneField":[1,2,3]}),
        json!({"recordName":"relationship","recordType":"CPLContainerRelation",
            "fields":{"linkedShare":{"value":{"recordName":"unresolved-target",
                "zoneID":{"zoneName":"external-zone","ownerRecordName":"external-owner"}}}}}),
        changes_master("known-master"),
        changes_asset("known-asset", "known-master"),
    ]);
    let body = String::from_utf8(serde_json::to_vec(&page).unwrap())
        .unwrap()
        .replace(
            "\"revision\":\"v1\"",
            "\"revision\":\"v1\",\"preciseNumber\":9007199254740993.0",
        )
        .into_bytes();
    let baseline = collect(&make_album_with_session(
        100,
        Box::new(BodySession(body.clone())),
    ))
    .await;
    assert!(baseline.1.is_empty());
    assert!(
        !baseline.0.is_empty(),
        "paired known records and tombstones are the positive control"
    );
    let bound = album(body.clone(), capture(&db));
    assert_eq!(collect(&bound.clone()).await, baseline);
    assert_eq!(counts(&db), (1, 5, 1));
    drop(bound);
    drop(db);
    for _ in 0..2 {
        let db = open(&path).await;
        let stored: Vec<u8> = db
            .acquire_lock("independent raw replay")
            .unwrap()
            .query_row("SELECT body FROM provider_shadow_pages", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, body);
        assert!(String::from_utf8_lossy(&stored).contains("9007199254740993.0"));
        {
            let conn = db.acquire_lock("scope and ordinal facts").unwrap();
            let (scope, request, successor, account, provider): (String,String,String,String,String) = conn.query_row(
                "SELECT scope,request_cursor,successor,account_key,provider_key FROM provider_shadow_pages", [],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).unwrap();
            let scope: Value = serde_json::from_str(&scope).unwrap();
            assert_eq!(scope["realm"], "com");
            assert_eq!(scope["container"], "com.apple.photos.cloud");
            assert_eq!(scope["environment"], "production");
            assert_eq!(scope["database"], "private");
            assert_eq!(scope["zone"]["ownerRecordName"], "_defaultOwner");
            assert_eq!(
                (request.as_str(), successor.as_str()),
                ("saved", "successor")
            );
            assert!(account.starts_with("account-v1-"));
            assert_eq!(provider.len(), 64);
            let mut query = conn.prepare("SELECT ordinal,record_name,record_type,deleted FROM provider_shadow_records ORDER BY ordinal").unwrap();
            let identities: Vec<(i64, String, Option<String>, bool)> = query
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                identities[0],
                (
                    0,
                    "future-source".into(),
                    Some("FutureRecordType".into()),
                    false
                )
            );
            assert_eq!(identities[1], (1, "tombstone".into(), None, true));
            assert_eq!(identities[2].1, "relationship");
            assert_eq!(
                conn.query_row::<String, _, _>(
                    "SELECT status FROM assets WHERE id='kept'",
                    [],
                    |r| r.get(0)
                )
                .unwrap(),
                "downloaded"
            );
        }
        let bound = album(stored, capture(&db));
        assert_eq!(collect(&bound).await, baseline);
        assert_eq!(
            counts(&db),
            (1, 5, 1),
            "exact observed-page replay must be idempotent"
        );
        assert_eq!(
            db.get_metadata("sync_token:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("saved")
        );
        assert_eq!(
            db.get_metadata("pending_sync_token:old-epoch:PrimarySync")
                .await
                .unwrap()
                .as_deref(),
            Some("retained-debt")
        );
        assert_eq!(std::fs::read(&media).unwrap(), b"preexisting-media");
    }
}

#[tokio::test]
async fn shadow_transaction_failure_and_capacity_hold_then_recover_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let db = open(&path).await;
    db.acquire_lock("capture fault").unwrap().execute_batch(
        "CREATE TRIGGER shadow_receipt_fault BEFORE INSERT ON provider_shadow_receipts BEGIN SELECT RAISE(ABORT,'synthetic receipt failure'); END").unwrap();
    let bound = album(unknown_page(), capture(&db));
    let (events, errors, token) = collect(&bound).await;
    assert!(events.is_empty());
    assert_eq!(errors.len(), 1);
    assert_eq!(token, "saved");
    assert_eq!(
        counts(&db),
        (0, 0, 0),
        "failure after page and record writes must roll back the whole page"
    );
    drop(bound);
    drop(db);
    let db = open(&path).await;
    assert_eq!(counts(&db), (0, 0, 0));
    db.acquire_lock("remove only injected fault")
        .unwrap()
        .execute_batch("DROP TRIGGER shadow_receipt_fault")
        .unwrap();
    let mut full = capture(&db);
    full.capacity = 0;
    let refused = album(unknown_page(), full.clone());
    let (events, errors, token) = collect(&refused).await;
    assert!(events.is_empty());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("inbox is full"));
    assert_eq!(token, "saved");
    assert_eq!(counts(&db), (0, 0, 0));
    drop(refused);
    let recovered = album(unknown_page(), capture(&db));
    assert_eq!(
        collect(&recovered).await,
        (Vec::new(), Vec::new(), "successor".into())
    );
    assert_eq!(counts(&db), (1, 1, 1));
    drop(recovered);
    // Existing exact observations do not need another byte allocation, even
    // when capacity is exhausted. A changed revision must hold, not overwrite.
    assert!(
        collect(&album(unknown_page(), full.clone()))
            .await
            .1
            .is_empty()
    );
    let changed = String::from_utf8(unknown_page())
        .unwrap()
        .replace("v1", "v2")
        .into_bytes();
    assert_eq!(collect(&album(changed, full)).await.2, "saved");
    assert_eq!(counts(&db), (1, 1, 1));
}

#[tokio::test]
async fn shadow_rejects_unusable_or_ambiguous_identity_before_any_page_write() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    let valid: Value = serde_json::from_slice(&unknown_page()).unwrap();
    let mut bodies = Vec::new();
    for value in [Value::Null, json!(""), json!(" ")] {
        let mut page = valid.clone();
        page["zones"][0]["records"][0]["recordName"] = value;
        bodies.push(serde_json::to_vec(&page).unwrap());
    }
    let mut wrong = valid.clone();
    wrong["zones"][0]["records"][0]["zoneID"] = json!({"zoneName":"wrong-zone"});
    bodies.push(serde_json::to_vec(&wrong).unwrap());
    let mut wrong = valid.clone();
    wrong["zones"][0]["records"][0]["zoneID"] =
        json!({"zoneName":"PrimarySync","ownerRecordName":"wrong-owner"});
    bodies.push(serde_json::to_vec(&wrong).unwrap());
    bodies.push(
        String::from_utf8(unknown_page())
            .unwrap()
            .replace(
                "\"recordName\":\"future-source\"",
                "\"recordName\":\"private-other\",\"recordName\":\"future-source\"",
            )
            .into_bytes(),
    );
    for body in bodies {
        let (events, errors, token) = collect(&album(body, capture(&db))).await;
        assert!(events.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(!errors[0].contains("private-other"));
        assert_eq!(token, "saved");
        assert_eq!(counts(&db), (0, 0, 0));
    }
}

#[tokio::test]
async fn shadow_scopes_and_revisions_remain_distinct_and_owner_is_rechecked() {
    let directory = tempfile::tempdir().unwrap();
    let db = open(&directory.path().join("owned.db")).await;
    assert!(
        collect(&album(unknown_page(), capture(&db)))
            .await
            .1
            .is_empty()
    );
    let mut shared = album(unknown_page(), capture(&db));
    shared.set_shadow_capture(capture(&db), Arc::from("shared"));
    assert!(collect(&shared).await.1.is_empty());
    let mut changed: Value = serde_json::from_slice(&unknown_page()).unwrap();
    changed["zones"][0]["zoneID"]["futureMetadata"] = json!({"version":"next"});
    assert!(
        collect(&album(serde_json::to_vec(&changed).unwrap(), capture(&db)))
            .await
            .1
            .is_empty()
    );
    assert_eq!(counts(&db), (3, 3, 2));
    {
        let conn = db
            .acquire_lock("independent stable scope and version facts")
            .unwrap();
        let scopes: Vec<String> = conn
            .prepare("SELECT scope FROM provider_shadow_pages ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            scopes[0], scopes[2],
            "unknown zone metadata remains payload, not identity"
        );
        assert_ne!(scopes[0], scopes[1], "private/shared scopes must not alias");
        let raw: Vec<u8> = conn
            .query_row(
                "SELECT body FROM provider_shadow_pages ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&raw).contains("futureMetadata"));
        conn.execute(
            "UPDATE account_owner SET provider_key=?1",
            [crate::account::provider_fingerprint(
                "com",
                "different-provider",
            )],
        )
        .unwrap();
    }
    let (events, errors, token) = collect(&album(unknown_page(), capture(&db))).await;
    assert!(events.is_empty());
    assert_eq!(token, "saved");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("account ownership does not match"));
    assert!(
        !errors[0].contains("synthetic@example.invalid")
            && !errors[0].contains("different-provider")
    );
    assert_eq!(
        counts(&db),
        (3, 3, 2),
        "wrong owner cannot even acknowledge an existing page"
    );
}

#[derive(Clone)]
struct DelayedBody {
    requested: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl PhotosSession for DelayedBody {
    async fn post(&self, _: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        anyhow::bail!("unexpected value-based request")
    }
    async fn post_changes_body(
        &self,
        _: &str,
        _: String,
        _: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        self.requested.notify_one();
        self.release.notified().await;
        Ok(unknown_page())
    }
    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn shadow_cancelled_receiver_before_capture_preserves_page_and_legacy_cursor() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let db = open(&path).await;
    db.set_metadata("sync_token:PrimarySync", "saved")
        .await
        .unwrap();
    let requested = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut bound = make_album_with_session(
        100,
        Box::new(DelayedBody {
            requested: requested.clone(),
            release: release.clone(),
        }),
    );
    bound.set_shadow_capture(capture(&db), Arc::from("private"));
    let (stream, token) = bound.changes_stream("saved");
    tokio::time::timeout(std::time::Duration::from_secs(5), requested.notified())
        .await
        .unwrap();
    drop(stream);
    release.notify_one();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), token)
            .await
            .unwrap()
            .unwrap(),
        "saved"
    );
    assert_eq!(counts(&db), (0, 0, 0));
    drop(bound);
    drop(db);
    let db = open(&path).await;
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved")
    );
    assert!(
        collect(&album(unknown_page(), capture(&db)))
            .await
            .1
            .is_empty()
    );
    assert_eq!(counts(&db), (1, 1, 1));
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved"),
        "capture is not checkpoint authority"
    );
}
