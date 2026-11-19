use crate::icloud::photos::album::test_support::make_album_with_session;
use crate::icloud::photos::inbox::ShadowCapture;
use crate::icloud::photos::session::PhotosSession;
use crate::state::SqliteStateDb;
use crate::state::db::account::AccountOwner;
use crate::test_helpers::{assert_no_retired_inbox_archive, single_recovery_bundle_connection};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_stream::StreamExt;

#[derive(Clone)]
pub(super) struct BodySession(pub(super) Vec<u8>);

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

pub(super) fn owner() -> AccountOwner {
    AccountOwner::authenticated(
        "synthetic@example.invalid",
        "com",
        &serde_json::from_value(json!({"dsInfo":{"dsid":"synthetic-provider"}})).unwrap(),
    )
    .unwrap()
}

pub(super) async fn open(path: &std::path::Path) -> Arc<SqliteStateDb> {
    Arc::new(SqliteStateDb::open_owned(path, &owner()).await.unwrap())
}

pub(super) fn capture(db: &Arc<SqliteStateDb>) -> ShadowCapture {
    ShadowCapture::new(Arc::clone(db), owner(), "com")
}

pub(super) fn album(body: Vec<u8>, capture: ShadowCapture) -> crate::icloud::photos::PhotoAlbum {
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

pub(super) async fn collect(
    album: &crate::icloud::photos::PhotoAlbum,
) -> (Vec<String>, Vec<String>, String) {
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
async fn shadow_source_payload_roundtrip_preserves_history_and_current_replay_parity() {
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
    assert_eq!(counts(&db), (1, 5, 0));
    // Synthetic version rewind exercises migration re-entry with observations
    // already present, as the established earlier-version controls do.
    db.acquire_lock("synthetic migration re-entry")
        .unwrap()
        .pragma_update(None, "user_version", 28)
        .unwrap();
    drop(bound);
    drop(db);
    for _ in 0..2 {
        let db = open(&path).await;
        let original = single_recovery_bundle_connection(directory.path());
        let stored: Vec<u8> = original
            .query_row("SELECT body FROM provider_shadow_pages", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, body);
        assert!(String::from_utf8_lossy(&stored).contains("9007199254740993.0"));
        {
            let conn = db.acquire_lock("scope and ordinal facts").unwrap();
            assert_no_retired_inbox_archive(&conn);
            let (scope, request, successor, account, provider): (String,String,String,String,String) = original.query_row(
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
            let mut query = original.prepare("SELECT ordinal,record_name,record_type,deleted FROM provider_shadow_records ORDER BY ordinal").unwrap();
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
            (1, 5, 0),
            "exact observed-page replay must be idempotent"
        );
        let conn = db.acquire_lock("retired checkpoint bytes").unwrap();
        crate::test_helpers::assert_checkpoint_evidence(&conn, "sync_token:PrimarySync", "saved");
        crate::test_helpers::assert_checkpoint_evidence(
            &conn,
            "pending_sync_token:old-epoch:PrimarySync",
            "retained-debt",
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
        "CREATE TRIGGER shadow_receipt_fault AFTER INSERT ON provider_shadow_records BEGIN SELECT RAISE(ABORT,'synthetic identity write failure'); END").unwrap();
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
        .execute_batch("DROP TRIGGER shadow_receipt_fault;
            CREATE TRIGGER obsolete_receipt_fault BEFORE INSERT ON provider_shadow_receipts BEGIN SELECT RAISE(ABORT,'obsolete pointer must not be written'); END;")
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
    assert_eq!(counts(&db), (1, 1, 0));
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
    assert_eq!(counts(&db), (1, 1, 0));
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
    assert_eq!(counts(&db), (3, 3, 0));
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
        (3, 3, 0),
        "wrong owner cannot even acknowledge an existing page"
    );
}

#[derive(Clone)]
struct DelayedBody {
    requested: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    retry_after: Option<std::time::Duration>,
    attempts: Arc<AtomicUsize>,
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
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.requested.notify_one();
        if let Some(delay) = self.retry_after {
            return Err(crate::icloud::photos::session::HttpStatusError {
                status: 503,
                url: "https://example.invalid".to_owned(),
                retry_after: Some(delay),
                body: None,
            }
            .into());
        }
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
            retry_after: None,
            attempts: Arc::new(AtomicUsize::new(0)),
        }),
    );
    bound.set_shadow_capture(capture(&db), Arc::from("private"));
    let (stream, mut token) = bound.changes_stream("saved");
    tokio::time::timeout(std::time::Duration::from_secs(5), requested.notified())
        .await
        .unwrap();
    drop(stream);
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut token).await;
    let drained = result.is_ok();
    // Unblock and join the old producer on red before checking the drain bound.
    release.notify_one();
    let token = match result {
        Ok(token) => token,
        Err(_) => tokio::time::timeout(std::time::Duration::from_secs(5), &mut token)
            .await
            .expect("released provider must finish cleanup"),
    };
    assert_eq!(token.unwrap(), "saved");
    assert!(drained, "receiver closure must cancel the pending request");
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
    assert_eq!(counts(&db), (1, 1, 0));
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved"),
        "capture is not checkpoint authority"
    );
}

#[tokio::test(start_paused = true)]
async fn issue765_receiver_close_cancels_scheduled_retry_after() {
    use std::time::Duration;

    const RETRY_DELAY: Duration = Duration::from_secs(60);
    const DRAIN_BOUND: Duration = Duration::from_secs(1);
    let requested = Arc::new(tokio::sync::Notify::new());
    let attempts = Arc::new(AtomicUsize::new(0));
    let (events, _default) = crate::test_helpers::TracingCapture::install();
    let bound = make_album_with_session(
        100,
        Box::new(DelayedBody {
            requested: requested.clone(),
            release: Arc::new(tokio::sync::Notify::new()),
            retry_after: Some(RETRY_DELAY),
            attempts: attempts.clone(),
        }),
    );
    let started = tokio::time::Instant::now();
    let (stream, mut token) = bound.changes_stream("saved");
    tokio::time::timeout(DRAIN_BOUND, requested.notified())
        .await
        .unwrap();
    assert!(events.contains_event(|event| {
        event.field("retry_delay_secs") == Some("60") && event.field("attempt") == Some("1")
    }));
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    drop(stream);
    let result = tokio::time::timeout(DRAIN_BOUND, &mut token).await;
    let drained = result.is_ok();
    let token = match result {
        Ok(token) => token,
        Err(_) => tokio::time::timeout(RETRY_DELAY * 4, &mut token)
            .await
            .expect("virtual-time cleanup joins the old retry loop"),
    };
    assert!(
        drained,
        "receiver closure must drain before Retry-After expires"
    );
    assert_eq!(token.unwrap(), "saved");
    assert!(started.elapsed() < RETRY_DELAY);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// A completed scan may return many record-empty continuation pages. They
/// carry no source identities, references or debt, so their admission charge
/// must be released while earlier unresolved evidence and its catalogue debt
/// stay durable. The bound is smaller than the combined empty-page bodies, so
/// retaining them as charged observations would stop the scan.
const RECLAMATION_INBOX_BUDGET: u64 = 12 * 1024;
const RECLAMATION_EMPTY_PAGES: usize = 5;
const RECLAMATION_CHANGED_URL: &str = "https://p77.icloud-content.com/changed.jpg";

fn empty_continuation(index: usize) -> Vec<u8> {
    serde_json::to_vec(&json!({"zones": [{
        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
        "syncToken": format!("empty-{index}"), "moreComing": true, "records": [],
        "quietPayloadPadding": "x".repeat(4 * 1024),
    }]}))
    .unwrap()
}

fn finished_page(changed: bool) -> Vec<u8> {
    use crate::icloud::photos::album::test_support::{changes_asset, changes_master};
    let mut master = changes_master("known-master");
    let mut asset = changes_asset("known-asset", "known-master");
    if changed {
        master["recordChangeTag"] = json!("ct-changed");
        master["fields"]["resOriginalRes"]["value"]["downloadURL"] = json!(RECLAMATION_CHANGED_URL);
        master["fields"]["resOriginalRes"]["value"]["fileChecksum"] = json!("changed-checksum");
        asset["recordChangeTag"] = json!("ct-changed-asset");
        asset["fields"]["assetDate"]["value"] = json!(1_700_000_001_000i64);
    }
    serde_json::to_vec(&json!({"zones": [{
        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
        "syncToken": "finished", "moreComing": false, "records": [master, asset],
    }]}))
    .unwrap()
}

/// The shared unresolved fixture is a single-page response; the reclamation
/// scan continues past it before the record-empty continuations arrive.
fn unresolved_page() -> Vec<u8> {
    String::from_utf8(unknown_page())
        .unwrap()
        .replace("\"moreComing\":false", "\"moreComing\":true")
        .into_bytes()
}

fn quiet_page() -> Vec<u8> {
    serde_json::to_vec(&json!({"zones": [{
        "zoneID": {"zoneName": "PrimarySync", "ownerRecordName": "_defaultOwner"},
        "syncToken": "quiet-successor", "moreComing": false, "records": [],
        "quietPayloadPadding": "y".repeat(2 * 1024),
    }]}))
    .unwrap()
}

fn reclamation_page(token: &str, phase: u8) -> anyhow::Result<Vec<u8>> {
    if token == "saved" {
        return Ok(unresolved_page());
    }
    if token == "successor" {
        return Ok(empty_continuation(1));
    }
    if let Some(index) = token.strip_prefix("empty-") {
        let Ok(index) = index.parse::<usize>() else {
            anyhow::bail!("unexpected reclamation continuation");
        };
        if index < RECLAMATION_EMPTY_PAGES {
            return Ok(empty_continuation(index + 1));
        }
        if index == RECLAMATION_EMPTY_PAGES {
            return Ok(finished_page(phase != 0));
        }
    }
    if token == "finished" && phase == 2 {
        return Ok(quiet_page());
    }
    anyhow::bail!("unexpected reclamation replay cursor")
}

#[derive(Clone)]
struct ReclamationSession {
    phase: u8,
    requests: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl PhotosSession for ReclamationSession {
    async fn post(&self, _: &str, _: String, _: &[(&str, &str)]) -> anyhow::Result<Value> {
        anyhow::bail!("unexpected value-based request")
    }

    async fn post_changes_body(
        &self,
        _: &str,
        body: String,
        _: &[(&str, &str)],
    ) -> anyhow::Result<Vec<u8>> {
        let request: Value = serde_json::from_str(&body)?;
        let token = request
            .pointer("/zones/0/syncToken")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.requests.lock().unwrap().push(token.clone());
        reclamation_page(&token, self.phase)
    }

    fn clone_box(&self) -> Box<dyn PhotosSession> {
        Box::new(self.clone())
    }
}

fn reclamation_album(
    session: ReclamationSession,
    capture: ShadowCapture,
) -> crate::icloud::photos::PhotoAlbum {
    let mut album = make_album_with_session(100, Box::new(session));
    album.set_shadow_capture(capture, Arc::from("private"));
    album
}

fn bounded_capture(db: &Arc<SqliteStateDb>) -> ShadowCapture {
    let mut capture = capture(db);
    capture.capacity = RECLAMATION_INBOX_BUDGET;
    capture
}

/// Capacity accounting is the live inbox: pages whose evidence was already
/// released never reserve budget again.
fn live_inbox_charge(db: &SqliteStateDb) -> i64 {
    db.acquire_lock("live reclamation inbox charge")
        .unwrap()
        .query_row(
            "SELECT COALESCE(SUM(p.charged_bytes),0) FROM provider_shadow_pages p WHERE NOT EXISTS(SELECT 1 FROM provider_finished_changes f WHERE f.page_id=p.id)",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

fn page_charge(db: &SqliteStateDb, cursor: &str, marker: &str) -> i64 {
    db.acquire_lock("retained reclamation page charge")
        .unwrap()
        .query_row(
            "SELECT charged_bytes FROM provider_shadow_pages WHERE request_cursor=?1 AND instr(CAST(body AS TEXT),?2)>0",
            rusqlite::params![cursor, marker],
            |row| row.get(0),
        )
        .unwrap()
}

fn live_pages_without_identities(db: &SqliteStateDb) -> i64 {
    db.acquire_lock("empty reclamation pages")
        .unwrap()
        .query_row(
            "SELECT count(*) FROM provider_shadow_pages p WHERE NOT EXISTS(SELECT 1 FROM provider_shadow_records r WHERE r.page_id=p.id) AND NOT EXISTS(SELECT 1 FROM provider_finished_changes f WHERE f.page_id=p.id)",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

async fn collect_urls(
    album: &crate::icloud::photos::PhotoAlbum,
    cursor: &str,
) -> (Vec<String>, Vec<String>, String) {
    let (stream, token) = album.changes_stream(cursor);
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

#[tokio::test]
async fn reclaimed_empty_continuations_keep_raw_debt_and_replay_a_changed_record() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("owned.db");
    let db = open(&path).await;
    db.set_metadata("sync_token:PrimarySync", "saved")
        .await
        .unwrap();
    db.set_metadata("pending_sync_token:old-epoch:PrimarySync", "retained-debt")
        .await
        .unwrap();

    let mut expected_requests = vec!["saved".to_owned(), "successor".to_owned()];
    expected_requests.extend((1..=RECLAMATION_EMPTY_PAGES).map(|index| format!("empty-{index}")));

    let first_requests = Arc::new(Mutex::new(Vec::new()));
    let first = reclamation_album(
        ReclamationSession {
            phase: 0,
            requests: first_requests.clone(),
        },
        bounded_capture(&db),
    );
    let (first_urls, first_errors, first_token) = collect_urls(&first, "saved").await;
    assert!(first_errors.is_empty(), "{first_errors:?}");
    assert_eq!(first_token, "finished");
    assert!(first_urls.iter().any(|url| url.contains("photo.jpg")));
    assert!(!first_urls.iter().any(|url| url == RECLAMATION_CHANGED_URL));
    assert_eq!(*first_requests.lock().unwrap(), expected_requests);

    let unresolved_charge = page_charge(&db, "saved", "future-source");
    let finished_charge = page_charge(&db, "empty-5", "photo.jpg");
    assert!(unresolved_charge > 0);
    assert!(finished_charge > 0);
    let retained_charge = unresolved_charge + finished_charge;
    assert!(u64::try_from(retained_charge).unwrap() <= RECLAMATION_INBOX_BUDGET);
    assert_eq!(live_inbox_charge(&db), retained_charge);
    assert_eq!(counts(&db), (2, 3, 0));
    assert_eq!(
        live_pages_without_identities(&db),
        0,
        "record-empty pages must not keep live inbox space"
    );
    let empty_bodies: u64 = (1..=RECLAMATION_EMPTY_PAGES)
        .map(|index| empty_continuation(index).len() as u64)
        .sum();
    assert!(
        empty_bodies > RECLAMATION_INBOX_BUDGET,
        "the continuation bodies alone exceed the whole inbox budget"
    );
    {
        let conn = db
            .acquire_lock("retained unresolved source and debt")
            .unwrap();
        let (page_id, body): (i64, Vec<u8>) = conn
            .query_row(
                "SELECT id,body FROM provider_shadow_pages WHERE request_cursor='saved'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(body, unresolved_page());
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_shadow_records WHERE page_id=?1",
                [page_id],
                |row| row.get(0),
            )
            .unwrap(),
            1
        );
        let (target, zone_name, zone_owner): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT target_record_name,target_zone_name,target_zone_owner FROM provider_catalog_references WHERE page_id=?1 AND target_record_name='target'",
                [page_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (target.as_str(), zone_name.as_deref(), zone_owner.as_deref()),
            ("target", Some("other-zone"), Some("other-owner"))
        );
        assert!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_catalog_debt WHERE page_id=?1 AND reason='unresolved_reference'",
                [page_id],
                |row| row.get(0),
            )
            .unwrap()
                > 0
        );
        crate::test_helpers::assert_checkpoint_evidence(&conn, "sync_token:PrimarySync", "saved");
        crate::test_helpers::assert_checkpoint_evidence(
            &conn,
            "pending_sync_token:old-epoch:PrimarySync",
            "retained-debt",
        );
    }
    drop(first);
    drop(db);

    // Shutdown/reopen keeps the old checkpoint and the retained evidence. The
    // replay starts there and reaches a later page whose record changed.
    let db = open(&path).await;
    assert_eq!(
        db.get_metadata("sync_token:PrimarySync")
            .await
            .unwrap()
            .as_deref(),
        Some("saved")
    );
    assert_eq!(counts(&db), (2, 3, 0));
    assert_eq!(live_inbox_charge(&db), retained_charge);
    let replay_requests = Arc::new(Mutex::new(Vec::new()));
    let replay = reclamation_album(
        ReclamationSession {
            phase: 1,
            requests: replay_requests.clone(),
        },
        bounded_capture(&db),
    );
    let (replay_urls, replay_errors, replay_token) = collect_urls(&replay, "saved").await;
    assert!(replay_errors.is_empty(), "{replay_errors:?}");
    assert_eq!(replay_token, "finished");
    assert_eq!(*replay_requests.lock().unwrap(), expected_requests);
    assert!(
        replay_urls.iter().any(|url| url == RECLAMATION_CHANGED_URL),
        "the old checkpoint replay must observe the changed subsequent record"
    );
    let changed_charge = page_charge(&db, "empty-5", RECLAMATION_CHANGED_URL);
    assert!(changed_charge > 0);
    assert_eq!(
        live_inbox_charge(&db),
        retained_charge + changed_charge,
        "empty continuations contribute nothing to the live inbox"
    );
    assert_eq!(counts(&db), (3, 5, 0));
    assert_eq!(live_pages_without_identities(&db), 0);
    {
        let conn = db
            .acquire_lock("changed record retained with original debt")
            .unwrap();
        assert_eq!(
            conn.query_row::<Vec<u8>, _, _>(
                "SELECT body FROM provider_shadow_pages WHERE request_cursor='saved'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
            unresolved_page()
        );
        assert!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_catalog_debt d JOIN provider_shadow_pages p ON p.id=d.page_id WHERE p.request_cursor='saved'",
                [],
                |row| row.get(0),
            )
            .unwrap()
                > 0
        );
        crate::test_helpers::assert_checkpoint_evidence(&conn, "sync_token:PrimarySync", "saved");
    }
    drop(replay);
    drop(db);

    // A quiet page after another restart is released again instead of turning
    // its admission charge into a permanent one.
    let db = open(&path).await;
    let quiet_requests = Arc::new(Mutex::new(Vec::new()));
    let quiet = reclamation_album(
        ReclamationSession {
            phase: 2,
            requests: quiet_requests.clone(),
        },
        bounded_capture(&db),
    );
    let (quiet_urls, quiet_errors, quiet_token) = collect_urls(&quiet, "finished").await;
    assert!(quiet_errors.is_empty(), "{quiet_errors:?}");
    assert!(quiet_urls.is_empty());
    assert_eq!(quiet_token, "quiet-successor");
    assert_eq!(*quiet_requests.lock().unwrap(), ["finished".to_owned()]);
    assert_eq!(live_inbox_charge(&db), retained_charge + changed_charge);
    assert_eq!(counts(&db), (3, 5, 0));
    assert_eq!(live_pages_without_identities(&db), 0);
    assert_eq!(
        db.acquire_lock("quiet page not retained")
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT count(*) FROM provider_shadow_pages p WHERE instr(CAST(p.body AS TEXT),'quietPayloadPadding')>0 AND NOT EXISTS(SELECT 1 FROM provider_finished_changes f WHERE f.page_id=p.id)",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        0
    );
}
